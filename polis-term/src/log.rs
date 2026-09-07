//! The per-pane raw byte log — what makes detaching survivable (ADR-0098).
//!
//! Every byte a pty produces is appended here before it is broadcast. That one
//! habit buys three things that would otherwise each be their own feature:
//!
//! * **Reattach.** A window that starts an hour after the daemon replays the log
//!   through its parser and gets the pane's real screen, scrollback included.
//!   There is nothing to serialise, because the bytes *are* the state.
//! * **Catch-up.** A window that was following passes the offset it last saw and
//!   receives only what it missed, so a dropped connection is not a lost screen.
//! * **Test fixtures.** A corrupt screen is a file that can be replayed into
//!   `tests/grid.rs`, rather than a bug reproduced by hand.
//!
//! `docs/roadmap/terminal-integration.md` listed "write the per-pane raw byte log
//! from day one" as the first of four things M7 had to leave in place for M8.
//! Building the daemon first makes it load-bearing immediately instead.
//!
//! # Offsets survive a roll
//!
//! The log is capped ([`ByteLog::CAP`]). When it fills, the file is emptied and
//! restarted with `ESC c` — a full terminal reset — so that whatever a client
//! replays next begins from a defined state rather than the middle of an escape
//! sequence. The **offsets do not restart**: they count every byte the pane has
//! ever produced, so a `since` a client held across a roll is still meaningful
//! and simply lands at [`ByteLog::base`].

use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

/// `ESC c` — RIS, a full terminal reset. The first bytes after a roll.
const RESET: &[u8] = b"\x1bc";

/// A pane's output, on disk, with offsets that never go backwards.
#[derive(Debug)]
pub struct ByteLog {
    path: PathBuf,
    file: Option<File>,
    /// Total bytes this pane has ever produced.
    len: u64,
    /// The offset of the first byte still held. Non-zero only after a roll.
    base: u64,
    cap: u64,
}

impl ByteLog {
    /// How much of one pane's output is kept.
    ///
    /// 8 MiB is roughly forty full-screen repaints per megabyte at Claude Code's
    /// density, so this is a long session rather than a long afternoon. It is
    /// the reattach budget, not the scrollback budget — the window keeps its own
    /// 10 000 lines once it has parsed them.
    pub const CAP: u64 = 8 * 1024 * 1024;

    /// Opens a fresh log for `pane` under `dir`, truncating any stale one.
    ///
    /// # Errors
    ///
    /// When the directory cannot be created or the file cannot be opened.
    pub fn create(dir: &Path, pane: crate::proto::PaneId) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("pane-{}.log", pane.0));
        let file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        Ok(Self {
            path,
            file: Some(file),
            len: 0,
            base: 0,
            cap: Self::CAP,
        })
    }

    /// A log that keeps nothing, for a pane whose state directory is unwritable.
    ///
    /// A daemon that cannot write a log still has to run panes: losing reattach
    /// is a degradation, and refusing to start an agent because a directory is
    /// read-only is not. Every method stays correct, `len` still counts, and
    /// [`ByteLog::read_from`] simply has nothing to give.
    #[must_use]
    pub fn discarding() -> Self {
        Self {
            path: PathBuf::new(),
            file: None,
            len: 0,
            base: 0,
            cap: 0,
        }
    }

    /// Appends `bytes`, returning the offset of the first of them.
    ///
    /// Errors are swallowed on purpose and cost only reattach: a full disk must
    /// not stop an agent that is running fine.
    pub fn append(&mut self, bytes: &[u8]) -> u64 {
        let offset = self.len;
        self.len += bytes.len() as u64;
        let Some(file) = self.file.as_mut() else {
            return offset;
        };
        if file.write_all(bytes).is_err() {
            self.file = None;
            return offset;
        }
        if self.len - self.base > self.cap {
            self.roll();
        }
        offset
    }

    /// Empties the file and restarts it from a terminal reset.
    fn roll(&mut self) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let rolled = file
            .set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)))
            .and_then(|_| file.write_all(RESET));
        if rolled.is_err() {
            self.file = None;
            return;
        }
        self.base = self.len;
        self.len += RESET.len() as u64;
    }

    /// Total bytes this pane has ever produced. The `since` a fresh client wants.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// True before the pane has produced anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The offset of the oldest byte still held.
    ///
    /// Non-zero only after a roll, and the point at which a client's `since` is
    /// silently clamped forwards.
    #[must_use]
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Everything from `since` onwards, and the offset it actually starts at.
    ///
    /// A `since` before [`ByteLog::base`] is clamped forwards rather than
    /// refused: the client asked for more history than exists, and the honest
    /// answer is all of it plus the offset that says so.
    ///
    /// # Errors
    ///
    /// When the file cannot be read. A caller treats that as "no history".
    pub fn read_from(&mut self, since: u64) -> std::io::Result<(u64, Vec<u8>)> {
        let start = since.max(self.base).min(self.len);
        let Some(file) = self.file.as_mut() else {
            return Ok((self.len, Vec::new()));
        };
        if start >= self.len {
            return Ok((self.len, Vec::new()));
        }
        file.seek(SeekFrom::Start(start - self.base))?;
        let mut bytes = Vec::with_capacity(usize::try_from(self.len - start).unwrap_or(0));
        file.read_to_end(&mut bytes)?;
        // The file is reopened by nobody else, but a short read is still
        // possible if the pane wrote while we seeked; the offset stays truthful.
        file.seek(SeekFrom::End(0))?;
        Ok((start, bytes))
    }

    /// Deletes the file. Silent on failure — a leftover log is not an incident.
    pub fn remove(&mut self) {
        self.file = None;
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::PaneId;

    #[test]
    fn what_goes_in_comes_out_at_the_offset_it_went_in_at() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = ByteLog::create(dir.path(), PaneId(1)).expect("create");
        assert!(log.is_empty());
        assert_eq!(log.append(b"hello "), 0);
        assert_eq!(log.append(b"world"), 6);
        assert_eq!(log.len(), 11);

        let (offset, all) = log.read_from(0).expect("read");
        assert_eq!(offset, 0);
        assert_eq!(all, b"hello world");

        let (offset, tail) = log.read_from(6).expect("read");
        assert_eq!(offset, 6);
        assert_eq!(tail, b"world");

        let (offset, nothing) = log.read_from(11).expect("read");
        assert_eq!(offset, 11);
        assert!(nothing.is_empty(), "caught up means nothing to send");
    }

    /// A client that reconnects passes `offset + data.len()`. That arithmetic
    /// has to land exactly on the next unseen byte, forever.
    #[test]
    fn catching_up_in_steps_sees_every_byte_exactly_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = ByteLog::create(dir.path(), PaneId(2)).expect("create");
        let mut written = Vec::new();
        let mut seen = Vec::new();
        let mut since = 0;
        for round in 0u8..64 {
            let chunk = vec![round; 37];
            log.append(&chunk);
            written.extend_from_slice(&chunk);
            let (offset, bytes) = log.read_from(since).expect("read");
            assert_eq!(offset, since, "no gap, no overlap");
            seen.extend_from_slice(&bytes);
            since = offset + bytes.len() as u64;
        }
        assert_eq!(seen, written);
        assert_eq!(since, log.len());
    }

    /// A rolled log must still hand back something a parser can start on, and
    /// must not rewind the offsets a client is holding.
    #[test]
    fn a_full_log_rolls_forward_and_starts_from_a_reset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = ByteLog::create(dir.path(), PaneId(3)).expect("create");
        log.cap = 4096;

        for _ in 0..8 {
            log.append(&vec![b'x'; 1024]);
        }
        assert!(log.base() > 0, "the log should have rolled");
        assert!(
            log.len() > 8 * 1024,
            "offsets count every byte ever written"
        );

        let (offset, bytes) = log.read_from(0).expect("read");
        assert_eq!(offset, log.base(), "an impossible `since` clamps forwards");
        assert!(
            bytes.starts_with(RESET),
            "a replay must begin from a defined terminal state"
        );
        assert_eq!(
            offset + bytes.len() as u64,
            log.len(),
            "the arithmetic a client does still lands on the end"
        );
    }

    /// A daemon whose state directory is read-only still runs agents.
    #[test]
    fn a_discarding_log_still_counts_and_never_panics() {
        let mut log = ByteLog::discarding();
        assert_eq!(log.append(b"abc"), 0);
        assert_eq!(log.append(b"de"), 3);
        assert_eq!(log.len(), 5);
        let (offset, bytes) = log.read_from(0).expect("read");
        assert_eq!(offset, 5, "there is no history, and it says so");
        assert!(bytes.is_empty());
        log.remove();
    }

    #[test]
    fn removing_a_log_deletes_its_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = ByteLog::create(dir.path(), PaneId(9)).expect("create");
        log.append(b"something");
        let path = log.path.clone();
        assert!(path.is_file());
        log.remove();
        assert!(!path.exists());
    }
}
