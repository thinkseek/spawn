//! Source extractors for the feature map's code dependency truth (GitHub
//! issue #100): the code graph a repository's files make, in the model of
//! [`spawn_core::feature_deps`].
//!
//! - `manifest`: strict built-in readers for the TOML subset of `Cargo.toml`
//!   and `pyproject.toml`, JSON/JSONC `package.json` / `tsconfig*.json` /
//!   `jsconfig*.json`, and `go.mod` module manifests (zero external parser crates);
//! - [`rust`]: what one Rust file says, read with `syn`;
//! - `ecma`: what one TypeScript / JavaScript file says, read with a
//!   hand-written lexer;
//! - [`python`]: what one Python `.py` / `.pyi` file says, read with a
//!   hand-written lexer;
//! - [`go`]: what one Go `.go` file says (`package` clause and static
//!   `import` declarations), read with a hand-written lexer;
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
pub const LIMITS: [&str; 20] = [
    "value-flow-without-a-named-path: misses an edge",
    "rust-shadowing-is-approximated: explicit names, child modules, crates, local items, then declarations behind globs, then macro mentions",
    "cfg-not-evaluated: adds an edge",
    "macro-generated-items: a name only an item-level macro call mentions is a guess with precision macro, consulted after every real declaration; others miss an edge",
    "deep-or-oversized-code-degrades: a file nesting brackets past 256 levels is unparsed (no outgoing edges); a part that cannot be cut to the parser's token budget is read with a token walk (token_fallback: its a::b paths only)",
    "only-rust-js-ts-python-go-and-their-manifests-are-read: other files (vue, svelte, shell, ...) have no edges",
    "js-ts-resolution-has-no-node-modules: external packages are counted, never followed; a dist/ target not in scope falls back to src/index or the package.json at precision package",
    "js-ts-barrels-follow-named-imports-only: default and namespace imports stop at the barrel",
    "js-ts-dynamic-import-of-a-non-literal-is-counted-not-guessed",
    "jsx-text-is-lexed-as-code: an apostrophe or quote in JSX text hides at most the rest of its line; a backtick or a block comment that never closes makes the file unparsed (no outgoing edges)",
    "tsconfig-references-are-not-followed: each project resolves with its nearest tsconfig.json / jsconfig.json and its extends chain only",
    "import-meta-glob-is-not-expanded: import.meta.glob(...) patterns and new URL(..., import.meta.url) miss an edge",
    "python-sys-path-and-dynamic-imports-not-followed: runtime sys.path mutation and non-literal importlib.import_module(...) calls are counted, not guessed",
    "python-stdlib-and-external-packages-counted-not-followed: external distributions and Python stdlib imports increment external_imports without synthetic edges",
    "python-init-barrels-follow-static-reexports-only: __init__.py/__init__.pyi re-export chains follow named from-imports, stars, and static __all__ up to 8 hops",
    "go-build-tags-not-evaluated: build-constrained files are all indexed together and may add edges across OS/arch constraints",
    "go-imports-resolve-at-package-granularity: importing a Go package connects to all non-test .go files in that directory rather than individual symbols",
    "reachability-is-per-file-not-per-item",
    "references-resolve-only-to-in-scope-inventory-files",
    "globs-are-not-chased-across-globs: a glob whose leading segment comes only from another glob (use super::*; use sub::*;) is not resolved; names it brings miss an edge",
];

/// Pure-Rust Go source header extractor (`package` clause and `import` declarations).
pub mod go {
    use spawn_core::feature_deps::Scope;

    /// One static Go `import` entry extracted from a `.go` file header.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct GoImport {
        /// Imported package path (for example `"github.com/acme/pkg"` or `"fmt"`).
        pub path: String,
        /// Optional explicit alias (`"alias"`, `"."`, or `"_"`).
        pub alias: Option<String>,
        /// 1-based line number of the import specification.
        pub line: u32,
    }

    /// Extracted header facts for a single `.go` source file.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct GoFileFacts {
        /// Package identifier from `package <name>`, if present.
        pub package_name: Option<String>,
        /// Static import declarations prior to the first non-`import` top-level declaration.
        pub imports: Vec<GoImport>,
        /// Parse failure reason when syntax prevents safe header extraction.
        pub unparsed: Option<String>,
    }

    /// Whether `path` is a Go source file (`.go`).
    pub fn is_go_source(path: &str) -> bool {
        std::path::Path::new(path)
            .extension()
            .is_some_and(|ext| ext == "go")
    }

    /// Whether `path` follows Go's test source naming convention (`*_test.go`).
    pub fn is_test_path(path: &str) -> bool {
        super::file_name(path).ends_with("_test.go")
    }

    /// Build/runtime scope attributed to a Go file path.
    pub fn scope_of(path: &str) -> Scope {
        if is_test_path(path) {
            Scope::Test
        } else {
            Scope::Prod
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum TokKind {
        Ident(String),
        Str(String),
        Dot,
        LParen,
        RParen,
        Semi,
        Other,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Tok {
        kind: TokKind,
        line: u32,
    }

    struct GoLexer {
        chars: Vec<char>,
        at: usize,
        line: u32,
    }

    impl GoLexer {
        fn new(source: &str) -> Self {
            let trimmed = source.strip_prefix('\u{FEFF}').unwrap_or(source);
            Self {
                chars: trimmed.chars().collect(),
                at: 0,
                line: 1,
            }
        }

        fn peek(&self) -> Option<char> {
            self.chars.get(self.at).copied()
        }

        fn peek_at(&self, offset: usize) -> Option<char> {
            self.chars.get(self.at + offset).copied()
        }

        fn bump(&mut self) -> Option<char> {
            let c = self.peek()?;
            self.at += 1;
            if c == '\n' {
                self.line += 1;
            }
            Some(c)
        }

        fn skip_ws_and_comments(&mut self) -> Result<(), String> {
            loop {
                match self.peek() {
                    Some(c) if c.is_whitespace() => {
                        self.bump();
                    }
                    Some('/') if self.peek_at(1) == Some('/') => {
                        self.bump();
                        self.bump();
                        while self.peek().is_some_and(|c| c != '\n') {
                            self.bump();
                        }
                    }
                    Some('/') if self.peek_at(1) == Some('*') => {
                        let start = self.line;
                        self.bump();
                        self.bump();
                        let mut closed = false;
                        while let Some(c) = self.bump() {
                            if c == '*' && self.peek() == Some('/') {
                                self.bump();
                                closed = true;
                                break;
                            }
                        }
                        if !closed {
                            return Err(format!(
                                "unterminated /* */ comment starting on line {start}"
                            ));
                        }
                    }
                    _ => return Ok(()),
                }
            }
        }

        fn read_escape(&mut self, start_line: u32) -> Result<char, String> {
            let Some(esc) = self.bump() else {
                return Err(format!("unterminated escape on line {start_line}"));
            };
            match esc {
                'a' => Ok('\x07'),
                'b' => Ok('\x08'),
                'f' => Ok('\x0c'),
                'n' => Ok('\n'),
                'r' => Ok('\r'),
                't' => Ok('\t'),
                'v' => Ok('\x0b'),
                '\\' | '"' | '\'' => Ok(esc),
                'x' => self.read_hex_escape(2, start_line),
                'u' => self.read_hex_escape(4, start_line),
                'U' => self.read_hex_escape(8, start_line),
                '0'..='7' => self.read_octal_escape(esc, start_line),
                other => Err(format!("unknown Go escape \\{other} on line {start_line}")),
            }
        }

        fn read_hex_escape(&mut self, digits: usize, start_line: u32) -> Result<char, String> {
            let mut value = 0u32;
            for _ in 0..digits {
                let Some(ch) = self.bump() else {
                    return Err(format!("short hex escape on line {start_line}"));
                };
                let Some(d) = ch.to_digit(16) else {
                    return Err(format!(
                        "invalid hex escape digit {ch:?} on line {start_line}"
                    ));
                };
                value = value * 16 + d;
            }
            char::from_u32(value)
                .ok_or_else(|| format!("invalid Unicode scalar in escape on line {start_line}"))
        }

        fn read_octal_escape(&mut self, first: char, start_line: u32) -> Result<char, String> {
            let mut value = first.to_digit(8).unwrap_or(0);
            for _ in 0..2 {
                let Some(ch) = self.bump() else {
                    return Err(format!("short octal escape on line {start_line}"));
                };
                let Some(d) = ch.to_digit(8) else {
                    return Err(format!(
                        "invalid octal escape digit {ch:?} on line {start_line}"
                    ));
                };
                value = value * 8 + d;
            }
            if value > 0xff {
                return Err(format!("octal escape > 255 on line {start_line}"));
            }
            Ok(char::from(u8::try_from(value).unwrap_or(0)))
        }

        fn read_interpreted_string(&mut self, start_line: u32) -> Result<String, String> {
            self.bump();
            let mut out = String::new();
            while let Some(c) = self.bump() {
                match c {
                    '"' => return Ok(out),
                    '\n' => {
                        return Err(format!("newline in string literal on line {start_line}"));
                    }
                    '\\' => out.push(self.read_escape(start_line)?),
                    other => out.push(other),
                }
            }
            Err(format!("unterminated string literal on line {start_line}"))
        }

        fn read_raw_string(&mut self, start_line: u32) -> Result<String, String> {
            self.bump();
            let mut out = String::new();
            while let Some(c) = self.bump() {
                match c {
                    '`' => return Ok(out),
                    '\r' => {}
                    other => out.push(other),
                }
            }
            Err(format!("unterminated raw string on line {start_line}"))
        }

        fn next_token(&mut self) -> Result<Option<Tok>, String> {
            self.skip_ws_and_comments()?;
            let line = self.line;
            let Some(ch) = self.peek() else {
                return Ok(None);
            };
            let kind = match ch {
                '(' => {
                    self.bump();
                    TokKind::LParen
                }
                ')' => {
                    self.bump();
                    TokKind::RParen
                }
                ';' => {
                    self.bump();
                    TokKind::Semi
                }
                '.' => {
                    self.bump();
                    TokKind::Dot
                }
                '"' => TokKind::Str(self.read_interpreted_string(line)?),
                '`' => TokKind::Str(self.read_raw_string(line)?),
                c if c.is_alphabetic() || c == '_' => {
                    let mut ident = String::new();
                    while let Some(nc) = self.peek() {
                        if nc.is_alphanumeric() || nc == '_' {
                            ident.push(nc);
                            self.bump();
                        } else {
                            break;
                        }
                    }
                    TokKind::Ident(ident)
                }
                _ => {
                    self.bump();
                    TokKind::Other
                }
            };
            Ok(Some(Tok { kind, line }))
        }
    }

    fn parse_import_spec(lexer: &mut GoLexer, first: Tok) -> Result<GoImport, String> {
        match first.kind {
            TokKind::Str(path) => Ok(GoImport {
                path,
                alias: None,
                line: first.line,
            }),
            TokKind::Dot => expect_import_path(lexer, Some(".".to_string()), first.line),
            TokKind::Ident(alias) => expect_import_path(lexer, Some(alias), first.line),
            _ => Err(format!(
                "unexpected token in Go import on line {}",
                first.line
            )),
        }
    }

    fn expect_import_path(
        lexer: &mut GoLexer,
        alias: Option<String>,
        alias_line: u32,
    ) -> Result<GoImport, String> {
        let Some(next) = lexer.next_token()? else {
            return Err(format!(
                "expected import path after alias on line {alias_line}"
            ));
        };
        let TokKind::Str(path) = next.kind else {
            return Err(format!(
                "expected string literal import path on line {}",
                next.line
            ));
        };
        Ok(GoImport {
            path,
            alias,
            line: next.line,
        })
    }

    fn parse_import_block(lexer: &mut GoLexer, open_line: u32) -> Result<Vec<GoImport>, String> {
        let mut out = Vec::new();
        while let Some(tok) = lexer.next_token()? {
            match tok.kind {
                TokKind::Semi => {}
                TokKind::RParen => return Ok(out),
                _ => out.push(parse_import_spec(lexer, tok)?),
            }
        }
        Err(format!(
            "unterminated import block starting on line {open_line}"
        ))
    }

    fn parse_inner(source: &str) -> Result<GoFileFacts, String> {
        let mut lexer = GoLexer::new(source);
        let mut facts = GoFileFacts::default();

        let Some(first) = lexer.next_token()? else {
            return Ok(facts);
        };
        if first.kind != TokKind::Ident("package".to_string()) {
            return Err(format!("expected 'package' clause on line {}", first.line));
        }
        let Some(pkg_tok) = lexer.next_token()? else {
            return Err(format!(
                "expected package name after 'package' on line {}",
                first.line
            ));
        };
        let TokKind::Ident(pkg_name) = pkg_tok.kind else {
            return Err(format!(
                "invalid package name token on line {}",
                pkg_tok.line
            ));
        };
        facts.package_name = Some(pkg_name);

        while let Some(tok) = lexer.next_token()? {
            match tok.kind {
                TokKind::Semi => {}
                TokKind::Ident(ref kw) if kw == "import" => {
                    let Some(next) = lexer.next_token()? else {
                        return Err(format!(
                            "incomplete import declaration on line {}",
                            tok.line
                        ));
                    };
                    if next.kind == TokKind::LParen {
                        facts
                            .imports
                            .extend(parse_import_block(&mut lexer, next.line)?);
                    } else {
                        facts.imports.push(parse_import_spec(&mut lexer, next)?);
                    }
                }
                // Go allows `import` declarations only before the first top-level
                // `func`/`type`/`var`/`const` declaration.
                _ => break,
            }
        }

        Ok(facts)
    }

    /// Parse the `package` name and static `import` declarations of a Go file.
    pub fn parse(source: &str) -> GoFileFacts {
        match parse_inner(source) {
            Ok(facts) => facts,
            Err(message) => GoFileFacts {
                package_name: None,
                imports: Vec::new(),
                unparsed: Some(message),
            },
        }
    }
}

/// Whether the extractors read `path` (repository-relative): Rust sources
/// and `Cargo.toml` manifests, TypeScript / JavaScript sources and
/// `package.json`, `tsconfig*.json` and `jsconfig*.json` manifests, Python
/// sources (`.py` / `.pyi`) and `pyproject.toml` manifests, and Go sources
/// (`.go`) and `go.mod` manifests.
pub fn wants(path: &str) -> bool {
    is_rust_source(path)
        || is_cargo_manifest(path)
        || ecma::is_ecma_source(path)
        || is_package_json(path)
        || is_tsconfig(path)
        || python::is_python_source(path)
        || is_pyproject_toml(path)
        || go::is_go_source(path)
        || is_go_mod(path)
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A `go.mod`, at the root or in a subdirectory.
pub(crate) fn is_go_mod(path: &str) -> bool {
    file_name(path) == "go.mod"
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

#[cfg(test)]
mod go_tests {
    use super::{go, is_go_mod, wants};
    use spawn_core::feature_deps::Scope;

    #[test]
    fn wants_accepts_go_sources_and_gomod_and_rejects_go_sum() {
        assert!(wants("go.mod"));
        assert!(wants("services/edge/go.mod"));
        assert!(wants("main.go"));
        assert!(wants("internal/hub/hub_test.go"));
        assert!(is_go_mod("services/edge/go.mod"));

        assert!(!wants("go.sum"));
        assert!(!wants("services/edge/go.sum"));
        assert!(!wants("docs/go.mod.md"));
        assert_eq!(go::scope_of("hub/hub.go"), Scope::Prod);
        assert_eq!(go::scope_of("hub/hub_test.go"), Scope::Test);
    }

    #[test]
    fn parses_all_go_import_forms_and_ignores_comments_and_body_strings() {
        let src = r#"//go:build linux
/* header
   block comment mentioning import "fake/comment"
*/
package hub

import "fmt"
import (
    // line comment inside block
    "context"
    cfg "github.com/acme/svc/internal/config"
    . "github.com/acme/svc/pkg/dot"
    _ "github.com/acme/svc/pkg/initonly"
    `github.com/acme/svc/pkg/rawstr`
)
import alt "net/http"

var rawDecoy = `
import "github.com/acme/svc/internal/fake_raw"
`
const strDecoy = "import \"github.com/acme/svc/internal/fake_str\""
// import "github.com/acme/svc/internal/fake_line_comment"
/* import "github.com/acme/svc/internal/fake_block_comment" */
func Run() {
    _ = `package decoy; import "github.com/acme/svc/internal/nested"`
}
"#;
        let facts = go::parse(src);
        assert_eq!(facts.unparsed, None);
        assert_eq!(facts.package_name.as_deref(), Some("hub"));
        let got: Vec<(&str, Option<&str>, u32)> = facts
            .imports
            .iter()
            .map(|imp| (imp.path.as_str(), imp.alias.as_deref(), imp.line))
            .collect();
        assert_eq!(
            got,
            vec![
                ("fmt", None, 7),
                ("context", None, 10),
                ("github.com/acme/svc/internal/config", Some("cfg"), 11),
                ("github.com/acme/svc/pkg/dot", Some("."), 12),
                ("github.com/acme/svc/pkg/initonly", Some("_"), 13),
                ("github.com/acme/svc/pkg/rawstr", None, 14),
                ("net/http", Some("alt"), 16),
            ]
        );
    }

    #[test]
    fn reports_unparsed_on_malformed_go_headers_and_handles_empty_files() {
        let empty = go::parse("// empty placeholder\n/* ok */\n");
        assert_eq!(empty.package_name, None);
        assert!(empty.imports.is_empty());
        assert_eq!(empty.unparsed, None);

        assert!(
            go::parse("package hub\nimport (\n  \"fmt\"\n")
                .unparsed
                .is_some()
        );
        assert!(
            go::parse("/* unterminated\npackage hub\n")
                .unparsed
                .is_some()
        );
        assert!(
            go::parse("package hub\nimport `unterminated")
                .unparsed
                .is_some()
        );
        assert!(go::parse("package 123").unparsed.is_some());
    }
}
