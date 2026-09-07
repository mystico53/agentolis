//! Plans, prices and — only when told to — generates model-written district
//! descriptions (PRD §12).
//!
//! ```text
//! cargo run --release -p polis-repo --example describe_llm -- <repo> [options]
//!
//!   (no flag)        dry run: what would be described, and what it would cost.
//!                    THE DEFAULT. Calls nothing.
//!   --generate       describe the districts that need it.
//!   --refresh        describe every eligible district, ignoring the cache.
//!   --yes            confirm spending. Required for a cold start.
//!   --clear-cache    forget every cached description, then exit.
//!   --show-cache     list what is cached and how fresh it is, then exit.
//!
//!   --provider NAME  glm | ollama | anthropic | openai-compatible
//!   --model ID       model id
//!   --base-url URL   API root
//!   --key-env NAME   environment variable holding the key (repeatable)
//!   --batch N        districts per request
//!   --concurrency N  requests in flight
//!   --top N          how many districts to print
//! ```
//!
//! # Nothing here is on the render path
//!
//! This is the explicit half of the feature. A snapshot or a launch calls
//! [`polis_repo::llm::apply_cached_model_descriptions`], which reads the cache
//! and opens no socket. Wiring `polis describe` in `polis-app` is one call to
//! [`polis_repo::llm::describe_with_model`] with the mode this example's flags
//! choose.
//!
//! Read-only with respect to the repository: it opens files for their first
//! 16 KiB and writes nothing into the checkout. The cache lives in the platform
//! state directory (ADR-0065).

use std::path::PathBuf;

use polis_repo::imports::ImportGraph;
use polis_repo::llm::{
    apply_cached_model_descriptions, default_cache_path, LlmConfig, LlmRunner, ModelCache,
    Provider, RunMode,
};
use polis_repo::tree::RepoIndex;

// Flag parsing and a report, in one flow. A tool's `main` reads better as one
// sequence than as six helpers that each run once.
#[allow(clippy::too_many_lines)]
fn main() -> anyhow::Result<()> {
    let mut root: Option<PathBuf> = None;
    let mut generate = false;
    let mut refresh = false;
    let mut yes = false;
    let mut clear = false;
    let mut show_cache = false;
    let mut top = 30usize;
    let mut provider: Option<Provider> = None;
    let mut model: Option<String> = None;
    let mut base_url: Option<String> = None;
    let mut key_env: Vec<String> = Vec::new();
    let mut batch: Option<usize> = None;
    let mut concurrency: Option<usize> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--generate" => generate = true,
            "--refresh" => refresh = true,
            "--yes" | "-y" => yes = true,
            "--clear-cache" => clear = true,
            "--show-cache" => show_cache = true,
            "--top" => top = args.next().and_then(|n| n.parse().ok()).unwrap_or(30),
            "--provider" => {
                let name = args.next().unwrap_or_default();
                provider = Some(match name.as_str() {
                    "glm" => Provider::Glm,
                    "ollama" => Provider::Ollama,
                    "anthropic" => Provider::Anthropic,
                    "openai-compatible" => Provider::OpenAiCompatible,
                    other => anyhow::bail!("unknown provider {other:?}"),
                });
            }
            "--model" => model = args.next(),
            "--base-url" => base_url = args.next(),
            "--key-env" => key_env.extend(args.next()),
            "--batch" => batch = args.next().and_then(|n| n.parse().ok()),
            "--concurrency" => concurrency = args.next().and_then(|n| n.parse().ok()),
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            other if other.starts_with("--") => anyhow::bail!("unknown flag {other:?}"),
            other => root = Some(PathBuf::from(other)),
        }
    }
    let Some(root) = root else {
        print_usage();
        std::process::exit(2);
    };

    let cache_path = default_cache_path(&root);
    if clear {
        match &cache_path {
            Some(path) if path.exists() => {
                std::fs::remove_file(path)?;
                println!("cleared {}", path.display());
            }
            Some(path) => println!("nothing to clear at {}", path.display()),
            None => println!("no state directory on this platform; nothing is cached"),
        }
        return Ok(());
    }

    // The configuration a repository carries, then the flags on top of it.
    let mut config = LlmConfig::for_repo(&root);
    // Typing the endpoint or the key variable is the act of choosing where a
    // key goes, so it lifts the repository-redirect guard that
    // `LlmConfig::load` puts on a file that arrived with a clone.
    let operator_chose_endpoint = provider.is_some() || base_url.is_some() || !key_env.is_empty();
    if let Some(provider) = provider {
        config = config.with_provider(provider);
    }
    if let Some(model) = model {
        config.model = model;
    }
    if let Some(url) = base_url {
        config.base_url = url;
    }
    if !key_env.is_empty() {
        config.key_env = key_env;
    }
    if operator_chose_endpoint {
        config = config.chosen_by_operator();
    }
    if let Some(n) = batch {
        config.batch_size = n;
    }
    if let Some(n) = concurrency {
        config.max_concurrency = n;
    }
    // The flags are an explicit act, so they imply the feature is wanted.
    if generate || refresh {
        config.enabled = true;
    }

    if show_cache {
        let cache = cache_path
            .as_deref()
            .map_or_else(ModelCache::default, ModelCache::read);
        println!("{} cached description(s)", cache.len());
        for entry in cache.entries() {
            println!(
                "  {:<40} {:>5} names  {}  {}",
                display(entry.path.as_str()),
                entry.sketch.count,
                entry.model,
                entry
                    .description
                    .as_ref()
                    .map_or("(nothing to say)", |d| d.label.as_str())
            );
        }
        return Ok(());
    }

    // The derived layer first: a README always wins, and the model is only asked
    // about what it could not answer.
    eprintln!("indexing {} …", root.display());
    let index = RepoIndex::open(&root)?;
    let graph = ImportGraph::build(index.tree());
    let mut hoods = index.neighborhoods_described(&graph.inbound_counts());
    apply_cached_model_descriptions(&root, &mut hoods, index.tree());

    let mode = if refresh {
        RunMode::Refresh { confirmed: yes }
    } else if generate {
        RunMode::Generate { confirmed: yes }
    } else {
        RunMode::DryRun
    };

    let mut cache = cache_path
        .as_deref()
        .map_or_else(ModelCache::default, ModelCache::read);
    let runner = LlmRunner::new(config);
    let report = runner.run(&mut hoods, index.tree(), &mut cache, mode);
    if mode.calls() {
        if let Some(path) = &cache_path {
            if cache.is_dirty() {
                cache.write(path);
            }
        }
    }

    // --- what it would cost -------------------------------------------------
    println!("\n{}", report.summary());

    // RunReport collects every failure and nothing printed them, so a run that
    // described 8 of 40 districts reported only the count and gave the operator
    // no way to find out why. Degradation is meant to be graceful, not silent.
    if !report.errors.is_empty() {
        println!("\n{} error(s):", report.errors.len());
        let mut seen: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for e in &report.errors {
            // Group by the error's first clause so a repeated timeout reads as
            // "12x transport: timed out" instead of twelve near-identical lines.
            let head = e.split_once(" (").map_or(e.as_str(), |(h, _)| h);
            *seen.entry(head).or_default() += 1;
        }
        for (head, count) in seen {
            if count == 1 {
                println!("  {head}");
            } else {
                println!("  {count}x {head}");
            }
        }
    }
    let plan = &report.plan;
    println!(
        "provider {} · model {} · endpoint {} · key from {} ({})",
        runner.config().provider,
        plan.model,
        plan.endpoint,
        plan.key_source,
        if plan.key_present {
            "present"
        } else {
            "ABSENT"
        },
    );
    println!(
        "districts: {} to describe · {} fresh · {} stale · {} kept from the repository · {} industrial",
        plan.len(),
        plan.fresh,
        plan.stale,
        plan.kept_derived,
        plan.industrial,
    );
    if !plan.redaction.is_empty() {
        println!(
            "outbound redaction: {} name(s), {} doc snippet(s), {} district(s) refused · {} unsafe character(s) stripped",
            plan.redaction.names_dropped,
            plan.redaction.docs_dropped,
            plan.redaction.districts_skipped,
            plan.redaction.unsafe_chars_removed,
        );
        for path in plan.redaction.paths.iter().take(10) {
            println!("   in {}", display(path.as_str()));
        }
    }
    if !plan.orphans.is_empty() {
        println!(
            "{} cached description(s) for districts that no longer exist",
            plan.orphans.len()
        );
    }

    // --- what would be sent, or what came back ------------------------------
    println!("\n| district | files | why | state | description |");
    println!("|---|---:|---|---|---|");
    // Industrial districts are never described, so they do not get to eat the
    // `--top` budget — filter first, then take.
    for hood in hoods
        .by_size()
        .into_iter()
        .filter(|h| !h.is_industrial())
        .take(top)
    {
        let why = plan
            .districts
            .iter()
            .find(|d| d.brief.path == hood.path)
            .map_or("—", |d| d.why.name());
        let state = match hood.freshness {
            polis_repo::llm::Freshness::Missing => "—".to_owned(),
            polis_repo::llm::Freshness::Fresh => "fresh".to_owned(),
            polis_repo::llm::Freshness::Stale(d) => {
                format!("STALE {} {}‰", d.cause.name(), d.permille)
            }
        };
        println!(
            "| `{}` | {} | {why} | {state} | *{}* {} |",
            display(hood.path.as_str()),
            hood.file_count,
            hood.description.as_ref().map_or("—", |d| d.source.name()),
            hood.label().unwrap_or(""),
        );
    }

    if !mode.calls() && !plan.is_empty() {
        println!(
            "\nThis was a dry run and nothing was called. Add --generate --yes to spend ~${:.4}.",
            plan.estimated_usd
        );
    }
    Ok(())
}

fn display(path: &str) -> &str {
    if path.is_empty() {
        "/"
    } else {
        path
    }
}

fn print_usage() {
    eprintln!(
        "usage: describe_llm <repo> [--generate|--refresh] [--yes] [--clear-cache] [--show-cache]\n\
         \x20             [--provider glm|ollama|anthropic|openai-compatible] [--model ID]\n\
         \x20             [--base-url URL] [--key-env NAME] [--batch N] [--concurrency N] [--top N]\n\
         \n\
         With no mode flag this is a DRY RUN: it prices the work and calls nothing."
    );
}
