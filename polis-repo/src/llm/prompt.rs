//! What the model is shown, what it is told to refuse, and what is done with
//! its answer.
//!
//! # The single most important instruction
//!
//! **Empty beats filler.** This is not a preference; it is the reason the
//! feature exists. `docs/design/NEIGHBORHOODS-REVIEW.md` graded 23 quoted
//! descriptions by hand and found nine of them were the folder name in different
//! words — `services/settings` → "The settings entry contract",
//! `services/dropZoneService` → "Drop Zone Service", `lib/demo` → "Demo mode
//! utilities". Replacing nine bad captions with nine *different* bad captions
//! would be a waste of the operator's money and a loss of their trust in the
//! map.
//!
//! So the prompt leads with the refusal, shows it three of the real failures
//! verbatim, and asks for `null`. And [`parse_reply`] enforces it independently
//! — [`restates_the_name`] rejects a tautology whatever the model returns,
//! because a rule that lives only in a prompt is a rule that holds only until
//! the next model.
//!
//! # What may be sent
//!
//! A district's path, the **names** of its files and subdirectories, its kind
//! mix, its monument, and the doc snippets [`crate::describe`] already extracted
//! and sanitised. **No source code bodies.** That is structural rather than a
//! promise: nothing in [`DistrictBrief`] can hold a file's contents, because
//! there is no field for it and the builder never opens a file. Everything that
//! does go passes [`crate::llm::outbound::vet`] first.
//!
//! # The answer is not trusted
//!
//! Every returned string goes through [`crate::describe::sanitise`] — the same
//! gate a README goes through, at the same lengths — because a description is
//! drawn onto an image the operator may share, and the source of the words does
//! not change that. A model can no more put a bidi override on the map than a
//! README can.

use polis_events::LogicalPath;
use serde::{Deserialize, Serialize};

use crate::describe::{
    sanitise, Description, DescriptionSource, DETAIL_MAX_CHARS, LABEL_MAX_CHARS,
};

use super::outbound::{vet, vet_names, OutboundReject, RedactionReport};
use super::LlmError;

/// The prompt's version.
///
/// Stored beside every cached description. A bump makes existing entries
/// [`super::DriftCause::Recipe`]-stale, which is visible and re-generable,
/// rather than silently mixing answers to two different questions on one map.
pub const PROMPT_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// What the model sees
// ---------------------------------------------------------------------------

/// One doc snippet already extracted from the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocSnippet {
    /// Which extractor found it — `readme`, `manifest`, `doc-comment`.
    pub source: String,
    /// The file it came from.
    pub origin: String,
    /// The text, already sanitised inbound and vetted outbound.
    pub text: String,
}

/// Everything one district contributes to a request.
///
/// Names and documentation. There is deliberately no field for file contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DistrictBrief {
    /// The district. Echoed back by the model so answers can be matched up.
    pub path: LogicalPath,
    /// The map label, which is what the description must not merely restate.
    pub name: String,
    /// Dominant [`crate::kinds::CodeKind`].
    pub kind: String,
    /// The composition, when it is a mixture worth stating.
    pub mix: String,
    /// Files this district owns.
    pub file_count: u32,
    /// Files under it at any depth.
    pub subtree_file_count: u32,
    /// File names, relative to the district, in path order.
    pub names: Vec<String>,
    /// How many more there were.
    pub names_omitted: u32,
    /// Districts immediately inside this one, by name.
    pub children: Vec<String>,
    /// The most-imported file, when there is one (PRD §8's monument).
    pub monument: Option<String>,
    /// What the repository already says, if anything.
    pub docs: Vec<DocSnippet>,
}

impl DistrictBrief {
    /// The path as the model should see it. The root is `/`, not the empty
    /// string.
    pub fn display_path(&self) -> &str {
        if self.path.is_root() {
            "/"
        } else {
            self.path.as_str()
        }
    }

    /// Renders one district into the block the request carries.
    ///
    /// Plain lines rather than JSON: the file-name list is the bulk of the
    /// payload and JSON quoting would add about a fifth to it for no gain in
    /// clarity. The *answer* is JSON, because that is the part that has to be
    /// parsed.
    pub fn render(&self) -> String {
        use std::fmt::Write;
        let mut out = String::with_capacity(256 + self.names.len() * 16);
        let _ = writeln!(out, "PATH: {}", self.display_path());
        let _ = writeln!(out, "MAP LABEL: {}", self.name);
        let _ = writeln!(
            out,
            "KIND: {}{}",
            self.kind,
            if self.mix.is_empty() {
                String::new()
            } else {
                format!(" ({})", self.mix)
            }
        );
        let _ = writeln!(
            out,
            "FILES: {} here, {} including subdirectories",
            self.file_count, self.subtree_file_count
        );
        if !self.children.is_empty() {
            let _ = writeln!(out, "SUB-DISTRICTS: {}", self.children.join(", "));
        }
        if let Some(monument) = &self.monument {
            let _ = writeln!(out, "MOST IMPORTED: {monument}");
        }
        if self.names.is_empty() {
            let _ = writeln!(out, "FILE NAMES: (none)");
        } else {
            let _ = write!(out, "FILE NAMES: {}", self.names.join(", "));
            if self.names_omitted > 0 {
                let _ = write!(out, " (+{} more)", self.names_omitted);
            }
            out.push('\n');
        }
        for doc in &self.docs {
            let _ = writeln!(out, "DOC ({} in {}): {}", doc.source, doc.origin, doc.text);
        }
        out
    }
}

/// Builds a brief, dropping anything [`crate::llm::outbound`] refuses.
///
/// `names` are the district's own file paths relative to it. `derived` is
/// whatever [`crate::describe`] already produced, which is evidence rather than
/// an answer.
///
/// Returns `None` when the district's own path is credential-shaped — the whole
/// district is skipped rather than sent with a hole in it.
#[allow(clippy::too_many_arguments)] // a brief is its fields; a struct here would only rename them
pub fn build_brief(
    path: &LogicalPath,
    name: &str,
    kind: &str,
    mix: &str,
    file_count: u32,
    subtree_file_count: u32,
    names: Vec<String>,
    children: Vec<String>,
    monument: Option<&LogicalPath>,
    derived: Option<&Description>,
    max_names: usize,
    report: &mut RedactionReport,
) -> Option<DistrictBrief> {
    // A directory whose *name* is credential-shaped is not worth arguing with.
    let Ok(clean_name) = vet(name, report) else {
        report.skip_district(path);
        return None;
    };
    if !path.is_root() && vet(path.as_str(), report).is_err() {
        report.skip_district(path);
        return None;
    }
    // Vet **before** truncating, not after. `stickingplacebooks` stores its
    // uploads as `1765241215200-A Cielo Abierto.png`; the leading epoch
    // millisecond makes every one of those names read as high-entropy to the
    // shared secret rule, so vetting the first sixty would have left a district
    // with almost no names to describe it by. Vetting first spends the budget on
    // names that can actually be sent.
    let mut names = vet_names(path, names, report);
    let total = names.len();
    names.truncate(max_names);
    let names_omitted = u32::try_from(total.saturating_sub(names.len())).unwrap_or(u32::MAX);
    let children = vet_names(path, children, report);
    let monument = monument
        .and_then(LogicalPath::file_name)
        .and_then(|m| vet(m, report).ok());

    let mut docs = Vec::new();
    if let Some(description) = derived {
        let origin = description
            .origin
            .as_ref()
            .map_or_else(|| "-".to_owned(), |o| o.as_str().to_owned());
        // The detail carries the label's words already, so one snippet is
        // enough and it is the longer one.
        match vet(&description.detail, report) {
            Ok(text) if !text.is_empty() => docs.push(DocSnippet {
                source: description.source.name().to_owned(),
                origin: vet(&origin, report).unwrap_or_else(|_| "-".to_owned()),
                text,
            }),
            Ok(_) => {}
            Err(OutboundReject::Secret) => report.drop_doc(path),
        }
    }

    Some(DistrictBrief {
        path: path.clone(),
        name: clean_name,
        kind: kind.to_owned(),
        mix: mix.to_owned(),
        file_count,
        subtree_file_count,
        names,
        names_omitted,
        children,
        monument,
        docs,
    })
}

// ---------------------------------------------------------------------------
// The prompt
// ---------------------------------------------------------------------------

/// The system prompt.
///
/// Deliberately blunt and deliberately front-loaded: the refusal rule is first,
/// it is in capitals, and it is illustrated with three real failures from the
/// operator's own repositories rather than with invented ones. Everything after
/// it is format.
pub fn system_prompt() -> String {
    format!(
        "You are writing captions for directories on a map of a software repository. \
         The reader is the engineer who owns the repository. They can already see the \
         directory's name, its size and what kind of code is in it; your job is to say \
         what it is FOR.\n\
         \n\
         RULE 1 — RETURN null WHEN YOU HAVE NOTHING REAL TO SAY. This is the most \
         important rule and an empty answer is a correct answer. A caption that only \
         restates the directory's own name is a FAILURE, not an answer. These are real \
         failures from this map and every one of them should have been null:\n\
         \x20 services/settings        -> \"The settings entry contract\"   WRONG\n\
         \x20 services/dropZoneService -> \"Drop Zone Service\"             WRONG\n\
         \x20 lib/demo                 -> \"Demo mode utilities\"           WRONG\n\
         \x20 utils                    -> \"Utility functions\"             WRONG\n\
         If the only thing you can say is the directory name with a category noun \
         attached (service, module, utilities, components, types, helpers, exports, \
         configuration), return null.\n\
         \n\
         RULE 2 — DO NOT GUESS. You are given names and documentation, never source \
         code. If the evidence does not support a claim, do not make it. Naming a \
         technology, a domain concept or a workflow is worth saying; inventing one is \
         not.\n\
         \n\
         RULE 3 — ONE FILE DOES NOT DESCRIBE A LARGE DIRECTORY. A doc comment quoted \
         under DOC may describe only the file it came from. If it plainly does not \
         cover the whole directory, ignore it; a 200-file directory labelled from one \
         of its files is worse than no label.\n\
         \n\
         RULE 4 — SAY IT ONCE. Do not repeat the file count, the kind, or the \
         most-imported file: the map already draws all three.\n\
         \n\
         FORMAT. Reply with JSON and nothing else — no prose, no markdown fence:\n\
         {{\"districts\": [{{\"path\": \"<the PATH exactly as given>\", \"label\": \
         \"<= {LABEL_MAX_CHARS} chars or null\", \"detail\": \"<= {DETAIL_MAX_CHARS} \
         chars or null\"}}]}}\n\
         One entry per district given, in the same order. label is a noun phrase with \
         no trailing full stop and no file paths in it. detail is one or two complete \
         sentences. Use null, not an empty string, and never write \"unknown\" or \
         \"no description available\"."
    )
}

/// The user message for one batch of districts.
pub fn user_prompt(briefs: &[DistrictBrief]) -> String {
    let mut out = String::with_capacity(briefs.len() * 512);
    out.push_str("Describe these ");
    out.push_str(&briefs.len().to_string());
    out.push_str(" directories. Return one JSON entry for each, in this order.\n");
    for brief in briefs {
        out.push_str("\n---\n");
        out.push_str(&brief.render());
    }
    out
}

// ---------------------------------------------------------------------------
// The answer
// ---------------------------------------------------------------------------

/// One district's answer, before it is trusted.
#[derive(Debug, Clone, Deserialize)]
struct RawAnswer {
    path: Option<String>,
    label: Option<String>,
    detail: Option<String>,
}

/// Why an answer was thrown away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerReject {
    /// The model returned `null`, or something that reduces to nothing. **Not a
    /// failure** — it is the answer the prompt asks for when there is nothing to
    /// say, and it is cached so it is not paid for twice.
    Nothing,
    /// It failed [`crate::describe::sanitise`]: code-shaped, credential-shaped,
    /// boilerplate, or too short.
    Sanitise,
    /// It restates the district's own name. See [`restates_the_name`].
    RestatesName,
    /// It named a district that was not in the batch.
    Unknown,
}

/// What one reply produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedReply {
    /// Descriptions, by district, in path order. A district present with `None`
    /// is a district the model correctly declined to describe.
    pub answers: Vec<(LogicalPath, Option<Description>)>,
    /// Answers thrown away, and why.
    pub rejected: Vec<(LogicalPath, AnswerReject)>,
    /// Districts in the batch the model said nothing about at all.
    pub unanswered: Vec<LogicalPath>,
}

impl ParsedReply {
    /// How many districts got words.
    pub fn described(&self) -> usize {
        self.answers.iter().filter(|(_, d)| d.is_some()).count()
    }
}

/// Parses a reply, tolerantly, and gates every string in it.
///
/// Tolerant on purpose: whether Z.ai honours `response_format` on
/// `glm-5.3-flash` is **not established** — `models.dev` says the model supports
/// structured output, Z.ai's documentation does not describe the syntax, and
/// this round had no key to settle it. So the parser handles a bare object, a
/// bare array, a fenced block, and prose with JSON somewhere inside it, and
/// nothing anywhere depends on the request field being honoured. If it turns out
/// to be honoured, this code is unchanged and merely never exercises its
/// fallbacks.
///
/// What it is *not* tolerant about is content: every label and detail goes
/// through [`crate::describe::sanitise`] and [`restates_the_name`].
pub fn parse_reply(text: &str, batch: &[DistrictBrief]) -> Result<ParsedReply, LlmError> {
    let json = extract_json(text).ok_or_else(|| {
        LlmError::Unusable(format!(
            "no JSON in the answer: {}",
            super::secret::bound_message(text, 160)
        ))
    })?;
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| LlmError::Unusable(format!("the answer is not JSON: {e}")))?;
    let items: Vec<RawAnswer> = if let Some(array) = value.as_array() {
        serde_json::from_value(serde_json::Value::Array(array.clone()))
            .map_err(|e| LlmError::Unusable(format!("bad answer array: {e}")))?
    } else if let Some(array) = value.get("districts").and_then(|d| d.as_array()) {
        serde_json::from_value(serde_json::Value::Array(array.clone()))
            .map_err(|e| LlmError::Unusable(format!("bad districts array: {e}")))?
    } else if value.is_object()
        && ["path", "label", "detail"]
            .iter()
            .any(|k| value.get(*k).is_some())
    {
        // A single object, which is what a one-district batch tends to produce.
        // At least one known key has to be there: an object with none of them
        // is a shape we do not understand, and quietly reading it as "nothing to
        // say" would cache a silence we never actually got.
        vec![serde_json::from_value(value.clone())
            .map_err(|e| LlmError::Unusable(format!("bad answer object: {e}")))?]
    } else {
        return Err(LlmError::Unusable("the answer is not an object".to_owned()));
    };

    let mut out = ParsedReply::default();
    let mut seen: std::collections::BTreeSet<LogicalPath> = std::collections::BTreeSet::new();
    for (index, item) in items.iter().enumerate() {
        // Match on the echoed path; fall back to position, because a model that
        // gets the order right and the echo wrong should not cost a whole batch.
        let brief = item
            .path
            .as_deref()
            .and_then(|p| find_brief(batch, p))
            .or_else(|| batch.get(index));
        let Some(brief) = brief else {
            continue;
        };
        if !seen.insert(brief.path.clone()) {
            continue;
        }
        match build_description(item, brief) {
            Ok(description) => out.answers.push((brief.path.clone(), description)),
            Err(AnswerReject::Nothing) => out.answers.push((brief.path.clone(), None)),
            Err(why) => out.rejected.push((brief.path.clone(), why)),
        }
    }
    for brief in batch {
        if !seen.contains(&brief.path) {
            out.unanswered.push(brief.path.clone());
        }
    }
    out.answers.sort_by(|a, b| a.0.cmp(&b.0));
    out.rejected.sort();
    out.unanswered.sort();
    Ok(out)
}

/// The brief whose path the model echoed, matching `/` to the root.
fn find_brief<'a>(batch: &'a [DistrictBrief], echoed: &str) -> Option<&'a DistrictBrief> {
    let trimmed = echoed.trim();
    batch.iter().find(|b| {
        b.display_path() == trimmed
            || (trimmed == "/" && b.path.is_root())
            || b.path.as_str() == trimmed.trim_start_matches('/')
    })
}

/// Turns one raw answer into a [`Description`], or says why not.
fn build_description(
    item: &RawAnswer,
    brief: &DistrictBrief,
) -> Result<Option<Description>, AnswerReject> {
    let label = item.label.as_deref().map(str::trim).unwrap_or_default();
    if is_nothing(label) {
        return Err(AnswerReject::Nothing);
    }
    let clean_label = sanitise(label, LABEL_MAX_CHARS).map_err(|_| AnswerReject::Sanitise)?;
    if restates_the_name(&clean_label, &brief.name) {
        return Err(AnswerReject::RestatesName);
    }
    let detail_raw = item.detail.as_deref().map(str::trim).unwrap_or_default();
    let detail = if is_nothing(detail_raw) {
        clean_label.clone()
    } else {
        sanitise(detail_raw, DETAIL_MAX_CHARS).unwrap_or_else(|_| clean_label.clone())
    };
    Ok(Some(Description {
        label: clean_label,
        detail,
        source: DescriptionSource::Model,
        // A model description is not quoted from a file, and claiming an origin
        // it does not have would make the drill-down panel lie.
        origin: None,
    }))
}

/// True for the several ways a model says "nothing".
fn is_nothing(text: &str) -> bool {
    let folded = text.trim().trim_matches('"').to_ascii_lowercase();
    folded.is_empty()
        || matches!(
            folded.as_str(),
            "null"
                | "none"
                | "n/a"
                | "na"
                | "-"
                | "unknown"
                | "no description"
                | "no description available"
                | "not applicable"
                | "nothing"
        )
}

/// Finds the JSON in a reply that may be wrapped in prose or a fence.
///
/// Scans for the first `{` or `[` and returns the balanced span that closes it,
/// ignoring braces inside strings. A model that answers with exactly the JSON
/// asked for takes the first branch and this costs one pass.
fn extract_json(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|b| *b == b'{' || *b == b'[')?;
    let open = bytes[start];
    let close = if open == b'{' { b'}' } else { b']' };
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, byte) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match *byte {
            b'"' => in_string = true,
            b if b == open => depth += 1,
            b if b == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The filler test
// ---------------------------------------------------------------------------

/// Words that carry no information when they sit next to a directory's own
/// name.
///
/// Every one of these was read off a real restatement in
/// `docs/design/NEIGHBORHOODS-REVIEW.md` §4. The list is deliberately closed and
/// deliberately short: it earns entries by appearing in a caption that told the
/// reader nothing, not by sounding generic.
const CATEGORY_NOUNS: &[&str] = &[
    "service",
    "module",
    "util",
    "utility",
    "helper",
    "component",
    "renderer",
    "function",
    "export",
    "layer",
    "system",
    "manager",
    "handler",
    "provider",
    "config",
    "configuration",
    "entry",
    "contract",
    "main",
    "mode",
    "type",
    "constant",
    "index",
    "definition",
    "wrapper",
    "abstraction",
    "logic",
    "code",
    "file",
    "folder",
    "directory",
    "package",
    "library",
    "crate",
];

/// Words that carry no information anywhere.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "for", "to", "in", "on", "with", "this", "that", "its",
    "all", "any", "is", "are",
];

/// True when a caption says nothing the map label already says.
///
/// The precise version of the review's complaint. Words are split on
/// non-alphanumerics **and** on `camelCase` boundaries — `dropZoneService`
/// becomes `drop zone service` — folded to lower case, and singularised on a
/// trailing `s`. Then: strip stopwords and `CATEGORY_NOUNS` from the caption,
/// and if nothing is left that the name does not already contain, the caption is
/// a tautology.
///
/// Graded against the review's own tables, and the shape of the rule is chosen
/// by that grading rather than by taste:
///
/// * It catches six of the nine captions the review called restatements —
///   "The settings entry contract", "Memory System — Main Export", "Drop Zone
///   Service", "A2UI Renderer", "Data layer exports", "Demo mode utilities".
/// * It catches **none** of the ten the review called informative. "Tool
///   Registry" survives, because `registry` is not a category noun and is not in
///   `services/tools`; "Firebase Configuration and Initialization" survives on
///   `firebase` and `initialization`.
///
/// The three it misses — "Types for web scraper", "Firebase Cloud Functions",
/// "Cloud Functions for Firebase" — all name a technology the directory name
/// does not, so a rule that caught them would have to be one that also lost
/// "Cloud Functions for Qurio Gmail Integration". Precision is the right side to
/// err on: a false positive here silently deletes a good caption.
pub fn restates_the_name(caption: &str, name: &str) -> bool {
    let caption_words = content_words(caption);
    if caption_words.is_empty() {
        return true;
    }
    let name_words: std::collections::BTreeSet<String> = content_words(name).into_iter().collect();
    if name_words.is_empty() {
        return false;
    }
    caption_words
        .iter()
        .all(|w| name_words.contains(w) || CATEGORY_NOUNS.contains(&w.as_str()))
}

/// Splits text into folded, singularised content words.
fn content_words(text: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for c in text.chars() {
        if !c.is_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            previous_lower = false;
            continue;
        }
        // A camelCase or PascalCase boundary: `dropZone` -> `drop`, `zone`.
        // A digit does not open one, or `A2UI` becomes `a2` and `ui`.
        if c.is_uppercase() && previous_lower && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        previous_lower = c.is_lowercase();
        current.extend(c.to_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words.retain(|w| !STOPWORDS.contains(&w.as_str()));
    for word in &mut words {
        // Crude singularisation. `tools` and `tool` must fold together, or
        // `services/tools` described as "Tool Registry" reads as new words; and
        // `utilities` must reach `utility`, or `lib/demo` described as "Demo
        // mode utilities" walks straight past the category-noun list.
        if word.len() > 4 && word.ends_with("ies") {
            word.truncate(word.len() - 3);
            word.push('y');
        } else if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") {
            word.pop();
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    fn brief(path: &str, name: &str) -> DistrictBrief {
        DistrictBrief {
            path: lp(path),
            name: name.to_owned(),
            kind: "source".to_owned(),
            mix: String::new(),
            file_count: 12,
            subtree_file_count: 12,
            names: vec!["a.ts".to_owned(), "b.ts".to_owned()],
            names_omitted: 0,
            children: Vec::new(),
            monument: None,
            docs: Vec::new(),
        }
    }

    // -- what is sent -------------------------------------------------------

    /// The structural promise: names and documentation, never a file's body.
    #[test]
    fn a_brief_carries_names_and_docs_and_has_nowhere_to_put_source_code() {
        let mut report = RedactionReport::default();
        let derived = Description {
            label: "Window management".to_owned(),
            detail: "Centralized Window Management Service.".to_owned(),
            source: DescriptionSource::DocComment,
            origin: Some(lp("src/services/WindowManager.js")),
        };
        let built = build_brief(
            &lp("src/services"),
            "src/services",
            "source",
            "62 % source, 20 % test",
            223,
            240,
            vec!["WindowManager.js".to_owned(), "memory/index.js".to_owned()],
            vec!["services/tools".to_owned()],
            Some(&lp("src/services/WindowManager.js")),
            Some(&derived),
            60,
            &mut report,
        )
        .expect("a brief");
        let rendered = built.render();
        assert!(rendered.contains("PATH: src/services"), "{rendered}");
        assert!(rendered.contains("WindowManager.js"), "{rendered}");
        assert!(
            rendered.contains("SUB-DISTRICTS: services/tools"),
            "{rendered}"
        );
        assert!(rendered.contains("223 here, 240 including"), "{rendered}");
        assert!(
            rendered.contains("DOC (doc-comment in src/services/WindowManager.js)"),
            "{rendered}"
        );
        // The only text fields are names and sanitised documentation.
        assert!(report.is_empty(), "{report:?}");
    }

    #[test]
    fn the_root_district_is_shown_as_a_path_the_model_can_echo_back() {
        let mut report = RedactionReport::default();
        let built = build_brief(
            &LogicalPath::root(),
            "/",
            "config",
            "",
            42,
            900,
            vec!["package.json".to_owned()],
            Vec::new(),
            None,
            None,
            60,
            &mut report,
        )
        .expect("a brief");
        assert_eq!(built.display_path(), "/");
        assert!(
            built.render().starts_with("PATH: /\n"),
            "{}",
            built.render()
        );
    }

    #[test]
    fn a_long_district_is_truncated_and_says_how_many_it_left_out() {
        let mut report = RedactionReport::default();
        let mut names: Vec<String> = (0..500).map(|i| format!("f{i:04}.ts")).collect();
        // Twenty names that trip the shared secret rule, at the front, where
        // truncating first would have spent the whole budget on them.
        for i in 0..20 {
            names.insert(i, format!("1765241215{i:03}-Upload Name.png"));
        }
        let built = build_brief(
            &lp("src/big"),
            "src/big",
            "source",
            "",
            520,
            520,
            names,
            Vec::new(),
            None,
            None,
            60,
            &mut report,
        )
        .expect("a brief");
        assert_eq!(report.names_dropped, 20, "vetted before truncating");
        assert_eq!(built.names.len(), 60, "a full budget of sendable names");
        assert!(built.names.iter().all(|n| n.starts_with('f')), "{built:?}");
        assert_eq!(built.names_omitted, 440);
        assert!(built.render().contains("(+440 more)"));
    }

    #[test]
    fn a_credential_shaped_district_is_skipped_whole_rather_than_sent_with_a_hole() {
        let mut report = RedactionReport::default();
        let built = build_brief(
            &lp("keys"),
            "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8",
            "config",
            "",
            1,
            1,
            Vec::new(),
            Vec::new(),
            None,
            None,
            60,
            &mut report,
        );
        assert!(built.is_none());
        assert_eq!(report.districts_skipped, 1);
    }

    #[test]
    fn the_system_prompt_leads_with_the_refusal_and_shows_the_real_failures() {
        let prompt = system_prompt();
        let rule_one = prompt.find("RETURN null").expect("rule 1");
        let rule_two = prompt.find("DO NOT GUESS").expect("rule 2");
        assert!(rule_one < rule_two, "the refusal comes first");
        for real_failure in [
            "The settings entry contract",
            "Drop Zone Service",
            "Demo mode utilities",
            "Utility functions",
        ] {
            assert!(prompt.contains(real_failure), "missing {real_failure}");
        }
        assert!(prompt.contains("70"), "the label bound is stated");
        assert!(prompt.contains("240"), "the detail bound is stated");
        // `response_format: json_object` requires the word; so does a model that
        // ignores the field.
        assert!(prompt.contains("JSON"), "{prompt}");
    }

    #[test]
    fn the_user_prompt_holds_one_block_per_district_in_order() {
        let batch = [brief("src/a", "src/a"), brief("src/b", "src/b")];
        let prompt = user_prompt(&batch);
        let a = prompt.find("PATH: src/a").expect("a");
        let b = prompt.find("PATH: src/b").expect("b");
        assert!(
            a < b,
            "order is preserved so a positional answer still lands"
        );
        assert!(prompt.contains("Describe these 2 directories"));
    }

    // -- what comes back ----------------------------------------------------

    #[test]
    fn a_clean_answer_becomes_a_description_marked_as_the_models() {
        let batch = [brief("src/auth", "src/auth")];
        let reply = parse_reply(
            r#"{"districts":[{"path":"src/auth","label":"Session issuing and refresh for the web client","detail":"Issues and refreshes sessions for the web client. Owns the cookie format."}]}"#,
            &batch,
        )
        .expect("parsed");
        assert_eq!(reply.answers.len(), 1);
        let description = reply.answers[0].1.as_ref().expect("described");
        assert_eq!(description.source, DescriptionSource::Model);
        assert!(!description.is_prose(), "a model is not a human author");
        assert_eq!(description.origin, None, "it is not quoted from a file");
        assert!(description.label.starts_with("Session issuing"));
        assert!(reply.rejected.is_empty() && reply.unanswered.is_empty());
        assert_eq!(reply.described(), 1);
    }

    /// The whole point. A model that says nothing is doing what it was asked.
    #[test]
    fn a_null_answer_is_recorded_as_an_answer_and_never_as_a_failure() {
        let batch = [brief("services/settings", "services/settings")];
        for body in [
            r#"{"districts":[{"path":"services/settings","label":null,"detail":null}]}"#,
            r#"{"districts":[{"path":"services/settings","label":"","detail":""}]}"#,
            r#"{"districts":[{"path":"services/settings","label":"N/A"}]}"#,
            r#"{"districts":[{"path":"services/settings","label":"no description available"}]}"#,
        ] {
            let reply = parse_reply(body, &batch).expect("parsed");
            assert_eq!(reply.answers, [(lp("services/settings"), None)], "{body}");
            assert!(reply.rejected.is_empty(), "{body}");
        }
    }

    #[test]
    fn a_restatement_is_refused_even_when_the_model_ignores_the_instruction() {
        // The nine the review graded as restatements, with their districts.
        for (district, caption) in [
            ("services/settings", "The settings entry contract"),
            ("services/memory", "Memory System — Main Export"),
            ("services/dropZoneService", "Drop Zone Service"),
            ("components/a2ui", "A2UI Renderer"),
            ("lib/data", "Data layer exports"),
            ("lib/demo", "Demo mode utilities"),
            ("utils", "Utility functions"),
        ] {
            let batch = [brief(district, district)];
            let body = format!(
                r#"{{"districts":[{{"path":"{district}","label":"{caption}","detail":"{caption} for the app."}}]}}"#
            );
            let reply = parse_reply(&body, &batch).expect("parsed");
            assert_eq!(
                reply.rejected,
                [(lp(district), AnswerReject::RestatesName)],
                "{district} -> {caption}"
            );
            assert!(reply.answers.is_empty(), "{district} -> {caption}");
        }
    }

    #[test]
    fn the_informative_captions_the_review_graded_all_survive() {
        for (district, caption) in [
            ("services/tools", "Tool Registry"),
            ("src/config", "Firebase Configuration and Initialization"),
            ("services/adapters", "Tool Format Converters"),
            ("functions", "Cloud Functions for Qurio Gmail Integration"),
            (
                "electron",
                "Qurio desktop app with native browser window overlays",
            ),
            (
                "services/integrations",
                "Gmail Service — Frontend integration with Firebase Functions",
            ),
            (
                "vcsheet-scraper",
                "Standalone Python scraper for vcsheet.com",
            ),
        ] {
            assert!(
                !restates_the_name(caption, district),
                "{district} -> {caption} must survive"
            );
        }
    }

    #[test]
    fn a_model_answer_goes_through_the_same_gate_a_readme_does() {
        let batch = [brief("src/auth", "src/auth")];
        for hostile in [
            // Code-shaped.
            "export const routes = { home: 1 }",
            // Credential-shaped.
            "Uses AKIAIOSFODNN7EXAMPLE to reach the bucket from the worker",
            // A generator's boilerplate, echoed back at us.
            "This is a Next.js project bootstrapped with create-next-app",
        ] {
            let body = serde_json::json!({
                "districts": [{"path": "src/auth", "label": hostile, "detail": hostile}]
            })
            .to_string();
            let reply = parse_reply(&body, &batch).expect("parsed");
            assert_eq!(
                reply.rejected,
                [(lp("src/auth"), AnswerReject::Sanitise)],
                "{hostile}"
            );
        }
        // And a bidi override cannot reach the map through a model either.
        let body = serde_json::json!({
            "districts": [{"path": "src/auth",
                           "label": "Auth\u{202E}sredner for the login flow of the client"}]
        })
        .to_string();
        let reply = parse_reply(&body, &batch).expect("parsed");
        let label = &reply.answers[0].1.as_ref().expect("described").label;
        assert!(!label.contains('\u{202E}'), "{label:?}");
    }

    #[test]
    fn json_is_found_inside_a_fence_a_preamble_or_a_bare_array() {
        let batch = [brief("src/auth", "src/auth")];
        let good =
            r#"{"path":"src/auth","label":"Session issuing for the web client","detail":"x"}"#;
        for body in [
            format!("```json\n{{\"districts\":[{good}]}}\n```"),
            format!("Here you go:\n\n{{\"districts\":[{good}]}}\n\nHope that helps."),
            format!("[{good}]"),
            good.to_owned(),
        ] {
            let reply = parse_reply(&body, &batch).expect(&body);
            assert_eq!(reply.described(), 1, "{body}");
        }
    }

    #[test]
    fn prose_with_no_json_is_unusable_and_degrades_rather_than_panicking() {
        let batch = [brief("src/auth", "src/auth")];
        for body in [
            "I'm sorry, I can't help with that.",
            "",
            "{ unbalanced",
            r#"{"districts": 7}"#,
        ] {
            let error = parse_reply(body, &batch).expect_err(body);
            assert!(matches!(error, LlmError::Unusable(_)), "{body}: {error:?}");
            assert!(!error.is_retryable(), "{body}");
        }
    }

    #[test]
    fn a_district_the_model_skipped_is_reported_rather_than_silently_lost() {
        let batch = [
            brief("src/a", "src/a"),
            brief("src/b", "src/b"),
            brief("src/c", "src/c"),
        ];
        let reply = parse_reply(
            r#"{"districts":[
                {"path":"src/a","label":"Ingests the nightly partner feed"},
                {"path":"src/zzz","label":"Never asked for"}
            ]}"#,
            &batch,
        )
        .expect("parsed");
        // `src/zzz` matched no brief, so it fell back to position 1 — `src/b`.
        // That is deliberate: a model that gets the order right and the echo
        // wrong should not cost the whole batch.
        assert_eq!(reply.unanswered, [lp("src/c")]);
        assert_eq!(reply.answers.len(), 2);
    }

    #[test]
    fn a_repeated_district_is_taken_once() {
        let batch = [brief("src/a", "src/a")];
        let reply = parse_reply(
            r#"{"districts":[
                {"path":"src/a","label":"Ingests the nightly partner feed"},
                {"path":"src/a","label":"Something else entirely for it"}
            ]}"#,
            &batch,
        )
        .expect("parsed");
        assert_eq!(reply.answers.len(), 1);
        assert!(reply.answers[0]
            .1
            .as_ref()
            .expect("described")
            .label
            .starts_with("Ingests"));
    }

    #[test]
    fn words_are_split_on_case_and_punctuation_and_singularised() {
        assert_eq!(
            content_words("dropZoneService"),
            ["drop", "zone", "service"]
        );
        assert_eq!(content_words("services/tools"), ["service", "tool"]);
        assert_eq!(
            content_words("The settings entry contract"),
            ["setting", "entry", "contract"]
        );
        assert_eq!(content_words("A2UI Renderer"), ["a2ui", "renderer"]);
        // A word ending in `ss` is not a plural.
        assert_eq!(content_words("access"), ["access"]);
    }
}
