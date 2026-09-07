//! Tests for [`super`]. In their own file only because the module they cover is
//! long; `#[path]`-included so they are the same module and can reach its
//! private constants.

use std::collections::BTreeMap;
use std::sync::Arc;

use polis_events::LogicalPath;

use super::{
    candidacy, district_names, model_description, Candidacy, Freshness, KeyWithheld, LlmConfig,
    LlmRunner, ModelCache, Neighborhood, Neighborhoods, PlannedDistrict, RepoTree, RunMode, Sketch,
    Usage, ESTIMATED_OUTPUT_TOKENS_PER_DISTRICT, PROMPT_VERSION,
};
use crate::describe::{Description, DescriptionSource};
use crate::llm::transport::testing::{FakeEndpoint, Reply};
use crate::llm::transport::{
    HttpRequest, HttpResponse, PlainHttpTransport, Transport, TransportError,
};
use crate::llm::{DriftCause, Price, Provider};
use crate::neighborhoods::NeighborhoodOptions;
use crate::FileMeta;

fn lp(s: &str) -> LogicalPath {
    LogicalPath::new(s).expect("test path")
}

/// A repository of directories with a given number of files each.
fn tree_with(dirs: &[(&str, usize)]) -> RepoTree {
    let mut tree = RepoTree {
        root: std::path::PathBuf::from("/fake"),
        ..RepoTree::default()
    };
    for (dir, count) in dirs {
        for i in 0..*count {
            let path = lp(&format!("{dir}/f{i:03}.ts"));
            tree.files
                .insert(path.clone(), FileMeta::untracked(path, 100));
        }
    }
    tree
}

fn hoods_of(tree: &RepoTree) -> Neighborhoods {
    Neighborhoods::build(tree, &NeighborhoodOptions::default())
}

/// An answer for whatever districts the partition produced, so a test does not
/// have to know how it came out.
fn answer_all(hoods: &Neighborhoods, label: &str) -> String {
    let districts: Vec<serde_json::Value> = hoods
        .all()
        .iter()
        .map(|h| {
            serde_json::json!({
                "path": if h.path.is_root() { "/" } else { h.path.as_str() },
                "label": format!("{label} for {}", h.name),
                "detail": format!("{label} for {}. Owns the wire format.", h.name),
            })
        })
        .collect();
    serde_json::json!({
        "choices": [{"message": {"content":
            serde_json::json!({"districts": districts}).to_string()},
            "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1200, "completion_tokens": 90}
    })
    .to_string()
}

/// A configuration pointed at `base`, with no key and no backoff.
fn config_for(base: &str) -> LlmConfig {
    let mut config = LlmConfig::default().with_provider(Provider::Ollama);
    config.enabled = true;
    config.base_url = base.to_owned();
    config.model = "test-model".to_owned();
    config.retry_base_ms = 0;
    config.batch_size = 4;
    config.max_concurrency = 2;
    config.price = Price::GLM_FLASH_PROMO;
    config
}

/// A transport that always fails, for the degradation tests.
#[derive(Debug)]
struct DeadTransport(TransportError);

impl Transport for DeadTransport {
    fn name(&self) -> &'static str {
        "dead"
    }

    fn post(&self, _request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        Err(self.0.clone())
    }
}

/// A transport that returns a fixed status and body, counting calls.
#[derive(Debug)]
struct CannedTransport {
    status: u16,
    body: String,
    calls: std::sync::atomic::AtomicU32,
}

impl CannedTransport {
    fn new(status: u16, body: &str) -> Self {
        Self {
            status,
            body: body.to_owned(),
            calls: std::sync::atomic::AtomicU32::new(0),
        }
    }

    fn call_count(&self) -> u32 {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Transport for CannedTransport {
    fn name(&self) -> &'static str {
        "canned"
    }

    fn post(&self, _request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(HttpResponse {
            status: self.status,
            body: self.body.clone(),
        })
    }
}

// -- candidacy --------------------------------------------------------------

/// The review's recommendation, executable: a human's sentence always wins, and
/// the model is asked only where the extractor failed or hedged.
#[test]
fn a_real_readme_sentence_is_never_replaced_by_a_model() {
    let config = LlmConfig::default();
    let tree = tree_with(&[("src/a", 10)]);
    let hoods = hoods_of(&tree);
    let mut hood = hoods.all()[0].clone();

    for source in [DescriptionSource::Readme, DescriptionSource::Manifest] {
        hood.description = Some(Description {
            label: "Routes every inbound webhook to the right handler".to_owned(),
            detail: "Routes every inbound webhook to the right handler.".to_owned(),
            source,
            origin: Some(lp("src/a/README.md")),
        });
        assert_eq!(candidacy(&hood, &config), None, "{source:?}");
    }

    hood.description = None;
    assert_eq!(candidacy(&hood, &config), Some(Candidacy::NoDescription));

    hood.description = Some(Description {
        label: "most imported: client.ts".to_owned(),
        detail: "12 files.".to_owned(),
        source: DescriptionSource::Inventory,
        origin: None,
    });
    assert_eq!(candidacy(&hood, &config), Some(Candidacy::Inventory));
    assert!(!Candidacy::Inventory.name().is_empty());
}

/// `src/services`, 223 files, labelled "Centralized Window Management Service"
/// from one of them. A doc comment speaks for a small directory.
#[test]
fn a_doc_comment_is_kept_on_a_small_district_and_refused_on_a_large_one() {
    let config = LlmConfig::default();
    let tree = tree_with(&[("src/a", 10)]);
    let hoods = hoods_of(&tree);
    let mut hood = hoods.all()[0].clone();
    hood.description = Some(Description {
        label: "Centralized Window Management Service".to_owned(),
        detail: "Unified registry and API for every overlay window.".to_owned(),
        source: DescriptionSource::DocComment,
        origin: Some(lp("src/a/WindowManager.js")),
    });
    hood.file_count = 12;
    assert_eq!(candidacy(&hood, &config), None, "small enough to trust");
    hood.file_count = 223;
    assert_eq!(
        candidacy(&hood, &config),
        Some(Candidacy::DocCommentOverreach)
    );
}

#[test]
fn a_derived_restatement_is_a_candidate_however_it_was_derived() {
    let config = LlmConfig::default();
    let tree = tree_with(&[("services/settings", 10)]);
    let hoods = hoods_of(&tree);
    let mut hood = hoods.all()[0].clone();
    hood.name = "services/settings".to_owned();
    hood.file_count = 16;
    hood.description = Some(Description {
        label: "The settings entry contract".to_owned(),
        detail: "The settings entry contract.".to_owned(),
        source: DescriptionSource::DocComment,
        origin: Some(lp("services/settings/index.ts")),
    });
    assert_eq!(candidacy(&hood, &config), Some(Candidacy::Restatement));
}

#[test]
fn an_industrial_district_is_never_described() {
    let config = LlmConfig::default();
    let tree = tree_with(&[("node_modules/x", 40), ("src", 10)]);
    let hoods = hoods_of(&tree);
    let vendored = hoods
        .all()
        .iter()
        .find(|h| h.is_industrial())
        .expect("a vendored district");
    assert_eq!(candidacy(vendored, &config), None);
}

// -- the dry run ------------------------------------------------------------

#[test]
fn a_dry_run_prices_the_work_and_calls_nothing() {
    let tree = tree_with(&[("src/a", 12), ("src/b", 12), ("src/c", 12)]);
    let mut hoods = hoods_of(&tree);
    let runner = LlmRunner::with_transport(
        config_for("http://127.0.0.1:1/v1"),
        Arc::new(DeadTransport(TransportError::Unreachable("no".to_owned()))),
    );
    let mut cache = ModelCache::default();
    let report = runner.run(&mut hoods, &tree, &mut cache, RunMode::DryRun);

    assert_eq!(report.mode, "dry-run");
    assert_eq!(report.calls_attempted, 0, "a dry run calls nothing");
    assert!(report.errors.is_empty());
    assert!(!report.plan.is_empty(), "there is work to do");
    assert!(report.plan.calls >= 1);
    assert!(report.plan.estimated_input_tokens > 0);
    assert_eq!(
        report.plan.estimated_output_tokens,
        report.plan.len() as u64 * ESTIMATED_OUTPUT_TOKENS_PER_DISTRICT
    );
    assert!(report.plan.estimated_usd > 0.0);
    assert!(cache.is_empty(), "nothing was written");
    assert!(
        report.summary().contains("to describe"),
        "{}",
        report.summary()
    );
    // The quoted price is the configured price applied to the quoted tokens,
    // not a number from anywhere else.
    let expected = Price::GLM_FLASH_PROMO.cost(Usage {
        input_tokens: report.plan.estimated_input_tokens,
        output_tokens: report.plan.estimated_output_tokens,
    });
    assert!((report.plan.estimated_usd - expected).abs() < 1e-12);
}

#[test]
fn a_cold_start_will_not_spend_money_without_being_told_to() {
    let tree = tree_with(&[("src/a", 12), ("src/b", 12)]);
    let mut hoods = hoods_of(&tree);
    let endpoint = FakeEndpoint::start(vec![Reply::Body(200, "{}".to_owned())]);
    let runner = LlmRunner::with_transport(
        config_for(&format!("{}/v1", endpoint.base)),
        Arc::new(PlainHttpTransport),
    );
    let mut cache = ModelCache::default();
    let report = runner.run(
        &mut hoods,
        &tree,
        &mut cache,
        RunMode::Generate { confirmed: false },
    );
    assert!(report.needs_confirmation);
    assert_eq!(report.calls_attempted, 0);
    assert_eq!(endpoint.call_count(), 0, "nothing reached the endpoint");
    assert!(report.summary().contains("--yes"), "{}", report.summary());
}

// -- the whole pipeline, over a real socket ---------------------------------

#[test]
fn the_pipeline_describes_a_repository_end_to_end_over_http() {
    let tree = tree_with(&[("src/a", 12), ("src/b", 12), ("src/c", 12)]);
    let mut hoods = hoods_of(&tree);
    let body = answer_all(&hoods, "Ingests the nightly partner feed");
    let endpoint = FakeEndpoint::start(vec![Reply::Body(200, body)]);
    let runner = LlmRunner::with_transport(
        config_for(&format!("{}/v1", endpoint.base)),
        Arc::new(PlainHttpTransport),
    );
    let mut cache = ModelCache::default();
    let report = runner.run(
        &mut hoods,
        &tree,
        &mut cache,
        RunMode::Generate { confirmed: true },
    );

    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(report.described > 0, "{}", report.summary());
    assert_eq!(report.calls_failed, 0);
    assert_eq!(report.retries, 0);
    // The provider's own numbers, not an estimate.
    assert!(report.usage_reported);
    assert_eq!(
        report.usage.input_tokens,
        1200 * u64::from(report.calls_attempted)
    );
    assert!(report.actual_usd > 0.0);

    // The words are on the map, marked as the model's and marked fresh.
    let described: Vec<&Neighborhood> = hoods
        .all()
        .iter()
        .filter(|h| h.has_model_description())
        .collect();
    assert!(!described.is_empty());
    for hood in &described {
        assert_eq!(hood.freshness, Freshness::Fresh, "{}", hood.path.as_str());
        assert!(!hood.has_prose(), "a model is not a human author");
        assert!(hood.label().expect("a label").starts_with("Ingests"));
    }
    assert_eq!(
        hoods.stats().described_model,
        u32::try_from(described.len()).expect("small")
    );

    // What the endpoint actually received: names and documentation, and no file
    // contents, because there is no field that could carry any.
    let joined = endpoint.bodies().join("\n");
    assert!(joined.contains("FILE NAMES"), "{joined}");
    assert!(joined.contains("f000.ts"), "{joined}");
    assert!(joined.contains("RETURN null"), "the refusal rule is sent");
}

#[test]
fn a_second_run_calls_nothing_because_the_cache_is_fresh() {
    let tree = tree_with(&[("src/a", 12), ("src/b", 12)]);
    let mut hoods = hoods_of(&tree);
    let body = answer_all(&hoods, "Ingests the nightly partner feed");
    let endpoint = FakeEndpoint::start(vec![Reply::Body(200, body)]);
    let runner = LlmRunner::with_transport(
        config_for(&format!("{}/v1", endpoint.base)),
        Arc::new(PlainHttpTransport),
    );
    let mut cache = ModelCache::default();
    runner.run(
        &mut hoods,
        &tree,
        &mut cache,
        RunMode::Generate { confirmed: true },
    );
    let first = endpoint.call_count();
    assert!(first > 0);

    // Rebuild the partition from scratch, as a fresh launch would.
    let mut again = hoods_of(&tree);
    let second = runner.run(
        &mut again,
        &tree,
        &mut cache,
        RunMode::Generate { confirmed: true },
    );
    assert_eq!(endpoint.call_count(), first, "no new calls");
    assert!(second.plan.is_empty(), "{}", second.summary());
    assert!(second.plan.fresh > 0);
    assert!(again.all().iter().any(Neighborhood::has_model_description));
}

/// The rule the whole cache design rests on.
#[test]
fn rewriting_every_file_costs_nothing_and_a_new_district_costs_a_call() {
    let tree = tree_with(&[("src/a", 12), ("src/b", 12)]);
    let mut hoods = hoods_of(&tree);
    let body = answer_all(&hoods, "Ingests the nightly partner feed");
    let endpoint = FakeEndpoint::start(vec![Reply::Body(200, body)]);
    let runner = LlmRunner::with_transport(
        config_for(&format!("{}/v1", endpoint.base)),
        Arc::new(PlainHttpTransport),
    );
    let mut cache = ModelCache::default();
    runner.run(
        &mut hoods,
        &tree,
        &mut cache,
        RunMode::Generate { confirmed: true },
    );
    let after_first = endpoint.call_count();

    // Every byte of every file changes; not one name does.
    let mut edited = tree.clone();
    for meta in edited.files.values_mut() {
        meta.size_bytes = 999_999;
    }
    let mut hoods = hoods_of(&edited);
    assert!(
        runner.plan(&hoods, &edited, &cache).is_empty(),
        "a content edit is not a change of meaning"
    );
    runner.run(
        &mut hoods,
        &edited,
        &mut cache,
        RunMode::Generate { confirmed: true },
    );
    assert_eq!(endpoint.call_count(), after_first, "no call for an edit");

    // A wholly new district, on the other hand, has never been described.
    let grown = tree_with(&[("src/a", 12), ("src/b", 12), ("src/c", 12)]);
    let hoods = hoods_of(&grown);
    let plan = runner.plan(&hoods, &grown, &cache);
    assert!(!plan.is_empty(), "a new district is missing");
    assert!(plan
        .districts
        .iter()
        .any(|d| d.freshness == Freshness::Missing));
}

/// The operator's question: what happens when a district splits?
#[test]
fn a_split_district_is_replanned_and_its_children_are_missing() {
    let tree = tree_with(&[("src/services", 20), ("src/other", 20)]);
    let mut hoods = hoods_of(&tree);
    let body = answer_all(&hoods, "Ingests the nightly partner feed");
    let endpoint = FakeEndpoint::start(vec![Reply::Body(200, body)]);
    let runner = LlmRunner::with_transport(
        config_for(&format!("{}/v1", endpoint.base)),
        Arc::new(PlainHttpTransport),
    );
    let mut cache = ModelCache::default();
    runner.run(
        &mut hoods,
        &tree,
        &mut cache,
        RunMode::Generate { confirmed: true },
    );
    assert!(hoods
        .get(&lp("src/services"))
        .expect("services")
        .has_model_description());

    // The directory grows until the partition splits it.
    let mut grown = tree.clone();
    for (sub, count) in [("src/services/auth", 40), ("src/services/billing", 40)] {
        for i in 0..count {
            let path = lp(&format!("{sub}/g{i:03}.ts"));
            grown
                .files
                .insert(path.clone(), FileMeta::untracked(path, 100));
        }
    }
    let split = hoods_of(&grown);
    assert!(
        split.get(&lp("src/services/auth")).is_some(),
        "the partition split: {:?}",
        split
            .all()
            .iter()
            .map(|h| h.path.as_str())
            .collect::<Vec<_>>()
    );
    let plan = runner.plan(&split, &grown, &cache);
    let by_path: BTreeMap<&str, &PlannedDistrict> = plan
        .districts
        .iter()
        .map(|d| (d.brief.path.as_str(), d))
        .collect();
    // The parent is stale because it gave part of itself away...
    let parent = by_path
        .get("src/services")
        .expect("the parent is replanned");
    assert_eq!(
        parent.freshness.drift().expect("stale").cause,
        DriftCause::Split
    );
    // ...and the two new districts have no description at all.
    for child in ["src/services/auth", "src/services/billing"] {
        assert_eq!(
            by_path.get(child).expect(child).freshness,
            Freshness::Missing
        );
    }
}

#[test]
fn a_stale_description_is_shown_marked_rather_than_silently_as_current() {
    let tree = tree_with(&[("src/a", 20), ("src/b", 20)]);
    let mut hoods = hoods_of(&tree);
    let body = answer_all(&hoods, "Payment processing");
    let endpoint = FakeEndpoint::start(vec![Reply::Body(200, body)]);
    let config = config_for(&format!("{}/v1", endpoint.base));
    let runner = LlmRunner::with_transport(config.clone(), Arc::new(PlainHttpTransport));
    let mut cache = ModelCache::default();
    runner.run(
        &mut hoods,
        &tree,
        &mut cache,
        RunMode::Generate { confirmed: true },
    );
    assert!(hoods
        .get(&lp("src/a"))
        .expect("src/a")
        .has_model_description());

    // `src/a` quietly becomes something else: every name replaced.
    let mut moved = RepoTree::default();
    for path in tree.files.keys() {
        let path = if path.as_str().starts_with("src/a/") {
            lp(&path.as_str().replace("src/a/f", "src/a/notify"))
        } else {
            path.clone()
        };
        moved
            .files
            .insert(path.clone(), FileMeta::untracked(path, 100));
    }
    let mut after = hoods_of(&moved);
    after.apply_model_descriptions(&cache, &config, &moved);
    let hood = after.get(&lp("src/a")).expect("src/a");
    // The words are still shown — blanking them would hide the drift the
    // operator was promised they would see.
    assert!(hood.has_model_description(), "{hood:?}");
    assert!(hood.label().expect("a label").contains("Payment"));
    assert!(hood.is_stale(), "{:?}", hood.freshness);
    let drift = hood.freshness.drift().expect("stale");
    assert_eq!(drift.cause, DriftCause::Names);
    assert_eq!(drift.permille, 1000, "every name changed");
    // And a run would put it back on the list.
    assert!(runner
        .plan(&after, &moved, &cache)
        .districts
        .iter()
        .any(|d| d.brief.path == lp("src/a")));
}

// -- degradation ------------------------------------------------------------

/// Every one of these leaves a map that looks exactly like the one you get with
/// the feature switched off.
#[test]
fn every_failure_degrades_to_the_derived_description_and_never_further() {
    let tree = tree_with(&[("src/a", 12), ("src/b", 12)]);
    let cases: Vec<(&str, Arc<dyn Transport>)> = vec![
        (
            "dead port",
            Arc::new(DeadTransport(TransportError::Unreachable(
                "connection refused".to_owned(),
            ))),
        ),
        ("timeout", Arc::new(DeadTransport(TransportError::Timeout))),
        (
            "no curl",
            Arc::new(DeadTransport(TransportError::Unavailable(
                "curl is not on PATH".to_owned(),
            ))),
        ),
        (
            "unauthorised",
            Arc::new(CannedTransport::new(
                401,
                r#"{"error":{"message":"invalid api key"}}"#,
            )),
        ),
        (
            "rate limited",
            Arc::new(CannedTransport::new(
                429,
                r#"{"error":{"message":"too many requests"}}"#,
            )),
        ),
        (
            "server error",
            Arc::new(CannedTransport::new(500, "<html>bad gateway</html>")),
        ),
        (
            "malformed body",
            Arc::new(CannedTransport::new(200, "not json at all")),
        ),
        (
            "a refusal instead of JSON",
            Arc::new(CannedTransport::new(
                200,
                &serde_json::json!({
                    "choices": [{"message": {"content":
                        "I'm sorry, I can't help with that."}}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 9}
                })
                .to_string(),
            )),
        ),
    ];
    for (name, transport) in cases {
        let mut hoods = hoods_of(&tree);
        let before: Vec<Option<Description>> =
            hoods.all().iter().map(|h| h.description.clone()).collect();
        let runner = LlmRunner::with_transport(config_for("http://127.0.0.1:1/v1"), transport);
        let mut cache = ModelCache::default();
        let report = runner.run(
            &mut hoods,
            &tree,
            &mut cache,
            RunMode::Generate { confirmed: true },
        );
        assert!(report.calls_failed > 0, "{name}: {}", report.summary());
        assert_eq!(report.described, 0, "{name}");
        assert!(!report.errors.is_empty(), "{name}");
        assert!(report.degraded(), "{name}");
        assert!(cache.is_empty(), "{name}: nothing was cached");
        let after: Vec<Option<Description>> =
            hoods.all().iter().map(|h| h.description.clone()).collect();
        assert_eq!(before, after, "{name}: the map is unchanged");
        for hood in hoods.all() {
            assert_eq!(hood.freshness, Freshness::Missing, "{name}");
        }
    }
}

#[test]
fn with_no_key_the_run_stops_at_the_plan_and_says_why() {
    let tree = tree_with(&[("src/a", 12)]);
    let mut hoods = hoods_of(&tree);
    // A hosted provider whose key variable is guaranteed unset.
    let endpoint = FakeEndpoint::start(vec![Reply::Body(200, "{}".to_owned())]);
    let mut config = LlmConfig::default().with_provider(Provider::Glm);
    config.enabled = true;
    config.key_env = vec!["POLIS_TEST_KEY_THAT_IS_NEVER_SET".to_owned()];
    config.base_url = format!("{}/v1", endpoint.base);
    let runner = LlmRunner::with_transport(config, Arc::new(PlainHttpTransport));
    let mut cache = ModelCache::default();
    let report = runner.run(
        &mut hoods,
        &tree,
        &mut cache,
        RunMode::Generate { confirmed: true },
    );
    assert!(!report.plan.key_present);
    assert_eq!(
        report.plan.key_source, "POLIS_TEST_KEY_THAT_IS_NEVER_SET",
        "names only, never a value"
    );
    assert!(report.plan.blocked.is_some(), "{:?}", report.plan.blocked);
    assert_eq!(endpoint.call_count(), 0, "nothing was called");
    assert_eq!(report.calls_attempted, 0);
    assert!(report.errors.iter().any(|e| e.contains("no API key")));
    // The plan is still complete, so a dry run is useful with no key.
    assert!(!report.plan.is_empty());
    assert!(report.plan.estimated_usd > 0.0);
}

#[test]
fn the_feature_being_off_is_reported_rather_than_ignored() {
    let tree = tree_with(&[("src/a", 12)]);
    let mut hoods = hoods_of(&tree);
    let mut config = config_for("http://127.0.0.1:1/v1");
    config.enabled = false;
    let runner =
        LlmRunner::with_transport(config, Arc::new(DeadTransport(TransportError::Timeout)));
    let mut cache = ModelCache::default();
    let report = runner.run(
        &mut hoods,
        &tree,
        &mut cache,
        RunMode::Generate { confirmed: true },
    );
    assert_eq!(report.calls_attempted, 0);
    assert!(report
        .plan
        .blocked
        .as_deref()
        .unwrap_or("")
        .contains("disabled"));
}

#[test]
fn a_retryable_failure_is_retried_a_bounded_number_of_times() {
    let tree = tree_with(&[("src/a", 12)]);
    let mut hoods = hoods_of(&tree);
    let mut config = config_for("http://127.0.0.1:1/v1");
    config.max_retries = 2;
    let transport = Arc::new(CannedTransport::new(
        503,
        r#"{"error":{"message":"unavailable"}}"#,
    ));
    let runner = LlmRunner::with_transport(config, Arc::clone(&transport) as Arc<dyn Transport>);
    let mut cache = ModelCache::default();
    let report = runner.run(
        &mut hoods,
        &tree,
        &mut cache,
        RunMode::Generate { confirmed: true },
    );
    assert_eq!(report.calls_attempted, 3, "one attempt plus two retries");
    assert_eq!(report.retries, 2);
    assert_eq!(transport.call_count(), 3, "the transport saw exactly three");
}

#[test]
fn an_unauthorised_failure_is_not_retried_because_it_will_not_change() {
    let tree = tree_with(&[("src/a", 12)]);
    let mut hoods = hoods_of(&tree);
    let transport = Arc::new(CannedTransport::new(
        401,
        r#"{"error":{"message":"invalid api key"}}"#,
    ));
    let runner = LlmRunner::with_transport(
        config_for("http://127.0.0.1:1/v1"),
        Arc::clone(&transport) as Arc<dyn Transport>,
    );
    let mut cache = ModelCache::default();
    let report = runner.run(
        &mut hoods,
        &tree,
        &mut cache,
        RunMode::Generate { confirmed: true },
    );
    assert_eq!(report.calls_attempted, 1);
    assert_eq!(report.retries, 0);
    assert_eq!(transport.call_count(), 1);
}

/// A key must not reach a log line even when the failure text came from a
/// subprocess that echoed it.
#[test]
fn a_key_never_reaches_the_report_even_through_an_error_message() {
    std::env::set_var("POLIS_TEST_RUN_KEY", "sk-run-abcdef123456");
    let tree = tree_with(&[("src/a", 12)]);
    let mut hoods = hoods_of(&tree);
    let mut config = LlmConfig::default().with_provider(Provider::Glm);
    config.enabled = true;
    config.retry_base_ms = 0;
    config.key_env = vec!["POLIS_TEST_RUN_KEY".to_owned()];
    let runner = LlmRunner::with_transport(
        config,
        // The shape a `curl` stderr takes when it echoes the request.
        Arc::new(DeadTransport(TransportError::Unreachable(
            "curl exit 7: failed to connect using Bearer sk-run-abcdef123456".to_owned(),
        ))),
    );
    std::env::remove_var("POLIS_TEST_RUN_KEY");
    let mut cache = ModelCache::default();
    let report = runner.run(
        &mut hoods,
        &tree,
        &mut cache,
        RunMode::Generate { confirmed: true },
    );
    let printed = report.summary();
    assert!(!printed.contains("sk-run-abcdef"), "{printed}");
    assert!(printed.contains("<redacted>"), "{printed}");
    assert!(!format!("{report:?}").contains("sk-run-abcdef"));
}

// -- outbound redaction on the real path ------------------------------------

#[test]
fn a_credential_shaped_file_name_never_reaches_the_endpoint() {
    let mut tree = tree_with(&[("src/a", 12)]);
    for name in [
        "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8",
        "AKIAIOSFODNN7EXAMPLE",
    ] {
        let path = lp(&format!("src/a/{name}"));
        tree.files
            .insert(path.clone(), FileMeta::untracked(path, 10));
    }
    let hoods = hoods_of(&tree);
    let runner = LlmRunner::with_transport(
        config_for("http://127.0.0.1:1/v1"),
        Arc::new(DeadTransport(TransportError::Timeout)),
    );
    let plan = runner.plan(&hoods, &tree, &ModelCache::default());
    assert_eq!(plan.redaction.names_dropped, 2, "{:?}", plan.redaction);
    let sent: String = plan
        .districts
        .iter()
        .map(|d| d.brief.render())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!sent.contains("ghp_"), "{sent}");
    assert!(!sent.contains("AKIA"), "{sent}");
    assert!(sent.contains("f000.ts"), "the rest still goes");
}

// -- the read path ----------------------------------------------------------

#[test]
fn applying_the_cache_calls_nothing_and_is_safe_on_the_launch_path() {
    let tree = tree_with(&[("src/a", 12), ("src/b", 12)]);
    let mut hoods = hoods_of(&tree);
    let config = LlmConfig {
        model: "test-model".to_owned(),
        ..LlmConfig::default()
    };
    let names = district_names(&hoods, &tree);
    let mut cache = ModelCache::default();
    for hood in hoods.all() {
        cache.put(crate::llm::cache::CachedModelDescription {
            path: hood.path.clone(),
            sketch: Sketch::build(names.get(&hood.path).cloned().unwrap_or_default()),
            children: hood.children.clone(),
            description: Some(model_description(
                "Ingests the nightly partner feed",
                "Ingests the nightly partner feed and reconciles it.",
            )),
            model: "test-model".to_owned(),
            prompt_version: PROMPT_VERSION,
        });
    }
    hoods.apply_model_descriptions(&cache, &config, &tree);
    let described = hoods
        .all()
        .iter()
        .filter(|h| h.has_model_description())
        .count();
    assert!(described > 0);
    assert_eq!(hoods.stats().described_model as usize, described);
    for hood in hoods.all().iter().filter(|h| h.has_model_description()) {
        assert_eq!(hood.freshness, Freshness::Fresh);
    }
    // An empty cache changes nothing at all.
    let mut untouched = hoods_of(&tree);
    let before = format!("{:?}", untouched.all());
    untouched.apply_model_descriptions(&ModelCache::default(), &config, &tree);
    assert_eq!(before, format!("{:?}", untouched.all()));
}

/// PRD §7.4: descriptions are text hung on a district, never an input to the
/// partition.
#[test]
fn a_model_description_cannot_move_the_partition() {
    let tree = tree_with(&[("src/a", 12), ("src/b", 12), ("node_modules/x", 40)]);
    let before = hoods_of(&tree);
    let shape_before: Vec<(String, u32, String)> = before
        .all()
        .iter()
        .map(|h| {
            (
                h.path.as_str().to_owned(),
                h.file_count,
                h.kind.name().to_owned(),
            )
        })
        .collect();

    let mut hoods = hoods_of(&tree);
    let body = answer_all(&hoods, "Ingests the nightly partner feed");
    let endpoint = FakeEndpoint::start(vec![Reply::Body(200, body)]);
    let runner = LlmRunner::with_transport(
        config_for(&format!("{}/v1", endpoint.base)),
        Arc::new(PlainHttpTransport),
    );
    let mut cache = ModelCache::default();
    runner.run(
        &mut hoods,
        &tree,
        &mut cache,
        RunMode::Generate { confirmed: true },
    );
    let shape_after: Vec<(String, u32, String)> = hoods
        .all()
        .iter()
        .map(|h| {
            (
                h.path.as_str().to_owned(),
                h.file_count,
                h.kind.name().to_owned(),
            )
        })
        .collect();
    assert_eq!(shape_before, shape_after, "geometry is untouched");
}

#[test]
fn a_background_run_writes_the_cache_and_never_blocks_the_caller() {
    let tree = tree_with(&[("src/a", 12), ("src/b", 12)]);
    let hoods = hoods_of(&tree);
    let body = answer_all(&hoods, "Ingests the nightly partner feed");
    let endpoint = FakeEndpoint::start(vec![Reply::Body(200, body)]);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("llm.json");
    let runner = LlmRunner::with_transport(
        config_for(&format!("{}/v1", endpoint.base)),
        Arc::new(PlainHttpTransport),
    );
    let handle = runner.spawn(
        hoods,
        tree.clone(),
        ModelCache::default(),
        RunMode::Generate { confirmed: true },
        Some(path.clone()),
    );
    let report = handle.join().expect("the background run");
    assert!(report.described > 0, "{}", report.summary());
    assert!(path.exists(), "the cache was written");
    assert!(!ModelCache::read(&path).is_empty());
}

// -- where a key is allowed to go -------------------------------------------

/// A key present in the environment is withheld when the endpoint would receive
/// it in the clear, and the run is blocked rather than made without it: sending
/// the district listing to a stranger is not a consolation prize.
#[test]
fn a_key_is_withheld_from_a_cleartext_endpoint_and_nothing_is_called() {
    std::env::set_var("POLIS_TEST_CLEARTEXT_KEY", "not-a-real-key-000000");
    let tree = tree_with(&[("src/a", 12)]);
    let mut hoods = hoods_of(&tree);
    let mut config = config_for("http://collector.example/v1");
    config.key_env = vec!["POLIS_TEST_CLEARTEXT_KEY".to_owned()];
    let transport = Arc::new(CannedTransport::new(200, "{}"));
    let runner = LlmRunner::with_transport(config, Arc::clone(&transport) as Arc<dyn Transport>);
    std::env::remove_var("POLIS_TEST_CLEARTEXT_KEY");

    assert!(
        matches!(runner.key_withheld(), Some(KeyWithheld::Cleartext { host }) if host == "collector.example"),
        "{:?}",
        runner.key_withheld()
    );
    let mut cache = ModelCache::default();
    let report = runner.run(
        &mut hoods,
        &tree,
        &mut cache,
        RunMode::Generate { confirmed: true },
    );
    assert_eq!(transport.call_count(), 0, "nothing was sent anywhere");
    let printed = report.summary();
    assert!(printed.contains("collector.example"), "{printed}");
    assert!(!printed.contains("not-a-real-key"), "{printed}");
    // The derived layer is untouched, which is the whole degradation contract.
    assert!(hoods.all().iter().all(|h| !h.has_model_description()));
}

/// The same guard, for the case TLS cannot help with: a `.polis/llm.json` that
/// arrived with a clone naming a host the operator never chose.
#[test]
fn a_key_is_withheld_when_the_repository_chose_the_host() {
    std::env::set_var("POLIS_TEST_REDIRECT_KEY", "not-a-real-key-111111");
    let tree = tree_with(&[("src/a", 12)]);
    let mut hoods = hoods_of(&tree);
    let mut config = LlmConfig::default().with_provider(Provider::Glm);
    config.enabled = true;
    config.retry_base_ms = 0;
    config.key_env = vec!["POLIS_TEST_REDIRECT_KEY".to_owned()];
    config.base_url = "https://collector.example/v1".to_owned();
    config.origin = crate::llm::ConfigOrigin::Repository;
    let transport = Arc::new(CannedTransport::new(200, "{}"));
    let runner = LlmRunner::with_transport(config, Arc::clone(&transport) as Arc<dyn Transport>);
    std::env::remove_var("POLIS_TEST_REDIRECT_KEY");

    assert!(
        matches!(
            runner.key_withheld(),
            Some(KeyWithheld::RepoRedirect { .. })
        ),
        "{:?}",
        runner.key_withheld()
    );
    let mut cache = ModelCache::default();
    let report = runner.run(
        &mut hoods,
        &tree,
        &mut cache,
        RunMode::Generate { confirmed: true },
    );
    assert_eq!(transport.call_count(), 0);
    assert!(!report.summary().contains("not-a-real-key"));
}
