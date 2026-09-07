//! Configuration (PRD §12, §13.1).
//!
//! Everything the operator can change. Deliberately small: PRD §2 rules out a
//! dashboard, and every knob here exists because a specific PRD section names it
//! as tunable or as an open question.

use std::path::PathBuf;

use polis_events::Channel;
use serde::{Deserialize, Serialize};

/// Operator configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Repository root. One repository, one city (PRD §2).
    pub repo_root: PathBuf,
    /// Command run when a building is clicked (PRD §12).
    ///
    /// > **Click a building** → open in `$EDITOR` via the configured command.
    /// > Nothing more.
    pub editor_command: String,
    /// Maximum simultaneously rendered clouds (PRD §10.4).
    ///
    /// > **Cap the number of visible clouds.** Forty threads means forty systems
    /// > and the map vanishes under haze.
    ///
    /// PRD §17 open question 1 asked what the right cap is, and whether dormant
    /// threads should dissipate entirely or leave a faint residue. Both are now
    /// answered against a real fleet —
    /// [`polis_world::territory::CLOUD_CAP`] carries the measurement and
    /// [`polis_world::territory::DORMANT_AFTER`] the dormancy sweep — and this
    /// setting defaults to that answer. It stays configurable because the
    /// measurement is one repository's.
    pub cloud_cap: usize,
    /// Whether the streets layer is on. Off by default at the widest zoom
    /// (PRD §9).
    pub streets: bool,
    /// Which ingest channels to run.
    pub channels: ChannelConfig,
    /// Where the state directory lives, when it should not be the platform
    /// default. Tests and replay set it; operators normally do not.
    pub state_dir: Option<PathBuf>,
    /// Whether a model may write the "what is it working on" line on each
    /// cloud's caption ([`crate::intent`]).
    ///
    /// **Off**, and it is the one setting here that sends anything off the
    /// machine, so it is off in [`Config::default`] rather than off in a file
    /// an operator has to find. `polis watch --captions` turns it on for one
    /// run; nothing turns it on permanently, because ADR-0089's rule is that
    /// nothing is called implicitly.
    ///
    /// With it off there is no worker thread, no key is read, and the caption
    /// shows the `ai-title` it has always shown.
    #[serde(default)]
    pub captions: bool,
    /// The two things about the cloud layer that are taste rather than finding,
    /// tunable from inside the window.
    #[serde(default)]
    pub look: Look,
}

/// What the operator can tune about the cloud layer while looking at it.
///
/// Both settings here are questions the code cannot answer on the operator's
/// behalf, and both were asked and left open on purpose:
///
/// * **How much of the city may vanish under a cloud.** A light veil keeps the
///   streets legible and separates weakly; a dense core reads from across the
///   room and hides what is underneath. There is no correct answer — it depends
///   on whether the operator is reading the map or watching it.
/// * **When the rail points at a cloud.** One connector at a time keeps the map
///   quiet; a connector per thread names every cloud at a glance and costs
///   several lines across the window.
///
/// They live on [`Config`] so a value the operator settles on survives a
/// restart, and they are `#[serde(default)]` so a config file written before
/// they existed still loads.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Look {
    /// How strong the cloud body is, `0.0`–`1.0`.
    ///
    /// Zero is the marks-only notation — contour and hatch over an untouched
    /// city, `0.000` levels of median disturbance. One is a core dense enough to
    /// hide the buildings under it. See
    /// [`polis_render::live::CLOUD_BODY_ALPHA`] for what the number scales and
    /// what it costs.
    pub cloud_veil: f64,
    /// When a rail row draws a line to its own cloud.
    pub connectors: Connectors,
}

/// When the rail points at the map.
///
/// The rail already names every thread and the map already draws every cloud;
/// what was missing is the line between them, and the operator's own words for
/// it were *"the rail thread should point to the cloud"*. It replaced a caption
/// on a leader out of each cloud, which said the same thing twice — once on the
/// map and once in the rail — and spent map area doing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Connectors {
    /// Never. The hue is the only link, as it was before.
    Off,
    /// Only for the thread the operator is pointing at, has selected, or is
    /// following — `ViewState::interrogates`. One line at a time.
    Asked,
    /// Every thread with a cloud on screen, all the time.
    Always,
}

impl Connectors {
    /// What the settings control calls it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Asked => "asked",
            Self::Always => "always",
        }
    }
}

/// What [`Look::remember`] writes, in the state directory.
const LOOK_FILE: &str = "look.json";

impl Look {
    /// The operator's last setting, or the shipped one.
    ///
    /// Separate from the config file on purpose. `Config` is what a run was
    /// *started* with — a repository, a cap, an editor command, all of them
    /// arguments — and these two are what the operator settled on while looking
    /// at the map. Writing them back into a file somebody hand-edited would
    /// rewrite their comments and their formatting to record a slider they
    /// dragged; a file of its own beside the corpus records it without touching
    /// anything they wrote.
    ///
    /// Every failure here is silent and falls back to the default, for the same
    /// reason [`Config::load`]'s is: a malformed file must not stop the window
    /// opening.
    #[must_use]
    pub fn load(state_dir: Option<&std::path::Path>) -> Self {
        let Some(dir) = state_dir.map(PathBuf::from).or_else(Self::state_dir) else {
            return Self::default();
        };
        std::fs::read_to_string(dir.join(LOOK_FILE))
            .ok()
            .and_then(|text| serde_json::from_str::<Self>(&text).ok())
            .map_or_else(Self::default, Self::sane)
    }

    /// Records the current setting. Silent on failure.
    pub fn remember(self, state_dir: Option<&std::path::Path>) {
        let Some(dir) = state_dir.map(PathBuf::from).or_else(Self::state_dir) else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(text) = serde_json::to_string_pretty(&self) {
            let _ = std::fs::write(dir.join(LOOK_FILE), text);
        }
    }

    fn state_dir() -> Option<PathBuf> {
        Config::default_state_dir()
    }

    /// A veil out of range is a file that was edited by hand, not a new
    /// notation: the slider cannot produce one, and a number outside `[0, 1]`
    /// would scale the body past opaque or invert it.
    fn sane(self) -> Self {
        Self {
            cloud_veil: self.cloud_veil.clamp(0.0, 1.0),
            ..self
        }
    }
}

impl Default for Look {
    fn default() -> Self {
        Self {
            cloud_veil: polis_render::live::CLOUD_VEIL,
            // One line at a time. The map is a picture before it is a diagram,
            // and `Always` is a line per thread across it.
            // Every cloud, not just the one being pointed at: the operator
            // asked for "a line to the thread in the rail it connects to", and
            // a link that only appears once you already know which cloud you
            // mean answers a question you have stopped asking.
            connectors: Connectors::Always,
        }
    }
}

/// Per-channel switches (PRD §4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// Which ingest channels to run, by name.
    ///
    /// A list of [`Channel`] rather than a flag per channel, so the config file
    /// reads `channels = ["Otel", "Hook"]` and cannot drift out of step with the
    /// enum PRD §4 defines. An unknown name is a warning and is skipped, never a
    /// startup failure.
    pub channels: Vec<Channel>,
    /// Whether to request the **beta** traces channel from agents Polis launches.
    ///
    /// On by default, because it is the only way to attribute a tool call to a
    /// subagent — the logs channel carries zero agent discriminators. Switchable
    /// because it is beta and Anthropic may change it; with it off, Polis
    /// attributes every tool call to the main agent and says so in the status bar
    /// rather than guessing (ADR-0006).
    pub subagent_traces: bool,
}

/// What a click on a building runs (PRD §12).
///
/// > **Click a building** → open in `$EDITOR` via the configured command.
/// > Nothing more.
///
/// `$EDITOR` is not on its own enough — it names an *editor*, not a command that
/// takes a file and a line — so three sources are tried in order:
///
/// 1. `POLIS_EDITOR`, a whole command template with `{path}` and `{line}`. This
///    is the one an operator sets when their editor wants arguments in a
///    particular shape, and it is what a test sets to something harmless.
/// 2. `VISUAL`, then `EDITOR`: a bare editor name, given the path as its only
///    argument. This is what PRD §12 literally names, and honouring it is why
///    the default is not simply hardcoded.
/// 3. VS Code's `--goto`, which is the common case on this platform.
///
/// An empty template disables the launch entirely, which is the setting for an
/// operator who wants the map to be a map and nothing else.
pub fn default_editor_command() -> String {
    editor_command_from(|name| std::env::var_os(name).map(|v| v.to_string_lossy().into_owned()))
}

/// [`default_editor_command`]'s rule, against an arbitrary environment.
///
/// Split out so the rule can be tested without mutating the process
/// environment, which is a global other tests share.
pub fn editor_command_from(get: impl Fn(&str) -> Option<String>) -> String {
    if let Some(template) = get("POLIS_EDITOR") {
        return template;
    }
    for name in ["VISUAL", "EDITOR"] {
        if let Some(editor) = get(name) {
            if !editor.trim().is_empty() {
                return format!("{} {{path}}", editor.trim());
            }
        }
    }
    "code --goto {path}:{line}".to_owned()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // `.` rather than a canonical path: `Cli::repo_root` overwrites this
            // for every real run, and a `Default` that reads the filesystem is a
            // `Default` that can fail.
            repo_root: PathBuf::from("."),
            editor_command: default_editor_command(),
            // PRD §17 open question 1: unanswered against a real fleet, so this
            // is a starting value and not a finding. Forty threads means forty
            // systems and the map vanishes under haze (PRD §10.4).
            cloud_cap: polis_world::territory::CLOUD_CAP,
            // Off at the widest zoom (PRD §9).
            streets: false,
            channels: ChannelConfig::default(),
            state_dir: None,
            // Nothing leaves the machine unless the operator asks, per run.
            captions: false,
            look: Look::default(),
        }
    }
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            channels: vec![
                Channel::Otel,
                Channel::Hook,
                Channel::Fs,
                Channel::Transcript,
            ],
            // On by default: it is the only way to attribute a tool call to a
            // subagent, because the logs channel carries no agent discriminator
            // at all (ADR-0006).
            subagent_traces: true,
        }
    }
}

impl Config {
    /// Loads configuration, falling back to defaults.
    ///
    /// A malformed config file is a warning and a fallback, never a startup
    /// failure — the same rule the four ingest channels follow.
    pub fn load(path: Option<&std::path::Path>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                eprintln!(
                    "polis: cannot read {} ({error}); using defaults",
                    path.display()
                );
                return Self::default();
            }
        };
        match serde_json::from_str(&text) {
            Ok(config) => config,
            Err(error) => {
                eprintln!(
                    "polis: {} is not valid configuration ({error}); using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// The platform state directory holding `corpus.db` and the hook endpoint
    /// file, used when [`Config::state_dir`] is `None`.
    ///
    /// `%LOCALAPPDATA%\polis` on Windows; `$XDG_STATE_HOME/polis` with a
    /// `~/.local/state` fallback elsewhere — `XDG_RUNTIME_DIR` is measurably
    /// unset in real environments, so it cannot be the only lookup.
    pub fn default_state_dir() -> Option<PathBuf> {
        #[cfg(windows)]
        {
            std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("polis"))
        }
        #[cfg(not(windows))]
        {
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
                })
                .map(|base| base.join("polis"))
        }
    }

    /// The state directory this configuration actually uses.
    pub fn state_dir(&self) -> Option<PathBuf> {
        self.state_dir.clone().or_else(Self::default_state_dir)
    }

    /// The configured channels as an ingest set.
    ///
    /// An unknown name never reaches here: `serde` would have rejected the whole
    /// file, and [`Config::load`] turns that into a warning plus defaults.
    pub fn channel_set(&self) -> polis_ingest::ChannelSet {
        polis_ingest::ChannelSet::from_channels(&self.channels.channels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_malformed_config_is_a_warning_and_a_fallback_never_a_startup_failure() {
        let dir = crate::testutil::scratch("config");
        let path = dir.join("polis.json");
        std::fs::write(&path, "{ not json at all").unwrap();
        let config = Config::load(Some(&path));
        assert_eq!(config.cloud_cap, Config::default().cloud_cap);

        // A file that is not there at all is not even a warning.
        let missing = dir.join("absent.json");
        assert_eq!(
            Config::load(Some(&missing)).editor_command,
            Config::default().editor_command
        );
    }

    #[test]
    fn the_editor_command_prefers_polis_editor_then_visual_then_editor() {
        let env = |pairs: Vec<(&'static str, &'static str)>| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert_eq!(
            editor_command_from(env(vec![
                ("POLIS_EDITOR", "ed {path} +{line}"),
                ("EDITOR", "vi")
            ])),
            "ed {path} +{line}"
        );
        assert_eq!(
            editor_command_from(env(vec![("VISUAL", "nvim"), ("EDITOR", "vi")])),
            "nvim {path}"
        );
        assert_eq!(
            editor_command_from(env(vec![("EDITOR", "vi")])),
            "vi {path}"
        );
        // A blank $EDITOR is not a choice, so it falls through.
        assert_eq!(
            editor_command_from(env(vec![("EDITOR", "   ")])),
            "code --goto {path}:{line}"
        );
        assert_eq!(
            editor_command_from(env(vec![])),
            "code --goto {path}:{line}"
        );
    }

    #[test]
    fn the_defaults_round_trip_through_the_config_file_format() {
        let text = serde_json::to_string_pretty(&Config::default()).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.channels.channels.len(), 4);
        assert!(
            back.channels.subagent_traces,
            "the beta traces channel is the only subagent discriminator (ADR-0006)"
        );
        assert_eq!(back.channel_set(), polis_ingest::ChannelSet::ALL);
    }
}
