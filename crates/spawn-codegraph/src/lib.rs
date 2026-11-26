//! Source extractors for the feature map's code dependency truth (GitHub
//! issue #100): the code graph a repository's files make, in the model of
//! [`spawn_core::feature_deps`].
//!
//! - `manifest`: a strict built-in reader for the TOML subset of
//!   `Cargo.toml` and `pyproject.toml` (no `toml` crate: the gate builds offline);
//! - [`rust`]: what one Rust file says, read with `syn`;
//! - `ecma`: what one TypeScript / JavaScript file says, read with a
//!   hand-written lexer (`package.json` and `tsconfig.json` are read by
//!   `manifest`);
//! - [`python`]: what one Python `.py` / `.pyi` file says, read with a
//!   hand-written lexer (`pyproject.toml` is read by `manifest`);
//! - [`resolve`]: units, targets, module trees, and every reference resolved
//!   to a file of the in-scope inventory.
//!
//! Pure: no filesystem, process or network. The caller (the `spawn features
//! graph` command) is the only reader of files; it passes the in-scope
//! inventory and the texts of the files [`wants`] accepts.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};

use spawn_core::feature_deps::CodeGraph;

mod ecma;
mod manifest;
pub mod python;
pub mod resolve;
pub mod rust;

/// A source file larger than this is not read (listed as skipped).
pub const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;

/// What the graph cannot see, and which way each blind spot errs.
pub const LIMITS: [&str; 18] = [
    "value-flow-without-a-named-path: misses an edge",
    "rust-shadowing-is-approximated: explicit names, child modules, crates, local items, then declarations behind globs, then macro mentions",
    "cfg-not-evaluated: adds an edge",
    "macro-generated-items: a name only an item-level macro call mentions is a guess with precision macro, consulted after every real declaration; others miss an edge",
    "deep-or-oversized-code-degrades: a file nesting brackets past 256 levels is unparsed (no outgoing edges); a part that cannot be cut to the parser's token budget is read with a token walk (token_fallback: its a::b paths only)",
    "only-rust-js-ts-python-and-their-manifests-are-read: other files (vue, svelte, shell, ...) have no edges",
    "js-ts-resolution-has-no-node-modules: external packages are counted, never followed; a dist/ target not in scope falls back to src/index or the package.json at precision package",
    "js-ts-barrels-follow-named-imports-only: default and namespace imports stop at the barrel",
    "js-ts-dynamic-import-of-a-non-literal-is-counted-not-guessed",
    "jsx-text-is-lexed-as-code: an apostrophe or quote in JSX text hides at most the rest of its line; a backtick or a block comment that never closes makes the file unparsed (no outgoing edges)",
    "tsconfig-references-are-not-followed: each project resolves with its nearest tsconfig.json / jsconfig.json and its extends chain only",
    "import-meta-glob-is-not-expanded: import.meta.glob(...) patterns and new URL(..., import.meta.url) miss an edge",
    "python-sys-path-and-dynamic-imports-not-followed: runtime sys.path mutation and non-literal importlib.import_module(...) calls are counted, not guessed",
    "python-stdlib-and-external-packages-counted-not-followed: external distributions and Python stdlib imports increment external_imports without synthetic edges",
    "python-init-barrels-follow-static-reexports-only: __init__.py/__init__.pyi re-export chains follow named from-imports, stars, and static __all__ up to 8 hops",
    "reachability-is-per-file-not-per-item",
    "references-resolve-only-to-in-scope-inventory-files",
    "globs-are-not-chased-across-globs: a glob whose leading segment comes only from another glob (use super::*; use sub::*;) is not resolved; names it brings miss an edge",
];

/// Whether the extractors read `path` (repository-relative): Rust sources
/// and `Cargo.toml` manifests, TypeScript / JavaScript sources and
/// `package.json`, `tsconfig*.json` and `jsconfig*.json` manifests, and
/// Python sources (`.py` / `.pyi`) and `pyproject.toml` manifests.
pub fn wants(path: &str) -> bool {
    is_rust_source(path)
        || is_cargo_manifest(path)
        || ecma::is_ecma_source(path)
        || is_package_json(path)
        || is_tsconfig(path)
        || python::is_python_source(path)
        || is_pyproject_toml(path)
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A `pyproject.toml`, at the root or in a directory.
pub(crate) fn is_pyproject_toml(path: &str) -> bool {
    file_name(path) == "pyproject.toml"
}

/// A `package.json`, at the root or in a directory.
pub(crate) fn is_package_json(path: &str) -> bool {
    file_name(path) == "package.json"
}

/// A `tsconfig*.json` or `jsconfig*.json` (an `extends` target may be
/// `tsconfig.base.json`).
pub(crate) fn is_tsconfig(path: &str) -> bool {
    let name = file_name(path);
    (name.starts_with("tsconfig") || name.starts_with("jsconfig"))
        && std::path::Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

/// A `.rs` file (the compiler reads no other spelling).
pub(crate) fn is_rust_source(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| extension == "rs")
}

/// A `Cargo.toml`, at the root or in a directory.
pub(crate) fn is_cargo_manifest(path: &str) -> bool {
    path == "Cargo.toml" || path.ends_with("/Cargo.toml")
}

/// Build the code graph. `inventory` is every in-scope path: the only
/// paths a reference may resolve to. `sources` holds the texts of the
/// in-scope paths [`wants`] accepts.
pub fn build(inventory: &BTreeSet<String>, sources: &BTreeMap<String, String>) -> CodeGraph {
    resolve::build(inventory, sources)
}

/// Build the code graph of a string table of `(path, text)`: every path is
/// in scope, and the texts of the paths [`wants`] accepts are read.
pub fn build_from_table(table: &[(&str, &str)]) -> CodeGraph {
    let inventory = table.iter().map(|(path, _)| (*path).to_string()).collect();
    let sources = table
        .iter()
        .filter(|(path, _)| wants(path))
        .map(|(path, text)| ((*path).to_string(), (*text).to_string()))
        .collect();
    build(&inventory, &sources)
}
