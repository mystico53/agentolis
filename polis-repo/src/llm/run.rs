//! Planning, pricing, calling, and the accounting that makes the bill visible
//! before it exists.
//!
//! # Nothing here is ever reached by accident
//!
//! [`crate::tree::RepoIndex::neighborhoods_described`] — the call a render and a
//! snapshot make — does not touch this module. It reads the derived
//! descriptions, and then [`crate::neighborhoods::Neighborhoods::apply_model_descriptions`]
//! reads the *cache*. Neither opens a socket. A model call happens when
//! [`LlmRunner::run`] is called, and that is either an explicit command or a
//! background thread the application started on purpose.
//!
//! # Dry run first, always
//!
//! [`RunMode::DryRun`] produces the whole [`Plan`] — which districts, how many
//! calls, how many tokens, how many dollars — having called nothing. And a
//! [`RunMode::Generate`] against a **cold cache** refuses to spend anything
//! unless it is told to: cold start is the expensive one, and nobody should
//! discover a bill.
//!
//! # Degradation is total
//!
//! No key, no network, a dead port, a 401, a 429, a timeout, a body that is not
//! JSON, a model that refuses: each is one line in [`RunReport::errors`], the
//! affected districts keep whatever [`crate::describe`] derived for them, and
//! the map is the one you get with the feature switched off. There is no path
//! through this module that can fail a render.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use polis_events::LogicalPath;
use serde::{Deserialize, Serialize};

use crate::describe::{Description, DescriptionSource};
use crate::neighborhoods::{Neighborhood, Neighborhoods};
use crate::RepoTree;

use super::cache::{Freshness, ModelCache, Sketch};
use super::outbound::RedactionReport;
use super::prompt::{
    build_brief, parse_reply, restates_the_name, system_prompt, user_prompt, AnswerReject,
    DistrictBrief, PROMPT_VERSION,
};
use super::provider::{ChatProvider, Usage};
use super::secret::{bound_message, scrub, Secret};
use super::transport::{DefaultTransport, Transport};
use super::{KeyWithheld, LlmConfig, LlmError};

// ---------------------------------------------------------------------------
// Which districts want a model
// ---------------------------------------------------------------------------

/// Why a district is a candidate for a model-written description.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Candidacy {
    /// The repository said nothing at all. 83 of the operator's 179 districts.
    NoDescription,
    /// The only description is a synthesised inventory. The review's
    /// recommendation 3: the monument is already labelled as a building
    /// (PRD §8), so "most imported: `BaseWindow.jsx`" on the district is the
    /// same fact twice, and belongs in the drill-down rather than on the map.
    Inventory,
    /// A doc comment is standing in for a district too large for one file to
    /// speak for. `src/services`, 223 files, labelled from `WindowManager.js`.
    DocCommentOverreach,
    /// The description restates the district's own name. See
    /// [`restates_the_name`].
    Restatement,
    /// The model already described it, and the district has moved since. Set by
    /// the planner rather than by [`candidacy`], which cannot see the cache.
    Drifted,
}

impl Candidacy {
    /// A stable name for the report.
    pub fn name(self) -> &'static str {
        match self {
            Self::NoDescription => "no-description",
            Self::Inventory => "inventory",
            Self::DocCommentOverreach => "doc-comment-overreach",
            Self::Restatement => "restatement",
            Self::Drifted => "drifted",
        }
    }
}

/// Whether a district wants a model-written description, and why.
///
/// **A real README or manifest sentence always wins.** The review was explicit
/// about this and it is the cheap half of the feature: the extractor keeps every
/// case where a human wrote a sentence about this directory, and the model is
/// asked only about the 141-of-179 where it did not.
pub fn candidacy(hood: &Neighborhood, config: &LlmConfig) -> Option<Candidacy> {
    if hood.is_industrial() {
        // Somebody else's library does not get to describe a district of this
        // city, and PRD §8 wants the eye to slide off it anyway.
        return None;
    }
    let Some(description) = &hood.description else {
        return Some(Candidacy::NoDescription);
    };
    if description.source == DescriptionSource::Model {
        // Already the model's. Whether it needs doing again is the *cache's*
        // question, not this function's, and the planner asks it — see
        // [`Candidacy::Drifted`].
        return None;
    }
    if restates_the_name(&description.label, &hood.name) {
        return Some(Candidacy::Restatement);
    }
    match description.source {
        DescriptionSource::Inventory => Some(Candidacy::Inventory),
        DescriptionSource::DocComment if hood.file_count > config.doc_comment_trust_max_files => {
            Some(Candidacy::DocCommentOverreach)
        }
        DescriptionSource::Readme
        | DescriptionSource::Manifest
        | DescriptionSource::DocComment
        | DescriptionSource::Model => None,
    }
}

/// Every district's own file names, relative to it, in path order.
///
/// The fingerprint input and the prompt input, computed once. A file belongs to
/// the deepest district that is a prefix of it
/// ([`Neighborhoods::district_of`]), which is the same ownership rule
/// [`Neighborhood::file_count`] counts with.
pub fn district_names(
    hoods: &Neighborhoods,
    tree: &RepoTree,
) -> BTreeMap<LogicalPath, Vec<String>> {
    let mut out: BTreeMap<LogicalPath, Vec<String>> = BTreeMap::new();
    for hood in hoods.all() {
        out.entry(hood.path.clone()).or_default();
    }
    for path in tree.files.keys() {
        let Some(owner) = hoods.district_of(path) else {
            continue;
        };
        let relative = if owner.path.is_root() {
            path.as_str()
        } else {
            path.as_str()
                .get(owner.path.as_str().len() + 1..)
                .unwrap_or(path.as_str())
        };
        out.entry(owner.path.clone())
            .or_default()
            .push(relative.to_owned());
    }
    out
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

/// One district the run would describe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedDistrict {
    /// What would be sent.
    pub brief: DistrictBrief,
    /// Why it is a candidate.
    pub why: Candidacy,
    /// Its state before the run.
    pub freshness: Freshness,
}

/// What a run would do, priced, having called nothing.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The districts that would be described, in path order.
    pub districts: Vec<PlannedDistrict>,
    /// Requests it would take.
    pub calls: u32,
    /// Districts with a current model description already.
    pub fresh: u32,
    /// Districts whose model description has drifted.
    pub stale: u32,
    /// Districts that keep a quotation from the repository.
    pub kept_derived: u32,
    /// Industrial districts, which are never described.
    pub industrial: u32,
    /// Cached entries for districts that no longer exist.
    pub orphans: Vec<LogicalPath>,
    /// Estimated tokens in, from the exact bytes that would be sent.
    pub estimated_input_tokens: u64,
    /// Estimated tokens out.
    pub estimated_output_tokens: u64,
    /// What that would cost at the configured [`super::Price`].
    pub estimated_usd: f64,
    /// What the outbound gate refused.
    pub redaction: RedactionReport,
    /// Whether a key was found. **Never the key.**
    pub key_present: bool,
    /// Where the key would come from — variable names only.
    pub key_source: String,
    /// The endpoint that would be called.
    pub endpoint: String,
    /// The model that would be asked.
    pub model: String,
    /// The price the estimate was computed at. Printed rather than assumed:
    /// the shipped default is a promotional rate that expires.
    pub price: super::Price,
    /// Why the run cannot proceed, if it cannot. A plan is still produced.
    pub blocked: Option<String>,
}

/// Characters per token, for the estimate only.
///
/// Four is the usual rule of thumb for English prose and it is close enough for
/// a *pre-flight* number. It is never used for the real figure: [`RunReport`]
/// reports what the provider counted, and says when the provider counted
/// nothing.
const CHARS_PER_TOKEN: u64 = 4;

/// Tokens a district's answer is expected to cost.
///
/// A 70-character label plus a 240-character detail plus JSON scaffolding, at
/// [`CHARS_PER_TOKEN`]. The operator's own estimate was ~70; this is ~90, which
/// errs towards over-quoting a bill rather than under-quoting one.
const ESTIMATED_OUTPUT_TOKENS_PER_DISTRICT: u64 = 90;

impl Plan {
    /// True when there is nothing to do.
    pub fn is_empty(&self) -> bool {
        self.districts.is_empty()
    }

    /// How many districts would be described.
    pub fn len(&self) -> usize {
        self.districts.len()
    }

    /// A one-line summary for a terminal.
    pub fn summary(&self) -> String {
        format!(
            "{} district(s) to describe in {} call(s); {} fresh, {} stale, {} kept from the \
             repository; ~{} in / ~{} out tokens, ~${:.4} at ${}/${} per Mtok",
            self.districts.len(),
            self.calls,
            self.fresh,
            self.stale,
            self.kept_derived,
            self.estimated_input_tokens,
            self.estimated_output_tokens,
            self.estimated_usd,
            self.price.usd_per_m_input,
            self.price.usd_per_m_output,
        )
    }
}

// ---------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------

/// What a run is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunMode {
    /// Count and price the work. Calls nothing. **The default.**
    #[default]
    DryRun,
    /// Describe the districts that need it.
    Generate {
        /// Required on a cold cache, where the whole repository is about to be
        /// described at once.
        confirmed: bool,
    },
    /// Describe every eligible district, ignoring fresh cache entries.
    Refresh {
        /// Always required: a refresh is a cold start by definition.
        confirmed: bool,
    },
}

impl RunMode {
    /// True when this mode is allowed to open a socket.
    pub fn calls(self) -> bool {
        !matches!(self, Self::DryRun)
    }

    /// A stable name for the report.
    pub fn name(self) -> &'static str {
        match self {
            Self::DryRun => "dry-run",
            Self::Generate { .. } => "generate",
            Self::Refresh { .. } => "refresh",
        }
    }

    /// Whether the operator has agreed to spend money.
    fn confirmed(self) -> bool {
        match self {
            Self::DryRun => true,
            Self::Generate { confirmed } | Self::Refresh { confirmed } => confirmed,
        }
    }
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// What a run actually did.
#[derive(Debug, Clone)]
pub struct RunReport {
    /// The mode it ran in.
    pub mode: &'static str,
    /// What it planned.
    pub plan: Plan,
    /// Requests sent, retries included.
    pub calls_attempted: u32,
    /// Requests that produced nothing usable.
    pub calls_failed: u32,
    /// Retries spent.
    pub retries: u32,
    /// Tokens, **as the provider counted them**.
    pub usage: Usage,
    /// False when the provider reported no usage, which makes
    /// [`RunReport::actual_usd`] an estimate again.
    pub usage_reported: bool,
    /// What it cost at the configured price.
    pub actual_usd: f64,
    /// Districts that got words.
    pub described: u32,
    /// Districts the model correctly declined to describe.
    pub declined: u32,
    /// Answers thrown away by the inbound sanitiser.
    pub rejected_sanitise: u32,
    /// Answers thrown away for restating the district's name.
    pub rejected_restates: u32,
    /// Districts the model said nothing about at all.
    pub unanswered: u32,
    /// One line per failure, key-scrubbed and bounded.
    pub errors: Vec<String>,
    /// True when the run stopped because a cold start was not confirmed. The
    /// plan is still filled in.
    pub needs_confirmation: bool,
}

impl RunReport {
    /// A dry run, or a run that never started.
    fn from_plan(mode: RunMode, plan: Plan) -> Self {
        Self {
            mode: mode.name(),
            plan,
            calls_attempted: 0,
            calls_failed: 0,
            retries: 0,
            usage: Usage::default(),
            usage_reported: false,
            actual_usd: 0.0,
            described: 0,
            declined: 0,
            rejected_sanitise: 0,
            rejected_restates: 0,
            unanswered: 0,
            errors: Vec::new(),
            needs_confirmation: false,
        }
    }

    /// True when every call failed and nothing was written.
    pub fn degraded(&self) -> bool {
        self.described == 0 && self.declined == 0 && !self.errors.is_empty()
    }

    /// A few lines for a terminal.
    pub fn summary(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!("{}: {}\n", self.mode, self.plan.summary());
        if self.needs_confirmation {
            out.push_str(
                "  cold start not confirmed — nothing was called. Re-run with --yes to spend.\n",
            );
            return out;
        }
        if !self.mode.eq("dry-run") {
            let _ = writeln!(
                out,
                "  {} call(s), {} failed, {} retr(ies); {} described, {} declined, \
                 {} rejected (sanitise {} · restates {}), {} unanswered",
                self.calls_attempted,
                self.calls_failed,
                self.retries,
                self.described,
                self.declined,
                self.rejected_sanitise + self.rejected_restates,
                self.rejected_sanitise,
                self.rejected_restates,
                self.unanswered,
            );
            let _ = writeln!(
                out,
                "  tokens {} in / {} out ({}), ${:.4}",
                self.usage.input_tokens,
                self.usage.output_tokens,
                if self.usage_reported {
                    "reported by the provider"
                } else {
                    "ESTIMATED — the provider reported none"
                },
                self.actual_usd,
            );
        }
        for error in &self.errors {
            out.push_str("  ! ");
            out.push_str(error);
            out.push('\n');
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The runner
// ---------------------------------------------------------------------------

/// Turns a plan into descriptions.
///
/// Holds the configuration, a transport, a provider client and — only when the
/// environment had one — a key. It is `Send + Sync`, so
/// [`LlmRunner::spawn`] can put a whole run on a background thread and a render
/// never waits for it.
#[derive(Debug)]
pub struct LlmRunner {
    config: LlmConfig,
    transport: Arc<dyn Transport>,
    provider: Box<dyn ChatProvider>,
    key: Option<Arc<Secret>>,
    /// Set when a key exists but this endpoint may not have it.
    key_withheld: Option<KeyWithheld>,
}

impl LlmRunner {
    /// A runner over the shipped transport.
    pub fn new(config: LlmConfig) -> Self {
        Self::with_transport(config, Arc::new(DefaultTransport::new()))
    }

    /// A runner over a transport of the caller's choosing.
    ///
    /// This is the seam the tests drive an in-process HTTP server through, and
    /// the seam a `ureq`-backed transport would drop into if the dependency
    /// decision in [`super::transport`] is ever revisited.
    pub fn with_transport(config: LlmConfig, transport: Arc<dyn Transport>) -> Self {
        // The one place a key is picked up, and therefore the one place to ask
        // whether this endpoint is allowed to receive it. A refusal leaves
        // `key: None`, which is the degraded state the feature already handles,
        // and is reported by `blocked` rather than applied in silence.
        let (key, key_withheld) = match config.key_destination() {
            Ok(()) => (config.key().map(Arc::new), None),
            Err(reason) => {
                // Only worth reporting when a key was actually there to withhold.
                let withheld = config.key().is_some().then_some(reason);
                (None, withheld)
            }
        };
        let provider = config.provider.client();
        Self {
            config,
            transport,
            provider,
            key,
            key_withheld,
        }
    }

    /// Why a key present in the environment is not being used, if it is not.
    pub fn key_withheld(&self) -> Option<&KeyWithheld> {
        self.key_withheld.as_ref()
    }

    /// The configuration in force.
    pub fn config(&self) -> &LlmConfig {
        &self.config
    }

    /// What a run would do, having called nothing.
    ///
    /// Always safe, always free, and it is what [`RunMode::DryRun`] returns.
    pub fn plan(&self, hoods: &Neighborhoods, tree: &RepoTree, cache: &ModelCache) -> Plan {
        self.plan_with(hoods, tree, cache, false)
    }

    // One pass over the neighborhoods, in one place: every branch decides the
    // same district's fate, and cutting it in half would mean threading the six
    // counters through a second signature for no gain in clarity.
    #[allow(clippy::too_many_lines)]
    fn plan_with(
        &self,
        hoods: &Neighborhoods,
        tree: &RepoTree,
        cache: &ModelCache,
        force: bool,
    ) -> Plan {
        let names = district_names(hoods, tree);
        let empty = Vec::new();
        let mut report = RedactionReport::default();
        let mut districts: Vec<PlannedDistrict> = Vec::new();
        let (mut fresh, mut stale, mut kept, mut industrial) = (0u32, 0u32, 0u32, 0u32);

        for hood in hoods.all() {
            if hood.is_industrial() {
                industrial += 1;
                continue;
            }
            let own = names.get(&hood.path).unwrap_or(&empty);
            let sketch = Sketch::build(own);
            let freshness = cache.freshness(
                &hood.path,
                &sketch,
                &hood.children,
                &self.config.model,
                PROMPT_VERSION,
                self.config.drift_threshold_permille,
            );
            // A district already carrying the model's words is not a
            // *candidacy* question — the extractor's verdict was overruled the
            // first time round — it is a freshness question, and the cache has
            // just answered it.
            let why = if hood.has_model_description() {
                Some(Candidacy::Drifted)
            } else {
                candidacy(hood, &self.config)
            };
            let Some(why) = why else {
                kept += 1;
                continue;
            };
            match freshness {
                Freshness::Fresh => {
                    fresh += 1;
                    if !force {
                        continue;
                    }
                }
                Freshness::Stale(_) => stale += 1,
                Freshness::Missing => {}
            }
            let children: Vec<String> = hood
                .children
                .iter()
                .filter_map(|c| hoods.get(c).map(|n| n.name.clone()))
                .collect();
            let mix = if hood
                .mix
                .ranked()
                .iter()
                .filter(|(k, _)| hood.mix.share(*k) >= 0.15)
                .count()
                >= 2
            {
                hood.mix.summary(3, 0.15)
            } else {
                String::new()
            };
            let Some(brief) = build_brief(
                &hood.path,
                &hood.name,
                hood.kind.name(),
                &mix,
                hood.file_count,
                hood.subtree_file_count,
                own.clone(),
                children,
                hood.monument.as_ref(),
                hood.description.as_ref(),
                self.config.max_names_per_district,
                &mut report,
            ) else {
                continue;
            };
            districts.push(PlannedDistrict {
                brief,
                why,
                freshness,
            });
        }

        districts.truncate(self.config.max_districts_per_run as usize);
        let calls = u32::try_from(districts.len().div_ceil(self.config.batch_size.max(1)))
            .unwrap_or(u32::MAX);

        // Priced from the exact bytes that would be sent, not from a guess at
        // how big a district is.
        let system = system_prompt();
        let mut input_chars = 0u64;
        for chunk in districts.chunks(self.config.batch_size.max(1)) {
            let briefs: Vec<DistrictBrief> = chunk.iter().map(|d| d.brief.clone()).collect();
            input_chars += (system.len() + user_prompt(&briefs).len()) as u64;
        }
        let estimated_input_tokens = input_chars / CHARS_PER_TOKEN;
        let estimated_output_tokens = districts.len() as u64 * ESTIMATED_OUTPUT_TOKENS_PER_DISTRICT;
        let estimated_usd = self.config.price.cost(Usage {
            input_tokens: estimated_input_tokens,
            output_tokens: estimated_output_tokens,
        });

        let live: BTreeSet<LogicalPath> = hoods.all().iter().map(|h| h.path.clone()).collect();
        Plan {
            calls,
            fresh,
            stale,
            kept_derived: kept,
            industrial,
            orphans: cache.orphans(&live),
            estimated_input_tokens,
            estimated_output_tokens,
            estimated_usd,
            redaction: report,
            key_present: self.key.is_some(),
            key_source: self.config.key_source(),
            endpoint: self.config.endpoint(),
            model: self.config.model.clone(),
            price: self.config.price,
            blocked: self.blocked(),
            districts,
        }
    }

    /// Why a run cannot proceed, if it cannot.
    ///
    /// Reported on the *plan*, before anything is spent, so "there is no key" is
    /// something the operator reads rather than something they discover from an
    /// empty map.
    fn blocked(&self) -> Option<String> {
        if !self.config.enabled {
            return Some("llm descriptions are disabled in .polis/llm.json".to_owned());
        }
        if let Err(error) = self.config.validate() {
            return Some(error.to_string());
        }
        // Before "there is no key": here the key exists and was refused, and
        // saying "set ZAI_API_KEY" to somebody who already has would send them
        // looking in the wrong place.
        if let Some(withheld) = &self.key_withheld {
            return Some(withheld.to_string());
        }
        if self.key.is_none() && !self.config.key_env.is_empty() {
            return Some(format!("no API key: set {}", self.config.key_source()));
        }
        None
    }

    /// Plans, then — unless the mode is a dry run — calls.
    ///
    /// Writes descriptions into `hoods` and into `cache`. Never returns an
    /// error: everything that can go wrong is a line in
    /// [`RunReport::errors`] and a district that keeps what it had.
    pub fn run(
        &self,
        hoods: &mut Neighborhoods,
        tree: &RepoTree,
        cache: &mut ModelCache,
        mode: RunMode,
    ) -> RunReport {
        let force = matches!(mode, RunMode::Refresh { .. });
        let plan = self.plan_with(hoods, tree, cache, force);
        if !mode.calls() || plan.blocked.is_some() || plan.is_empty() {
            let mut report = RunReport::from_plan(mode, plan);
            if let Some(reason) = report.plan.blocked.clone() {
                if mode.calls() {
                    report.errors.push(reason);
                }
            }
            hoods.apply_model_descriptions(cache, &self.config, tree);
            return report;
        }
        // Cold start is the expensive one, so it is the one that asks.
        if cache.is_empty() && !mode.confirmed() {
            let mut report = RunReport::from_plan(mode, plan);
            report.needs_confirmation = true;
            return report;
        }

        let batch_size = self.config.batch_size.max(1);
        let batches: Vec<Vec<DistrictBrief>> = plan
            .districts
            .chunks(batch_size)
            .map(|c| c.iter().map(|d| d.brief.clone()).collect())
            .collect();
        let outcomes = self.call_batches(&batches);

        let mut report = RunReport::from_plan(mode, plan);
        let names = district_names(hoods, tree);
        let empty = Vec::new();
        let children_of: BTreeMap<LogicalPath, Vec<LogicalPath>> = hoods
            .all()
            .iter()
            .map(|h| (h.path.clone(), h.children.clone()))
            .collect();

        for (index, outcome) in outcomes {
            let batch = &batches[index];
            report.calls_attempted += outcome.attempts;
            report.retries += outcome.attempts.saturating_sub(1);
            match outcome.result {
                Err(error) => {
                    report.calls_failed += 1;
                    report.errors.push(format!(
                        "[{}] {} district(s) kept their derived description: {}",
                        error.tag(),
                        batch.len(),
                        bound_message(&error.to_string(), 200)
                    ));
                }
                Ok((parsed, usage)) => {
                    report.usage.add(usage);
                    if !usage.is_zero() {
                        report.usage_reported = true;
                    }
                    for (path, description) in parsed.answers {
                        if description.is_some() {
                            report.described += 1;
                        } else {
                            report.declined += 1;
                        }
                        cache.put(super::cache::CachedModelDescription {
                            sketch: Sketch::build(names.get(&path).unwrap_or(&empty)),
                            children: children_of.get(&path).cloned().unwrap_or_default(),
                            description,
                            model: self.config.model.clone(),
                            prompt_version: PROMPT_VERSION,
                            path,
                        });
                    }
                    for (path, why) in parsed.rejected {
                        match why {
                            AnswerReject::Sanitise | AnswerReject::Unknown => {
                                report.rejected_sanitise += 1;
                            }
                            AnswerReject::RestatesName => report.rejected_restates += 1,
                            AnswerReject::Nothing => report.declined += 1,
                        }
                        tracing::debug!(district = %path.as_str(), reason = why.name_or_debug(),
                                        "a model answer was refused");
                    }
                    report.unanswered += u32::try_from(parsed.unanswered.len()).unwrap_or(u32::MAX);
                }
            }
        }
        report.actual_usd = self.config.price.cost(report.usage);
        if !report.usage_reported {
            // Do not print $0.00 for work that was done.
            report.usage = Usage {
                input_tokens: report.plan.estimated_input_tokens,
                output_tokens: report.plan.estimated_output_tokens,
            };
            report.actual_usd = self.config.price.cost(report.usage);
        }
        hoods.apply_model_descriptions(cache, &self.config, tree);
        report
    }

    /// Runs the batches across at most [`LlmConfig::max_concurrency`] threads.
    ///
    /// Results come back keyed on the batch index, so the merge order is the
    /// plan's order whatever order the network answered in.
    fn call_batches(&self, batches: &[Vec<DistrictBrief>]) -> Vec<(usize, BatchOutcome)> {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let out: Mutex<Vec<(usize, BatchOutcome)>> = Mutex::new(Vec::with_capacity(batches.len()));
        let threads = self.config.max_concurrency.max(1).min(batches.len());
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some(batch) = batches.get(index) else {
                        return;
                    };
                    let outcome = self.call_batch(batch);
                    out.lock()
                        .expect("no panic while holding it")
                        .push((index, outcome));
                });
            }
        });
        let mut out = out.into_inner().expect("no panic while holding it");
        out.sort_by_key(|(i, _)| *i);
        out
    }

    /// One batch, with bounded retries and exponential backoff.
    fn call_batch(&self, batch: &[DistrictBrief]) -> BatchOutcome {
        let system = system_prompt();
        let user = user_prompt(batch);
        let mut attempts = 0u32;
        let mut last = LlmError::Config("no attempt was made".to_owned());
        for attempt in 0..=self.config.max_retries {
            attempts += 1;
            match self.call_once(&system, &user, batch) {
                Ok(ok) => {
                    return BatchOutcome {
                        attempts,
                        result: Ok(ok),
                    }
                }
                Err(error) => {
                    if !error.is_retryable() || attempt == self.config.max_retries {
                        return BatchOutcome {
                            attempts,
                            result: Err(error),
                        };
                    }
                    let backoff = self
                        .config
                        .retry_base_ms
                        .saturating_mul(1u64 << attempt.min(16));
                    if backoff > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(backoff));
                    }
                    last = error;
                }
            }
        }
        BatchOutcome {
            attempts,
            result: Err(last),
        }
    }

    /// One request.
    fn call_once(
        &self,
        system: &str,
        user: &str,
        batch: &[DistrictBrief],
    ) -> Result<(super::prompt::ParsedReply, Usage), LlmError> {
        let request = self
            .provider
            .request(&self.config, self.key.as_ref(), system, user)?;
        let response = self.transport.post(&request).map_err(|e| {
            // A transport error can carry a subprocess's stderr; scrub it.
            LlmError::Transport(match e {
                super::TransportError::Unreachable(m) => {
                    super::TransportError::Unreachable(scrub(&m, self.key.as_deref()))
                }
                super::TransportError::Unavailable(m) => {
                    super::TransportError::Unavailable(scrub(&m, self.key.as_deref()))
                }
                super::TransportError::Io(m) => {
                    super::TransportError::Io(scrub(&m, self.key.as_deref()))
                }
                other => other,
            })
        })?;
        let reply = self.provider.parse(&response)?;
        let parsed = parse_reply(&reply.text, batch)?;
        Ok((parsed, reply.usage))
    }

    /// Runs on a background thread, so nothing waits for a network.
    ///
    /// PRD §13.1's frame budget is not negotiable and a model call is orders of
    /// magnitude outside it. This is the whole answer to "regeneration must
    /// never block a render": the runner owns its inputs, writes the cache when
    /// it is done, and the next frame picks the result up from there.
    pub fn spawn(
        self,
        mut hoods: Neighborhoods,
        tree: RepoTree,
        mut cache: ModelCache,
        mode: RunMode,
        cache_path: Option<std::path::PathBuf>,
    ) -> std::thread::JoinHandle<RunReport> {
        std::thread::Builder::new()
            .name("polis-llm".to_owned())
            .spawn(move || {
                let report = self.run(&mut hoods, &tree, &mut cache, mode);
                if let Some(path) = cache_path {
                    if cache.is_dirty() {
                        cache.write(&path);
                    }
                }
                report
            })
            .expect("a named thread")
    }
}

/// What one batch produced, and how many attempts it took.
struct BatchOutcome {
    attempts: u32,
    result: Result<(super::prompt::ParsedReply, Usage), LlmError>,
}

impl AnswerReject {
    /// A name for the debug log.
    fn name_or_debug(self) -> &'static str {
        match self {
            Self::Nothing => "nothing",
            Self::Sanitise => "sanitise",
            Self::RestatesName => "restates-name",
            Self::Unknown => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// The read path
// ---------------------------------------------------------------------------

impl Neighborhoods {
    /// Fills in model-written descriptions from the cache, and marks how current
    /// each one is.
    ///
    /// **Reads. Never calls.** This is what a render and a snapshot use, and it
    /// is the reason a normal launch cannot produce a bill.
    ///
    /// A cached description that has drifted is still shown — marked
    /// [`Freshness::Stale`], never silently as current. Blanking it would trade
    /// a caption the operator can see is old for no caption at all, and the
    /// requirement was that staleness be *visible*, not that it be hidden.
    pub fn apply_model_descriptions(
        &mut self,
        cache: &ModelCache,
        config: &LlmConfig,
        tree: &RepoTree,
    ) {
        let names = district_names(self, tree);
        let empty = Vec::new();
        let mut model_described = 0u32;
        for index in 0..self.len() {
            let path = self.all()[index].path.clone();
            let Some(entry) = cache.get(&path) else {
                continue;
            };
            let hood = &self.all()[index];
            if hood.is_industrial() {
                continue;
            }
            let wanted = candidacy(hood, config).is_some()
                || hood
                    .description
                    .as_ref()
                    .is_some_and(|d| d.source == DescriptionSource::Model);
            let Some(description) = entry.description.clone() else {
                continue;
            };
            if !wanted {
                continue;
            }
            let sketch = Sketch::build(names.get(&path).unwrap_or(&empty));
            let freshness = cache.freshness(
                &path,
                &sketch,
                &hood.children,
                &config.model,
                PROMPT_VERSION,
                config.drift_threshold_permille,
            );
            self.set_model_description(index, description, freshness);
            model_described += 1;
        }
        self.set_model_described(model_described);
    }
}

// ---------------------------------------------------------------------------
// The whole thing, for a caller that just wants it done
// ---------------------------------------------------------------------------

/// Plans or runs against a checkout, reading and writing the state-directory
/// cache.
///
/// The one call an application needs. `mode` decides whether anything is spent;
/// [`RunMode::DryRun`] is free and calls nothing.
pub fn describe_with_model(
    repo_root: &Path,
    hoods: &mut Neighborhoods,
    tree: &RepoTree,
    mode: RunMode,
) -> RunReport {
    let config = LlmConfig::for_repo(repo_root);
    let cache_path = super::default_cache_path(repo_root);
    let mut cache = cache_path
        .as_deref()
        .map_or_else(ModelCache::default, ModelCache::read);
    let runner = LlmRunner::new(config);
    let report = runner.run(hoods, tree, &mut cache, mode);
    if let Some(path) = &cache_path {
        if cache.is_dirty() {
            cache.write(path);
        }
    }
    report
}

/// Fills in cached model descriptions for a render. **Calls nothing.**
///
/// Safe to put on the launch path: with no cache it does nothing at all, and
/// with one it is a read of a small JSON file.
pub fn apply_cached_model_descriptions(
    repo_root: &Path,
    hoods: &mut Neighborhoods,
    tree: &RepoTree,
) {
    let config = LlmConfig::for_repo(repo_root);
    let Some(path) = super::default_cache_path(repo_root) else {
        return;
    };
    let cache = ModelCache::read(&path);
    if cache.is_empty() {
        return;
    }
    hoods.apply_model_descriptions(&cache, &config, tree);
}

/// A model-written description, for a test or a caller building one by hand.
pub fn model_description(label: &str, detail: &str) -> Description {
    Description {
        label: label.to_owned(),
        detail: detail.to_owned(),
        source: DescriptionSource::Model,
        origin: None,
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
