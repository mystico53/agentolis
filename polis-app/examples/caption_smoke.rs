//! One real call to the configured endpoint, so the request shape is verified
//! rather than assumed.
//!
//! ADR-0089 shipped the Anthropic provider unverified and said so. This is the
//! opposite discipline for the caption path: `cargo run -p polis-app --example
//! caption_smoke` builds a brief from a made-up thread whose notes are the ones
//! this very session wrote, prints exactly what would leave the machine, calls
//! the endpoint once, and prints the phrase and what it cost.
//!
//! It needs `ZAI_API_KEY` (or `GLM_API_KEY`). With no key it prints the payload
//! and stops, which is the dry run.

use std::sync::Arc;
use std::time::Instant;

use polis_app::intent::{parse_reply, system_prompt, user_prompt, Brief};
use polis_events::{SessionId, ThreadId, ToolKind};
use polis_repo::llm::transport::DefaultTransport;
use polis_repo::llm::{LlmConfig, Secret, Transport};
use polis_world::{Intent, Thread, ThreadStatus};

fn main() {
    let now = Instant::now();
    let session = SessionId::new("smoke");
    let mut thread = Thread::new(ThreadId::of_session(session.clone()), session, now);
    thread.status = ThreadStatus::Working;
    thread.tool_calls = 34;

    // Real notes, from this session's own transcript.
    for note in [
        "List project structure and file sizes",
        "Read explain.rs",
        "Find where Operation is constructed",
        "Check polis-world dependencies",
        "Check polis-app compiles",
        "Add and run intent tests",
        "Run the map frame integration tests",
    ] {
        thread.intents.push_back(Intent {
            tool: ToolKind::Bash,
            text: note.to_owned(),
            at: now,
        });
    }
    for path in ["polis-app/src/intent.rs", "polis-world/src/apply.rs"] {
        thread
            .trail
            .push_back((polis_events::LogicalPath::new(path).expect("path"), now));
    }

    let brief = Brief::of(&thread).expect("a brief");
    let user = user_prompt(&brief);
    println!("--- everything that would leave this machine ---\n{user}");

    let config = LlmConfig {
        enabled: true,
        reasoning_effort: Some("low".to_owned()),
        ..LlmConfig::default()
    };
    let Some(key) = Secret::from_env(&config.key_env).map(Arc::new) else {
        println!("no key in {:?} — stopping here", config.key_env);
        return;
    };
    println!(
        "--- calling {} {} ---",
        config.provider.name(),
        config.model
    );

    let provider = config.provider.client();
    let request = provider
        .request(&config, Some(&key), &system_prompt(), &user)
        .expect("a request");
    let started = Instant::now();
    let response = DefaultTransport::new().post(&request).expect("a response");
    let elapsed = started.elapsed();
    let reply = provider.parse(&response).expect("a reply");

    println!("raw:      {}", reply.text.trim());
    match parse_reply(&reply.text) {
        Ok(phrase) => println!("phrase:   {phrase:?} ({} chars)", phrase.chars().count()),
        Err(refusal) => println!("refused:  {}", refusal.why()),
    }
    println!(
        "cost:     {} in / {} out tokens · ${:.6} · {} ms",
        reply.usage.input_tokens,
        reply.usage.output_tokens,
        config.price.cost(reply.usage),
        elapsed.as_millis()
    );
}
