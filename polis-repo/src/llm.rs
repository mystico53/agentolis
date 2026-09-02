//! Model-written district descriptions (PRD §12), and the accounting,
//! caching and degradation that make them safe to turn on.
//!
//! # Why this exists
//!
//! [`crate::describe`] derives a district's description by *quoting* the
//! repository — a README, a manifest `description`, a module doc comment.
//! `docs/design/NEIGHBORHOODS-REVIEW.md` measured what that actually achieves on
//! the operator's own repositories, and the answer was not what ADR-0087
//! estimated: **23 prose descriptions across 160 districts (14 %), of which 10
//! told the reader something the label did not.** Nine of the 23 merely restated
//! the folder name — `services/settings` → "The settings entry contract".
//!
//! The shortfall is structural, not a missing rule: *most directories in a
//! working repository contain no sentence saying what they are.* `biwt` has 20
//! districts and zero prose in the entire checkout. No extractor can quote what
//! nobody wrote.
//!
//! So a model writes the ones the repository does not. This is a deliberate,
//! operator-approved bend in PRD §2's local-only stance and it is fenced in
//! every direction:
//!
//! * **Names and documentation leave the machine. Source code bodies never do.**
//!   The payload is a district's path, its file and subdirectory *names*, its
//!   kind mix, and the doc snippets [`crate::describe`] already extracted — all
//!   of which are already sanitised. See [`prompt`].
//! * **Nothing leaves without passing [`outbound`]**, which applies
//!   [`crate::describe::looks_like_secret`] on the way *out* as well as in.
//! * **Nothing is called implicitly.** A render, a snapshot and
//!   [`crate::tree::RepoIndex::neighborhoods_described`] read the cache and
//!   never open a socket. Generation is [`run::LlmRunner::run`], which a caller
//!   invokes on purpose or on a background thread.
//! * **Every failure degrades to the derived description, and then to nothing.**
//!   No key, no network, a dead port, a 429, a timeout, a malformed body, a
//!   refusal: all of them are [`run::RunReport::errors`] and a map that looks
//!   exactly like the one you get with the feature switched off.
//!
//! # The layout never sees any of this
//!
//! PRD §7.4 requires byte-identical geometry on every machine and every launch.
//! A description is *text hung on a district*, not an input to the partition or
//! the layout: [`crate::neighborhoods::Neighborhoods::build`] is a pure function
//! of the file list and runs before anything here. The worst a model can do to
//! the map is change a caption.
//!
//! # Reading order
//!
//! | Module | What it owns |
//! |---|---|
//! | [`secret`] | The key. Read from the environment, never written anywhere. |
//! | [`transport`] | Bytes over a socket. One trait, two shipped implementations. |
//! | [`provider`] | Request and response shapes. `GLM`/Z.ai, Anthropic, Ollama. |
//! | [`prompt`] | What the model is shown, and what it is told to refuse. |
//! | [`outbound`] | The last gate before anything is sent. |
//! | [`cache`] | Identity, fingerprints, drift, and staleness as a state. |
//! | [`run`] | Planning, dry-run pricing, concurrency, retries, accounting. |

pub mod cache;
pub mod outbound;
pub mod prompt;
pub mod provider;
pub mod run;
pub mod secret;
pub mod transport;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use cache::{Drift, DriftCause, Freshness, ModelCache, Sketch};
pub use provider::{ChatReply, Provider, Usage};
pub use run::{
    apply_cached_model_descriptions, describe_with_model, Candidacy, LlmRunner, Plan, RunMode,
    RunReport,
};
pub use secret::Secret;
pub use transport::{HttpRequest, HttpResponse, Transport, TransportError};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything that can go wrong on the way to a description.
///
/// Every variant is *recoverable by doing nothing*: the district keeps whatever
/// [`crate::describe`] derived for it, or keeps `None`. There is no variant that
/// a caller has to handle to stay correct, which is the whole point.
///
/// **No variant may carry an API key.** The one place a key could plausibly
/// arrive in an error string is a transport's captured stderr, and
/// [`secret::scrub`] is applied there before the string is ever constructed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LlmError {
    /// The configured environment variable holds nothing. Not an error the
    /// operator did anything wrong to cause — it is the default state, and the
    /// correct response is a dry run.
    #[error("no API key: none of {0} is set in the environment")]
    NoKey(String),
    /// The transport could not complete the request.
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    /// The endpoint answered with a status that is not 2xx.
    #[error("http {status}: {message}")]
    Status {
        /// The HTTP status code.
        status: u16,
        /// The provider's own error message, bounded and sanitised.
        message: String,
    },
    /// The body parsed as JSON but was not the shape the provider promises.
    #[error("malformed response: {0}")]
    Malformed(String),
    /// The model answered, but with prose rather than the requested JSON, or
    /// with a refusal. Distinguished from [`LlmError::Malformed`] because it is
    /// a *model* outcome and belongs in the report differently.
    #[error("unusable answer: {0}")]
    Unusable(String),
    /// The configuration cannot produce a request at all.
    #[error("configuration: {0}")]
    Config(String),
}

impl LlmError {
    /// True when trying the same request again could plausibly succeed.
    ///
    /// A 429 and a 5xx are the endpoint asking for patience; a 400 or a 401 is
    /// the endpoint saying the request is wrong, and repeating it wastes the
    /// operator's money and time. A malformed body is retried once because the
    /// common cause is a truncated stream, not a broken provider.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(e) => e.is_retryable(),
            Self::Status { status, .. } => {
                *status == 408 || *status == 409 || *status == 429 || *status >= 500
            }
            Self::Malformed(_) => true,
            Self::NoKey(_) | Self::Unusable(_) | Self::Config(_) => false,
        }
    }

    /// A short, stable tag for the report.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::NoKey(_) => "no-key",
            Self::Transport(_) => "transport",
            Self::Status { .. } => "http-status",
            Self::Malformed(_) => "malformed",
            Self::Unusable(_) => "unusable",
            Self::Config(_) => "config",
        }
    }
}

// ---------------------------------------------------------------------------
// Pricing
// ---------------------------------------------------------------------------

/// What a million tokens costs, in US dollars.
///
/// Carried in the configuration rather than compiled in, because it is the one
/// number in this module with an expiry date: the shipped default is
/// `GLM-5.3-Flash`'s **promotional** rate, half of list, and the promotion ends
/// on 2026-09-09. A price that is wrong makes the accounting a lie, and the
/// accounting exists precisely so the operator does not have to trust an
/// estimate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price {
    /// USD per million input tokens.
    pub usd_per_m_input: f64,
    /// USD per million output tokens.
    pub usd_per_m_output: f64,
}

impl Price {
    /// `GLM-5.3-Flash` on Z.ai, at the promotional rate in force on 2026-09-01
    /// (50 % off the $0.15 / $0.50 list, ending 2026-09-09).
    pub const GLM_FLASH_PROMO: Self = Self {
        usd_per_m_input: 0.075,
        usd_per_m_output: 0.25,
    };

    /// `GLM-5.3-Flash` list price, for after the promotion ends.
    pub const GLM_FLASH_LIST: Self = Self {
        usd_per_m_input: 0.15,
        usd_per_m_output: 0.50,
    };

    /// A local endpoint costs nothing.
    pub const FREE: Self = Self {
        usd_per_m_input: 0.0,
        usd_per_m_output: 0.0,
    };

    /// What `usage` costs at this price.
    #[allow(clippy::cast_precision_loss)] // token counts are far below 2^53
    pub fn cost(self, usage: Usage) -> f64 {
        (usage.input_tokens as f64 / 1_000_000.0) * self.usd_per_m_input
            + (usage.output_tokens as f64 / 1_000_000.0) * self.usd_per_m_output
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The whole feature's configuration, and the reason a different provider is a
/// config line rather than a rewrite.
///
/// Read from `<repo>/.polis/llm.json` by [`LlmConfig::for_repo`], beside the
/// `neighborhoods.json` that [`crate::neighborhoods::NeighborhoodConfig`]
/// already reads. `.polis` is in `WalkExclusions`' shipped default (ADR-0065),
/// so a configuration file cannot become a building.
///
/// **The key itself is never in here.** [`LlmConfig::key_env`] is the *name* of
/// an environment variable, which is what lets the same struct describe a
/// hosted endpoint that needs a key and a local Ollama that needs none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    /// Off unless the operator says otherwise. Nothing in Polis turns this on
    /// for them.
    pub enabled: bool,
    /// Which request and response shape to speak.
    pub provider: Provider,
    /// The model id, exactly as the provider spells it.
    pub model: String,
    /// The API root, without a trailing slash. The provider appends its own
    /// path — `/chat/completions`, `/v1/messages`.
    pub base_url: String,
    /// Environment variables to read the key from, in order. **Names, not
    /// values.** An empty list means "this endpoint needs no key", which is the
    /// normal case for a local Ollama.
    pub key_env: Vec<String>,
    /// Per-request wall clock ceiling.
    pub timeout_secs: u64,
    /// Retries *after* the first attempt. `2` means at most three tries.
    pub max_retries: u32,
    /// First backoff, doubling per retry. Zero in tests.
    pub retry_base_ms: u64,
    /// Requests in flight at once. A hard cap, not a target.
    pub max_concurrency: usize,
    /// Districts per request. Cold start batches; incremental updates are one
    /// district each and land in one batch anyway.
    pub batch_size: usize,
    /// Districts one run may describe, whatever the plan says. The stop that is
    /// not a price: a repository that suddenly grows 4 000 districts should
    /// fail loudly rather than bill quietly.
    pub max_districts_per_run: u32,
    /// File names shown to the model per district, before truncation.
    pub max_names_per_district: usize,
    /// What a million tokens costs. See [`Price`].
    pub price: Price,
    /// How far a district's file names may drift before its description is
    /// [`Freshness::Stale`]. In parts per thousand; see
    /// [`cache::DEFAULT_DRIFT_THRESHOLD_PERMILLE`] for the measurement behind
    /// the default.
    pub drift_threshold_permille: u16,
    /// Ask for a JSON object with `response_format`.
    ///
    /// `models.dev` reports that `glm-5.3-flash` supports structured output, and
    /// Z.ai's own documentation does not describe the syntax. This flag exists
    /// so an operator whose endpoint rejects the field can turn it off without
    /// a rebuild; the parser in [`prompt::parse_reply`] is tolerant either way
    /// and never depends on it.
    pub request_json_object: bool,
    /// Districts whose description came from a doc comment are re-described when
    /// they hold more files than this.
    ///
    /// The review's finding, made into a number: `src/services` (223 files) was
    /// labelled "Centralized Window Management Service" from one of them, and
    /// `src/hooks` (46 files) "useDropZone Hook". A doc comment on a directory's
    /// anchor file speaks for a small directory and not for a large one.
    pub doc_comment_trust_max_files: u32,
    /// Who chose [`LlmConfig::base_url`]. Never read from or written to the
    /// file; see [`ConfigOrigin`].
    #[serde(skip)]
    pub origin: ConfigOrigin,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: Provider::Glm,
            model: "glm-5.3-flash".to_owned(),
            base_url: "https://api.z.ai/api/paas/v4".to_owned(),
            key_env: vec!["ZAI_API_KEY".to_owned(), "GLM_API_KEY".to_owned()],
            timeout_secs: 60,
            max_retries: 2,
            retry_base_ms: 500,
            max_concurrency: 4,
            batch_size: 8,
            max_districts_per_run: 400,
            max_names_per_district: 60,
            price: Price::GLM_FLASH_PROMO,
            drift_threshold_permille: cache::DEFAULT_DRIFT_THRESHOLD_PERMILLE,
            request_json_object: true,
            doc_comment_trust_max_files: 25,
            origin: ConfigOrigin::Operator,
        }
    }
}

/// The configuration file's path inside a checkout.
pub fn config_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".polis").join("llm.json")
}

/// Who chose the endpoint a key would be sent to.
///
/// Not a serialised field: a repository cannot promote its own configuration by
/// writing `"origin": "operator"` into it, because the field is
/// `#[serde(skip)]` and a file that mentions it is rejected outright by
/// `deny_unknown_fields`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConfigOrigin {
    /// Built in this process, or from a flag the operator typed. Trusted to
    /// name the host their key goes to.
    #[default]
    Operator,
    /// Read out of `<repo>/.polis/llm.json` — a file that arrives with a clone
    /// and that the operator has usually never opened.
    Repository,
}

/// Why a key that *is* present in the environment was not attached to a
/// request.
///
/// The key is still in the environment and still readable by
/// [`LlmConfig::key`]; what this records is that
/// [`LlmConfig::key_destination`] refused to let it leave for this particular
/// endpoint. Both variants name a host, never a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyWithheld {
    /// The endpoint is `http://` and the host is not loopback, so the
    /// `Authorization` header would cross the network in the clear.
    Cleartext {
        /// The host that would have received it.
        host: String,
    },
    /// The repository's own configuration named a host the operator never
    /// chose. A clone that ships a `.polis/llm.json` must not be able to
    /// redirect somebody's key to a collector.
    RepoRedirect {
        /// The host the repository asked for.
        host: String,
        /// The host the provider's own default names.
        expected: String,
    },
}

impl std::fmt::Display for KeyWithheld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cleartext { host } => write!(
                f,
                "the API key was withheld: {host} would receive it over http://, in the clear. \
                 Use https://, or a loopback address for a local model."
            ),
            Self::RepoRedirect { host, expected } => write!(
                f,
                "the API key was withheld: this repository's .polis/llm.json points at {host}, \
                 which is not the provider's own {expected}. A repository does not get to choose \
                 where your key goes — pass --base-url yourself if you meant it."
            ),
        }
    }
}

/// True for a host that never leaves the machine.
///
/// Names as well as literals: a local Ollama is reached at `localhost` at least
/// as often as at `127.0.0.1`, and `::1` is normal on a dual-stack box.
fn is_loopback_host(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        // A name that is not an IP literal and is not `localhost` resolves
        // wherever DNS says, which is not something this can vouch for.
        Err(_) => false,
    }
}

impl LlmConfig {
    /// Reads `<repo>/.polis/llm.json`, or the shipped defaults.
    ///
    /// Never an error: an absent, unreadable or malformed file is the same
    /// situation as no file, and the same situation as a fresh checkout. A
    /// malformed one is logged at debug so `polis doctor` can say so.
    pub fn for_repo(repo_root: &Path) -> Self {
        Self::load(&config_path(repo_root))
    }

    /// [`LlmConfig::for_repo`] for an explicit path.
    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return Self::default();
        };
        match serde_json::from_slice::<Self>(&bytes) {
            // The file came out of a checkout, so it does not get to name the
            // host a key is sent to. See `key_destination`.
            Ok(mut config) => {
                config.origin = ConfigOrigin::Repository;
                config
            }
            Err(error) => {
                tracing::debug!(%error, path = %path.display(), "ignoring a malformed llm.json");
                Self::default()
            }
        }
    }

    /// Marks this configuration as one the operator chose themselves.
    ///
    /// For a caller that has just applied a `--base-url` or `--key-env` the
    /// operator typed: typing the host *is* the act of choosing it, so the
    /// repository-redirect rule no longer applies.
    #[must_use]
    pub fn chosen_by_operator(mut self) -> Self {
        self.origin = ConfigOrigin::Operator;
        self
    }

    /// Whether a key may be attached to a request for this configuration's
    /// endpoint, and why not when it may not.
    ///
    /// # Two ways a key leaves without anyone deciding it should
    ///
    /// The rest of this module keeps a key out of files, logs, argument lists
    /// and `Debug` output. Those all assume the key reaches the *right server
    /// over a protected channel*. Two configurations break that assumption, and
    /// neither of them is exotic:
    ///
    /// 1. **`http://` to somewhere that is not this machine.** The header is
    ///    then readable by every hop in between. A local Ollama is the reason
    ///    plain HTTP is supported at all, and a local Ollama is on loopback.
    /// 2. **A host the repository picked.** `.polis/llm.json` arrives with a
    ///    clone. A file in it saying `base_url: "https://collector.example"`
    ///    with `key_env: ["ZAI_API_KEY"]` would post the operator's key to a
    ///    stranger, over TLS, with no error and no prompt.
    ///
    /// Both return the key to `None`, which is a state the whole feature
    /// already degrades through cleanly, and both are reported rather than
    /// silently applied.
    pub fn key_destination(&self) -> Result<(), KeyWithheld> {
        let endpoint = self.endpoint();
        // An endpoint that will not parse cannot be reached at all; `validate`
        // is what reports that, and there is nothing to withhold from it.
        let Ok(parts) = transport::parse_url(&endpoint) else {
            return Ok(());
        };
        if parts.scheme == "http" && !is_loopback_host(&parts.host) {
            return Err(KeyWithheld::Cleartext { host: parts.host });
        }
        if self.origin == ConfigOrigin::Repository {
            let default = self.provider.default_base_url();
            let expected = transport::parse_url(&format!("{}{}", default, self.provider.path()))
                .map(|p| p.host)
                .unwrap_or_default();
            // A repository may still point at a model on this machine: that
            // reaches nobody, and it is how a checked-in Ollama setup works.
            if !expected.is_empty()
                && !parts.host.eq_ignore_ascii_case(&expected)
                && !is_loopback_host(&parts.host)
            {
                return Err(KeyWithheld::RepoRedirect {
                    host: parts.host,
                    expected,
                });
            }
        }
        Ok(())
    }

    /// The defaults for one provider, keeping everything else.
    ///
    /// This is the "a different model is a config line" claim, executable: it
    /// swaps the base URL, the model, the key variable and the price together,
    /// because changing one without the others is the failure this exists to
    /// prevent.
    #[must_use]
    pub fn with_provider(mut self, provider: Provider) -> Self {
        self.provider = provider;
        provider.default_model().clone_into(&mut self.model);
        provider.default_base_url().clone_into(&mut self.base_url);
        self.key_env = provider
            .default_key_env()
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        self.price = provider.default_price();
        self
    }

    /// The key, from the first of [`LlmConfig::key_env`] that is set and
    /// non-empty.
    ///
    /// `None` is a normal state, not a failure: a local endpoint needs no key,
    /// and a hosted one with no key present degrades to a dry run.
    pub fn key(&self) -> Option<Secret> {
        Secret::from_env(&self.key_env)
    }

    /// A description of where the key would come from, for a report. **Names
    /// only.**
    pub fn key_source(&self) -> String {
        if self.key_env.is_empty() {
            return "none required".to_owned();
        }
        self.key_env.join(" or ")
    }

    /// Rejects a configuration that cannot produce a request.
    pub fn validate(&self) -> Result<(), LlmError> {
        if self.model.trim().is_empty() {
            return Err(LlmError::Config("model id is empty".to_owned()));
        }
        let url = self.base_url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(LlmError::Config(format!(
                "base_url must start with http:// or https://, got {url:?}"
            )));
        }
        if self.batch_size == 0 {
            return Err(LlmError::Config("batch_size is zero".to_owned()));
        }
        if self.max_concurrency == 0 {
            return Err(LlmError::Config("max_concurrency is zero".to_owned()));
        }
        Ok(())
    }

    /// The endpoint this configuration would call.
    pub fn endpoint(&self) -> String {
        format!(
            "{}{}",
            self.base_url.trim_end_matches('/'),
            self.provider.path()
        )
    }
}

/// The on-disk location of a repository's model-description cache.
///
/// **In the platform state directory, never in the repository.** ADR-0065
/// records what happens when Polis reads its own output: the town grew by
/// itself between two runs that were supposed to be identical, nothing errored,
/// and the only symptom was a layout that drifted. Beside
/// [`crate::describe::default_cache_path`], keyed the same way on the
/// normalised repository root.
pub fn default_cache_path(repo_root: &Path) -> Option<PathBuf> {
    let key =
        crate::git::fnv1a64(crate::git::normalize_root(&repo_root.to_string_lossy()).as_bytes());
    Some(
        crate::corpus::state_dir()?
            .join("llm")
            .join(format!("{key:016x}.json")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_default_is_off_and_points_at_glm() {
        let config = LlmConfig::default();
        assert!(!config.enabled, "nothing turns this on for the operator");
        assert_eq!(config.provider, Provider::Glm);
        assert_eq!(config.model, "glm-5.3-flash");
        assert_eq!(
            config.endpoint(),
            "https://api.z.ai/api/paas/v4/chat/completions"
        );
        assert_eq!(config.key_source(), "ZAI_API_KEY or GLM_API_KEY");
        config.validate().expect("the default is valid");
    }

    /// A key may cross the network only where the network protects it. Plain
    /// HTTP is supported for a local Ollama, and a local Ollama is on loopback.
    #[test]
    fn a_key_is_never_sent_in_the_clear_to_another_machine() {
        let mut config = LlmConfig::default().with_provider(Provider::OpenAiCompatible);
        config.base_url = "http://collector.example/v1".to_owned();
        assert_eq!(
            config.key_destination(),
            Err(KeyWithheld::Cleartext {
                host: "collector.example".to_owned()
            })
        );
        // The message names the host and never a credential.
        let printed = config.key_destination().unwrap_err().to_string();
        assert!(printed.contains("collector.example"), "{printed}");
        assert!(printed.contains("in the clear"), "{printed}");

        // Loopback, in each of the three spellings a local model is reached by,
        // is fine: those bytes never reach a wire.
        for local in [
            "http://127.0.0.1:11434/v1",
            "http://localhost:11434/v1",
            "http://[::1]:11434/v1",
        ] {
            config.base_url = local.to_owned();
            assert_eq!(config.key_destination(), Ok(()), "{local}");
        }
        // And TLS to anywhere is what the shipped default already does.
        config.base_url = "https://api.z.ai/api/paas/v4".to_owned();
        assert_eq!(config.key_destination(), Ok(()));
    }

    /// `.polis/llm.json` arrives with a clone. It may configure the feature; it
    /// may not choose who receives the operator's credential.
    #[test]
    fn a_repository_cannot_redirect_the_operators_key_to_a_host_it_picked() {
        let mut config = LlmConfig {
            origin: ConfigOrigin::Repository,
            base_url: "https://collector.example/v1".to_owned(),
            ..LlmConfig::default()
        };
        assert_eq!(
            config.key_destination(),
            Err(KeyWithheld::RepoRedirect {
                host: "collector.example".to_owned(),
                expected: "api.z.ai".to_owned(),
            }),
            "TLS does not make a stranger's server the right one"
        );

        // The provider's own host is what the operator agreed to.
        config.base_url = "https://api.z.ai/api/paas/v4".to_owned();
        assert_eq!(config.key_destination(), Ok(()));

        // A repository may still point at a model on this machine.
        let mut local = LlmConfig::default().with_provider(Provider::Ollama);
        local.origin = ConfigOrigin::Repository;
        assert_eq!(local.key_destination(), Ok(()));

        // And an operator who types the host themselves has chosen it.
        config.base_url = "https://collector.example/v1".to_owned();
        assert_eq!(
            config.clone().chosen_by_operator().key_destination(),
            Ok(())
        );
    }

    /// The guard would be worthless if a checked-in file could switch it off.
    #[test]
    fn a_config_file_cannot_promote_itself_to_operator_origin() {
        let dir = std::env::temp_dir().join("polis-llm-origin-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("llm.json");

        // `origin` is `serde(skip)`, and the container denies unknown fields, so
        // naming it is a parse error and a parse error falls back to defaults.
        std::fs::write(
            &path,
            br#"{"base_url":"https://collector.example","origin":"operator"}"#,
        )
        .expect("write");
        let loaded = LlmConfig::load(&path);
        assert_eq!(loaded.base_url, LlmConfig::default().base_url);
        assert_eq!(loaded.origin, ConfigOrigin::Operator, "it is the default");

        // A well-formed file is honoured, and is marked as the repository's.
        std::fs::write(&path, br#"{"base_url":"https://collector.example"}"#).expect("write");
        let loaded = LlmConfig::load(&path);
        assert_eq!(loaded.base_url, "https://collector.example");
        assert_eq!(loaded.origin, ConfigOrigin::Repository);
        assert!(loaded.key_destination().is_err(), "and so it is refused");

        // Nothing about the origin is ever written back out.
        let json = serde_json::to_string(&LlmConfig::default()).expect("serialize");
        assert!(!json.contains("origin"), "{json}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn switching_provider_moves_url_model_key_and_price_together() {
        let ollama = LlmConfig::default().with_provider(Provider::Ollama);
        assert!(ollama.base_url.starts_with("http://"), "{ollama:?}");
        assert!(
            ollama.key_env.is_empty(),
            "a local endpoint needs no key: {ollama:?}"
        );
        assert_eq!(ollama.key_source(), "none required");
        assert_eq!(ollama.price, Price::FREE);
        assert!(ollama.key().is_none());

        let anthropic = LlmConfig::default().with_provider(Provider::Anthropic);
        assert!(
            anthropic.endpoint().ends_with("/v1/messages"),
            "{anthropic:?}"
        );
        assert_eq!(anthropic.key_env, ["ANTHROPIC_API_KEY"]);
    }

    #[test]
    fn a_missing_or_broken_config_file_is_the_default_and_never_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(LlmConfig::for_repo(dir.path()), LlmConfig::default());
        let path = config_path(dir.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, b"{ not json").expect("write");
        assert_eq!(LlmConfig::for_repo(dir.path()), LlmConfig::default());
        std::fs::write(&path, br#"{"enabled":true,"batch_size":3}"#).expect("write");
        let config = LlmConfig::for_repo(dir.path());
        assert!(config.enabled);
        assert_eq!(config.batch_size, 3);
        assert_eq!(config.model, "glm-5.3-flash", "the rest stays default");
    }

    #[test]
    fn a_configuration_that_cannot_make_a_request_is_refused() {
        let no_scheme = LlmConfig {
            base_url: "api.z.ai".to_owned(),
            ..LlmConfig::default()
        };
        assert!(matches!(no_scheme.validate(), Err(LlmError::Config(_))));
        let no_model = LlmConfig {
            model: "  ".to_owned(),
            ..LlmConfig::default()
        };
        assert!(matches!(no_model.validate(), Err(LlmError::Config(_))));
        let no_batch = LlmConfig {
            batch_size: 0,
            ..LlmConfig::default()
        };
        assert!(matches!(no_batch.validate(), Err(LlmError::Config(_))));
    }

    #[test]
    fn the_price_is_the_operators_number_not_an_estimate() {
        // The brief's arithmetic, reproduced: ~60 districts, ~1 000 input and
        // ~70 output tokens each.
        let usage = Usage {
            input_tokens: 60 * 1_000,
            output_tokens: 60 * 70,
        };
        let cost = Price::GLM_FLASH_PROMO.cost(usage);
        assert!((cost - 0.00555).abs() < 1e-6, "{cost}");
        assert!(Price::FREE.cost(usage).abs() < f64::EPSILON);
        // Double the price, double the bill: the field is load-bearing.
        assert!((Price::GLM_FLASH_LIST.cost(usage) - 2.0 * cost).abs() < 1e-9);
    }

    #[test]
    fn a_retryable_error_is_the_endpoint_asking_for_patience() {
        assert!(LlmError::Status {
            status: 429,
            message: String::new()
        }
        .is_retryable());
        assert!(LlmError::Status {
            status: 503,
            message: String::new()
        }
        .is_retryable());
        assert!(!LlmError::Status {
            status: 401,
            message: String::new()
        }
        .is_retryable());
        assert!(!LlmError::NoKey("ZAI_API_KEY".to_owned()).is_retryable());
        assert!(!LlmError::Unusable("refused".to_owned()).is_retryable());
    }

    #[test]
    fn the_cache_never_lands_inside_the_repository() {
        // ADR-0065: Polis must not ingest its own output.
        let Some(path) = default_cache_path(Path::new("C:/coding/agentolis")) else {
            return; // no state directory on this machine; nothing to assert
        };
        assert!(
            !path.starts_with("C:/coding/agentolis"),
            "{}",
            path.display()
        );
        assert!(path.to_string_lossy().contains("llm"), "{}", path.display());
    }
}
