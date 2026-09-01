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
        todo!("PRD §12 — defaults; the cloud cap is PRD §17's open question 1")
    }
}

impl Config {
    /// Loads configuration, falling back to defaults.
    ///
    /// A malformed config file is a warning and a fallback, never a startup
    /// failure — the same rule the four ingest channels follow.
    pub fn load(path: Option<&std::path::Path>) -> Self {
        let _ = path;
        todo!("PRD §12 — load, warn on malformed, never fail to start")
    }

    /// The platform state directory holding `corpus.db` and the hook endpoint
    /// file, used when [`Config::state_dir`] is `None`.
    ///
    /// `%LOCALAPPDATA%\polis` on Windows; `$XDG_STATE_HOME/polis` with a
    /// `~/.local/state` fallback elsewhere — `XDG_RUNTIME_DIR` is measurably
    /// unset in real environments, so it cannot be the only lookup.
    pub fn default_state_dir() -> Option<PathBuf> {
        todo!("PRD §6.1 — %LOCALAPPDATA% / XDG_STATE_HOME / ~/.local/state")
    }
}
