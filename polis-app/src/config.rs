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
    /// PRD §17 open question 1 asks what the right cap is, and whether dormant
    /// threads should dissipate entirely or leave a faint residue. It is
    /// configurable until that is answered against a real fleet.
    pub cloud_cap: usize,
    /// Whether the streets layer is on. Off by default at the widest zoom
    /// (PRD §9).
    pub streets: bool,
    /// Which ingest channels to run.
    pub channels: ChannelConfig,
    /// Where the state directory lives, when it should not be the platform
    /// default. Tests and replay set it; operators normally do not.
    pub state_dir: Option<PathBuf>,
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

impl Default for Config {
    fn default() -> Self {
        Self {
            // `.` rather than a canonical path: `Cli::repo_root` overwrites this
            // for every real run, and a `Default` that reads the filesystem is a
            // `Default` that can fail.
            repo_root: PathBuf::from("."),
            // `$EDITOR` is not enough on its own — it names an editor, not a
            // command that takes a file — so the default spells the whole thing
            // and an operator with a different editor replaces one string.
            editor_command: "code --goto {path}:{line}".to_owned(),
            // PRD §17 open question 1: unanswered against a real fleet, so this
            // is a starting value and not a finding. Forty threads means forty
            // systems and the map vanishes under haze (PRD §10.4).
            cloud_cap: 12,
            // Off at the widest zoom (PRD §9).
            streets: false,
            channels: ChannelConfig::default(),
            state_dir: None,
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
