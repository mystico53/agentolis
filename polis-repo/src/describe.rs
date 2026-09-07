//! What a neighborhood *does*, derived from the repository itself (PRD §12).
//!
//! The map already says where a district is and, since [`crate::kinds`], what
//! kind of code is in it. What it cannot say is what the code is *for*, and that
//! is the thing a person actually wants next to a label. This module derives a
//! sentence for it from what is already checked in — a README, a package
//! manifest, a module doc comment.
//!
//! # No model, local or remote
//!
//! Nothing here calls anything. PRD §2 is explicit:
//!
//! > Single operator, local machine, local data. No server, no auth, no
//! > telemetry leaving the box.
//!
//! So the only sources are files in the repository, and the only transformation
//! is extraction, sanitising and truncation. A description is always a quotation
//! of the operator's own repository, never a generation.
//!
//! # Priority, and where it stops
//!
//! [`Neighborhoods::describe`](crate::neighborhoods::Neighborhoods::describe)
//! tries four sources in this order and takes the first that survives
//! [`sanitise`]:
//!
//! 1. **A README in the directory** — its first sentence, or its first heading
//!    when that heading says something the directory name does not.
//! 2. **A package manifest in the directory** — `package.json`'s
//!    `"description"`, `Cargo.toml`'s and `pyproject.toml`'s `description`.
//! 3. **A module doc comment** on the district's anchor file — Rust `//!`, a
//!    Python module docstring, a leading JS/TS block comment. Parsed with the
//!    same tree-sitter grammars [`crate::imports`] loads, not with regexes.
//! 4. **An inventory**, synthesised from the district's own contents.
//!
//! Source 4 is fenced off from the other three by
//! [`DescriptionSource::is_prose`], and it is deliberately a *statement of
//! fact* — "38 files, 62% source · most imported: client.ts" — rather than a
//! guess at intent. The reason is the standard the brief sets and this module
//! is held to: a wrong description is worse than none. "utils" described as
//! "Utility functions" is noise; "utils — 22 files, most imported: retry.ts" is
//! not, because it is true and it is not in the name.
//!
//! When even the inventory has nothing beyond what the label already says, the
//! answer is [`None`]. Silence is a correct answer here.
//!
//! # A description is rendered onto an image the operator may share
//!
//! Which makes [`sanitise`] load-bearing rather than hygiene. Every candidate is
//! stripped of markup and control characters, **rejected outright** if it looks
//! like source code or carries anything credential-shaped, and bounded to a
//! fixed length. See [`sanitise`] for the exact rules and
//! [`SanitiseReject`] for what each rejection means.
//!
//! # Determinism
//!
//! Every extractor is a pure function of the bytes it is given. The cache
//! ([`DescriptionCache`]) is keyed on a 128-bit content digest plus the byte
//! length, so a hit is only ever the answer those exact bytes produced — the
//! same guarantee, and the same digest, [`crate::imports::ParseCache`] uses.

use std::collections::BTreeMap;
use std::path::Path;

use polis_events::LogicalPath;
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser};

use crate::Language;

// ---------------------------------------------------------------------------
// The description
// ---------------------------------------------------------------------------

/// Where a description came from.
///
/// Kept on the description rather than thrown away, for two reasons. The
/// renderer may want to draw a quoted README differently from a synthesised
/// inventory; and the honest coverage number — "how many districts could we
/// actually describe" — is not answerable without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DescriptionSource {
    /// The first sentence, or first heading, of a README in the directory.
    Readme,
    /// The `description` field of a package manifest in the directory.
    Manifest,
    /// A module-level doc comment on the district's anchor file.
    DocComment,
    /// Synthesised from the district's contents. Facts, not intent.
    Inventory,
    /// Written by a language model from the district's file names and the
    /// documentation the other three sources found (PRD §12, [`crate::llm`]).
    ///
    /// Fenced off from the other four the same way [`Self::Inventory`] is, and
    /// for a sharper reason: it is the only source that is neither a quotation
    /// nor a fact. [`DescriptionSource::is_prose`] stays false for it so the
    /// honest coverage number — *how well does this repository document
    /// itself* — cannot be inflated by having paid for sentences.
    Model,
}

impl DescriptionSource {
    /// True when a **human** wrote the words.
    ///
    /// The coverage number that matters, and the reason
    /// [`DescriptionSource::Model`] is excluded: `docs/design/NEIGHBORHOODS-REVIEW.md`
    /// reports 14 % prose across the operator's repositories, and that number is
    /// a fact about how repositories are written. A model turning it into 90 %
    /// would destroy the only measurement there is of the thing the extractor
    /// exists to find.
    pub fn is_prose(self) -> bool {
        matches!(self, Self::Readme | Self::Manifest | Self::DocComment)
    }

    /// True when a model wrote the words.
    pub fn is_model(self) -> bool {
        matches!(self, Self::Model)
    }

    /// A stable name for logs and reports.
    pub fn name(self) -> &'static str {
        match self {
            Self::Readme => "readme",
            Self::Manifest => "manifest",
            Self::DocComment => "doc-comment",
            Self::Inventory => "inventory",
            Self::Model => "model",
        }
    }
}

impl std::fmt::Display for DescriptionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What a neighborhood does, in two lengths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Description {
    /// One line to sit beside the district name on the map. Bounded to
    /// [`LABEL_MAX_CHARS`].
    pub label: String,
    /// A sentence or two for PRD §12's drill-down panel. Bounded to
    /// [`DETAIL_MAX_CHARS`]. Equal to [`Description::label`] when the source had
    /// only one sentence to give.
    pub detail: String,
    /// Which of the four sources produced it.
    pub source: DescriptionSource,
    /// The file it was quoted from, when it was quoted from one.
    pub origin: Option<LogicalPath>,
}

impl Description {
    /// True when a human wrote the words — see [`DescriptionSource::is_prose`].
    pub fn is_prose(&self) -> bool {
        self.source.is_prose()
    }

    /// True when a model wrote the words (PRD §12, [`crate::llm`]).
    pub fn is_model(&self) -> bool {
        self.source.is_model()
    }
}

/// The map label's ceiling, in characters.
///
/// The brief asks for "roughly 40–70 characters". 70 is the ceiling rather than
/// the target: a shorter sentence is not padded, and PRD §13's label layer has
/// to fit this next to a district name without colliding with its neighbour.
pub const LABEL_MAX_CHARS: usize = 70;

/// The drill-down panel's ceiling, in characters.
pub const DETAIL_MAX_CHARS: usize = 240;

/// The shortest text worth showing. Below this it is a fragment, not a
/// description.
pub const MIN_CHARS: usize = 12;

// ---------------------------------------------------------------------------
// Sanitising
// ---------------------------------------------------------------------------

/// Why a candidate description was thrown away.
///
/// Counted rather than logged: "we dropped 4 descriptions on this repository and
/// three of them looked like code" is a fact about the extractors that a silent
/// `None` would hide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SanitiseReject {
    /// Nothing left after markup and control characters were stripped, or what
    /// was left was shorter than [`MIN_CHARS`].
    TooShort,
    /// It reads as source code, not prose. Braces, statement terminators, a
    /// leading `fn`/`def`/`import`, or too few letters per character.
    LooksLikeCode,
    /// It carries something credential-shaped: a known key prefix, a PEM
    /// header, a high-entropy token, a URL with a password in it, or an
    /// assignment to a name like `api_key`.
    LooksLikeSecret,
    /// It is text a *tool* wrote, not text about this repository: a linter
    /// pragma at the top of a file, or a project generator's untouched README.
    /// See [`looks_like_boilerplate`].
    Boilerplate,
}

impl SanitiseReject {
    /// A stable name for reports.
    pub fn name(self) -> &'static str {
        match self {
            Self::TooShort => "too-short",
            Self::LooksLikeCode => "looks-like-code",
            Self::LooksLikeSecret => "looks-like-secret",
            Self::Boilerplate => "boilerplate",
        }
    }
}

/// Turns a candidate string into safe display prose, or rejects it.
///
/// # What it strips
///
/// * **Control characters.** Every C0 and C1 control, plus the Unicode bidi
///   overrides and zero-width characters (`U+200B`–`U+200F`, `U+202A`–`U+202E`,
///   `U+2066`–`U+2069`, `U+FEFF`). Those can make rendered text read as
///   something other than its bytes, which on an image the operator may share is
///   a spoofing channel, not a typographic nicety.
/// * **Markup.** Markdown images are deleted whole (a badge row is not a
///   description), links keep their text and lose their URL, and `#`, `*`,
///   backticks and blockquote markers are removed. HTML tags are deleted.
/// * **Whitespace.** Newlines and tabs become spaces; runs collapse to one.
///
/// # What it rejects, and why rejection rather than redaction
///
/// A partially-redacted secret is still evidence that there is a secret at
/// `path/to/file`, and a partially-redacted line of code is still unreadable.
/// So a candidate that trips a rule is dropped whole and the next source is
/// tried. See [`SanitiseReject`].
///
/// The third rule is [`looks_like_boilerplate`]: text a tool wrote about
/// nothing in particular — a linter pragma, a project generator's untouched
/// README — is not a description of this repository and is rejected the same
/// way, so the next source gets its turn.
///
/// # Bounding
///
/// The result is truncated to `max` characters — counted in `char`s, so a
/// multi-byte character is never split — at a word boundary where there is one
/// within the last quarter of the budget, with `…` appended.
pub fn sanitise(raw: &str, max: usize) -> Result<String, SanitiseReject> {
    let stripped = strip_markup(raw);
    let text = collapse_whitespace(&stripped);
    if text.chars().count() < MIN_CHARS || !text.chars().any(char::is_alphabetic) {
        return Err(SanitiseReject::TooShort);
    }
    if looks_like_secret(&text) {
        return Err(SanitiseReject::LooksLikeSecret);
    }
    if looks_like_code(&text) {
        return Err(SanitiseReject::LooksLikeCode);
    }
    if looks_like_boilerplate(&text) {
        return Err(SanitiseReject::Boilerplate);
    }
    Ok(truncate_chars(&text, max))
}

/// Removes markup and every character that is not safe to draw.
///
/// Three passes, in this order and for this reason: images are deleted before
/// links, because a badge row is `[![build](shield.svg)](ci)` — a link wrapping
/// an image — and a link-first pass leaves the image's URL behind as text.
fn strip_markup(raw: &str) -> String {
    let images = strip_images(raw);
    let links = strip_links(&images);
    strip_tags_and_marks(&links)
}

/// Deletes `![alt](target)` and `![alt][ref]` entirely.
fn strip_images(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '!' && chars.get(i + 1) == Some(&'[') {
            if let Some(after) = skip_bracketed(&chars, i + 1) {
                i = skip_target(&chars, after);
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Replaces `[text](target)` and `[text][ref]` with `text`.
fn strip_links(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            if let Some(after) = skip_bracketed(&chars, i) {
                let end = skip_target(&chars, after);
                // The text between the brackets, which may itself have been an
                // image and is therefore empty by now.
                out.extend(&chars[i + 1..after - 1]);
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// The index just past the `]` that closes the `[` at `open`, honouring nesting.
/// `None` when it is never closed.
fn skip_bracketed(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in chars.iter().enumerate().skip(open) {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// The index just past a `(…)` target or `[…]` reference at `at`, if there is
/// one. Returns `at` unchanged when there is not.
fn skip_target(chars: &[char], at: usize) -> usize {
    let (open, close) = match chars.get(at) {
        Some('(') => ('(', ')'),
        Some('[') => ('[', ']'),
        _ => return at,
    };
    let mut depth = 0usize;
    for (i, c) in chars.iter().enumerate().skip(at) {
        if *c == open {
            depth += 1;
        } else if *c == close {
            depth -= 1;
            if depth == 0 {
                return i + 1;
            }
        }
    }
    at
}

/// Deletes HTML tags, the remaining Markdown markers, and every character that
/// must not reach a rendered label.
fn strip_tags_and_marks(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_tag = false;
    for c in raw.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            '#' | '*' | '`' | '>' | '|' | '\\' | '[' | ']' => {}
            _ if is_unsafe_char(c) => {}
            _ => out.push(c),
        }
    }
    out
}

/// True for a character that must never reach a rendered label.
///
/// Every control character, and the Unicode formatting characters that can
/// reorder or hide the text around them.
///
/// Shared with [`crate::llm::outbound`], which applies the same rule on the way
/// *out*: text that renders as something other than its bytes is a spoofing
/// channel whichever direction it is travelling, and two copies of this list
/// would be one copy too many.
pub(crate) fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
            | '\u{00AD}')
}

/// Newlines and runs of whitespace become one space; the result is trimmed.
fn collapse_whitespace(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut space = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
            }
            space = false;
            out.push(c);
        }
    }
    out
}

/// Substrings that mean the text is program source rather than prose.
const CODE_MARKERS: &[&str] = &["{", "}", ";", "=>", "()", "&&", "||", "::=", "<?", "?>"];

/// Words that, at the very start, mean the text is a line of code.
const CODE_LEADERS: &[&str] = &[
    "fn ", "def ", "class ", "const ", "let ", "var ", "pub ", "use ", "impl ", "struct ", "enum ",
    "package ", "func ", "#include", "#!", "async ", "export ", "return ", "if (", "for (",
    // A shell transcript. Real READMEs open with one often enough that without
    // this the label of `examples/imagepipe` came out as `$ imagepipe_`.
    "$ ", "> ", "ps> ", "c:\\", "sudo ", "npm ", "pip ", "cargo ",
];

/// True when the text reads as code — or as a file path, which is the same
/// problem wearing different punctuation.
///
/// Four independent tests, any of which is enough. The alphabetic-density one
/// is the general catch: prose is mostly letters and spaces, and a line of code
/// is mostly punctuation, so a run of sigils fails it even when it uses none of
/// the listed markers.
fn looks_like_code(text: &str) -> bool {
    if CODE_MARKERS.iter().any(|m| text.contains(m)) {
        return true;
    }
    // A bare file path is not a description. Real repositories put a
    // `// src/components/BaseWindow/BaseWindow.jsx` banner at the top of a file
    // often enough that without this the map fills up with its own addresses,
    // which the operator can already read off the tree.
    let tokens = text.split_whitespace().count();
    let pathish = text.contains('/') || text.contains('\\');
    if (pathish && tokens < 4) || (tokens < 2 && text.contains('.')) {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    if CODE_LEADERS.iter().any(|m| lower.starts_with(m)) {
        return true;
    }
    let total = text.chars().count();
    let letters = text
        .chars()
        .filter(|c| c.is_alphabetic() || c.is_whitespace() || matches!(c, ',' | '.' | '\'' | '-'))
        .count();
    total > 0 && (letters * 100) / total < 70
}

/// Linter, formatter and type-checker directives, matched against the start of
/// a candidate.
///
/// A leading `/* eslint-disable */` is a block comment in the position a module
/// doc comment occupies, and tree-sitter is right to hand it over: it *is* the
/// first comment in the file. It is simply not prose. Found on
/// `qurio-toolset`'s `src/components/landing`, whose whole description was the
/// two words `eslint-disable`.
///
/// Prefix-matched, not contained, so a sentence that happens to mention a rule
/// name survives.
const PRAGMA_PREFIXES: &[&str] = &[
    "eslint-disable",
    "eslint-enable",
    "eslint-env",
    "prettier-ignore",
    "stylelint-disable",
    "stylelint-enable",
    "biome-ignore",
    "deno-lint-ignore",
    "istanbul ignore",
    "c8 ignore",
    "v8 ignore",
    "jshint ",
    "jslint ",
    "ts-nocheck",
    "ts-ignore",
    "ts-expect-error",
    "@ts-",
    "@flow",
    "noqa",
    "type: ignore",
    "pylint:",
    "mypy:",
    "flake8:",
    "ruff:",
    // `-*- coding: utf-8 -*-`. The `*`s are markup and are gone by the time
    // this runs, which is why the leading punctuation is trimmed first.
    "coding:",
    "webpackignore",
    "webpackchunkname",
    "vite-ignore",
];

/// Verbatim output of a project generator, matched anywhere in a candidate.
///
/// A scaffolded README that nobody edited describes the *generator*, not the
/// repository, and it lands on the one district that can least afford a wrong
/// label: the root — PRD §8's civic square, at the historic centre of the map.
/// Four of the operator's eight repositories had one, and three of those four
/// were the same sentence.
///
/// Kept deliberately short and verbatim. A phrase earns its place here only by
/// being a string a tool emits, never by being a phrase that sounds generic;
/// the general "this says nothing new" test is [`says_nothing_new`], and the
/// general "there is nothing to say" answer is `None`.
const BOILERPLATE_PHRASES: &[&str] = &[
    "bootstrapped with create-next-app",
    "bootstrapped with create react app",
    "getting started with create react app",
    "minimal setup to get react working in vite",
    "this template should help get you started developing with vue",
    "recommended ide setup",
    "learn more about the power of turborepo",
    "your new site is ready",
    "this project was generated with angular cli",
];

/// True when the text is a tool's output rather than a description of this
/// repository.
///
/// Two families, and they fail for the same reason: the text is real, it is
/// where a description would be, and it is about something other than this
/// directory. See `PRAGMA_PREFIXES` and `BOILERPLATE_PHRASES`.
pub fn looks_like_boilerplate(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    // Leading punctuation is stripped before the prefix test because the
    // markup pass has already been through: `-*- coding: utf-8 -*-` arrives as
    // `-- coding: utf-8 --`, and `# eslint-disable` as `eslint-disable`.
    let head = lower.trim_start_matches(|c: char| !c.is_alphanumeric());
    if PRAGMA_PREFIXES.iter().any(|p| head.starts_with(p)) {
        return true;
    }
    BOILERPLATE_PHRASES.iter().any(|p| lower.contains(p))
}

/// Key prefixes that are a credential by construction.
///
/// These are published, documented formats. A string starting with one of them
/// is not "possibly a secret"; it is a secret, or a deliberate example of one,
/// and neither belongs on a picture.
const SECRET_PREFIXES: &[&str] = &[
    "-----begin",
    "sk-",
    "sk_live_",
    "sk_test_",
    "pk_live_",
    "rk_live_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
    "xapp-",
    "akia",
    "asia",
    "aiza",
    "ya29.",
    "eyj",
    "npm_",
    "dckr_pat_",
    "hf_",
    "shpat_",
    "shpss_",
    "sq0atp-",
    "sqOatp-",
    "amzn.mws.",
];

/// Names whose assigned value is a credential.
const SECRET_NAMES: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "api-key",
    "access_key",
    "private_key",
    "client_secret",
    "credential",
    "authorization",
    "auth_token",
];

/// True when the text carries anything credential-shaped.
///
/// Four independent tests:
///
/// * a **known key prefix** anywhere in a token;
/// * an **assignment** to one of `SECRET_NAMES` with a non-empty value —
///   which is why the plain word "secret" in a sentence is allowed through, and
///   `secret="hunter2"` is not;
/// * a **URL with user information**, `https://user:pass@host`;
/// * a **high-entropy token**: twenty or more characters from the base64/hex
///   alphabet, mixing upper case, lower case and digits, with no word structure.
///   That last one is the general catch, and it is why the length floor is 20:
///   below it, ordinary identifiers start tripping it.
///
/// **Public because it is the outbound rule as well.** [`crate::llm::outbound`]
/// applies exactly this predicate to every file name and doc snippet before it
/// leaves the machine. Having one definition of "looks like a secret" rather
/// than two is the point: a second copy is a copy that rots.
pub fn looks_like_secret(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("-----begin") {
        return true;
    }
    if secret_assignment(&lower) {
        return true;
    }
    for token in text.split_whitespace() {
        let token = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_');
        if token.is_empty() {
            continue;
        }
        let tl = token.to_ascii_lowercase();
        if SECRET_PREFIXES.iter().any(|p| tl.starts_with(p)) {
            return true;
        }
        if high_entropy(token) {
            return true;
        }
    }
    // `scheme://user:pass@host`.
    if let Some(rest) = lower.split_once("://") {
        let authority = rest.1.split(['/', ' ']).next().unwrap_or("");
        if let Some((userinfo, _)) = authority.split_once('@') {
            if userinfo.contains(':') {
                return true;
            }
        }
    }
    false
}

/// True when the text assigns a value to a credential-shaped name.
fn secret_assignment(lower: &str) -> bool {
    for name in SECRET_NAMES {
        let mut from = 0;
        while let Some(at) = lower[from..].find(name) {
            let after = from + at + name.len();
            from = after;
            let tail = lower[after..].trim_start_matches(['"', '\'']);
            let tail = tail.trim_start();
            let Some(rest) = tail
                .strip_prefix('=')
                .or_else(|| tail.strip_prefix(':'))
                .or_else(|| tail.strip_prefix("is "))
            else {
                continue;
            };
            let value = rest.trim_start_matches(['=', '>', ' ', '"', '\'']);
            if !value.is_empty() {
                return true;
            }
        }
    }
    false
}

/// True for a token that looks drawn from a random alphabet rather than a
/// language.
fn high_entropy(token: &str) -> bool {
    if token.chars().count() < 20 {
        return false;
    }
    let mut upper = 0usize;
    let mut lower = 0usize;
    let mut digit = 0usize;
    for c in token.chars() {
        if c.is_ascii_uppercase() {
            upper += 1;
        } else if c.is_ascii_lowercase() {
            lower += 1;
        } else if c.is_ascii_digit() {
            digit += 1;
        } else if !matches!(c, '+' | '/' | '=' | '_' | '-' | '.') {
            // A character outside the base64/hex alphabets: this is a word, not
            // a key. `polis_repo::describe` and `well-documented-module` land
            // here, which is the point.
            return false;
        }
    }
    upper > 0 && lower > 0 && digit > 0
}

/// Truncates to `max` characters at a word boundary, appending `…`.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    // `max - 1` leaves room for the ellipsis.
    let budget = max.saturating_sub(1);
    let cut: usize = text
        .char_indices()
        .nth(budget)
        .map_or(text.len(), |(i, _)| i);
    let head = &text[..cut];
    // Prefer a word boundary, but only if one is reasonably close to the end;
    // otherwise a single long token would throw away most of the budget.
    let floor = head.len().saturating_sub(head.len() / 4);
    let cut = head
        .rfind(' ')
        .filter(|i| *i >= floor)
        .unwrap_or(head.len());
    let mut out = head[..cut]
        .trim_end_matches([' ', ',', ';', ':', '-'])
        .to_owned();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Extractors — README
// ---------------------------------------------------------------------------

/// What a README yielded before sanitising.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadmeText {
    /// The first heading, with its `#` markers removed.
    pub heading: Option<String>,
    /// The first paragraph of body prose.
    pub body: Option<String>,
}

/// Reads the first heading and the first paragraph out of a Markdown or
/// reStructuredText README.
///
/// Deliberately simple, and every skip is a case seen on a real repository:
/// YAML front matter, fenced code blocks, badge rows (a paragraph that is
/// nothing but images and links vanishes in `strip_markup` and is skipped as
/// empty), tables, and HTML blocks.
///
/// The heading is kept separately from the body because it is only useful
/// *sometimes* — see
/// [`Neighborhoods::describe`](crate::neighborhoods::Neighborhoods::describe),
/// which uses it only when it says something the directory name does not.
pub fn read_readme(text: &str) -> ReadmeText {
    let mut out = ReadmeText::default();
    let mut lines = text.lines().peekable();
    // YAML or TOML front matter.
    if matches!(lines.peek(), Some(&("---" | "+++"))) {
        let fence = lines.next().unwrap_or("---").to_owned();
        for line in lines.by_ref() {
            if line.trim_end() == fence {
                break;
            }
        }
    }
    let mut fenced = false;
    let mut paragraph = String::new();
    let mut previous = String::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") || trimmed.starts_with("::::") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        if trimmed.is_empty() {
            if has_prose(&paragraph) {
                break;
            }
            // A paragraph that is nothing but badges and links: it vanishes in
            // [`strip_markup`], so keep looking rather than reporting it.
            paragraph.clear();
            previous.clear();
            continue;
        }
        // A Markdown ATX heading, or a reStructuredText / setext underline of
        // the line before it.
        if let Some(rest) = trimmed.strip_prefix('#') {
            if out.heading.is_none() {
                out.heading = Some(rest.trim_start_matches('#').trim().to_owned());
            }
            continue;
        }
        if is_underline(trimmed) {
            if out.heading.is_none() && !previous.is_empty() {
                out.heading = Some(previous.clone());
                paragraph.clear();
            }
            previous.clear();
            continue;
        }
        trimmed.clone_into(&mut previous);
        // A navigation bar — `Documentation | Chat | Contributing` — is a table
        // row without the leading pipe, and the pipes vanish in
        // [`strip_markup`], so it has to be caught here or it arrives as prose.
        if trimmed.contains('|') {
            continue;
        }
        // Tables, list items and directive blocks are not a description.
        if trimmed.starts_with('|')
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with(".. ")
            || trimmed.starts_with(':')
        {
            continue;
        }
        if !paragraph.is_empty() {
            paragraph.push(' ');
        }
        paragraph.push_str(trimmed);
        // A paragraph long enough to hold a sentence is enough; the rest of it
        // is bounded away anyway.
        if paragraph.len() > 600 {
            break;
        }
    }
    if has_prose(&paragraph) {
        out.body = Some(paragraph);
    }
    out
}

/// True when a candidate still has a *sentence* in it once the markup is gone.
///
/// Four words, not one: a README's opening paragraph is prose or it is
/// furniture, and the furniture — a logo, a badge row, an `<h1>` with two
/// navigation links in it — reduces to one or two words that read like a
/// description and are not one. Measured on Neovim, whose README opens with
/// exactly that and whose district was labelled `Documentation Chat` until this
/// bar went in.
fn has_prose(raw: &str) -> bool {
    let text = collapse_whitespace(&strip_markup(raw));
    text.chars().count() >= MIN_CHARS
        && text.split_whitespace().count() >= 4
        && text.chars().any(char::is_alphabetic)
}

/// True for a reStructuredText or setext underline: three or more of one
/// punctuation character and nothing else.
fn is_underline(line: &str) -> bool {
    let mut chars = line.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !matches!(first, '=' | '-' | '~' | '^' | '#' | '*' | '"' | '+') {
        return false;
    }
    line.chars().count() >= 3 && chars.all(|c| c == first)
}

// ---------------------------------------------------------------------------
// Extractors — manifests
// ---------------------------------------------------------------------------

/// The `"description"` field of a `package.json`, if it has one.
pub fn package_json_description(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let description = value.get("description")?.as_str()?.trim();
    (!description.is_empty()).then(|| description.to_owned())
}

/// A `description = "…"` from one of the named TOML tables.
///
/// A line scanner rather than a TOML parser, and that is a deliberate limit
/// rather than a shortcut: the workspace has no TOML dependency, the field is
/// one line in every manifest that has it, and the failure mode of not finding
/// it is a district with one fewer candidate description. Handles basic and
/// literal strings, and multi-line strings by taking their first line.
///
/// `description.workspace = true` is skipped rather than reported, because the
/// value is in another file and a `true` is not a description.
pub fn toml_description(text: &str, tables: &[&str]) -> Option<String> {
    let mut table = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('[') {
            rest.trim_end_matches(']')
                .trim_matches('[')
                .trim()
                .clone_into(&mut table);
            continue;
        }
        if !tables.iter().any(|t| t.eq_ignore_ascii_case(&table)) {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("description") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim();
        let value = if let Some(v) = rest.strip_prefix("\"\"\"") {
            v.trim_end_matches("\"\"\"")
        } else if let Some(v) = rest.strip_prefix("'''") {
            v.trim_end_matches("'''")
        } else if let Some(v) = rest.strip_prefix('"') {
            v.strip_suffix('"').unwrap_or(v)
        } else if let Some(v) = rest.strip_prefix('\'') {
            v.strip_suffix('\'').unwrap_or(v)
        } else {
            continue;
        };
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_owned());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Extractors — module doc comments
// ---------------------------------------------------------------------------

/// The module-level documentation at the top of a source file.
///
/// Uses the same four tree-sitter grammars [`crate::imports`] loads, through the
/// same `crate::imports::ts_language`, so the two modules can never end up on
/// different parsers. Returns the comment's text with its syntax removed:
///
/// * **Rust** — leading `//!` lines and `/*! … */` blocks, skipping any `#![…]`
///   inner attributes above them.
/// * **Python** — the module docstring, the first statement in the file.
/// * **JavaScript, TypeScript, TSX** — the leading comment block, stopping at
///   the first `JSDoc` tag, because `@param` onwards is a signature and not a
///   description.
///
/// A file that does not parse yields `None`, which is PRD §9's non-fatal rule
/// applied to a second thing the parse is used for.
pub fn module_doc(language: Language, source: &str) -> Option<String> {
    let mut parser = Parser::new();
    parser
        .set_language(&crate::imports::ts_language(language))
        .ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let bytes = source.as_bytes();
    match language {
        Language::Rust => rust_inner_doc(root, bytes),
        Language::Python => python_docstring(root, bytes),
        Language::JavaScript | Language::TypeScript | Language::Tsx => js_leading_doc(root, bytes),
    }
}

/// Rust `//!` and `/*! … */`, gathered from the top of the file.
fn rust_inner_doc(root: Node<'_>, bytes: &[u8]) -> Option<String> {
    let mut cursor = root.walk();
    let mut out: Vec<String> = Vec::new();
    for child in root.children(&mut cursor) {
        let kind = child.kind();
        if kind.contains("comment") {
            let text = node_text(child, bytes);
            if let Some(rest) = text.strip_prefix("//!") {
                out.push(rest.trim().to_owned());
                continue;
            }
            if let Some(rest) = text.strip_prefix("/*!") {
                for line in rest.trim_end_matches("*/").lines() {
                    out.push(line.trim().trim_start_matches('*').trim().to_owned());
                }
                continue;
            }
            // An ordinary `//` comment above the doc block: skip it and keep
            // looking. A licence header is the common case.
            continue;
        }
        // `#![...]` inner attributes sit above the doc block often enough to be
        // worth walking past.
        if kind == "attribute_item" || kind == "inner_attribute_item" {
            continue;
        }
        break;
    }
    let joined = join_doc(&out);
    (!joined.is_empty()).then_some(joined)
}

/// A Python module docstring — the first statement in the module.
fn python_docstring(root: Node<'_>, bytes: &[u8]) -> Option<String> {
    let mut cursor = root.walk();
    let statement = root.children(&mut cursor).next()?;
    if statement.kind() != "expression_statement" {
        return None;
    }
    let mut inner = statement.walk();
    let string = statement.children(&mut inner).next()?;
    if !string.kind().contains("string") {
        return None;
    }
    let raw = node_text(string, bytes);
    let raw = raw.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let body = raw
        .trim_start_matches("\"\"\"")
        .trim_start_matches("'''")
        .trim_end_matches("\"\"\"")
        .trim_end_matches("'''")
        .trim_matches('"')
        .trim_matches('\'');
    let out: Vec<String> = body.lines().map(|l| l.trim().to_owned()).collect();
    let joined = join_doc(&out);
    (!joined.is_empty()).then_some(joined)
}

/// The leading comment block of a JavaScript, TypeScript or TSX file.
fn js_leading_doc(root: Node<'_>, bytes: &[u8]) -> Option<String> {
    let mut cursor = root.walk();
    let mut out: Vec<String> = Vec::new();
    for child in root.children(&mut cursor) {
        if !child.kind().contains("comment") {
            break;
        }
        let text = node_text(child, bytes);
        if let Some(rest) = text.strip_prefix("/*") {
            for line in rest.trim_end_matches("*/").lines() {
                let line = line.trim().trim_start_matches('*').trim();
                // `@param`, `@returns`, `@module` — a signature, not a
                // description.
                if line.starts_with('@') {
                    break;
                }
                out.push(line.to_owned());
            }
            break;
        }
        if let Some(rest) = text.strip_prefix("//") {
            out.push(rest.trim().to_owned());
            continue;
        }
        break;
    }
    let joined = join_doc(&out);
    (!joined.is_empty()).then_some(joined)
}

/// Joins the lines of a doc comment into prose.
///
/// One rule beyond "join with spaces", and it earns its place on real
/// repositories: a **short first line with no terminal punctuation is a title**,
/// so it gets a line break after it. Without that, a `JSDoc` block whose first
/// line is `Centralized Window Management Service` and whose second is
/// `Unified registry and API for …` produces one run-on string, and the label —
/// which is the first *sentence* — becomes the two of them jammed together.
fn join_doc(lines: &[String]) -> String {
    let mut out = String::new();
    let mut emitted = 0usize;
    for line in lines {
        if out.is_empty() {
            if line.is_empty() {
                continue;
            }
            out.push_str(line);
            emitted = 1;
            continue;
        }
        let title = emitted == 1
            && !out.contains('\n')
            && out.chars().count() <= 60
            && !out.ends_with(['.', '!', '?', ':', ',', ';']);
        out.push(if line.is_empty() || title { '\n' } else { ' ' });
        out.push_str(line);
        if !line.is_empty() {
            emitted += 1;
        }
    }
    out.trim().to_owned()
}

/// A node's source text.
fn node_text<'a>(node: Node<'_>, bytes: &'a [u8]) -> &'a str {
    std::str::from_utf8(&bytes[node.start_byte()..node.end_byte()]).unwrap_or("")
}

/// The first sentence of a block of prose, and then the rest of it.
///
/// A sentence ends at `. `, `! ` or `? ` — or at a newline, which is how a
/// one-line doc comment with no full stop still yields a label. An abbreviation
/// like "e.g." is not special-cased: the cost of getting it wrong is a label
/// that stops early, which is bounded and harmless.
pub fn first_sentence(text: &str) -> (&str, &str) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if matches!(c, b'.' | b'!' | b'?') {
            let next = bytes.get(i + 1).copied();
            if next.is_none_or(|n| n == b' ' || n == b'\n' || n == b'\r') {
                // Not a sentence end if it is a single initial or an ellipsis.
                let end = i + 1;
                return (text[..end].trim(), text[end..].trim_start());
            }
        }
        if c == b'\n' && i > 0 {
            return (text[..i].trim(), text[i..].trim_start());
        }
        i += 1;
    }
    (text.trim(), "")
}

// ---------------------------------------------------------------------------
// The cache
// ---------------------------------------------------------------------------

/// Version of the on-disk cache. A bump discards every entry.
///
/// **Bump this whenever an extraction or sanitising rule changes.** The cache is
/// keyed on the bytes of the file, not on the rules that read them, so a warm
/// cache written by the previous rules would keep serving the answer the new
/// rule exists to refuse — and it would do it only on the machines that had run
/// before, which is the worst possible way to find out. Version 2 is the
/// [`SanitiseReject::Boilerplate`] rule.
const DESCRIPTION_CACHE_VERSION: u32 = 2;

/// One cached extraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedDescription {
    /// The file the description was read out of.
    path: LogicalPath,
    /// The two halves of the content digest. See
    /// [`crate::imports::content_digest`].
    lo: u64,
    hi: u64,
    /// Source length in bytes: a third, free, independent check.
    len: u64,
    /// What the extractor returned. `None` is cached too — "this file has no
    /// description" is as expensive to work out as the other answer.
    description: Option<Description>,
}

/// The on-disk file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DescriptionCacheFile {
    version: u32,
    entries: Vec<CachedDescription>,
}

/// What the previous run extracted, keyed on the exact bytes it read.
///
/// Same construction and same guarantee as [`crate::imports::ParseCache`]: a hit
/// is verified against a 128-bit content digest **and** the byte length, so a
/// file whose bytes changed by one bit is a miss, and the digest is written out
/// rather than taken from `DefaultHasher` so two machines cannot disagree about
/// a hit (PRD §7.4).
#[derive(Debug, Default)]
pub struct DescriptionCache {
    by_path: BTreeMap<LogicalPath, CachedDescription>,
    dirty: bool,
}

impl DescriptionCache {
    /// Reads a cache file, or an empty cache when there is none, it is
    /// unreadable, or a different version wrote it.
    ///
    /// Never an error: a cold cache and a corrupt one are the same situation.
    pub fn read(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return Self::default();
        };
        let Ok(file) = serde_json::from_slice::<DescriptionCacheFile>(&bytes) else {
            return Self::default();
        };
        if file.version != DESCRIPTION_CACHE_VERSION {
            return Self::default();
        }
        Self {
            by_path: file
                .entries
                .into_iter()
                .map(|e| (e.path.clone(), e))
                .collect(),
            dirty: false,
        }
    }

    /// What these exact bytes produced last time.
    fn get(&self, path: &LogicalPath, source: &[u8]) -> Option<&Option<Description>> {
        let entry = self.by_path.get(path)?;
        if entry.len != source.len() as u64 {
            return None;
        }
        let (lo, hi) = crate::imports::content_digest(source);
        (entry.lo == lo && entry.hi == hi).then_some(&entry.description)
    }

    /// Records an extraction.
    fn put(&mut self, path: &LogicalPath, source: &[u8], description: Option<Description>) {
        let (lo, hi) = crate::imports::content_digest(source);
        self.by_path.insert(
            path.clone(),
            CachedDescription {
                path: path.clone(),
                lo,
                hi,
                len: source.len() as u64,
                description,
            },
        );
        self.dirty = true;
    }

    /// Writes the cache for the files that exist **now**, so a repository that
    /// shrinks does not carry its deleted files forever.
    ///
    /// Failure is silent by design: an unwritable state directory costs the next
    /// launch a re-read and nothing else.
    pub fn write(&self, path: &Path) {
        let file = DescriptionCacheFile {
            version: DESCRIPTION_CACHE_VERSION,
            entries: self.by_path.values().cloned().collect(),
        };
        if let Ok(bytes) = serde_json::to_vec(&file) {
            if let Err(error) = crate::git::write_atomic(path, &bytes) {
                tracing::debug!(%error, "could not write the description cache");
            }
        }
    }

    /// How many files it holds.
    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    /// True when it holds nothing.
    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    /// True when something has been added since it was read.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
}

/// The default on-disk location for a repository's description cache.
///
/// Beside the import cache, in the platform state directory, keyed on the
/// normalised repository root — so two checkouts of the same repository share
/// nothing and one repository moved on disk does not carry a stale cache.
pub fn default_cache_path(repo_root: &Path) -> Option<std::path::PathBuf> {
    let key =
        crate::git::fnv1a64(crate::git::normalize_root(&repo_root.to_string_lossy()).as_bytes());
    Some(
        crate::corpus::state_dir()?
            .join("describe")
            .join(format!("{key:016x}.json")),
    )
}

// ---------------------------------------------------------------------------
// The describer
// ---------------------------------------------------------------------------

/// How many bytes of a candidate file are read.
///
/// A description lives at the top of a file by every convention this module
/// knows about, and reading 16 KiB of a 4 MiB minified bundle to find out it has
/// no doc comment is the difference between a cold start inside PRD §13.1's
/// budget and outside it.
pub const READ_LIMIT_BYTES: usize = 16 * 1024;

/// Counters from one pass, for the honesty report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DescribeStats {
    /// Files opened.
    pub files_read: u32,
    /// Bytes read from them.
    pub bytes_read: u64,
    /// Cache hits.
    pub cache_hits: u32,
    /// Candidates thrown away because they were too short or empty.
    pub rejected_short: u32,
    /// Candidates thrown away because they read as code.
    pub rejected_code: u32,
    /// Candidates thrown away because they carried something credential-shaped.
    pub rejected_secret: u32,
    /// Candidates thrown away because a tool wrote them: a linter pragma, or a
    /// project generator's untouched README.
    pub rejected_boilerplate: u32,
}

impl DescribeStats {
    /// Records one rejection.
    fn reject(&mut self, why: SanitiseReject) {
        match why {
            SanitiseReject::TooShort => self.rejected_short += 1,
            SanitiseReject::LooksLikeCode => self.rejected_code += 1,
            SanitiseReject::LooksLikeSecret => self.rejected_secret += 1,
            SanitiseReject::Boilerplate => self.rejected_boilerplate += 1,
        }
    }

    /// Every rejection, however caused.
    pub fn rejected(&self) -> u32 {
        self.rejected_short + self.rejected_code + self.rejected_secret + self.rejected_boilerplate
    }
}

/// Reads descriptions out of a checkout.
///
/// Holds the cache and the counters across a whole pass so one district's read
/// is another's cache hit — a monorepo where ten packages share a README is the
/// normal case, not the exotic one.
#[derive(Debug)]
pub struct Describer<'a> {
    root: &'a Path,
    cache: DescriptionCache,
    stats: DescribeStats,
}

/// The file names a README can have, in the order they are tried.
pub const README_NAMES: &[&str] = &[
    "README.md",
    "README.rst",
    "README.txt",
    "README",
    "README.markdown",
    "readme.md",
    "Readme.md",
    "index.md",
    "OVERVIEW.md",
];

/// The manifests that carry a `description`, and the TOML tables to look in.
const MANIFESTS: &[(&str, &[&str])] = &[
    ("package.json", &[]),
    ("Cargo.toml", &["package"]),
    ("pyproject.toml", &["project", "tool.poetry"]),
];

/// The file names that are a directory's own entry point, in priority order.
///
/// A doc comment on one of these describes the directory; a doc comment on an
/// arbitrary file in it describes that file.
pub const ANCHOR_NAMES: &[&str] = &[
    "mod.rs",
    "lib.rs",
    "main.rs",
    "__init__.py",
    "__main__.py",
    "index.ts",
    "index.tsx",
    "index.js",
    "index.mjs",
    "app.py",
    "apps.py",
];

impl<'a> Describer<'a> {
    /// Opens a describer over a checkout, with no cache.
    pub fn new(root: &'a Path) -> Self {
        Self {
            root,
            cache: DescriptionCache::default(),
            stats: DescribeStats::default(),
        }
    }

    /// Opens a describer with a cache read from `path`.
    pub fn with_cache(root: &'a Path, path: &Path) -> Self {
        Self {
            root,
            cache: DescriptionCache::read(path),
            stats: DescribeStats::default(),
        }
    }

    /// The counters from this pass.
    pub fn stats(&self) -> &DescribeStats {
        &self.stats
    }

    /// The cache, for writing back.
    pub fn cache(&self) -> &DescriptionCache {
        &self.cache
    }

    /// Reads at most [`READ_LIMIT_BYTES`] of one file.
    ///
    /// `None` for a file that cannot be read or is not UTF-8. Neither is an
    /// error: a binary file simply has no description, exactly as a file that
    /// fails to parse simply has no streets (PRD §9).
    fn read_head(&mut self, path: &LogicalPath) -> Option<Vec<u8>> {
        use std::io::Read;
        let physical = self.root.join(path.as_str());
        let file = std::fs::File::open(physical).ok()?;
        let mut buffer = Vec::new();
        file.take(READ_LIMIT_BYTES as u64)
            .read_to_end(&mut buffer)
            .ok()?;
        self.stats.files_read += 1;
        self.stats.bytes_read += buffer.len() as u64;
        Some(buffer)
    }

    /// A description from one candidate file, cached on its exact bytes.
    ///
    /// `extract` is given the file's text and returns the raw, unsanitised
    /// candidate; sanitising and bounding happen here so every source goes
    /// through the same gate.
    #[allow(clippy::wrong_self_convention)] // `from` names the source, not a conversion
    fn from_file(
        &mut self,
        path: &LogicalPath,
        source: DescriptionSource,
        extract: impl FnOnce(&str) -> Option<String>,
    ) -> Option<Description> {
        let bytes = self.read_head(path)?;
        if let Some(hit) = self.cache.get(path, &bytes) {
            self.stats.cache_hits += 1;
            self.stats.files_read = self.stats.files_read.saturating_sub(1);
            return hit.clone();
        }
        let text = String::from_utf8(bytes.clone()).ok();
        let raw = text.as_deref().and_then(extract);
        let built = raw.and_then(|raw| self.build(&raw, source, path));
        self.cache.put(path, &bytes, built.clone());
        built
    }

    /// Sanitises a raw candidate into a [`Description`], counting rejections.
    fn build(
        &mut self,
        raw: &str,
        source: DescriptionSource,
        origin: &LogicalPath,
    ) -> Option<Description> {
        let (sentence, _) = first_sentence(raw);
        let label = match sanitise(sentence, LABEL_MAX_CHARS) {
            Ok(label) => label,
            Err(why) => {
                self.stats.reject(why);
                return None;
            }
        };
        // The detail is the whole opening, not the whole file: two sentences is
        // what PRD §12's panel has room for, and the bound is what stops a
        // README's entire first section arriving.
        let detail = sanitise(raw, DETAIL_MAX_CHARS).unwrap_or_else(|_| label.clone());
        Some(Description {
            label,
            detail,
            source,
            origin: Some(origin.clone()),
        })
    }

    /// A README description for a directory, given the files it contains.
    ///
    /// `name` is the district's display name; the README's *heading* is only
    /// used when it differs from that name, because a `# Auth` heading over the
    /// `auth` district is the definition of a filler description.
    pub fn from_readme(
        &mut self,
        directory: &LogicalPath,
        name: &str,
        exists: &impl Fn(&LogicalPath) -> bool,
    ) -> Option<Description> {
        for candidate in README_NAMES {
            let Some(path) = join(directory, candidate) else {
                continue;
            };
            if !exists(&path) {
                continue;
            }
            let name = name.to_owned();
            if let Some(found) = self.from_file(&path, DescriptionSource::Readme, move |text| {
                let readme = read_readme(text);
                readme
                    .body
                    .or_else(|| readme.heading.filter(|h| !says_nothing_new(h, &name)))
            }) {
                return Some(found);
            }
        }
        None
    }

    /// A manifest description for a directory.
    pub fn from_manifest(
        &mut self,
        directory: &LogicalPath,
        exists: &impl Fn(&LogicalPath) -> bool,
    ) -> Option<Description> {
        for (file, tables) in MANIFESTS {
            let Some(path) = join(directory, file) else {
                continue;
            };
            if !exists(&path) {
                continue;
            }
            let tables = *tables;
            if let Some(found) = self.from_file(&path, DescriptionSource::Manifest, move |text| {
                if tables.is_empty() {
                    package_json_description(text)
                } else {
                    toml_description(text, tables)
                }
            }) {
                return Some(found);
            }
        }
        None
    }

    /// A doc-comment description, from the directory's anchor file or from the
    /// candidates a caller ranks ahead of it.
    ///
    /// `extra` is tried after [`ANCHOR_NAMES`] — the natural thing to pass is
    /// the district's most-imported file, which
    /// [`crate::imports::ImportGraph::inbound_counts`] already computes and
    /// which PRD §8 uses for monuments.
    ///
    /// # An `extra` candidate must sit in the directory itself
    ///
    /// A doc comment describes the file it is written in. An [`ANCHOR_NAMES`]
    /// file is the directory's declared front door and may speak for it; any
    /// other file speaks for the directory only by proximity, and proximity runs
    /// out at the first subdirectory. Without this, `qurio-toolset`'s **root**
    /// — PRD §8's civic square, the most prominent label on the map — was
    /// described as "a DEV-ONLY Vite plugin", because its most-imported file was
    /// `vite-plugins/manualReloadPlugin.js`; and `components/MediaWindow` was
    /// described as a `YouTube` renderer, from
    /// `MediaWindow/renderers/YouTubeRenderer.jsx`.
    ///
    /// This does not make the remaining ones true of the whole district — one
    /// file's doc comment standing in for a 200-file `services/` tree is a
    /// judgement no path rule can make. It removes the cases where the rule had
    /// no basis at all.
    pub fn from_doc_comment(
        &mut self,
        directory: &LogicalPath,
        extra: &[LogicalPath],
        exists: &impl Fn(&LogicalPath) -> bool,
    ) -> Option<Description> {
        let anchors = ANCHOR_NAMES
            .iter()
            .filter_map(|name| join(directory, name))
            .filter(|p| exists(p));
        let nearby = extra
            .iter()
            .filter(|p| p.parent().as_ref() == Some(directory))
            .cloned();
        for path in anchors.chain(nearby) {
            let Some(language) = crate::tree::language_for(&path) else {
                continue;
            };
            if let Some(found) = self.from_file(&path, DescriptionSource::DocComment, move |text| {
                module_doc(language, text)
            }) {
                return Some(found);
            }
        }
        None
    }
}

/// `directory/name`, or `None` when the join is not a legal logical path.
fn join(directory: &LogicalPath, name: &str) -> Option<LogicalPath> {
    if directory.is_root() {
        LogicalPath::new(name).ok()
    } else {
        directory.join(name).ok()
    }
}

/// True when a heading adds nothing to the name already on the map.
///
/// Compared on letters and digits only, so `# Auth`, `## auth`, `# The auth
/// module` and `# auth/` all say nothing new about a district called `auth`.
/// This is the "utils described as Utility functions" test, applied where it can
/// actually be applied.
pub fn says_nothing_new(heading: &str, name: &str) -> bool {
    let fold = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let h = fold(heading);
    let n = fold(name);
    if h.is_empty() || n.is_empty() {
        return true;
    }
    // Also catches `# The auth module` and `# auth crate`.
    let stripped = h
        .trim_start_matches("the")
        .trim_end_matches("module")
        .trim_end_matches("package")
        .trim_end_matches("crate")
        .trim_end_matches("library")
        .trim_end_matches("directory")
        .trim_end_matches("folder")
        .trim_end_matches('s');
    stripped == n || h == n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    // -- sanitising ---------------------------------------------------------

    #[test]
    fn markup_and_badges_are_stripped_but_the_words_survive() {
        let raw = "[![build](https://img.shields.io/x.svg)](https://ci.example) \
                   **Polis** renders a `repository` as a [city](./docs/city.md).";
        let clean = sanitise(raw, DETAIL_MAX_CHARS).expect("prose");
        assert_eq!(clean, "Polis renders a repository as a city.");
    }

    #[test]
    fn control_characters_and_bidi_overrides_never_reach_the_label() {
        let raw = "Auth\u{202E}sredner\u{0007} the login flow for the web client";
        let clean = sanitise(raw, DETAIL_MAX_CHARS).expect("prose");
        assert!(!clean.chars().any(is_unsafe_char), "{clean:?}");
        assert!(clean.starts_with("Authsredner"), "{clean:?}");
    }

    #[test]
    fn a_line_of_code_is_rejected_rather_than_shown() {
        for candidate in [
            "pub fn describe(path: &LogicalPath) -> Option<Description>",
            "const routes = { home: '/', login: '/login' }",
            "def main(argv): return run(argv)",
            "if (x) { return y; }",
            "return (a && b) || c;",
            "src/components/BaseWindow/BaseWindow.jsx",
            "polis-repo/src/describe.rs",
            "$ imagepipe --in a.jpg --out b.jpg",
        ] {
            assert_eq!(
                sanitise(candidate, DETAIL_MAX_CHARS),
                Err(SanitiseReject::LooksLikeCode),
                "{candidate}"
            );
        }
    }

    /// The two families found by reading real output, not by reasoning: a
    /// linter pragma sitting where a module doc comment goes, and a project
    /// generator's untouched README.
    #[test]
    fn a_tools_own_words_are_not_a_description_of_the_repository() {
        for candidate in [
            // `qurio-toolset/src/components/landing`, verbatim.
            "eslint-disable no-unused-vars, no-console",
            "prettier-ignore for the table below",
            "ts-nocheck because the generated types are wrong",
            "-*- coding: utf-8 -*- for the legacy importer",
            "noqa: E501 line too long in the fixture",
            // `stickingplacebooks`, `Squigglo` and `vc-tower/web`, verbatim.
            "This is a Next.js project bootstrapped with create-next-app.",
            // `qurio-toolset`, verbatim.
            "This template provides a minimal setup to get React working in Vite \
             with HMR and some ESLint rules.",
            "Getting Started with Create React App and a few notes",
        ] {
            assert_eq!(
                sanitise(candidate, DETAIL_MAX_CHARS),
                Err(SanitiseReject::Boilerplate),
                "{candidate}"
            );
        }
        // Caught a rule earlier — `looks_like_code`'s bare-path test — which is
        // the right answer for a different reason. Asserted so a later change to
        // either rule cannot let it through.
        assert!(sanitise(
            "eslint-disable-next-line react-hooks/exhaustive-deps",
            DETAIL_MAX_CHARS
        )
        .is_err());
    }

    /// The rule is prefix-and-verbatim on purpose: prose that merely *mentions*
    /// a tool is still prose, and rejecting it would cost real descriptions.
    #[test]
    fn prose_that_merely_mentions_a_tool_survives() {
        for candidate in [
            "The build runs eslint-disable comments through a codemod first here",
            "Vite and Next.js are both supported by this adapter package",
            "Rules for when to add an eslint-disable, and when to fix the code",
        ] {
            assert!(sanitise(candidate, DETAIL_MAX_CHARS).is_ok(), "{candidate}");
        }
    }

    #[test]
    fn anything_credential_shaped_is_dropped_whole() {
        for candidate in [
            "Deploy key sk-abcdefghijklmnopqrstuvwxyz012345 for the staging cluster",
            "Set the api_key=\"9f2b8c1d\" before running the importer here",
            "Connect with postgres://admin:hunter2@db.internal/app for the tests",
            "-----BEGIN RSA PRIVATE KEY----- and the rest of the material",
            "The upload token is ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8 exactly",
            "Uses AKIAIOSFODNN7EXAMPLE to reach the bucket from the worker",
            "A high entropy value dGhpc0lzQVNlY3JldFZhbHVlMTIz sits in the env",
        ] {
            assert_eq!(
                sanitise(candidate, DETAIL_MAX_CHARS),
                Err(SanitiseReject::LooksLikeSecret),
                "{candidate}"
            );
        }
    }

    #[test]
    fn the_word_secret_in_a_sentence_is_still_allowed() {
        // Rejecting the *word* would silently blank the description of every
        // credentials module in every repository, which is a worse failure than
        // the one it prevents.
        let ok = sanitise(
            "Secret rotation for the deployment pipeline and its audit trail",
            DETAIL_MAX_CHARS,
        );
        assert!(ok.is_ok(), "{ok:?}");
        let ok = sanitise(
            "Token bucket rate limiting for the public API",
            DETAIL_MAX_CHARS,
        );
        assert!(ok.is_ok(), "{ok:?}");
    }

    #[test]
    fn a_label_is_bounded_at_a_word_boundary() {
        let long = "Territory inference for a main agent, estimated as a density \
                    field over the layout from sparse path observations";
        let label = sanitise(long, LABEL_MAX_CHARS).expect("prose");
        assert!(label.chars().count() <= LABEL_MAX_CHARS, "{label:?}");
        assert!(label.ends_with('…'), "{label:?}");
        assert!(!label.contains("  "), "{label:?}");
        // The cut lands on a word, not mid-word.
        let body = label.trim_end_matches('…');
        assert!(long.starts_with(body), "{label:?}");
    }

    #[test]
    fn a_multibyte_character_is_never_split_by_the_bound() {
        let long = "日本語".repeat(60);
        let label = sanitise(&long, LABEL_MAX_CHARS).expect("prose");
        assert!(label.chars().count() <= LABEL_MAX_CHARS);
    }

    #[test]
    fn an_empty_or_tiny_candidate_is_too_short() {
        assert_eq!(sanitise("", 70), Err(SanitiseReject::TooShort));
        assert_eq!(sanitise("   ", 70), Err(SanitiseReject::TooShort));
        assert_eq!(sanitise("utils", 70), Err(SanitiseReject::TooShort));
        assert_eq!(
            sanitise("1234567890123456", 70),
            Err(SanitiseReject::TooShort)
        );
    }

    // -- README -------------------------------------------------------------

    #[test]
    fn a_readme_yields_its_heading_and_first_paragraph() {
        let text = "---\ntitle: x\n---\n\n# Polis\n\n\
                    [![ci](a.svg)](b)\n\n\
                    A live, glanceable city map of what your coding agents are doing.\n\
                    It renders the repository from above.\n\n\
                    ## Install\n\nrun it\n";
        let readme = read_readme(text);
        assert_eq!(readme.heading.as_deref(), Some("Polis"));
        let body = readme.body.expect("body");
        assert!(body.starts_with("A live, glanceable city map"), "{body:?}");
        assert!(!body.contains("Install"), "{body:?}");
    }

    #[test]
    fn a_fenced_code_block_is_not_a_description() {
        let text =
            "# Tool\n\n```bash\ncargo run --release\n```\n\nRuns the indexer over a checkout.\n";
        let readme = read_readme(text);
        assert_eq!(
            readme.body.as_deref(),
            Some("Runs the indexer over a checkout.")
        );
    }

    #[test]
    fn a_navigation_bar_is_not_a_description() {
        // Measured on Neovim, whose README opens with one.
        let text = "# Neovim

Documentation | Chat | Contributing

                    Neovim is a project that seeks to aggressively refactor Vim.
";
        let readme = read_readme(text);
        assert_eq!(
            readme.body.as_deref(),
            Some("Neovim is a project that seeks to aggressively refactor Vim.")
        );
    }

    #[test]
    fn a_restructuredtext_underline_is_a_heading() {
        let text = "Click\n=====\n\nA simple wrapper around optparse for powerful command line utilities.\n";
        let readme = read_readme(text);
        assert_eq!(readme.heading.as_deref(), Some("Click"));
        assert!(readme.body.expect("body").starts_with("A simple wrapper"));
    }

    #[test]
    fn a_badge_only_readme_yields_nothing_to_say() {
        let text = "# lib\n\n[![a](x.svg)](y) [![b](z.svg)](w)\n";
        let readme = read_readme(text);
        let body = readme.body.unwrap_or_default();
        assert_eq!(sanitise(&body, 70), Err(SanitiseReject::TooShort));
    }

    // -- manifests ----------------------------------------------------------

    #[test]
    fn manifest_descriptions_are_read_from_all_three_formats() {
        assert_eq!(
            package_json_description(r#"{"name":"a","description":"The web client"}"#).as_deref(),
            Some("The web client")
        );
        assert_eq!(package_json_description(r#"{"name":"a"}"#), None);
        assert_eq!(package_json_description("not json"), None);
        let cargo = "[package]\nname = \"polis-repo\"\ndescription = \"The repository index\"\n";
        assert_eq!(
            toml_description(cargo, &["package"]).as_deref(),
            Some("The repository index")
        );
        // The field in the wrong table is not this package's description.
        let other = "[dependencies]\ndescription = \"nope\"\n";
        assert_eq!(toml_description(other, &["package"]), None);
        let py = "[project]\nname = \"x\"\ndescription = 'A test runner'\n";
        assert_eq!(
            toml_description(py, &["project"]).as_deref(),
            Some("A test runner")
        );
        // `description.workspace = true` is not a description.
        let ws = "[package]\ndescription.workspace = true\n";
        assert_eq!(toml_description(ws, &["package"]), None);
    }

    // -- doc comments -------------------------------------------------------

    #[test]
    fn rust_inner_doc_comments_are_read_through_tree_sitter() {
        let source = "// SPDX-License-Identifier: MIT\n#![allow(dead_code)]\n\
                      //! Territory inference for a main agent.\n\
                      //!\n//! A density field over the layout.\n\
                      pub fn f() {}\n";
        let doc = module_doc(Language::Rust, source).expect("doc");
        assert!(
            doc.starts_with("Territory inference for a main agent."),
            "{doc:?}"
        );
        assert!(doc.contains("density field"), "{doc:?}");
    }

    #[test]
    fn a_python_module_docstring_is_read() {
        let source =
            "\"\"\"Serialisation helpers for the wire format.\n\nMore words.\n\"\"\"\nimport os\n";
        let doc = module_doc(Language::Python, source).expect("doc");
        assert!(doc.starts_with("Serialisation helpers"), "{doc:?}");
        // A file whose first statement is not a string has none.
        assert_eq!(module_doc(Language::Python, "import os\n"), None);
    }

    #[test]
    fn a_leading_js_block_comment_stops_at_the_first_jsdoc_tag() {
        let source = "/**\n * Firestore access for the reading list.\n * @module store\n * @param x\n */\nexport const a = 1;\n";
        let doc = module_doc(Language::TypeScript, source).expect("doc");
        assert_eq!(doc.trim(), "Firestore access for the reading list.");
    }

    #[test]
    fn a_short_unpunctuated_first_line_is_a_title_and_ends_the_sentence() {
        // Measured on a real repository: without the title rule the label came
        // out as the heading and the body run together.
        let source = "/**\n * Centralized Window Management Service\n \
                      * Unified registry and API for every overlay window.\n */\nexport const a = 1;\n";
        let doc = module_doc(Language::TypeScript, source).expect("doc");
        assert_eq!(
            first_sentence(&doc).0,
            "Centralized Window Management Service"
        );
        assert!(doc.contains("Unified registry"), "{doc:?}");
    }

    #[test]
    fn a_file_that_does_not_parse_simply_has_no_description() {
        // PRD §9's non-fatal rule, applied to the second thing the parse is for.
        let doc = module_doc(Language::Rust, "fn ( ) ) ) {{{ unterminated");
        assert_eq!(doc, None);
    }

    // -- sentences ----------------------------------------------------------

    #[test]
    fn the_first_sentence_stops_at_the_first_full_stop_or_newline() {
        assert_eq!(first_sentence("One. Two. Three.").0, "One.");
        assert_eq!(first_sentence("One\nTwo").0, "One");
        assert_eq!(first_sentence("No stop at all").0, "No stop at all");
        assert_eq!(
            first_sentence("Version 1.2 is out. Yes.").0,
            "Version 1.2 is out."
        );
    }

    // -- the filler test ----------------------------------------------------

    #[test]
    fn a_heading_that_repeats_the_name_says_nothing_new() {
        assert!(says_nothing_new("auth", "auth"));
        assert!(says_nothing_new("# Auth", "auth"));
        assert!(says_nothing_new("The auth module", "auth"));
        assert!(says_nothing_new("Utils", "utils"));
        assert!(says_nothing_new("polis-repo", "polis_repo"));
        assert!(!says_nothing_new("Territory inference", "world"));
        assert!(!says_nothing_new("Reading list storage", "store"));
    }

    // -- the cache ----------------------------------------------------------

    #[test]
    fn a_cache_hit_needs_the_same_bytes_and_the_same_length() {
        let mut cache = DescriptionCache::default();
        let path = lp("src/lib.rs");
        let description = Description {
            label: "A thing".to_owned(),
            detail: "A thing that does a thing.".to_owned(),
            source: DescriptionSource::DocComment,
            origin: Some(path.clone()),
        };
        cache.put(&path, b"//! A thing.\n", Some(description.clone()));
        assert_eq!(
            cache.get(&path, b"//! A thing.\n"),
            Some(&Some(description))
        );
        assert_eq!(cache.get(&path, b"//! A different thing.\n"), None);
        assert_eq!(cache.get(&lp("src/other.rs"), b"//! A thing.\n"), None);
        assert!(cache.is_dirty());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn a_cache_round_trips_and_a_bad_file_is_a_cold_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("d.json");
        let mut cache = DescriptionCache::default();
        cache.put(&lp("a.rs"), b"//! Doc.\n", None);
        cache.write(&path);
        let back = DescriptionCache::read(&path);
        assert_eq!(back.len(), 1);
        assert!(!back.is_dirty());
        std::fs::write(&path, b"garbage").expect("write");
        assert!(DescriptionCache::read(&path).is_empty());
        assert!(DescriptionCache::read(&dir.path().join("absent.json")).is_empty());
    }

    // -- the describer over a real directory --------------------------------

    #[test]
    fn the_describer_prefers_a_readme_then_a_manifest_then_a_doc_comment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("pkg")).expect("mkdir");
        std::fs::write(
            root.join("pkg/Cargo.toml"),
            "[package]\ndescription = \"Growth order from git history\"\n",
        )
        .expect("write");
        std::fs::write(
            root.join("pkg/lib.rs"),
            "//! A doc comment nobody should reach.\n",
        )
        .expect("write");
        let exists = |p: &LogicalPath| root.join(p.as_str()).exists();

        let mut describer = Describer::new(root);
        let manifest = describer
            .from_manifest(&lp("pkg"), &exists)
            .expect("manifest");
        assert_eq!(manifest.source, DescriptionSource::Manifest);
        assert_eq!(manifest.label, "Growth order from git history");
        assert_eq!(
            manifest.origin.as_ref().map(LogicalPath::as_str),
            Some("pkg/Cargo.toml")
        );

        // A README beats it once there is one.
        std::fs::write(
            root.join("pkg/README.md"),
            "# pkg\n\nReplays the commit log to date every building in the city.\n",
        )
        .expect("write");
        let readme = describer
            .from_readme(&lp("pkg"), "pkg", &exists)
            .expect("readme");
        assert_eq!(readme.source, DescriptionSource::Readme);
        assert!(
            readme.label.starts_with("Replays the commit log"),
            "{readme:?}"
        );

        let doc = describer
            .from_doc_comment(&lp("pkg"), &[], &exists)
            .expect("doc");
        assert_eq!(doc.source, DescriptionSource::DocComment);
        assert!(describer.stats().files_read >= 3);
    }

    /// The root of `qurio-toolset` was described as "a DEV-ONLY Vite plugin"
    /// because its most-imported file lived in `vite-plugins/`. A file speaks
    /// for the directory it is in, and no further.
    #[test]
    fn a_monument_in_a_subdirectory_does_not_describe_the_district() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("plugins")).expect("mkdir");
        std::fs::write(
            root.join("plugins/reload.js"),
            "/* DEV-ONLY plugin that reloads the window. */\n",
        )
        .expect("write");
        std::fs::write(
            root.join("server.js"),
            "/* Serves the API that the desktop shell talks to. */\n",
        )
        .expect("write");
        let exists = |p: &LogicalPath| root.join(p.as_str()).exists();
        let mut describer = Describer::new(root);

        // One level down: not this district's to speak for.
        assert!(describer
            .from_doc_comment(&LogicalPath::root(), &[lp("plugins/reload.js")], &exists)
            .is_none());

        // Directly in the district: allowed. This also pins that a top-level
        // file's parent *is* the root.
        let found = describer
            .from_doc_comment(&LogicalPath::root(), &[lp("server.js")], &exists)
            .expect("a file in the district itself");
        assert!(found.label.starts_with("Serves the API"), "{found:?}");
    }

    #[test]
    fn a_readme_whose_heading_only_repeats_the_name_yields_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("utils")).expect("mkdir");
        std::fs::write(root.join("utils/README.md"), "# Utils\n").expect("write");
        let exists = |p: &LogicalPath| root.join(p.as_str()).exists();
        let mut describer = Describer::new(root);
        assert_eq!(describer.from_readme(&lp("utils"), "utils", &exists), None);
    }

    #[test]
    fn the_describer_is_deterministic_and_the_cache_changes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("a")).expect("mkdir");
        std::fs::write(
            root.join("a/README.md"),
            "# a\n\nRoutes every inbound webhook to the right handler.\n",
        )
        .expect("write");
        let exists = |p: &LogicalPath| root.join(p.as_str()).exists();
        let mut first = Describer::new(root);
        let a = first.from_readme(&lp("a"), "a", &exists);
        let b = first.from_readme(&lp("a"), "a", &exists);
        assert_eq!(a, b, "a cache hit must give the same answer as a read");
        assert_eq!(first.stats().cache_hits, 1);
        let mut second = Describer::new(root);
        assert_eq!(a, second.from_readme(&lp("a"), "a", &exists));
    }
}
