//! What *kind* of code a file is, and what kind a neighborhood is (PRD §8).
//!
//! [`crate::FileClass`] answers PRD §8's wayfinding question — is this file a
//! monument, an industrial mass, the civic square — and it is deliberately
//! sparse: almost every file is [`crate::FileClass::Ordinary`]. That is the
//! right answer for landmarks and the wrong one for colour. A map whose
//! districts are tinted apart purely to *tell them apart* spends its only free
//! channel, hue, on identity; the same ink spent on **what the code is** buys a
//! reading — this quarter is tests, that one is documentation, that dull slab is
//! somebody else's package.
//!
//! This module is that second axis. [`CodeKind`] is a small, closed set chosen
//! so a person can hold it in their head, and [`KindMix`] keeps the *proportions*
//! as well as the winner, because "60 % test, 40 % source" is a real and
//! interesting shape that a single label throws away.
//!
//! # The industrial rule is generalised, not duplicated
//!
//! PRD §8 already names one kind:
//!
//! > **Industrial zone** — `node_modules`, vendored, generated, `target/` —
//! > large, uniform, deliberately dull […] the eye should slide off them.
//!
//! [`CodeKind::Vendored`] *is* that zone. It is decided by the existing
//! [`crate::tree::IndustrialRules`], not by a second list that would drift out
//! of step with it, and it wins over every other rule for the same reason
//! [`crate::tree::classify_with`] checks it first: `node_modules` contains ten
//! thousand files called `index.js`, and a classifier that looked at names
//! before trees would report a repository made mostly of other people's code.
//!
//! # Determinism
//!
//! Classification is a pure function of `(path, rules)` — plus, for
//! [`kind_of_with_content`], the first few hundred bytes of the file. No wall
//! clock, no ambient state, no map iteration reaches a result. Every lookup
//! table is a `BTreeMap` keyed on an ASCII-lowercased string, matching
//! [`polis_events::LogicalPath`]'s own case folding (ADR-0028).
//!
//! # Configurable, because a shipped list is wrong somewhere
//!
//! Every project has its own conventions. Django ships a `django/test/` package
//! that is production source; a Java repository puts its tests in `src/test/`;
//! somebody's `generated/` is hand-written. [`KindRulesConfig`] is a JSON
//! overlay that adds to, or removes from, the shipped defaults, and
//! [`KindRules::load`] treats a malformed file as a warning and a fallback —
//! never a failure — the same way `polis_app::Config` does.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::OnceLock;

use polis_events::LogicalPath;
use serde::{Deserialize, Serialize};

use crate::tree::IndustrialRules;

// ---------------------------------------------------------------------------
// The kinds
// ---------------------------------------------------------------------------

/// What kind of code a file is — the second colour axis, orthogonal to
/// [`crate::FileClass`].
///
/// Nine variants, and the count is the point: this is read at a glance from
/// across the room (PRD §1), so the legend has to fit in one line of a person's
/// memory. Anything finer belongs in the drill-down panel.
///
/// The declaration order is the tie-break order for [`KindMix::dominant`] and
/// the render order for [`KindMix::ranked`]. It runs from "the thing the
/// repository is" to "the thing the repository merely contains".
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum CodeKind {
    /// Hand-written program text: the thing the repository is for.
    #[default]
    Source,
    /// Tests, benchmarks and the fixtures wired directly into them.
    Test,
    /// Prose meant to be read by a person: Markdown, reStructuredText, the
    /// licence, the changelog.
    Docs,
    /// Declarative settings and dependency manifests: `Cargo.toml`,
    /// `package.json`, `tsconfig.json`, dotfiles, lock files.
    Config,
    /// How the thing is built, containerised, released or run in CI.
    Build,
    /// Images, fonts, video, PDFs — bytes shipped alongside the code.
    Assets,
    /// Data the program reads: CSV, SQL, locale catalogues, recorded fixtures.
    Data,
    /// PRD §8's industrial zone: `node_modules`, vendored trees, `target/`, and
    /// anything a tool wrote. **Not a judgement about quality** — it is a
    /// statement that nobody in this repository edits it.
    Vendored,
    /// No rule matched. Reported honestly rather than folded into
    /// [`CodeKind::Source`], because "12 % of this district is unclassified" is
    /// a fact about the rules that a silent default would hide.
    Unknown,
}

impl CodeKind {
    /// Every kind, in declaration order. Also the tie-break order.
    pub const ALL: [Self; 9] = [
        Self::Source,
        Self::Test,
        Self::Docs,
        Self::Config,
        Self::Build,
        Self::Assets,
        Self::Data,
        Self::Vendored,
        Self::Unknown,
    ];

    /// How many kinds there are. The width of [`KindMix`]'s counters.
    pub const COUNT: usize = Self::ALL.len();

    /// A stable machine name: the config-file spelling and the log spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Test => "test",
            Self::Docs => "docs",
            Self::Config => "config",
            Self::Build => "build",
            Self::Assets => "assets",
            Self::Data => "data",
            Self::Vendored => "vendored",
            Self::Unknown => "unknown",
        }
    }

    /// The word a person reads on the map legend.
    pub fn label(self) -> &'static str {
        match self {
            Self::Source => "Source",
            Self::Test => "Tests",
            Self::Docs => "Docs",
            Self::Config => "Config",
            Self::Build => "Build & CI",
            Self::Assets => "Assets",
            Self::Data => "Data",
            Self::Vendored => "Vendored",
            Self::Unknown => "Unclassified",
        }
    }

    /// Parses [`CodeKind::name`]. Case-insensitive; `None` for anything else.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|k| k.name().eq_ignore_ascii_case(name))
    }

    /// Its index into a [`KindMix`] counter array.
    pub fn index(self) -> usize {
        self as usize
    }

    /// True for PRD §8's industrial zone — the one kind rendered as a single
    /// mass rather than as buildings.
    ///
    /// The bridge back to [`crate::FileClass::Industrial`]: the two agree by
    /// construction, because both are decided by
    /// [`IndustrialRules::is_industrial_file`].
    pub fn is_industrial(self) -> bool {
        self == Self::Vendored
    }

    /// True for a kind whose files nobody in this repository writes by hand.
    ///
    /// [`CodeKind::Vendored`] only. Used to hold vendored trees out of the
    /// neighborhood sizing budget (see [`crate::neighborhoods`]): a repository
    /// that is 98 % `node_modules` should not have its own source flattened into
    /// one district because the dependency tree set the scale.
    pub fn is_foreign(self) -> bool {
        self == Self::Vendored
    }
}

impl std::fmt::Display for CodeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

// ---------------------------------------------------------------------------
// The mix
// ---------------------------------------------------------------------------

/// The kinds of every file in a neighborhood, with their proportions.
///
/// A district's kind is the dominant kind of its files — but the dominant kind
/// alone throws away the interesting half. A quarter that is 60 % test and 40 %
/// source is a different place from one that is 100 % test, and the renderer may
/// want to say so (a second stripe, a hatch, a tooltip). So the counts are kept.
///
/// Counters are a fixed-width array indexed by [`CodeKind::index`], not a map:
/// there is no iteration order to get wrong, and the whole struct is 116 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KindMix {
    /// Files of each kind, indexed by [`CodeKind::index`].
    files: [u32; CodeKind::COUNT],
    /// Bytes of each kind, indexed by [`CodeKind::index`].
    bytes: [u64; CodeKind::COUNT],
}

impl KindMix {
    /// An empty mix.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one file.
    pub fn push(&mut self, kind: CodeKind, size_bytes: u64) {
        let i = kind.index();
        self.files[i] = self.files[i].saturating_add(1);
        self.bytes[i] = self.bytes[i].saturating_add(size_bytes);
    }

    /// Adds another mix into this one.
    pub fn merge(&mut self, other: &Self) {
        for i in 0..CodeKind::COUNT {
            self.files[i] = self.files[i].saturating_add(other.files[i]);
            self.bytes[i] = self.bytes[i].saturating_add(other.bytes[i]);
        }
    }

    /// This mix with `other`'s counts taken out of it, saturating at zero.
    ///
    /// What a district would look like if one of its children were drawn as its
    /// own neighborhood. `polis_repo::neighborhoods` asks exactly that question:
    /// a `contrib/admin` whose dominant kind is `data` only because of the
    /// translation catalogues underneath it becomes `source` the moment they are
    /// taken out, and *that* is the signal that they should be.
    #[must_use]
    pub fn without(&self, other: &Self) -> Self {
        let mut out = *self;
        for i in 0..CodeKind::COUNT {
            out.files[i] = out.files[i].saturating_sub(other.files[i]);
            out.bytes[i] = out.bytes[i].saturating_sub(other.bytes[i]);
        }
        out
    }

    /// Files of one kind.
    pub fn count(&self, kind: CodeKind) -> u32 {
        self.files[kind.index()]
    }

    /// Bytes of one kind.
    pub fn byte_count(&self, kind: CodeKind) -> u64 {
        self.bytes[kind.index()]
    }

    /// Files of every kind.
    pub fn total(&self) -> u32 {
        self.files.iter().fold(0u32, |a, b| a.saturating_add(*b))
    }

    /// Bytes of every kind.
    pub fn total_bytes(&self) -> u64 {
        self.bytes.iter().fold(0u64, |a, b| a.saturating_add(*b))
    }

    /// True when nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// One kind's share of the files, in `0.0..=1.0`. Zero for an empty mix.
    pub fn share(&self, kind: CodeKind) -> f32 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        f64_share(self.count(kind), total)
    }

    /// The kind with the most files.
    ///
    /// Ties break toward the earlier variant in [`CodeKind::ALL`], which is a
    /// fixed order, so two runs and two machines agree. An empty mix is
    /// [`CodeKind::Unknown`] — there is nothing there to be a kind *of*.
    pub fn dominant(&self) -> CodeKind {
        if self.is_empty() {
            return CodeKind::Unknown;
        }
        let mut best = CodeKind::Unknown;
        let mut best_count = 0;
        for kind in CodeKind::ALL {
            let count = self.count(kind);
            if count > best_count {
                best = kind;
                best_count = count;
            }
        }
        best
    }

    /// The second most common kind, when it holds at least `floor` of the files.
    ///
    /// This is what makes "60 % test, 40 % source" sayable in one line. `None`
    /// when the district is of one kind, or when the runner-up is a rounding
    /// error.
    pub fn secondary(&self, floor: f32) -> Option<CodeKind> {
        let ranked = self.ranked();
        let (kind, _) = *ranked.get(1)?;
        (self.share(kind) >= floor).then_some(kind)
    }

    /// Every kind that has at least one file, most files first.
    ///
    /// Ties break on [`CodeKind::ALL`] order, so the vector is a deterministic
    /// function of the counts.
    pub fn ranked(&self) -> Vec<(CodeKind, u32)> {
        let mut out: Vec<(CodeKind, u32)> = CodeKind::ALL
            .into_iter()
            .map(|k| (k, self.count(k)))
            .filter(|(_, n)| *n > 0)
            .collect();
        // Stable sort on the descending count leaves ties in `ALL` order.
        out.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        out
    }

    /// A one-line composition, most common first: `"62% source, 30% test"`.
    ///
    /// At most `max_parts` entries, and nothing below `floor`. Empty string for
    /// an empty mix. Percentages are rounded, so they need not sum to 100 — the
    /// alternative is a largest-remainder apportionment nobody reading a map
    /// will thank you for.
    pub fn summary(&self, max_parts: usize, floor: f32) -> String {
        let mut parts: Vec<String> = Vec::new();
        for (kind, _) in self.ranked() {
            if parts.len() >= max_parts {
                break;
            }
            let share = self.share(kind);
            if share < floor {
                continue;
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            // `share` is in `0.0..=1.0`, so the product is in `0..=100`.
            let pct = (share * 100.0).round() as u32;
            parts.push(format!("{pct}% {}", kind.name()));
        }
        parts.join(", ")
    }
}

/// `count / total` as an `f32`, computed in `f64` so the division is exact for
/// every count a repository can hold.
#[allow(clippy::cast_possible_truncation)] // a ratio in `0.0..=1.0`, displayed
fn f64_share(count: u32, total: u32) -> f32 {
    (f64::from(count) / f64::from(total)) as f32
}

// ---------------------------------------------------------------------------
// The rules
// ---------------------------------------------------------------------------

/// The lookup tables [`kind_of_with`] consults, in the order it consults them.
///
/// Six passes, cheapest and most specific first. The order is the whole design;
/// the tables are just data.
///
/// 1. **Vendored** — [`IndustrialRules`], the existing PRD §8 rule. Wins over
///    everything, because a name means nothing inside somebody else's package.
/// 2. **Test** — a test directory anywhere on the path, or a test-shaped file
///    name.
/// 3. **CI** — a build-system directory anywhere on the path. Before the
///    extension table, so `.github/workflows/ci.yml` is Build and not Config.
/// 4. **Exact file name**, then **stem** (the name minus its final extension,
///    so one entry covers `vite.config.ts`, `.js` and `.mjs`).
/// 5. **A code extension** — `.rs`, `.py`, `.tsx`. Before the directory hints,
///    so `docs/conf.py` stays [`CodeKind::Source`] instead of becoming prose.
/// 6. **A directory hint** — `docs/`, `assets/`, `locale/` — and only then the
///    remaining extensions, so `data/cities.json` is Data and
///    `src/settings.json` is Config.
///
/// Anything that survives all six is [`CodeKind::Unknown`], which is reported
/// rather than hidden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindRules {
    /// PRD §8's industrial zone. Shared with [`crate::tree::classify_with`].
    industrial: IndustrialRules,
    /// Directory names that make everything under them a test.
    test_dirs: BTreeSet<String>,
    /// Exact file names that are tests: `conftest.py`.
    test_names: BTreeSet<String>,
    /// Stem prefixes: `test_` catches `test_views.py`.
    test_prefixes: Vec<String>,
    /// Stem suffixes: `_test`, `.test`, `.spec`, `_spec`. Matched on the
    /// lowercased stem, so they must carry their own delimiter.
    test_suffixes: Vec<String>,
    /// Stem suffixes matched **case-sensitively**: `Test`, `Spec`. See
    /// [`TEST_CAMEL_SUFFIXES`].
    test_camel_suffixes: Vec<String>,
    /// Directory names that make everything under them build or CI plumbing.
    ci_dirs: BTreeSet<String>,
    /// Exact file names.
    names: BTreeMap<String, CodeKind>,
    /// File name minus its final extension.
    stems: BTreeMap<String, CodeKind>,
    /// Extensions that are program text, checked before the directory hints.
    code_extensions: BTreeSet<String>,
    /// Every other extension.
    extensions: BTreeMap<String, CodeKind>,
    /// Weak directory hints, checked after the code extensions.
    dirs: BTreeMap<String, CodeKind>,
    /// Byte markers that mean "a tool wrote this", matched against the head of
    /// the file by [`kind_of_with_content`].
    generated_markers: Vec<String>,
}

/// Directory names that make everything below them a test.
///
/// `bench`/`benches` are here on purpose: a benchmark is verification code, not
/// shipped source, and giving it its own kind would spend a legend slot on
/// something most repositories do not have.
///
/// **Known false positive, and it is deliberate.** Django ships `django/test/`
/// — the test *framework*, which is production source — and this rule calls it
/// Test. The alternative rules all cost more than they buy, and the honest
/// answer is [`KindRulesConfig::remove_test_dirs`]: a project that names a
/// shipped package `test` says so once, in its own config file.
const TEST_DIRS: &[&str] = &[
    "test",
    "tests",
    "__tests__",
    "__test__",
    "spec",
    "specs",
    "e2e",
    "integration-tests",
    "testing",
    "bench",
    "benches",
    "benchmarks",
];

/// Exact file names that are test infrastructure.
const TEST_NAMES: &[&str] = &[
    "conftest.py",
    "jest.setup.js",
    "jest.setup.ts",
    "setup-tests.ts",
];

/// Stem prefixes that mean a test.
const TEST_PREFIXES: &[&str] = &["test_"];

/// Stem suffixes that mean a test, matched on the **lowercased** stem.
///
/// Every one of them carries its own delimiter, and that is the point: a bare
/// `test` suffix would call `src/latest.ts` a test, which it is not.
const TEST_SUFFIXES: &[&str] = &[
    ".test", ".spec", ".tests", "_test", "_spec", "_tests", "-test", "-spec", "-tests",
];

/// Stem suffixes matched **case-sensitively**, against the stem as written.
///
/// This is how `AuthTest.java` and `AuthSpec.scala` are found without `latest`
/// and `protest` coming with them: the capital is the delimiter in a camel-case
/// name, and folding the name to lower case throws it away. The one rule in this
/// module that is not case-insensitive, and it is case-*sensitivity* that makes
/// it correct.
const TEST_CAMEL_SUFFIXES: &[&str] = &["Test", "Tests", "Spec", "Specs", "TestCase"];

/// Directory names that make everything below them build or CI plumbing.
const CI_DIRS: &[&str] = &[
    ".github",
    ".gitlab",
    ".circleci",
    ".buildkite",
    ".azure-pipelines",
    ".azuredevops",
    ".husky",
    ".devcontainer",
];

/// Exact file names, checked before stems and extensions.
const NAMES: &[(&str, CodeKind)] = &[
    // Build recipes and containers.
    ("makefile", CodeKind::Build),
    ("gnumakefile", CodeKind::Build),
    ("dockerfile", CodeKind::Build),
    ("containerfile", CodeKind::Build),
    ("jenkinsfile", CodeKind::Build),
    ("rakefile", CodeKind::Build),
    ("vagrantfile", CodeKind::Build),
    ("justfile", CodeKind::Build),
    ("procfile", CodeKind::Build),
    ("earthfile", CodeKind::Build),
    ("cmakelists.txt", CodeKind::Build),
    ("meson.build", CodeKind::Build),
    ("build", CodeKind::Build),
    ("build.bazel", CodeKind::Build),
    ("workspace", CodeKind::Build),
    ("build.rs", CodeKind::Build),
    ("setup.py", CodeKind::Build),
    ("noxfile.py", CodeKind::Build),
    ("manage.py", CodeKind::Source),
    (".dockerignore", CodeKind::Build),
    // Dependency manifests and lock files.
    ("package.json", CodeKind::Config),
    ("package-lock.json", CodeKind::Config),
    ("cargo.toml", CodeKind::Config),
    ("cargo.lock", CodeKind::Config),
    ("pyproject.toml", CodeKind::Config),
    ("requirements.txt", CodeKind::Config),
    ("constraints.txt", CodeKind::Config),
    ("pipfile", CodeKind::Config),
    ("gemfile", CodeKind::Config),
    ("go.mod", CodeKind::Config),
    ("go.sum", CodeKind::Config),
    ("composer.json", CodeKind::Config),
    ("tsconfig.json", CodeKind::Config),
    ("jsconfig.json", CodeKind::Config),
    ("components.json", CodeKind::Config),
    ("tox.ini", CodeKind::Build),
    ("setup.cfg", CodeKind::Config),
];

/// File stems — the name with its final extension removed — checked after
/// [`NAMES`]. One entry covers every extension a config file is written in.
const STEMS: &[(&str, CodeKind)] = &[
    // A dotfile family: `.env`, `.env.local`, `.env.production`.
    (".env", CodeKind::Config),
    // Orientation documents, whatever they are written in.
    ("readme", CodeKind::Docs),
    ("license", CodeKind::Docs),
    ("licence", CodeKind::Docs),
    ("copying", CodeKind::Docs),
    ("notice", CodeKind::Docs),
    ("authors", CodeKind::Docs),
    ("contributors", CodeKind::Docs),
    ("changelog", CodeKind::Docs),
    ("changes", CodeKind::Docs),
    ("history", CodeKind::Docs),
    ("contributing", CodeKind::Docs),
    ("code_of_conduct", CodeKind::Docs),
    ("security", CodeKind::Docs),
    ("governance", CodeKind::Docs),
    ("roadmap", CodeKind::Docs),
    ("todo", CodeKind::Docs),
    ("claude", CodeKind::Docs),
    ("agents", CodeKind::Docs),
    // Bundlers and build tooling.
    ("vite.config", CodeKind::Build),
    ("vitest.config", CodeKind::Build),
    ("webpack.config", CodeKind::Build),
    ("rollup.config", CodeKind::Build),
    ("esbuild.config", CodeKind::Build),
    ("tsup.config", CodeKind::Build),
    ("babel.config", CodeKind::Build),
    ("next.config", CodeKind::Build),
    ("nuxt.config", CodeKind::Build),
    ("svelte.config", CodeKind::Build),
    ("metro.config", CodeKind::Build),
    ("gulpfile", CodeKind::Build),
    ("gruntfile", CodeKind::Build),
    ("docker-compose", CodeKind::Build),
    ("compose", CodeKind::Build),
    ("apphosting", CodeKind::Build),
    // Linting, formatting and framework settings.
    ("eslint.config", CodeKind::Config),
    (".eslintrc", CodeKind::Config),
    (".prettierrc", CodeKind::Config),
    (".stylelintrc", CodeKind::Config),
    (".babelrc", CodeKind::Config),
    ("tailwind.config", CodeKind::Config),
    ("postcss.config", CodeKind::Config),
    ("commitlint.config", CodeKind::Config),
    ("lint-staged.config", CodeKind::Config),
    ("firebase", CodeKind::Config),
    ("firestore.rules", CodeKind::Config),
    ("firestore.indexes", CodeKind::Config),
    // Test-runner configuration. Kind Test rather than Config: it is part of the
    // test estate and reads better grouped with it.
    ("jest.config", CodeKind::Test),
    ("playwright.config", CodeKind::Test),
    ("cypress.config", CodeKind::Test),
    ("karma.conf", CodeKind::Test),
    ("pytest.ini", CodeKind::Test),
];

/// Extensions that are program text.
///
/// Checked **before** the directory hints, which is what keeps `docs/conf.py`
/// from being classified as prose because of the folder it sits in. Stylesheets
/// and templates are here rather than under [`CodeKind::Assets`]: somebody wrote
/// them by hand and somebody edits them.
const CODE_EXTENSIONS: &[&str] = &[
    "rs", "py", "pyi", "pyx", "pxd", "js", "jsx", "mjs", "cjs", "ts", "mts", "cts", "tsx", "go",
    "java", "kt", "kts", "scala", "clj", "cljs", "cljc", "c", "h", "cc", "cpp", "cxx", "hpp", "hh",
    "hxx", "cs", "rb", "rake", "php", "swift", "m", "mm", "lua", "sh", "bash", "zsh", "fish",
    "ps1", "psm1", "bat", "cmd", "pl", "pm", "r", "dart", "ex", "exs", "erl", "hrl", "hs", "ml",
    "mli", "fs", "fsx", "nim", "zig", "v", "vue", "svelte", "astro", "elm", "jl", "groovy", "sol",
    "proto", "graphql", "gql", "css", "scss", "sass", "less", "styl", "html", "htm", "hbs", "ejs",
    "jinja", "jinja2", "j2", "twig", "liquid", "mustache", "erb", "haml", "slim", "njk", "ipynb",
    "asm", "wgsl", "glsl", "hlsl", "metal", "vert", "frag", "comp", "vim", "vimrc", "el", "scm",
    "rkt", "awk", "sed", "tcl", "vb", "pas", "d", "cr", "coffee", "res", "purs", "gleam", "odin",
];

/// Every other extension.
const EXTENSIONS: &[(&str, CodeKind)] = &[
    // Prose.
    ("md", CodeKind::Docs),
    ("mdx", CodeKind::Docs),
    ("markdown", CodeKind::Docs),
    ("rst", CodeKind::Docs),
    ("txt", CodeKind::Docs),
    ("adoc", CodeKind::Docs),
    ("asciidoc", CodeKind::Docs),
    ("org", CodeKind::Docs),
    ("tex", CodeKind::Docs),
    ("rtf", CodeKind::Docs),
    ("tutor", CodeKind::Docs),
    ("mdc", CodeKind::Docs),
    ("1", CodeKind::Docs),
    ("po", CodeKind::Data),
    ("pot", CodeKind::Data),
    ("mo", CodeKind::Data),
    // Declarative settings.
    ("toml", CodeKind::Config),
    ("yaml", CodeKind::Config),
    ("yml", CodeKind::Config),
    ("ini", CodeKind::Config),
    ("cfg", CodeKind::Config),
    ("conf", CodeKind::Config),
    ("properties", CodeKind::Config),
    ("env", CodeKind::Config),
    ("plist", CodeKind::Config),
    ("lock", CodeKind::Config),
    ("json", CodeKind::Config),
    ("json5", CodeKind::Config),
    ("jsonc", CodeKind::Config),
    ("xml", CodeKind::Config),
    ("tf", CodeKind::Config),
    ("tfvars", CodeKind::Config),
    ("hcl", CodeKind::Config),
    ("nix", CodeKind::Config),
    // Certificates and key material. Classified, never opened: nothing in this
    // crate reads a file whose path-kind is not `Source`.
    ("pem", CodeKind::Config),
    ("crt", CodeKind::Config),
    ("cer", CodeKind::Config),
    ("der", CodeKind::Config),
    ("p12", CodeKind::Config),
    ("pfx", CodeKind::Config),
    ("keystore", CodeKind::Config),
    ("gradle", CodeKind::Build),
    ("bazel", CodeKind::Build),
    ("bzl", CodeKind::Build),
    ("mk", CodeKind::Build),
    ("cmake", CodeKind::Build),
    ("in", CodeKind::Build),
    ("zon", CodeKind::Build),
    ("spec", CodeKind::Build),
    // Bytes shipped alongside the code.
    ("png", CodeKind::Assets),
    ("jpg", CodeKind::Assets),
    ("jpeg", CodeKind::Assets),
    ("gif", CodeKind::Assets),
    ("svg", CodeKind::Assets),
    ("webp", CodeKind::Assets),
    ("avif", CodeKind::Assets),
    ("bmp", CodeKind::Assets),
    ("ico", CodeKind::Assets),
    ("icns", CodeKind::Assets),
    ("woff", CodeKind::Assets),
    ("woff2", CodeKind::Assets),
    ("ttf", CodeKind::Assets),
    ("otf", CodeKind::Assets),
    ("eot", CodeKind::Assets),
    ("mp3", CodeKind::Assets),
    ("wav", CodeKind::Assets),
    ("ogg", CodeKind::Assets),
    ("mp4", CodeKind::Assets),
    ("webm", CodeKind::Assets),
    ("mov", CodeKind::Assets),
    ("pdf", CodeKind::Assets),
    ("psd", CodeKind::Assets),
    ("sketch", CodeKind::Assets),
    ("fig", CodeKind::Assets),
    // Data the program reads.
    ("csv", CodeKind::Data),
    ("tsv", CodeKind::Data),
    ("parquet", CodeKind::Data),
    ("avro", CodeKind::Data),
    ("sqlite", CodeKind::Data),
    ("sqlite3", CodeKind::Data),
    ("db", CodeKind::Data),
    ("sql", CodeKind::Data),
    ("jsonl", CodeKind::Data),
    ("ndjson", CodeKind::Data),
    ("geojson", CodeKind::Data),
    ("snap", CodeKind::Data),
    ("golden", CodeKind::Data),
    ("bin", CodeKind::Data),
    ("dat", CodeKind::Data),
    ("pickle", CodeKind::Data),
    ("npy", CodeKind::Data),
    ("npz", CodeKind::Data),
    ("mpack", CodeKind::Data),
    ("msgpack", CodeKind::Data),
    ("log", CodeKind::Data),
    ("patch", CodeKind::Data),
    ("diff", CodeKind::Data),
    ("zip", CodeKind::Data),
    ("gz", CodeKind::Data),
    ("tgz", CodeKind::Data),
    ("bz2", CodeKind::Data),
    ("xz", CodeKind::Data),
    ("zst", CodeKind::Data),
    ("tar", CodeKind::Data),
    ("7z", CodeKind::Data),
    ("ics", CodeKind::Data),
    // Build-tool bookkeeping: a tool wrote it and nobody reads it.
    ("tsbuildinfo", CodeKind::Vendored),
];

/// Weak directory hints, checked after the code extensions and before the
/// remaining extension table.
const DIRS: &[(&str, CodeKind)] = &[
    ("docs", CodeKind::Docs),
    ("doc", CodeKind::Docs),
    ("documentation", CodeKind::Docs),
    ("man", CodeKind::Docs),
    ("manual", CodeKind::Docs),
    ("adr", CodeKind::Docs),
    ("rfc", CodeKind::Docs),
    ("rfcs", CodeKind::Docs),
    ("assets", CodeKind::Assets),
    ("static", CodeKind::Assets),
    ("public", CodeKind::Assets),
    ("images", CodeKind::Assets),
    ("img", CodeKind::Assets),
    ("icons", CodeKind::Assets),
    ("fonts", CodeKind::Assets),
    ("media", CodeKind::Assets),
    ("screenshots", CodeKind::Assets),
    ("data", CodeKind::Data),
    ("datasets", CodeKind::Data),
    ("fixtures", CodeKind::Data),
    ("testdata", CodeKind::Data),
    ("snapshots", CodeKind::Data),
    ("__snapshots__", CodeKind::Data),
    ("locale", CodeKind::Data),
    ("locales", CodeKind::Data),
    ("i18n", CodeKind::Data),
    ("translations", CodeKind::Data),
    ("seeds", CodeKind::Data),
    ("config", CodeKind::Config),
    ("configs", CodeKind::Config),
    ("settings", CodeKind::Config),
];

/// Byte markers that mean a tool wrote the file.
///
/// Matched case-insensitively against the head of the file. The first two are
/// the conventions Go, Bazel and protobuf settled on; the rest catch the
/// hand-rolled banners everything else emits.
const GENERATED_MARKERS: &[&str] = &[
    "@generated",
    "code generated by",
    "do not edit",
    "automatically generated",
    "autogenerated",
    "auto-generated",
    "this file was generated",
    "generated by the protocol buffer compiler",
];

/// How many bytes of a file [`kind_of_with_content`] reads looking for a
/// `GENERATED_MARKERS` banner.
///
/// A generated-file banner is in the first line or two by universal convention;
/// 512 bytes covers a shebang, a licence line and the banner, and bounds the
/// cost at one page per file.
pub const GENERATED_MARKER_SCAN_BYTES: usize = 512;

/// The shipped rules, built once.
///
/// A `OnceLock` for the same reason [`crate::tree::default_industrial_rules`] is
/// one: this is consulted once per file on a cold start, and rebuilding four
/// hundred `String`s into `BTreeMap`s per file is most of PRD §13.1's budget.
pub fn default_kind_rules() -> &'static KindRules {
    static RULES: OnceLock<KindRules> = OnceLock::new();
    RULES.get_or_init(KindRules::shipped)
}

impl Default for KindRules {
    fn default() -> Self {
        Self::shipped()
    }
}

impl KindRules {
    /// The shipped defaults.
    pub fn shipped() -> Self {
        Self {
            industrial: IndustrialRules::default(),
            test_dirs: lowered(TEST_DIRS),
            test_names: lowered(TEST_NAMES),
            test_prefixes: TEST_PREFIXES
                .iter()
                .map(|s| s.to_ascii_lowercase())
                .collect(),
            test_suffixes: TEST_SUFFIXES
                .iter()
                .map(|s| s.to_ascii_lowercase())
                .collect(),
            test_camel_suffixes: TEST_CAMEL_SUFFIXES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            ci_dirs: lowered(CI_DIRS),
            names: keyed(NAMES),
            stems: keyed(STEMS),
            code_extensions: lowered(CODE_EXTENSIONS),
            extensions: keyed(EXTENSIONS),
            dirs: keyed(DIRS),
            generated_markers: GENERATED_MARKERS
                .iter()
                .map(|s| s.to_ascii_lowercase())
                .collect(),
        }
    }

    /// Rules that classify nothing: every file is [`CodeKind::Unknown`] except
    /// what the industrial rules claim.
    ///
    /// The starting point for a project that wants to spell out its own
    /// conventions from scratch rather than extend the shipped ones.
    pub fn empty() -> Self {
        Self {
            industrial: IndustrialRules::empty(),
            test_dirs: BTreeSet::new(),
            test_names: BTreeSet::new(),
            test_prefixes: Vec::new(),
            test_suffixes: Vec::new(),
            test_camel_suffixes: Vec::new(),
            ci_dirs: BTreeSet::new(),
            names: BTreeMap::new(),
            stems: BTreeMap::new(),
            code_extensions: BTreeSet::new(),
            extensions: BTreeMap::new(),
            dirs: BTreeMap::new(),
            generated_markers: Vec::new(),
        }
    }

    /// The industrial rules in force — PRD §8's zone, shared with
    /// [`crate::tree::classify_with`].
    pub fn industrial(&self) -> &IndustrialRules {
        &self.industrial
    }

    /// Replaces the industrial rules.
    pub fn set_industrial(&mut self, rules: IndustrialRules) {
        self.industrial = rules;
    }

    /// The generated-file banners [`kind_of_with_content`] looks for.
    pub fn generated_markers(&self) -> &[String] {
        &self.generated_markers
    }

    /// Applies a configuration overlay in place.
    pub fn apply(&mut self, config: &KindRulesConfig) {
        for (name, kind) in &config.extensions {
            let key = name.trim_start_matches('.').to_ascii_lowercase();
            // An extension declared as Source joins the table checked *before*
            // the directory hints, which is the whole difference between
            // `docs/conf.py` being code and being prose.
            if *kind == CodeKind::Source {
                self.extensions.remove(&key);
                self.code_extensions.insert(key);
            } else {
                self.code_extensions.remove(&key);
                self.extensions.insert(key, *kind);
            }
        }
        for (name, kind) in &config.names {
            self.names.insert(name.to_ascii_lowercase(), *kind);
        }
        for (name, kind) in &config.stems {
            self.stems.insert(name.to_ascii_lowercase(), *kind);
        }
        for (name, kind) in &config.dirs {
            self.dirs.insert(name.to_ascii_lowercase(), *kind);
        }
        for name in &config.test_dirs {
            self.test_dirs.insert(name.to_ascii_lowercase());
        }
        for name in &config.test_names {
            self.test_names.insert(name.to_ascii_lowercase());
        }
        for name in &config.ci_dirs {
            self.ci_dirs.insert(name.to_ascii_lowercase());
        }
        for name in &config.test_suffixes {
            push_unique(&mut self.test_suffixes, name);
        }
        for name in &config.test_prefixes {
            push_unique(&mut self.test_prefixes, name);
        }
        for name in &config.test_camel_suffixes {
            if !self.test_camel_suffixes.contains(name) {
                self.test_camel_suffixes.push(name.clone());
            }
        }
        for marker in &config.generated_markers {
            push_unique(&mut self.generated_markers, marker);
        }
        for name in &config.vendored_dirs {
            self.industrial.push_dir(name);
        }
        for path in &config.vendored_prefixes {
            self.industrial.push_prefix(path);
        }
        for suffix in &config.generated_suffixes {
            self.industrial.push_generated_suffix(suffix);
        }
        for name in &config.remove_extensions {
            let key = name.trim_start_matches('.').to_ascii_lowercase();
            self.extensions.remove(&key);
            self.code_extensions.remove(&key);
        }
        for name in &config.remove_names {
            self.names.remove(&name.to_ascii_lowercase());
        }
        for name in &config.remove_stems {
            self.stems.remove(&name.to_ascii_lowercase());
        }
        for name in &config.remove_dirs {
            self.dirs.remove(&name.to_ascii_lowercase());
        }
        for name in &config.remove_test_dirs {
            self.test_dirs.remove(&name.to_ascii_lowercase());
        }
        for name in &config.remove_vendored_dirs {
            self.industrial.remove_dir(name);
        }
    }

    /// Shipped defaults with an overlay applied, or a bare
    /// [`KindRules::empty`] plus the overlay when
    /// [`KindRulesConfig::extend_defaults`] is `false`.
    pub fn from_config(config: &KindRulesConfig) -> Self {
        let mut rules = if config.extend_defaults.unwrap_or(true) {
            Self::shipped()
        } else {
            Self::empty()
        };
        rules.apply(config);
        rules
    }

    /// Reads a JSON overlay from disk.
    ///
    /// A missing file is the shipped defaults. A malformed one is a warning on
    /// stderr and the shipped defaults — never a failure. This is the same rule
    /// the four ingest channels follow and the same one `polis_app::Config`
    /// follows: a typo in a preferences file must not stop a city being drawn.
    pub fn load(path: &Path) -> Self {
        Self::from_config(&KindRulesConfig::load(path))
    }

    /// True when any component of `path` is a test directory.
    fn in_test_dir(&self, path: &LogicalPath) -> bool {
        directories(path).any(|c| self.test_dirs.contains(&c.to_ascii_lowercase()))
    }

    /// True when any component of `path` is a CI or build directory.
    fn in_ci_dir(&self, path: &LogicalPath) -> bool {
        directories(path).any(|c| self.ci_dirs.contains(&c.to_ascii_lowercase()))
    }

    /// True when the file's own name says it is a test.
    fn is_test_name(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        if self.test_names.contains(&lower) {
            return true;
        }
        let stem = stem_of(&lower);
        if self
            .test_prefixes
            .iter()
            .any(|p| stem.starts_with(p.as_str()))
            || self
                .test_suffixes
                .iter()
                .any(|s| stem.ends_with(s.as_str()))
        {
            return true;
        }
        let cased = stem_of(name);
        self.test_camel_suffixes
            .iter()
            .any(|s| cased.ends_with(s.as_str()))
    }

    /// The first directory hint on the path, walking from the file outwards.
    ///
    /// Outwards rather than from the root: `public/docs/x.pdf` is documentation
    /// that happens to be published, and the nearer directory is the more
    /// specific claim.
    fn dir_hint(&self, path: &LogicalPath) -> Option<CodeKind> {
        let dirs: Vec<&str> = directories(path).collect();
        dirs.iter()
            .rev()
            .find_map(|c| self.dirs.get(&c.to_ascii_lowercase()).copied())
    }
}

/// A JSON overlay on [`KindRules`].
///
/// Every field is optional and additive unless it starts with `remove_`. Written
/// to `<repo>/.polis/neighborhoods.json` (see
/// [`crate::neighborhoods::NeighborhoodConfig`]), which the repository walk
/// already refuses to enter, so configuring Polis never adds a building to the
/// city.
///
/// ```json
/// {
///   "extend_defaults": true,
///   "remove_test_dirs": ["test"],
///   "dirs": { "prompts": "data", "releasenotes": "docs" },
///   "extensions": { "hbs": "source" },
///   "vendored_dirs": ["emulator-data"]
/// }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KindRulesConfig {
    /// Start from the shipped rules. `true` when absent; set `false` to
    /// classify with nothing but what this file declares.
    pub extend_defaults: Option<bool>,
    /// Extension (with or without the leading dot) to kind.
    pub extensions: BTreeMap<String, CodeKind>,
    /// Exact file name to kind.
    pub names: BTreeMap<String, CodeKind>,
    /// File stem — the name minus its final extension — to kind.
    pub stems: BTreeMap<String, CodeKind>,
    /// Directory name to kind, at any depth.
    pub dirs: BTreeMap<String, CodeKind>,
    /// Extra test directory names.
    pub test_dirs: Vec<String>,
    /// Extra exact test file names.
    pub test_names: Vec<String>,
    /// Extra test stem prefixes.
    pub test_prefixes: Vec<String>,
    /// Extra test stem suffixes, matched on the lowercased stem. Include the
    /// delimiter: `"_it"`, not `"it"`.
    pub test_suffixes: Vec<String>,
    /// Extra test stem suffixes matched case-sensitively: `"IT"`, `"Fixture"`.
    pub test_camel_suffixes: Vec<String>,
    /// Extra CI and build directory names.
    pub ci_dirs: Vec<String>,
    /// Extra PRD §8 industrial directory names.
    pub vendored_dirs: Vec<String>,
    /// Extra PRD §8 industrial subtrees, root-anchored.
    pub vendored_prefixes: Vec<String>,
    /// Extra file-name endings that mean a tool wrote the file.
    pub generated_suffixes: Vec<String>,
    /// Extra in-file banners that mean a tool wrote the file.
    pub generated_markers: Vec<String>,
    /// Extensions to forget.
    pub remove_extensions: Vec<String>,
    /// Exact file names to forget.
    pub remove_names: Vec<String>,
    /// Stems to forget.
    pub remove_stems: Vec<String>,
    /// Directory hints to forget.
    pub remove_dirs: Vec<String>,
    /// Test directory names to forget — the escape hatch for a shipped package
    /// genuinely called `test`.
    pub remove_test_dirs: Vec<String>,
    /// Industrial directory names to forget, for a repository whose `generated/`
    /// is hand-written.
    pub remove_vendored_dirs: Vec<String>,
}

impl KindRulesConfig {
    /// Reads one from disk. Missing or malformed is an empty overlay.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!(
                        "polis: {} is not valid kind configuration ({error}); using defaults",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                eprintln!(
                    "polis: cannot read {} ({error}); using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// What kind of code a file is, from its path alone, under the shipped rules.
pub fn kind_of(path: &LogicalPath) -> CodeKind {
    kind_of_with(path, default_kind_rules())
}

/// What kind of code a file is, from its path alone.
///
/// Path-only, like [`crate::tree::classify`], so it can run before any git or
/// import data exists and cannot make the map depend on ingest timing
/// (PRD §7.4). See [`KindRules`] for the order of the six passes and why it is
/// that order.
pub fn kind_of_with(path: &LogicalPath, rules: &KindRules) -> CodeKind {
    if path.is_root() {
        return CodeKind::Unknown;
    }
    // 1. PRD §8's industrial zone wins over every name-based rule.
    if rules.industrial.is_industrial_file(path) {
        return CodeKind::Vendored;
    }
    let Some(name) = path.file_name() else {
        return CodeKind::Unknown;
    };
    // 2. Tests, by directory or by file name.
    if rules.in_test_dir(path) || rules.is_test_name(name) {
        return CodeKind::Test;
    }
    // 3. CI and build directories, before the extension table so a workflow
    //    file is Build and not Config.
    if rules.in_ci_dir(path) {
        return CodeKind::Build;
    }
    let lower = name.to_ascii_lowercase();
    // 4. Exact name, then stem.
    if let Some(kind) = rules.names.get(&lower) {
        return *kind;
    }
    if let Some(kind) = rules.stems.get(stem_of(&lower)) {
        return *kind;
    }
    // 5. Program text, before the directory hints.
    let ext = path.extension().map(str::to_ascii_lowercase);
    if let Some(ext) = &ext {
        if rules.code_extensions.contains(ext) {
            return CodeKind::Source;
        }
    }
    // 6. A directory hint, then the remaining extensions.
    if let Some(kind) = rules.dir_hint(path) {
        return kind;
    }
    if let Some(ext) = &ext {
        if let Some(kind) = rules.extensions.get(ext) {
            return *kind;
        }
    }
    CodeKind::Unknown
}

/// [`kind_of_with`], plus the one content heuristic: a generated-file banner.
///
/// `head` is the first [`GENERATED_MARKER_SCAN_BYTES`] of the file (fewer is
/// fine). A file whose head carries `@generated`, `Code generated by …` or
/// `DO NOT EDIT` is [`CodeKind::Vendored`] whatever its path says — which is
/// exactly PRD §8's rule applied to the case the path cannot see: a checked-in
/// generated file sitting in the middle of hand-written source.
///
/// Two guards, both deliberate:
///
/// * Only a path-kind of [`CodeKind::Source`] can be overridden. A README that
///   says "do not edit" is still documentation, and a `package-lock.json` is
///   already Config, which is the more useful answer.
/// * Only the head is read, and only the head. The marker is a banner by
///   universal convention, and scanning a whole file for the phrase "do not
///   edit" would reclassify every style guide in the repository.
pub fn kind_of_with_content(path: &LogicalPath, head: &[u8], rules: &KindRules) -> CodeKind {
    let kind = kind_of_with(path, rules);
    if kind != CodeKind::Source || rules.generated_markers.is_empty() {
        return kind;
    }
    if has_generated_marker(head, &rules.generated_markers) {
        return CodeKind::Vendored;
    }
    kind
}

/// True when the head of a file carries one of the banners.
///
/// ASCII-lowercases a bounded copy of the head rather than the whole file, and
/// only looks at valid-UTF-8-or-not bytes as bytes, so a binary file is safe to
/// pass in.
fn has_generated_marker(head: &[u8], markers: &[String]) -> bool {
    let n = head.len().min(GENERATED_MARKER_SCAN_BYTES);
    let mut lower = Vec::with_capacity(n);
    lower.extend(head[..n].iter().map(u8::to_ascii_lowercase));
    markers
        .iter()
        .any(|m| !m.is_empty() && contains_bytes(&lower, m.as_bytes()))
}

/// A plain substring search over bytes.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The directory components of a path — every component except the file name.
fn directories(path: &LogicalPath) -> impl Iterator<Item = &str> + '_ {
    let n = path.depth().saturating_sub(1);
    path.components().take(n)
}

/// A file name with its final extension removed, using the same rule as
/// [`LogicalPath::extension`]: a leading dot is part of the name.
///
/// `vite.config.ts` → `vite.config`; `.eslintrc` → `.eslintrc`; `Makefile` →
/// `Makefile`.
fn stem_of(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

/// A lowercased set from a static table.
fn lowered(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_ascii_lowercase()).collect()
}

/// A lowercased map from a static table.
fn keyed(items: &[(&str, CodeKind)]) -> BTreeMap<String, CodeKind> {
    items
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), *v))
        .collect()
}

/// Appends a lowercased entry unless it is already there, keeping the vector a
/// set with a stable order.
fn push_unique(list: &mut Vec<String>, item: &str) {
    let lower = item.to_ascii_lowercase();
    if !list.contains(&lower) {
        list.push(lower);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    fn kind(s: &str) -> CodeKind {
        kind_of(&lp(s))
    }

    #[test]
    fn the_industrial_zone_wins_over_every_name_rule() {
        // Ten thousand files called `index.js` do not make a repository of entry
        // points, and a README inside a package is not this repository's docs.
        assert_eq!(kind("node_modules/react/index.js"), CodeKind::Vendored);
        assert_eq!(kind("node_modules/react/README.md"), CodeKind::Vendored);
        assert_eq!(
            kind("node_modules/react/__tests__/a.js"),
            CodeKind::Vendored
        );
        assert_eq!(kind("target/debug/build/x.rs"), CodeKind::Vendored);
        assert_eq!(kind("assets/app.min.js"), CodeKind::Vendored);
    }

    #[test]
    fn tests_are_found_by_directory_and_by_name() {
        assert_eq!(kind("tests/golden.rs"), CodeKind::Test);
        assert_eq!(kind("src/__tests__/auth.ts"), CodeKind::Test);
        assert_eq!(kind("src/auth.test.ts"), CodeKind::Test);
        assert_eq!(kind("src/auth.spec.tsx"), CodeKind::Test);
        assert_eq!(kind("pkg/auth_test.go"), CodeKind::Test);
        assert_eq!(kind("app/test_views.py"), CodeKind::Test);
        assert_eq!(kind("src/AuthTest.java"), CodeKind::Test);
        assert_eq!(kind("tests/conftest.py"), CodeKind::Test);
        assert_eq!(kind("benches/throughput.rs"), CodeKind::Test);
        // And a file that merely mentions the word is not one.
        assert_eq!(kind("src/latest.ts"), CodeKind::Source);
        assert_eq!(kind("src/protest.py"), CodeKind::Source);
        assert_eq!(kind("src/contest.rs"), CodeKind::Source);
        assert_eq!(kind("app/manifest.json"), CodeKind::Config);
    }

    #[test]
    fn a_code_extension_beats_the_directory_it_sits_in() {
        // The trap this ordering exists for: `docs/` full of Python.
        assert_eq!(kind("docs/conf.py"), CodeKind::Source);
        assert_eq!(kind("docs/index.md"), CodeKind::Docs);
        assert_eq!(kind("docs/architecture.txt"), CodeKind::Docs);
        // And the mirror image: a directory hint beats the *remaining*
        // extensions, so a data file is not config because it is JSON.
        assert_eq!(kind("data/cities.json"), CodeKind::Data);
        assert_eq!(kind("src/settings.json"), CodeKind::Config);
    }

    #[test]
    fn ci_directories_are_build_not_config() {
        assert_eq!(kind(".github/workflows/ci.yml"), CodeKind::Build);
        assert_eq!(kind(".circleci/config.yml"), CodeKind::Build);
        assert_eq!(kind("Dockerfile"), CodeKind::Build);
        assert_eq!(kind("Makefile"), CodeKind::Build);
        assert_eq!(kind("build.rs"), CodeKind::Build);
        assert_eq!(kind("vite.config.ts"), CodeKind::Build);
        assert_eq!(kind("vite.config.mjs"), CodeKind::Build);
    }

    #[test]
    fn manifests_and_prose_land_where_a_person_would_put_them() {
        assert_eq!(kind("Cargo.toml"), CodeKind::Config);
        assert_eq!(kind("package.json"), CodeKind::Config);
        assert_eq!(kind("README.md"), CodeKind::Docs);
        assert_eq!(kind("README"), CodeKind::Docs);
        assert_eq!(kind("LICENSE"), CodeKind::Docs);
        assert_eq!(kind("CHANGELOG.rst"), CodeKind::Docs);
        assert_eq!(kind("public/logo.svg"), CodeKind::Assets);
        assert_eq!(
            kind("django/conf/locale/de/LC_MESSAGES/django.po"),
            CodeKind::Data
        );
        assert_eq!(kind("src/styles/app.scss"), CodeKind::Source);
        assert_eq!(kind("templates/base.html"), CodeKind::Source);
    }

    #[test]
    fn an_unmatched_file_says_unknown_rather_than_guessing() {
        assert_eq!(kind("src/mystery.qqq"), CodeKind::Unknown);
        assert_eq!(kind("desktop.ini"), CodeKind::Config);
        assert_eq!(kind("weird-no-extension-file"), CodeKind::Unknown);
        // And the extensions the real repositories turned up as gaps.
        assert_eq!(kind("runtime/syntax/vim.vim"), CodeKind::Source);
        assert_eq!(kind("runtime/queries/lua/highlights.scm"), CodeKind::Source);
        assert_eq!(kind("firestore-debug.log"), CodeKind::Data);
        assert_eq!(kind("certs/server.pem"), CodeKind::Config);
        assert_eq!(kind(".env.local"), CodeKind::Config);
        assert_eq!(kind("tsconfig.tsbuildinfo"), CodeKind::Vendored);
    }

    #[test]
    fn a_generated_banner_reclassifies_source_and_nothing_else() {
        let rules = KindRules::shipped();
        let p = lp("src/api/schema.ts");
        assert_eq!(kind_of_with(&p, &rules), CodeKind::Source);
        assert_eq!(
            kind_of_with_content(
                &p,
                b"// Code generated by openapi-gen. DO NOT EDIT.\n",
                &rules
            ),
            CodeKind::Vendored
        );
        assert_eq!(
            kind_of_with_content(&p, b"// a hand-written module\n", &rules),
            CodeKind::Source
        );
        // A style guide that says "do not edit" is still documentation.
        let doc = lp("docs/style.md");
        assert_eq!(
            kind_of_with_content(&doc, b"# Style\n\nDO NOT EDIT this by hand.\n", &rules),
            CodeKind::Docs
        );
        // And only the head is read.
        let mut tail = vec![b' '; GENERATED_MARKER_SCAN_BYTES + 32];
        tail.extend_from_slice(b"@generated");
        assert_eq!(kind_of_with_content(&p, &tail, &rules), CodeKind::Source);
    }

    #[test]
    fn the_mix_keeps_the_proportions_not_just_the_winner() {
        let mut mix = KindMix::new();
        for _ in 0..6 {
            mix.push(CodeKind::Test, 100);
        }
        for _ in 0..4 {
            mix.push(CodeKind::Source, 200);
        }
        assert_eq!(mix.dominant(), CodeKind::Test);
        assert_eq!(mix.total(), 10);
        assert_eq!(mix.total_bytes(), 6 * 100 + 4 * 200);
        assert!((mix.share(CodeKind::Source) - 0.4).abs() < 1e-6);
        assert_eq!(mix.secondary(0.2), Some(CodeKind::Source));
        assert_eq!(mix.secondary(0.5), None);
        assert_eq!(mix.summary(3, 0.05), "60% test, 40% source");
    }

    #[test]
    fn a_tie_breaks_the_same_way_on_every_run() {
        let mut mix = KindMix::new();
        mix.push(CodeKind::Test, 1);
        mix.push(CodeKind::Source, 1);
        // `Source` is first in `ALL`, so it wins — and it wins identically
        // whichever order the two files arrived in.
        assert_eq!(mix.dominant(), CodeKind::Source);
        let mut other = KindMix::new();
        other.push(CodeKind::Source, 1);
        other.push(CodeKind::Test, 1);
        assert_eq!(mix, other);
        assert_eq!(mix.ranked(), other.ranked());
    }

    #[test]
    fn taking_a_child_out_of_a_mix_can_change_the_winner() {
        // Django's `contrib/admin`, in miniature.
        let mut whole = KindMix::new();
        let mut locale = KindMix::new();
        for _ in 0..386 {
            whole.push(CodeKind::Data, 900);
            locale.push(CodeKind::Data, 900);
        }
        for _ in 0..110 {
            whole.push(CodeKind::Source, 4_000);
        }
        assert_eq!(whole.dominant(), CodeKind::Data);
        let rest = whole.without(&locale);
        assert_eq!(rest.dominant(), CodeKind::Source);
        assert_eq!(rest.total(), 110);
        // Subtracting more than there is saturates rather than wrapping.
        assert!(rest.without(&whole).is_empty());
    }

    #[test]
    fn an_empty_mix_is_unknown_rather_than_source() {
        let mix = KindMix::new();
        assert_eq!(mix.dominant(), CodeKind::Unknown);
        assert!(mix.is_empty());
        assert_eq!(mix.summary(3, 0.0), "");
        assert!(mix.share(CodeKind::Source).abs() < f32::EPSILON);
    }

    #[test]
    fn a_config_overlay_adds_and_removes() {
        let config = KindRulesConfig {
            remove_test_dirs: vec!["test".to_owned()],
            dirs: [("prompts".to_owned(), CodeKind::Data)]
                .into_iter()
                .collect(),
            extensions: [("hbs".to_owned(), CodeKind::Docs)].into_iter().collect(),
            vendored_dirs: vec!["emulator-data".to_owned()],
            ..KindRulesConfig::default()
        };
        let rules = KindRules::from_config(&config);
        // Django's shipped `django/test/` package is source again.
        assert_eq!(
            kind_of_with(&lp("django/test/client.py"), &rules),
            CodeKind::Source
        );
        // But `tests/` still is not.
        assert_eq!(
            kind_of_with(&lp("tests/test_client.py"), &rules),
            CodeKind::Test
        );
        assert_eq!(
            kind_of_with(&lp("prompts/summary.txt"), &rules),
            CodeKind::Data
        );
        // `hbs` moved out of the code table into the extension table, so the
        // directory hint now beats it.
        assert_eq!(
            kind_of_with(&lp("templates/card.hbs"), &rules),
            CodeKind::Docs
        );
        assert_eq!(
            kind_of_with(&lp("emulator-data/a.json"), &rules),
            CodeKind::Vendored
        );
    }

    #[test]
    fn an_overlay_can_declare_an_extension_to_be_code() {
        let config = KindRulesConfig {
            extensions: [("md".to_owned(), CodeKind::Source)].into_iter().collect(),
            ..KindRulesConfig::default()
        };
        let rules = KindRules::from_config(&config);
        // Source extensions are checked before the directory hints, so this is
        // now code even inside `docs/`.
        assert_eq!(kind_of_with(&lp("docs/guide.md"), &rules), CodeKind::Source);
    }

    #[test]
    fn empty_rules_classify_nothing_but_still_see_the_industrial_zone() {
        let mut rules = KindRules::empty();
        rules.set_industrial(IndustrialRules::default());
        assert_eq!(kind_of_with(&lp("src/main.rs"), &rules), CodeKind::Unknown);
        assert_eq!(
            kind_of_with(&lp("node_modules/a/b.js"), &rules),
            CodeKind::Vendored
        );
    }

    #[test]
    fn every_kind_has_a_distinct_name_and_round_trips() {
        let mut names: Vec<&str> = CodeKind::ALL.iter().map(|k| k.name()).collect();
        let n = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), n, "duplicate kind name");
        for kind in CodeKind::ALL {
            assert_eq!(CodeKind::parse(kind.name()), Some(kind));
            assert_eq!(CodeKind::parse(&kind.name().to_uppercase()), Some(kind));
            assert_eq!(CodeKind::ALL[kind.index()], kind, "index must match ALL");
        }
        assert_eq!(CodeKind::parse("nonsense"), None);
    }

    #[test]
    fn the_config_format_round_trips_through_json() {
        let config = KindRulesConfig {
            extend_defaults: Some(false),
            dirs: [("prompts".to_owned(), CodeKind::Data)]
                .into_iter()
                .collect(),
            ..KindRulesConfig::default()
        };
        let text = serde_json::to_string(&config).expect("serialize");
        assert!(
            text.contains("\"data\""),
            "kinds are lowercase in the file: {text}"
        );
        let back: KindRulesConfig = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(config, back);
    }

    #[test]
    fn a_missing_or_broken_config_falls_back_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("absent.json");
        assert_eq!(KindRulesConfig::load(&missing), KindRulesConfig::default());
        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, b"{ not json").expect("write");
        assert_eq!(KindRulesConfig::load(&broken), KindRulesConfig::default());
    }

    #[test]
    fn classification_is_a_pure_function_of_the_path() {
        // The determinism claim, asserted rather than asserted-in-prose: the
        // same path gives the same kind however many times it is asked, and the
        // order the paths arrive in changes nothing.
        let paths = [
            "src/main.rs",
            "tests/a.rs",
            "docs/x.md",
            "node_modules/p/i.js",
            "public/logo.png",
        ];
        let forward: Vec<CodeKind> = paths.iter().map(|p| kind(p)).collect();
        let backward: Vec<CodeKind> = paths.iter().rev().map(|p| kind(p)).collect();
        let mut reversed = backward;
        reversed.reverse();
        assert_eq!(forward, reversed);
    }
}
