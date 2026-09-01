//! Proof that the four pinned tree-sitter grammars load into the pinned runtime.
//!
//! The version numbers look mismatched — runtime 0.27.0 against grammars 0.24.2,
//! 0.25.0, 0.23.2 and 0.25.0 — and a grammar/runtime ABI mismatch is a
//! **runtime** failure, not a compile error: `Parser::set_language` returns
//! `Err(LanguageError)` and the natural reading of PRD §9's "failure to parse a
//! file is non-fatal" would swallow it. Every file would silently have no
//! streets, and the map would look fine.
//!
//! So this test loads each grammar and parses a snippet containing the import
//! construct `polis_repo::imports` actually queries for. If a grammar bump ever
//! crosses an ABI boundary, this fails loudly instead of quietly costing the
//! product its streets. See ADR-0051.

use tree_sitter::{Language, Parser};

/// Loads one grammar and parses one snippet, asserting the tree is usable.
fn parses(name: &str, language: &Language, source: &str, expected_root: &str) {
    let mut parser = Parser::new();
    parser
        .set_language(language)
        .unwrap_or_else(|e| panic!("{name}: grammar rejected by the runtime: {e}"));

    let tree = parser
        .parse(source, None)
        .unwrap_or_else(|| panic!("{name}: parser returned no tree"));
    let root = tree.root_node();

    assert_eq!(root.kind(), expected_root, "{name}: unexpected root node");
    assert!(
        !root.has_error(),
        "{name}: parse errors in a snippet that should be valid:\n{}",
        root.to_sexp()
    );
    assert!(
        root.named_child_count() > 0,
        "{name}: root has no named children"
    );
}

#[test]
fn rust_grammar_loads_and_parses_a_use_declaration() {
    parses(
        "rust",
        &tree_sitter_rust::LANGUAGE.into(),
        "use crate::imports::ImportEdge;\nfn main() {}\n",
        "source_file",
    );
}

#[test]
fn javascript_grammar_loads_and_parses_an_import() {
    parses(
        "javascript",
        &tree_sitter_javascript::LANGUAGE.into(),
        "import { a } from './auth.js';\nconst b = require('node:fs');\n",
        "program",
    );
}

#[test]
fn typescript_grammar_loads_and_parses_a_type_import() {
    parses(
        "typescript",
        &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "import type { A } from './a';\nexport const x: A = 1 as unknown as A;\n",
        "program",
    );
}

#[test]
fn tsx_is_a_separate_grammar_from_typescript() {
    // `.tsx` is not a flag on the TypeScript grammar; `polis_repo::Language`
    // models it as its own variant for exactly this reason.
    parses(
        "tsx",
        &tree_sitter_typescript::LANGUAGE_TSX.into(),
        "import React from 'react';\nexport const V = () => <div className=\"x\" />;\n",
        "program",
    );
}

#[test]
fn python_grammar_loads_and_parses_an_import() {
    parses(
        "python",
        &tree_sitter_python::LANGUAGE.into(),
        "from .auth import login\nimport os.path\n",
        "module",
    );
}

/// The four grammars must be distinguishable at runtime, or `language_for`
/// mapping two extensions to "the same" grammar would go unnoticed.
#[test]
fn the_five_loaded_grammars_are_distinct() {
    let languages: Vec<Language> = vec![
        tree_sitter_rust::LANGUAGE.into(),
        tree_sitter_javascript::LANGUAGE.into(),
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        tree_sitter_typescript::LANGUAGE_TSX.into(),
        tree_sitter_python::LANGUAGE.into(),
    ];
    assert_eq!(languages.len(), polis_repo::Language::ALL.len());
    for (i, a) in languages.iter().enumerate() {
        for b in languages.iter().skip(i + 1) {
            assert_ne!(a, b, "two Language values resolved to the same grammar");
        }
    }
}
