//! What a thread is *working on*, in a phrase, written by a model (PRD §10.3,
//! §12; ADR-0089).
//!
//! # The question the notation cannot answer
//!
//! [`crate::explain`] answers *"why does this cloud look like that"* exactly,
//! with the constants that decide it, and it is the right answer to the
//! question it was built for. It cannot answer the other one. A cloud says
//! **where** a thread is working and **how hard**; a status word says whether
//! it is stuck. Nothing on the map says what the work *is*, and no amount of
//! geometry will, because intent is not in the geometry: `Edit
//! src/auth/token.rs` is equally *"adding the refresh path"* and *"reverting
//! yesterday's refresh path"*, and those are opposite facts about the same
//! pixel.
//!
//! The caption's first line has been standing in for it. That line is
//! `thread.title`, which is Claude Code's `ai-title`, generated from the
//! session's **first** prompt and never revised: a thread that began as *"fix
//! the build"* and is three hours into the renderer still reads *"fix the
//! build"*. ADR-0089 §5 is about exactly this — a caption that is confidently
//! wrong is worse than no caption, because the operator will believe it.
//!
//! # Where the words come from, and what leaves the machine
//!
//! One rule, and it is short enough to hold in the head:
//!
//! > **Only text the agent wrote about its own calls leaves the machine.**
//!
//! That is [`polis_world::Intent`] — the `description` field Claude Code asks
//! every `Bash`, `PowerShell` and `Agent` call to carry, already vetted by
//! [`polis_repo::llm::outbound::vet`] at the moment it was captured. Twenty of
//! them is a near-complete account of what a thread has been up to, written by
//! the only party that knows.
//!
//! Deliberately **not** sent, each for its own reason:
//!
//! | Not sent | Why |
//! |---|---|
//! | the operator's prompt | the most sensitive text on the machine, and the notes already carry the intent |
//! | assistant prose and thinking | long, and routinely quotes the code it is about |
//! | `tool_result` bodies | the output of the work is the work |
//! | command lines | a command line routinely holds a token |
//! | `thread.title` | it is a model's summary of the operator's prompt, so sending it would make the rule above untrue |
//!
//! [`Brief`] has no field that could hold any of them, which is the same
//! structural argument `DistrictBrief` makes and for the same reason: a promise
//! is checked by review, and a missing field is checked by the compiler.
//!
//! # Nothing is called implicitly
//!
//! Off unless the operator turns it on, one call at a time on one background
//! thread, never inside a frame, and every failure degrades to the `ai-title`
//! the caption showed before. See [`Captions`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use polis_events::ThreadId;
use polis_repo::llm::cache::Sketch;
use polis_repo::llm::provider::ChatProvider;
use polis_repo::llm::{outbound, transport::DefaultTransport, LlmConfig, Secret, Transport, Usage};
use polis_world::{Thread, ThreadStatus};

/// The longest phrase asked for, in characters.
///
/// Forty. It is drawn on a caption plate beside a cloud, and
/// [`crate::mapview`]'s own note on `MAP_TITLE_CHARS` is the constraint: a long
/// line of type laid across four districts is not a label, it is a banner.
/// Twenty-two is right for a *name*, which only has to tell four threads apart;
/// a phrase has to survive being read, and forty is about six words.
pub const PHRASE_CHARS: usize = 40;

/// How many of a thread's notes go into one brief.
///
/// All of them — [`polis_world::INTENT_CAP`] is 20 and the ring is already the
/// answer to "the last few minutes". Truncating further here would only make
/// the model guess at the middle of a story it was given the end of.
pub const NOTES_PER_BRIEF: usize = polis_world::INTENT_CAP;

/// How many places a brief names.
///
/// Grounding, not an inventory. The notes say what is being done; these say
/// where, and five directories is enough to distinguish *"in the ingest crate"*
/// from *"across the whole workspace"*.
pub const PLACES_PER_BRIEF: usize = 5;

/// How far a thread's notes may drift before its phrase is stale, in
/// parts per thousand.
///
/// # The number is arithmetic, not taste
///
/// The ring holds [`polis_world::INTENT_CAP`] = 20 notes and rolls forward, so
/// after `k` new calls the old set and the new set share `20 - k` notes out of
/// a union of `20 + k`, and the Jaccard distance is exactly `2k / (20 + k)`:
///
/// | new notes | drift |
/// |---:|---:|
/// | 3 | 261 ‰ |
/// | **5** | **400 ‰** |
/// | 8 | 571 ‰ |
/// | 10 | 667 ‰ |
///
/// So 400 ‰ means *"re-ask after five described calls"*, which is the
/// granularity the phrase is worth: fewer and it re-asks inside one piece of
/// work, more and it is describing what the thread had finished doing. It is
/// close to ADR-0089's measured 440 ‰ for districts by coincidence of shape,
/// not by derivation — that one is about file names over months, this one is
/// about notes over minutes.
pub const DRIFT_THRESHOLD_PERMILLE: u16 = 400;

/// The shortest time between two calls about one thread.
///
/// The drift rule already spaces calls by five described tool calls, which in
/// real work is a minute or more. This is the backstop for the case it does not
/// cover: a thread that fires five `Bash` calls in eight seconds, which happens
/// at the start of a turn and is not five pieces of work.
pub const MIN_INTERVAL: Duration = Duration::from_secs(45);

/// How many calls may be outstanding at once.
///
/// One. The worker is a single thread and this is the queue behind it, so the
/// bound is what stops nine simultaneously-drifting threads from becoming a
/// nine-deep backlog whose last answer describes work that finished a minute
/// ago. The thread that misses its turn is asked on the next pump.
pub const MAX_IN_FLIGHT: usize = 1;

// ---------------------------------------------------------------------------
// What leaves the machine
// ---------------------------------------------------------------------------

/// One thread, as the model sees it.
///
/// **Every field here is either a count, a status word, a directory name, or a
/// sentence the agent wrote about its own call.** There is no field for a file
/// body, a diff, a command line or a prompt, and that is the enforcement — see
/// the module documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brief {
    /// The agent's own notes, oldest first, already vetted at capture.
    pub notes: Vec<String>,
    /// Directories the thread has been working in.
    pub places: Vec<String>,
    /// `working`, `waiting`, `ready` — [`ThreadStatus`]'s own word.
    pub status: &'static str,
    /// How many tool calls it has made.
    pub calls: u32,
    /// How many of them failed.
    pub failures: u32,
    /// Running subagents, and how many there are in total.
    pub workers: (usize, usize),
}

impl Brief {
    /// The brief for one thread, or `None` when it has written no notes.
    ///
    /// No notes is not a degraded case to be papered over: a thread that has
    /// only read files has told us nothing about why, and the honest answer is
    /// the `ai-title` the caption already shows.
    pub fn of(thread: &Thread) -> Option<Self> {
        if thread.intents.is_empty() {
            return None;
        }
        let notes: Vec<String> = thread
            .intents
            .iter()
            .rev()
            .take(NOTES_PER_BRIEF)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|i| i.text.clone())
            .collect();

        // Directory names, not file names, and de-duplicated: twenty calls in
        // one crate should read as one place, not as twenty.
        let mut report = outbound::RedactionReport::default();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut places = Vec::new();
        for (path, _) in thread.trail.iter().rev() {
            let text = path.as_str();
            let dir = text.rsplit_once('/').map_or(text, |(head, _)| head);
            if dir.is_empty() || !seen.insert(dir.to_owned()) {
                continue;
            }
            // A path is a name and names are what ADR-0089 already sends, but
            // it is sent through the same gate anyway: the rule is that
            // *nothing* leaves unvetted, not that most things do.
            if let Ok(clean) = outbound::vet(dir, &mut report) {
                places.push(clean);
            }
            if places.len() >= PLACES_PER_BRIEF {
                break;
            }
        }

        Some(Self {
            notes,
            places,
            status: status_word(thread.status),
            calls: thread.tool_calls,
            failures: thread.failures,
            workers: (thread.running_workers(), thread.workers.len()),
        })
    }

    /// A sketch of the notes this brief was built from.
    ///
    /// The staleness key: a phrase is written from a set of notes, and it is
    /// stale when that set has moved on. See [`DRIFT_THRESHOLD_PERMILLE`].
    pub fn sketch(&self) -> Sketch {
        Sketch::build(&self.notes)
    }

    /// The brief as the model reads it.
    pub fn render(&self) -> String {
        use std::fmt::Write as _;

        // Every `write!` into a `String` is infallible; the results are dropped
        // rather than propagated because there is no failure here to report.
        let mut out = String::new();
        let _ = write!(out, "agent: {}, {} tool calls", self.status, self.calls);
        if self.failures > 0 {
            let _ = write!(out, ", {} of them failed", self.failures);
        }
        match self.workers {
            (_, 0) => {}
            (live, total) => {
                let _ = write!(out, ", {live} of {total} subagents running");
            }
        }
        out.push('\n');
        if !self.places.is_empty() {
            let _ = writeln!(out, "working in: {}", self.places.join(", "));
        }
        out.push_str("its notes on its own recent calls, oldest first:\n");
        for (i, note) in self.notes.iter().enumerate() {
            let _ = writeln!(out, "{}. {note}", i + 1);
        }
        out
    }
}

/// [`ThreadStatus`]'s own word, so the model and the rail cannot disagree.
fn status_word(status: ThreadStatus) -> &'static str {
    match status {
        ThreadStatus::Working => "working",
        ThreadStatus::Waiting => "waiting on the operator",
        ThreadStatus::Interrupted => "interrupted",
        ThreadStatus::Parked => "waiting on a background job",
        ThreadStatus::Ready => "finished its turn",
        ThreadStatus::Idle => "quiet",
        ThreadStatus::Done => "done",
    }
}

// ---------------------------------------------------------------------------
// The prompt
// ---------------------------------------------------------------------------

/// Bumping this invalidates every cached phrase.
///
/// A phrase written by a different prompt is a different claim, and leaving the
/// old ones on the map would mean two captions on one screen answering slightly
/// different questions.
pub const PROMPT_VERSION: u32 = 1;

/// What the model is told, once.
///
/// The refusal rule leads, in capitals, illustrated with the filler it is meant
/// to prevent — the same construction ADR-0089 §7 arrived at, for the same
/// reason: a phrase that would fit any agent on any day is worse than silence,
/// because it occupies the slot the operator looks at.
pub fn system_prompt() -> String {
    format!(
        "You caption live coding agents on a map. You are given the notes an agent wrote about \
         its own recent tool calls, oldest first, and you answer with a short phrase naming what \
         it is working on right now.\n\
         \n\
         REFUSE RATHER THAN PAD. If the notes do not support a specific answer, answer null. All \
         of these are refusals:\n\
         - \"working on the code\"\n\
         - \"making changes to files\"\n\
         - \"running commands and tests\"\n\
         A phrase that would fit any agent on any day is worse than no phrase at all, because the \
         operator will read it and believe it.\n\
         \n\
         Rules:\n\
         - At most {PHRASE_CHARS} characters. It is drawn beside a cloud on a map.\n\
         - A verb phrase in the present tense: \"rewriting the token refresh\", \"chasing a flaky \
         test in ingest\".\n\
         - The last few notes are what it is doing now; the earlier ones are how it got there. \
         Weight the end.\n\
         - Name the work, never the tool. \"Edit\", \"Bash\" and \"Grep\" mean nothing to the \
         reader.\n\
         - No jargon, no file extensions, no full paths.\n\
         - Lower case, unless it is a proper noun.\n\
         \n\
         Answer with JSON and nothing else: {{\"doing\": \"...\"}} or {{\"doing\": null}}"
    )
}

/// The brief, as one user turn.
pub fn user_prompt(brief: &Brief) -> String {
    brief.render()
}

/// Why an answer was not used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The model answered `null`, as it was asked to when it had nothing.
    ModelDeclined,
    /// Every content word was generic — see [`is_filler`].
    Filler,
    /// Nothing was left after trimming.
    Empty,
}

/// Reads the phrase out of a reply, or says why there is none.
///
/// Tolerant of the four shapes a model actually returns — a bare object, a
/// fenced block, an object inside prose, and a bare string — because
/// `response_format` is requested and, per ADR-0089, never relied on.
pub fn parse_reply(text: &str) -> Result<String, Refusal> {
    let phrase = extract(text).ok_or(Refusal::ModelDeclined)?;
    let phrase = phrase.trim().trim_matches('"').trim();
    if phrase.is_empty() || phrase.eq_ignore_ascii_case("null") {
        return Err(Refusal::Empty);
    }
    if is_filler(phrase) {
        return Err(Refusal::Filler);
    }
    Ok(shorten(phrase, PHRASE_CHARS))
}

/// The `doing` value, from whatever the model wrapped it in.
fn extract(text: &str) -> Option<String> {
    // The JSON object, wherever it is: a fenced block, a preamble, or on its
    // own. Scanning for the braces covers all three without a fence parser.
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
        if start < end {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text[start..=end]) {
                return match value.get("doing") {
                    Some(serde_json::Value::String(s)) => Some(s.clone()),
                    // An explicit `null` is the refusal working, not a parse
                    // failure, and both arrive here as `None`.
                    _ => None,
                };
            }
        }
    }
    // A model that ignored the shape entirely and answered with the phrase.
    let bare = text.trim();
    (!bare.is_empty() && !bare.contains('\n') && bare.chars().count() <= PHRASE_CHARS * 2)
        .then(|| bare.to_owned())
}

/// Whether a phrase says nothing that could not be said about any agent.
///
/// The second enforcement of the refusal rule, independent of the prompt,
/// because a rule that lives only in a prompt holds only until the next model —
/// the same argument as `prompt::restates_the_name`, and the same bias: it is
/// deliberately **high precision**, because a false positive here silently
/// deletes a good caption.
///
/// A phrase is filler only when *every* content word in it is generic.
/// *"fixing the failing tests"* survives on `failing`; *"running the tests"*
/// does not.
pub fn is_filler(phrase: &str) -> bool {
    /// Verbs and nouns that describe any agent's work equally well.
    const GENERIC: &[&str] = &[
        "work",
        "working",
        "make",
        "making",
        "do",
        "doing",
        "change",
        "changing",
        "changes",
        "update",
        "updating",
        "run",
        "running",
        "edit",
        "editing",
        "modify",
        "modifying",
        "handle",
        "handling",
        "fix",
        "fixing",
        "code",
        "file",
        "files",
        "stuff",
        "thing",
        "things",
        "task",
        "tasks",
        "item",
        "items",
        "project",
        "repo",
        "repository",
        "codebase",
        "command",
        "commands",
        "script",
        "scripts",
        "test",
        "tests",
        "some",
        "various",
        "several",
    ];
    /// Words that carry no meaning on their own in any phrase.
    const STOPWORDS: &[&str] = &[
        "a", "an", "the", "of", "in", "on", "at", "to", "for", "and", "or", "with", "into",
        "across", "over", "its", "it", "this", "that", "up", "out",
    ];

    let mut content = 0usize;
    for word in phrase.split(|c: char| !c.is_alphanumeric()) {
        let word = word.to_lowercase();
        if word.is_empty() || STOPWORDS.contains(&word.as_str()) {
            continue;
        }
        if !GENERIC.contains(&word.as_str()) {
            return false;
        }
        content += 1;
    }
    // Nothing but stopwords is not filler, it is empty; `parse_reply` has
    // already refused that case, and saying so twice would be a second answer
    // to one question.
    content > 0
}

/// Truncates on a character boundary, with an ellipsis when it cut.
fn shorten(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_owned();
    }
    let cut = text
        .char_indices()
        .nth(chars.saturating_sub(1))
        .map_or(text.len(), |(i, _)| i);
    let mut out = text[..cut].trim_end().to_owned();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// The live cache
// ---------------------------------------------------------------------------

/// One thread's phrase, and the notes it was written from.
#[derive(Debug, Clone)]
pub struct Caption {
    /// The phrase, ready to draw.
    pub text: String,
    /// When the answer arrived.
    pub written_at: Instant,
    /// The notes it describes. Compared against the thread's current notes to
    /// decide whether it still describes them.
    pub sketch: Sketch,
}

impl Caption {
    /// How far this caption's notes have drifted from a thread's current ones.
    pub fn drift_permille(&self, current: &Sketch) -> u16 {
        self.sketch.distance_permille(current)
    }

    /// Whether the thread has moved on from what this says.
    ///
    /// A stale caption is **still shown**, dimmed — ADR-0089 §5's rule, which
    /// applies here more sharply than it did to districts: blanking it would
    /// trade a phrase the operator can see is old for no phrase at all, and the
    /// thing it is about is moving.
    pub fn is_stale(&self, current: &Sketch) -> bool {
        self.drift_permille(current) >= DRIFT_THRESHOLD_PERMILLE
    }

    /// How long ago it was written.
    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.written_at)
    }
}

/// One question for the worker thread.
#[derive(Debug)]
struct Job {
    thread: ThreadId,
    sketch: Sketch,
    system: String,
    user: String,
}

/// One answer coming back.
#[derive(Debug)]
struct Answer {
    thread: ThreadId,
    sketch: Sketch,
    at: Instant,
    /// The phrase, or why there is none.
    phrase: Result<String, String>,
    usage: Usage,
}

/// Every thread's phrase, kept current by one background worker.
///
/// # Why a worker thread and not a future
///
/// PRD §13.1 budgets a frame; a network call is three orders of magnitude
/// outside it. `LlmRunner::spawn` makes the same call for districts and for the
/// same reason. One worker, one call at a time ([`MAX_IN_FLIGHT`]), so the
/// feature cannot become a thread pool that a busy map turns into a bill.
///
/// # Every failure is a phrase that does not appear
///
/// No key, a dead endpoint, a 429, a timeout, a body that is not JSON, a model
/// that refuses: each one leaves the caption showing whatever it showed before,
/// which on a thread that has never had a phrase is the `ai-title`. There is no
/// error path a caller has to handle to stay correct — the last error is kept
/// only so [`Captions::last_error`] can put it in the status bar, where an
/// operator who turned the feature on and sees nothing can find out why.
pub struct Captions {
    /// What the operator turned on, and where it sends.
    config: LlmConfig,
    /// Answers, keyed by thread.
    phrases: BTreeMap<ThreadId, Caption>,
    /// Threads the worker is currently being asked about.
    in_flight: BTreeSet<ThreadId>,
    /// When each thread was last asked about, in flight or not.
    last_asked: BTreeMap<ThreadId, Instant>,
    /// The notes that were asked about and produced no phrase.
    ///
    /// Without this, a thread whose notes the model will not caption — the
    /// honest outcome for a thread that has only been running builds — is asked
    /// again every [`MIN_INTERVAL`] for as long as the window is open, and each
    /// of those calls is billed to say nothing. Remembering the *set of notes*
    /// that was refused, rather than the fact of a refusal, is what lets the
    /// next real piece of work still get a phrase: the same drift rule that
    /// makes a caption stale makes a refusal worth revisiting.
    refused: BTreeMap<ThreadId, Sketch>,
    /// To the worker.
    jobs: Option<Sender<Job>>,
    /// From the worker.
    answers: Receiver<Answer>,
    /// What every call so far has cost, as the provider counted it.
    usage: Usage,
    /// How many calls have been made.
    calls: u64,
    /// The most recent failure, for the status bar.
    last_error: Option<String>,
}

impl std::fmt::Debug for Captions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Captions")
            .field("enabled", &self.config.enabled)
            .field("phrases", &self.phrases.len())
            .field("calls", &self.calls)
            .finish_non_exhaustive()
    }
}

impl Captions {
    /// A cache that will never call anything.
    ///
    /// What every window has unless the operator asked otherwise. `pump` still
    /// runs and still costs nothing, so no caller needs an `Option`.
    #[must_use]
    pub fn off() -> Self {
        let (_, answers) = std::sync::mpsc::channel();
        Self {
            config: LlmConfig::default(),
            phrases: BTreeMap::new(),
            in_flight: BTreeSet::new(),
            last_asked: BTreeMap::new(),
            refused: BTreeMap::new(),
            jobs: None,
            answers,
            usage: Usage::default(),
            calls: 0,
            last_error: None,
        }
    }

    /// A cache that will call `config`'s endpoint, with the default transport.
    #[must_use]
    pub fn new(config: LlmConfig) -> Self {
        Self::with_transport(config, Arc::new(DefaultTransport::new()))
    }

    /// What a window builds from [`crate::config::Config::captions`].
    ///
    /// The shipped provider and price, with only `enabled` coming from the
    /// operator. `<repo>/.polis/llm.json` is deliberately **not** read here:
    /// that file arrives with a clone and ADR-0089 already had to invent
    /// `ConfigOrigin` to stop a repository naming the host somebody's key goes
    /// to. Live captions are not the place to reopen that question — an
    /// operator who wants another endpoint changes it in one place, in code,
    /// and knows they did.
    #[must_use]
    pub fn for_operator(enabled: bool) -> Self {
        if !enabled {
            return Self::off();
        }
        Self::new(LlmConfig {
            enabled: true,
            // Measured against this exact endpoint: see
            // `LlmConfig::reasoning_effort`. A caption is a labelling task, and
            // paying for 226 tokens of deliberation to produce the same six
            // words as 12 is the difference between a feature that costs
            // nothing and one an operator notices.
            reasoning_effort: Some("low".to_owned()),
            ..LlmConfig::default()
        })
    }

    /// [`Captions::new`] against an explicit transport, for tests.
    #[must_use]
    pub fn with_transport(config: LlmConfig, transport: Arc<dyn Transport>) -> Self {
        if !config.enabled {
            return Self::off();
        }
        let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
        let (answer_tx, answer_rx) = std::sync::mpsc::channel::<Answer>();
        let worker_config = config.clone();
        // Detached on purpose: the channel closing is what ends it, and that
        // happens when the window drops this struct. A join handle would only
        // give the shutdown path something to wait on.
        let spawned = std::thread::Builder::new()
            .name("polis-intent".to_owned())
            .spawn(move || worker(&worker_config, transport.as_ref(), &job_rx, &answer_tx));
        let mut out = Self::off();
        if spawned.is_ok() {
            out.jobs = Some(job_tx);
            out.answers = answer_rx;
            out.config = config;
        }
        out
    }

    /// A cache holding one phrase and calling nothing, for tests.
    ///
    /// The only way to get a phrase into a `Captions` otherwise is to make a
    /// call, and a test of how a *panel draws a phrase* should not need a
    /// transport, a worker thread and a settle loop to say what it is about.
    #[cfg(test)]
    pub(crate) fn seeded(thread: ThreadId, text: &str, notes: &[&str]) -> Self {
        let mut out = Self::off();
        out.phrases.insert(
            thread,
            Caption {
                text: text.to_owned(),
                written_at: Instant::now(),
                sketch: Sketch::build(notes),
            },
        );
        out
    }

    /// Whether anything will be called.
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.jobs.is_some()
    }

    /// This thread's phrase, however old.
    #[must_use]
    pub fn get(&self, thread: &ThreadId) -> Option<&Caption> {
        self.phrases.get(thread)
    }

    /// What every call so far has cost in dollars, at the configured price.
    ///
    /// Zero for a local endpoint, which is a real answer and not a missing one.
    #[must_use]
    pub fn spent(&self) -> f64 {
        self.config.price.cost(self.usage)
    }

    /// Tokens in and out of every call so far, as the provider counted them.
    ///
    /// Beside [`Captions::spent`] because a provider that reports no usage
    /// makes the cost an estimate, and the two together are what let the status
    /// bar say which it is showing.
    #[must_use]
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// How many calls have been made.
    #[must_use]
    pub fn calls(&self) -> u64 {
        self.calls
    }

    /// The most recent failure, if any.
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Takes in answers, then asks about whatever has drifted.
    ///
    /// Called once a frame. Never blocks: the receive is a `try_recv` loop and
    /// the send is to an unbounded channel behind a one-deep gate.
    pub fn pump(&mut self, threads: &[Thread], now: Instant) {
        self.collect(now);
        if self.jobs.is_none() {
            return;
        }
        self.forget_dead(threads);
        // In the order the world publishes threads, which is the rail's order:
        // waiting first. When several threads have drifted at once and only one
        // may be asked, the one the operator is most likely to be looking at
        // gets the turn, and `MIN_INTERVAL` stops it from taking every turn.
        for thread in threads {
            if self.in_flight.len() >= MAX_IN_FLIGHT {
                break;
            }
            self.consider(thread, now);
        }
    }

    /// Drains the worker's answers into the cache.
    fn collect(&mut self, now: Instant) {
        loop {
            match self.answers.try_recv() {
                Ok(answer) => {
                    self.in_flight.remove(&answer.thread);
                    self.usage.add(answer.usage);
                    self.calls = self.calls.saturating_add(1);
                    match answer.phrase {
                        Ok(text) => {
                            self.last_error = None;
                            self.refused.remove(&answer.thread);
                            self.phrases.insert(
                                answer.thread,
                                Caption {
                                    text,
                                    written_at: answer.at,
                                    sketch: answer.sketch,
                                },
                            );
                        }
                        Err(why) => {
                            // A refusal is not an error to report at the same
                            // volume as a dead endpoint, but both leave the
                            // previous phrase standing, and the operator asking
                            // "why is nothing appearing" needs the last one.
                            self.last_error = Some(why);
                            self.last_asked.insert(answer.thread.clone(), now);
                            // And these exact notes are not asked about again.
                            self.refused.insert(answer.thread, answer.sketch);
                        }
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    /// Drops captions for threads the world no longer has.
    fn forget_dead(&mut self, threads: &[Thread]) {
        if self.phrases.is_empty() && self.last_asked.is_empty() && self.refused.is_empty() {
            return;
        }
        let live: BTreeSet<&ThreadId> = threads.iter().map(|t| &t.id).collect();
        self.phrases.retain(|id, _| live.contains(id));
        self.last_asked.retain(|id, _| live.contains(id));
        self.refused.retain(|id, _| live.contains(id));
    }

    /// Asks about one thread, if it needs asking about.
    fn consider(&mut self, thread: &Thread, now: Instant) {
        if self.in_flight.contains(&thread.id) {
            return;
        }
        if self
            .last_asked
            .get(&thread.id)
            .is_some_and(|at| now.saturating_duration_since(*at) < MIN_INTERVAL)
        {
            return;
        }
        let Some(brief) = Brief::of(thread) else {
            return;
        };
        let sketch = brief.sketch();
        if self
            .phrases
            .get(&thread.id)
            .is_some_and(|c| !c.is_stale(&sketch))
        {
            return;
        }
        // These notes, or nearly these notes, have already been refused once.
        if self
            .refused
            .get(&thread.id)
            .is_some_and(|s| s.distance_permille(&sketch) < DRIFT_THRESHOLD_PERMILLE)
        {
            return;
        }
        let job = Job {
            thread: thread.id.clone(),
            sketch,
            system: system_prompt(),
            user: user_prompt(&brief),
        };
        if let Some(jobs) = &self.jobs {
            if jobs.send(job).is_ok() {
                self.in_flight.insert(thread.id.clone());
                self.last_asked.insert(thread.id.clone(), now);
            } else {
                // The worker is gone. Nothing will ever answer again, and a
                // window that keeps queueing into a dead channel would spend
                // the rest of its life pretending it was about to.
                self.jobs = None;
                self.last_error = Some("the caption worker stopped".to_owned());
            }
        }
    }
}

/// The worker loop: one call per job, until the channel closes.
fn worker(
    config: &LlmConfig,
    transport: &dyn Transport,
    jobs: &Receiver<Job>,
    answers: &Sender<Answer>,
) {
    let provider = config.provider.client();
    // Read once, at the top of the worker's life. `Secret::from_env` is the
    // only place the key is read and this is the only thread that holds one.
    let key = Secret::from_env(&config.key_env).map(Arc::new);
    while let Ok(job) = jobs.recv() {
        let (phrase, usage) = ask(config, provider.as_ref(), transport, key.as_ref(), &job);
        let answer = Answer {
            thread: job.thread,
            sketch: job.sketch,
            at: Instant::now(),
            phrase,
            usage,
        };
        if answers.send(answer).is_err() {
            return;
        }
    }
}

/// One call, start to finish.
///
/// Returns the usage alongside the outcome, because a call that produced a
/// refusal still cost tokens and an accounting that only counted the successes
/// would understate the bill — which is the one number the operator is entitled
/// to have be exact.
fn ask(
    config: &LlmConfig,
    provider: &dyn ChatProvider,
    transport: &dyn Transport,
    key: Option<&Arc<Secret>>,
    job: &Job,
) -> (Result<String, String>, Usage) {
    let request = match provider.request(config, key, &job.system, &job.user) {
        Ok(request) => request,
        Err(e) => return (Err(e.to_string()), Usage::default()),
    };
    let response = match transport.post(&request) {
        Ok(response) => response,
        Err(e) => return (Err(e.to_string()), Usage::default()),
    };
    let reply = match provider.parse(&response) {
        Ok(reply) => reply,
        // A non-2xx body arrives here as a `Status` error carrying the
        // provider's own message, which is the most useful thing there is when
        // a model id is wrong. It is bounded and already scrubbed.
        Err(e) => return (Err(e.to_string()), Usage::default()),
    };
    let phrase = parse_reply(&reply.text).map_err(|refusal| refusal.why().to_owned());
    (phrase, reply.usage)
}

impl Refusal {
    /// Why there is no phrase, in the words the status bar shows.
    #[must_use]
    pub fn why(self) -> &'static str {
        match self {
            Self::ModelDeclined => "the model had nothing specific to say",
            Self::Filler => "the model's answer said nothing",
            Self::Empty => "the model answered with nothing",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use polis_events::{LogicalPath, SessionId, ToolKind};
    use polis_repo::llm::transport::{HttpRequest, HttpResponse, TransportError};
    use polis_repo::llm::Provider;
    use polis_world::Intent;

    fn thread_with(notes: &[&str]) -> Thread {
        let now = Instant::now();
        let session = SessionId::new("caption");
        let mut thread = Thread::new(
            polis_events::ThreadId::of_session(session.clone()),
            session,
            now,
        );
        thread.status = ThreadStatus::Working;
        thread.tool_calls = 12;
        for note in notes {
            thread.intents.push_back(Intent {
                tool: ToolKind::Bash,
                text: (*note).to_owned(),
                at: now,
            });
        }
        for path in ["polis-app/src/intent.rs", "polis-world/src/apply.rs"] {
            thread
                .trail
                .push_back((LogicalPath::new(path).expect("path"), now));
        }
        thread
    }

    /// The whole privacy argument is that a brief cannot carry the work, only
    /// the agent's notes about it. This is that claim, checked: the notes go in
    /// verbatim, the places are **directories**, and nothing else is prose.
    #[test]
    fn a_brief_carries_the_notes_and_the_directories_and_nothing_else() {
        let thread = thread_with(&["Read the callout module", "Run the workspace tests"]);
        let brief = Brief::of(&thread).expect("a brief");
        assert_eq!(
            brief.notes,
            vec!["Read the callout module", "Run the workspace tests"],
            "oldest first, verbatim"
        );
        assert_eq!(
            brief.places,
            vec!["polis-world/src", "polis-app/src"],
            "directories, newest place first, never file names"
        );
        let rendered = brief.render();
        assert!(
            rendered.contains("1. Read the callout module"),
            "{rendered}"
        );
        assert!(rendered.contains("agent: working"), "{rendered}");
        // The file names the trail carried must not survive into the payload.
        assert!(
            !rendered.contains("apply.rs"),
            "a file name leaked: {rendered}"
        );
        assert!(
            !rendered.contains("intent.rs"),
            "a file name leaked: {rendered}"
        );
    }

    /// A thread that has written no notes gets no brief, and therefore no call.
    /// Silence is the honest answer, and it is also the free one.
    #[test]
    fn a_thread_with_no_notes_is_never_asked_about() {
        let thread = thread_with(&[]);
        assert!(Brief::of(&thread).is_none());
    }

    /// The shapes a model actually answers in. `response_format` is requested
    /// and, per ADR-0089, never relied on.
    #[test]
    fn every_shape_a_model_answers_in_is_read() {
        let fenced = "```json\n{\"doing\": \"rewriting the token refresh\"}\n```";
        let chatty = "Here you go:\n{\"doing\": \"rewriting the token refresh\"}\nHope that helps.";
        for body in [
            "{\"doing\": \"rewriting the token refresh\"}",
            fenced,
            chatty,
        ] {
            assert_eq!(
                parse_reply(body).expect("a phrase"),
                "rewriting the token refresh",
                "not read: {body}"
            );
        }
        // A model that ignored the shape and answered with the phrase itself.
        assert_eq!(
            parse_reply("rewriting the token refresh").expect("a phrase"),
            "rewriting the token refresh"
        );
    }

    /// `null` is the refusal the prompt asks for, and it must arrive as a
    /// refusal rather than as the word "null" drawn on the map.
    #[test]
    fn a_refusal_is_a_refusal_and_not_a_caption() {
        assert_eq!(
            parse_reply("{\"doing\": null}"),
            Err(Refusal::ModelDeclined)
        );
        assert_eq!(parse_reply("{\"doing\": \"\"}"), Err(Refusal::Empty));
        assert_eq!(parse_reply("null"), Err(Refusal::Empty));
    }

    /// ADR-0089 §7's second enforcement, in this module's terms: a phrase that
    /// would fit any agent on any day is deleted even when the model sent it.
    #[test]
    fn filler_is_refused_independently_of_the_prompt() {
        for filler in [
            "working on the code",
            "making changes to files",
            "running commands and tests",
            "updating the project",
            "doing some work",
        ] {
            assert!(is_filler(filler), "not caught: {filler}");
            assert_eq!(parse_reply(filler), Err(Refusal::Filler), "{filler}");
        }
        // High precision: one specific word is enough to save a phrase, because
        // a false positive here silently deletes a good caption.
        for real in [
            "rewriting the token refresh",
            "fixing the failing ingest tests",
            "chasing a flaky test in ingest",
            "running the migration for billing",
        ] {
            assert!(!is_filler(real), "wrongly caught: {real}");
        }
    }

    /// A phrase is drawn beside a cloud, so it is cut to fit rather than
    /// allowed to widen every plate on the map.
    #[test]
    fn an_over_long_phrase_is_cut_to_the_caption_budget() {
        let long = "rewriting the token refresh path and every one of its callers";
        let phrase = parse_reply(long).expect("a phrase");
        assert!(phrase.chars().count() <= PHRASE_CHARS, "{phrase}");
        assert!(phrase.ends_with('…'), "a cut is visible: {phrase}");
    }

    /// The drift table in [`DRIFT_THRESHOLD_PERMILLE`]'s documentation is an
    /// arithmetic claim about this ring, and a claim in a doc comment that
    /// nothing checks is a claim that stops being true.
    #[test]
    fn the_drift_table_is_what_the_sketch_actually_computes() {
        let base: Vec<String> = (0..20).map(|i| format!("note {i}")).collect();
        let old = Sketch::build(&base);
        for (new_notes, expected) in [(3u32, 261u16), (5, 400), (8, 571), (10, 667)] {
            let rolled: Vec<String> = (new_notes..new_notes + 20)
                .map(|i| format!("note {i}"))
                .collect();
            let drift = old.distance_permille(&Sketch::build(&rolled));
            assert!(
                drift.abs_diff(expected) <= 1,
                "{new_notes} new notes drifted {drift} per mille, not {expected}"
            );
        }
        // And the threshold is crossed at five, which is the whole claim.
        let four: Vec<String> = (4..24).map(|i| format!("note {i}")).collect();
        let five: Vec<String> = (5..25).map(|i| format!("note {i}")).collect();
        assert!(old.distance_permille(&Sketch::build(&four)) < DRIFT_THRESHOLD_PERMILLE);
        assert!(old.distance_permille(&Sketch::build(&five)) >= DRIFT_THRESHOLD_PERMILLE);
    }

    /// A transport that answers whatever it was built with, without a socket.
    #[derive(Debug)]
    struct Canned {
        body: String,
        status: u16,
    }

    impl Transport for Canned {
        fn post(&self, _request: &HttpRequest) -> Result<HttpResponse, TransportError> {
            Ok(HttpResponse {
                status: self.status,
                body: self.body.clone(),
            })
        }
        fn name(&self) -> &'static str {
            "canned"
        }
    }

    fn canned(body: &str, status: u16) -> Arc<dyn Transport> {
        Arc::new(Canned {
            body: body.to_owned(),
            status,
        })
    }

    fn config() -> LlmConfig {
        // Ollama needs no key, which is what lets these run with no
        // environment and no network.
        let mut config = LlmConfig::default().with_provider(Provider::Ollama);
        config.enabled = true;
        config
    }

    /// The worker is a real thread, so the answer arrives on its own schedule;
    /// every pump after the first is a `try_recv` and costs nothing.
    fn settle(captions: &mut Captions, threads: &[Thread]) {
        for _ in 0..2000 {
            captions.pump(threads, Instant::now());
            if captions.calls() > 0 {
                return;
            }
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_phrase_arrives_and_then_the_thread_is_not_asked_again() {
        let body = "{\"choices\":[{\"message\":{\"content\":\"{\\\"doing\\\": \\\"rewriting the token refresh\\\"}\"}}],\"usage\":{\"prompt_tokens\":400,\"completion_tokens\":12}}";
        let mut captions = Captions::with_transport(config(), canned(body, 200));
        let threads = vec![thread_with(&["Read the callout module"])];
        settle(&mut captions, &threads);
        let caption = captions.get(&threads[0].id).expect("a phrase");
        assert_eq!(caption.text, "rewriting the token refresh");
        assert_eq!(captions.calls(), 1);
        // The notes have not moved, so no amount of pumping asks again.
        for _ in 0..50 {
            captions.pump(&threads, Instant::now());
        }
        assert_eq!(captions.calls(), 1, "a still thread is asked once");
        // Accounted for even though this endpoint is free: the tokens are what
        // the provider reported, and the price is a separate question.
        assert_eq!(captions.usage().input_tokens, 400);
        assert_eq!(captions.usage().output_tokens, 12);
    }

    /// Every failure leaves the map exactly as it is with the feature off.
    #[test]
    fn a_dead_endpoint_produces_no_caption_and_no_panic() {
        let mut captions = Captions::with_transport(config(), canned("<html>502</html>", 502));
        let threads = vec![thread_with(&["Read the callout module"])];
        settle(&mut captions, &threads);
        assert!(captions.get(&threads[0].id).is_none(), "nothing is drawn");
        assert!(
            captions.last_error().is_some(),
            "and the reason is available"
        );
    }

    /// The default is off, and off calls nothing however hard it is pumped.
    #[test]
    fn the_default_is_off_and_off_never_calls() {
        let mut captions = Captions::new(LlmConfig::default());
        assert!(!captions.is_on());
        let threads = vec![thread_with(&["Read the callout module"])];
        for _ in 0..50 {
            captions.pump(&threads, Instant::now());
        }
        assert_eq!(captions.calls(), 0);
        assert!(captions.get(&threads[0].id).is_none());
    }

    /// A thread whose notes the model will not caption must not be asked about
    /// once every [`MIN_INTERVAL`] for the rest of the window's life. Each of
    /// those calls is billed to say nothing.
    #[test]
    fn a_refused_set_of_notes_is_not_asked_about_again() {
        let body = "{\"choices\":[{\"message\":{\"content\":\"{\\\"doing\\\": null}\"}}]}";
        let mut captions = Captions::with_transport(config(), canned(body, 200));
        let mut threads = vec![thread_with(&["Run the workspace tests"])];
        settle(&mut captions, &threads);
        assert_eq!(captions.calls(), 1, "asked once");
        assert!(captions.get(&threads[0].id).is_none(), "and refused");

        // `MIN_INTERVAL` has passed, and the notes have not moved.
        let later = Instant::now() + MIN_INTERVAL + Duration::from_secs(1);
        for _ in 0..50 {
            captions.pump(&threads, later);
        }
        assert_eq!(captions.calls(), 1, "the same notes are not re-asked");

        // Real new work drifts past the threshold and is asked about again.
        for i in 0..6 {
            threads[0].intents.push_back(polis_world::Intent {
                tool: ToolKind::Bash,
                text: format!("Rewrite the token refresh, step {i}"),
                at: Instant::now(),
            });
        }
        for _ in 0..2000 {
            captions.pump(&threads, later);
            if captions.calls() > 1 {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(captions.calls(), 2, "new work is asked about");
    }

    /// A thread that ends takes its phrase with it, or a long-lived window
    /// accumulates one caption per session it has ever seen.
    #[test]
    fn a_finished_thread_is_forgotten() {
        let body = "{\"choices\":[{\"message\":{\"content\":\"{\\\"doing\\\": \\\"rewriting the refresh\\\"}\"}}]}";
        let mut captions = Captions::with_transport(config(), canned(body, 200));
        let threads = vec![thread_with(&["Read the callout module"])];
        settle(&mut captions, &threads);
        assert!(captions.get(&threads[0].id).is_some());
        captions.pump(&[], Instant::now());
        assert!(
            captions.get(&threads[0].id).is_none(),
            "dropped with its thread"
        );
    }
}
