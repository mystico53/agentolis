//! The last gate before anything leaves the machine.
//!
//! # Symmetry, not paranoia
//!
//! [`crate::describe::sanitise`] already refuses credential-shaped text on the
//! way *in*, because a description is drawn onto an image the operator may
//! share (ADR-0087). The moment a description can also be *sent somewhere*, the
//! same rule has to run in the other direction, over a larger surface: not only
//! the sentence that would be drawn, but every file name and every doc snippet
//! that goes into the prompt.
//!
//! The two directions are not the same check applied twice. Inbound, a
//! credential-shaped candidate is thrown away and the next source is tried, so
//! the cost of a false positive is one blank label. Outbound, the cost of a
//! false *negative* is a key on somebody else's server, permanently. So the
//! outbound gate keeps the inbound rule
//! ([`crate::describe::looks_like_secret`], which is the shared definition and
//! deliberately not a second one) and drops **whole fields** rather than
//! redacting inside them: a partially-redacted secret still says *there is a
//! secret in this district*, which is itself worth not sending.
//!
//! # What the report may say
//!
//! [`RedactionReport`] counts and names *paths*. It never carries the offending
//! text — a report of what the redactor caught, printed to a terminal or pasted
//! into an issue, must not be the leak.

use std::collections::BTreeSet;

use polis_events::LogicalPath;
use serde::{Deserialize, Serialize};

/// Why a field was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboundReject {
    /// It matched [`crate::describe::looks_like_secret`]: a published key
    /// prefix, an assignment to a name like `api_key`, a URL with a password in
    /// it, or a high-entropy token.
    Secret,
}

/// What one pass refused to send.
///
/// **Paths and counts only.** See the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionReport {
    /// File names dropped from a district's listing.
    pub names_dropped: u32,
    /// Doc snippets dropped from a district's evidence.
    pub docs_dropped: u32,
    /// Districts skipped whole, because their own path was credential-shaped.
    pub districts_skipped: u32,
    /// Characters removed because they were control codes or bidi overrides.
    pub unsafe_chars_removed: u64,
    /// Where it happened, in path order and de-duplicated, so the operator can
    /// go and look. At most [`RedactionReport::MAX_PATHS`] of them.
    pub paths: BTreeSet<LogicalPath>,
}

impl RedactionReport {
    /// How many paths the report will name before it stops collecting.
    pub const MAX_PATHS: usize = 64;

    /// True when nothing was refused.
    pub fn is_empty(&self) -> bool {
        self.names_dropped == 0 && self.docs_dropped == 0 && self.districts_skipped == 0
    }

    /// Everything refused, however caused.
    pub fn total(&self) -> u32 {
        self.names_dropped + self.docs_dropped + self.districts_skipped
    }

    /// Merges another pass's counters.
    pub fn merge(&mut self, other: &Self) {
        self.names_dropped += other.names_dropped;
        self.docs_dropped += other.docs_dropped;
        self.districts_skipped += other.districts_skipped;
        self.unsafe_chars_removed += other.unsafe_chars_removed;
        for path in &other.paths {
            if self.paths.len() >= Self::MAX_PATHS {
                break;
            }
            self.paths.insert(path.clone());
        }
    }

    /// Records a path, up to the cap.
    fn note(&mut self, path: &LogicalPath) {
        if self.paths.len() < Self::MAX_PATHS {
            self.paths.insert(path.clone());
        }
    }

    /// Records a dropped file name.
    pub fn drop_name(&mut self, district: &LogicalPath) {
        self.names_dropped += 1;
        self.note(district);
    }

    /// Records a dropped doc snippet.
    pub fn drop_doc(&mut self, district: &LogicalPath) {
        self.docs_dropped += 1;
        self.note(district);
    }

    /// Records a district skipped whole.
    pub fn skip_district(&mut self, district: &LogicalPath) {
        self.districts_skipped += 1;
        self.note(district);
    }
}

/// The longest field this module will send. Anything longer is a file, not a
/// name or a sentence.
pub const MAX_FIELD_CHARS: usize = 400;

/// Prepares one field for sending, or refuses it.
///
/// Two steps, in this order:
///
/// 1. **Strip what must never be transmitted or drawn** — every control
///    character and the Unicode bidi overrides and zero-width characters, using
///    `crate::describe::is_unsafe_char`, the same predicate the inbound
///    sanitiser uses. Whitespace runs collapse to one space.
/// 2. **Refuse the whole field** if what remains is credential-shaped.
///
/// The result is bounded to [`MAX_FIELD_CHARS`], counted in `char`s so a
/// multi-byte character is never split.
pub fn vet(raw: &str, report: &mut RedactionReport) -> Result<String, OutboundReject> {
    let mut removed = 0u64;
    let mut out = String::with_capacity(raw.len());
    let mut space = false;
    for c in raw.chars() {
        if crate::describe::is_unsafe_char(c) {
            removed += 1;
            continue;
        }
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    report.unsafe_chars_removed += removed;
    if crate::describe::looks_like_secret(&out) {
        return Err(OutboundReject::Secret);
    }
    if out.chars().count() > MAX_FIELD_CHARS {
        let cut = out
            .char_indices()
            .nth(MAX_FIELD_CHARS)
            .map_or(out.len(), |(i, _)| i);
        out.truncate(cut);
    }
    Ok(out)
}

/// [`vet`] for a whole list, dropping the fields that fail and counting them
/// against `district`.
pub fn vet_names(
    district: &LogicalPath,
    names: impl IntoIterator<Item = String>,
    report: &mut RedactionReport,
) -> Vec<String> {
    let mut out = Vec::new();
    for name in names {
        match vet(&name, report) {
            Ok(clean) if !clean.is_empty() => out.push(clean),
            Ok(_) => {}
            Err(OutboundReject::Secret) => report.drop_name(district),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    #[test]
    fn ordinary_names_and_sentences_go_out_unchanged() {
        let mut report = RedactionReport::default();
        for text in [
            "BookContext.tsx",
            "use_case_handlers.py",
            "Firestore access for the reading list.",
            "components/MediaWindow/renderers/YouTubeRenderer.jsx",
            "Секретариат.ts",
        ] {
            assert_eq!(vet(text, &mut report).as_deref(), Ok(text), "{text}");
        }
        assert!(report.is_empty(), "{report:?}");
    }

    /// The inbound rule, applied outbound. The same table
    /// `describe::anything_credential_shaped_is_dropped_whole` uses, because
    /// having two definitions of "looks like a secret" is how one of them rots.
    #[test]
    fn anything_credential_shaped_is_refused_whole() {
        let mut report = RedactionReport::default();
        for text in [
            "Deploy key sk-abcdefghijklmnopqrstuvwxyz012345 for the staging cluster",
            "Set the api_key=\"9f2b8c1d\" before running the importer here",
            "Connect with postgres://admin:hunter2@db.internal/app for the tests",
            "-----BEGIN RSA PRIVATE KEY----- and the rest of the material",
            "The upload token is ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8 exactly",
            "Uses AKIAIOSFODNN7EXAMPLE to reach the bucket from the worker",
            "A high entropy value dGhpc0lzQVNlY3JldFZhbHVlMTIz sits in the env",
        ] {
            assert_eq!(
                vet(text, &mut report),
                Err(OutboundReject::Secret),
                "{text}"
            );
        }
    }

    /// A file *named* like a credential is real: `qurio-toolset` has an
    /// `AuthKey_*.p8` beside a `p12-password.txt`. Its name is not a secret, but
    /// a name that trips the rule is not worth arguing with.
    #[test]
    fn a_credential_shaped_file_name_is_dropped_and_the_rest_of_the_district_still_goes() {
        let mut report = RedactionReport::default();
        let names = vet_names(
            &lp("electron/signing/apple"),
            [
                "notarize.js".to_owned(),
                "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8".to_owned(),
                "entitlements.plist".to_owned(),
            ],
            &mut report,
        );
        assert_eq!(names, ["notarize.js", "entitlements.plist"]);
        assert_eq!(report.names_dropped, 1);
        assert_eq!(report.total(), 1);
        assert!(!report.is_empty());
        assert!(report.paths.contains(&lp("electron/signing/apple")));
    }

    #[test]
    fn control_characters_and_bidi_overrides_never_leave_the_machine() {
        let mut report = RedactionReport::default();
        let clean = vet("auth\u{202E}gnitset\u{0007} service", &mut report).expect("prose");
        assert!(!clean.chars().any(char::is_control), "{clean:?}");
        assert!(!clean.contains('\u{202E}'), "{clean:?}");
        assert_eq!(clean, "authgnitset service");
        assert_eq!(report.unsafe_chars_removed, 2);
        // Stripping is not refusing: the field still goes.
        assert!(report.is_empty());
    }

    #[test]
    fn a_field_longer_than_a_sentence_is_bounded_not_sent() {
        let mut report = RedactionReport::default();
        let long = "word ".repeat(500);
        let clean = vet(&long, &mut report).expect("prose");
        assert_eq!(clean.chars().count(), MAX_FIELD_CHARS);
        let multibyte = "日本語".repeat(500);
        let clean = vet(&multibyte, &mut report).expect("prose");
        assert_eq!(clean.chars().count(), MAX_FIELD_CHARS);
    }

    #[test]
    fn the_report_names_paths_and_never_the_text_it_caught() {
        let mut report = RedactionReport::default();
        let secret = "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
        assert!(vet(secret, &mut report).is_err());
        report.drop_doc(&lp("src/auth"));
        let printed = format!("{report:?}");
        assert!(!printed.contains("ghp_"), "{printed}");
        let json = serde_json::to_string(&report).expect("serialize");
        assert!(!json.contains("ghp_"), "{json}");
        assert!(json.contains("src/auth"), "{json}");
    }

    #[test]
    fn the_path_list_is_capped_so_a_report_cannot_become_a_dump() {
        let mut report = RedactionReport::default();
        for i in 0..(RedactionReport::MAX_PATHS * 3) {
            report.drop_name(&lp(&format!("d{i:04}")));
        }
        assert_eq!(report.paths.len(), RedactionReport::MAX_PATHS);
        assert_eq!(
            report.names_dropped,
            u32::try_from(RedactionReport::MAX_PATHS * 3).expect("small"),
            "the count is not capped, only the list"
        );
        let mut other = RedactionReport::default();
        other.skip_district(&lp("zzz"));
        report.merge(&other);
        assert_eq!(report.districts_skipped, 1);
        assert_eq!(report.paths.len(), RedactionReport::MAX_PATHS);
    }
}
