//! The TypeScript / JavaScript extractor end to end, on string-table
//! fixtures (never directories): the lexer's strings, comments, templates
//! and regular expressions, relative / `paths` / workspace / `#imports`
//! resolution, barrels, dynamic imports, assets, test scope, units, lexer
//! safety on malformed input, and determinism (Spawn's own `web/` included).

use std::collections::{BTreeMap, BTreeSet};

use spawn_codegraph::{build, build_from_table};
use spawn_core::feature_deps::{
    CodeGraph, DiagnosticCode, EdgeKind, FileClass, Language, Precision, Scope, UnitKind,
};

use EdgeKind::{Asset, DynamicImport, Import, ImportType, Reexport, Require};
use Scope::{Prod, Test};

/// The edges as `(from, to, kind, scope, line)`.
fn edges(graph: &CodeGraph) -> Vec<(&str, &str, EdgeKind, Scope, u32)> {
    graph
        .edges
        .iter()
        .map(|e| (e.from.as_str(), e.to.as_str(), e.kind, e.scope, e.line))
        .collect()
}

/// The edges from `from` as `(to, kind, line)`.
fn edges_from<'g>(graph: &'g CodeGraph, from: &str) -> Vec<(&'g str, EdgeKind, u32)> {
    graph
        .edges
        .iter()
        .filter(|e| e.from == from)
        .map(|e| (e.to.as_str(), e.kind, e.line))
        .collect()
}

fn diagnostics(graph: &CodeGraph) -> Vec<(DiagnosticCode, &str, Option<u32>)> {
    graph
        .diagnostics
        .iter()
        .map(|d| (d.code, d.path.as_str(), d.line))
        .collect()
}

#[test]
fn imports_inside_strings_comments_and_templates_are_ignored() {
    let graph = build_from_table(&[
        (
            "src/main.ts",
            concat!(
                "// import a from './a';\n",
                "/* import b from './b';\n",
                "   require('./b'); */\n",
                "const s = \"import c from './c'\";\n",
                "const t = 'export * from \"./c\"';\n",
                "const u = `import d from './d' ${'x'} require('./d')`;\n",
                "const v = `line\n",
                "import e from './e'\n",
                "`;\n",
                "import { real } from './real';\n",
            ),
        ),
        ("src/a.ts", ""),
        ("src/b.ts", ""),
        ("src/c.ts", ""),
        ("src/d.ts", ""),
        ("src/e.ts", ""),
        ("src/real.ts", "export const real = 1;\n"),
    ]);
    assert_eq!(
        edges_from(&graph, "src/main.ts"),
        vec![("src/real.ts", Import, 10)]
    );
    assert!(graph.diagnostics.is_empty(), "{:?}", graph.diagnostics);
    let info = &graph.files["src/main.ts"];
    assert!(info.parsed);
    assert_eq!(info.language, Language::Typescript);
}

#[test]
fn a_regex_literal_does_not_open_a_string() {
    let graph = build_from_table(&[
        (
            "src/main.js",
            concat!(
                "const quote = /'/g;\n",
                "const dq = /[\"`]/;\n",
                "const slash = /\\/'/;\n",
                "const half = total / 2; const other = rate / 'x'.length;\n",
                "if (ok) { return /`/.test(s) }\n",
                "import { a } from './a.js';\n",
                "const b = require('./b');\n",
            ),
        ),
        ("src/a.ts", "export function a() {}\n"),
        ("src/b.js", "module.exports = {};\n"),
    ]);
    assert_eq!(
        edges_from(&graph, "src/main.js"),
        vec![("src/a.ts", Import, 6), ("src/b.js", Require, 7)]
    );
    assert_eq!(graph.files["src/main.js"].language, Language::Javascript);
}

#[test]
fn relative_imports_try_ts_for_js_and_index() {
    let graph = build_from_table(&[
        (
            "app/main.ts",
            concat!(
                "import './side.js';\n",
                "import x from './lib';\n",
                "import y from './comp.jsx';\n",
                "import z from './dir';\n",
                "import w from '../shared/util.mjs';\n",
                "import t from './types.js';\n",
                "import p from './pkgdir';\n",
                "import exact from './exact.js';\n",
                "import data from './data';\n",
                "import gone from './gone';\n",
            ),
        ),
        ("app/side.ts", ""),
        ("app/lib.tsx", ""),
        ("app/comp.tsx", ""),
        ("app/dir/index.ts", ""),
        ("shared/util.mts", ""),
        ("app/types.d.ts", ""),
        ("app/pkgdir/package.json", "{ \"main\": \"./entry.js\" }"),
        ("app/pkgdir/entry.ts", ""),
        ("app/exact.js", ""),
        ("app/exact.ts", ""),
        ("app/data.json", "{}"),
    ]);
    assert_eq!(
        edges_from(&graph, "app/main.ts"),
        vec![
            ("app/comp.tsx", Import, 3),
            // A specifier is an asset by what it reaches, not its spelling.
            ("app/data.json", Asset, 9),
            ("app/dir/index.ts", Import, 4),
            ("app/exact.js", Import, 8),
            ("app/lib.tsx", Import, 2),
            ("app/pkgdir/entry.ts", Import, 7),
            ("app/side.ts", Import, 1),
            ("app/types.d.ts", Import, 6),
            ("shared/util.mts", Import, 5),
        ]
    );
    assert_eq!(
        diagnostics(&graph),
        vec![(DiagnosticCode::Unresolved, "app/main.ts", Some(10))]
    );
}

#[test]
fn tsconfig_extends_and_paths_resolve() {
    let graph = build_from_table(&[
        (
            "tsconfig.base.json",
            concat!(
                "{\n",
                "  // shared options\n",
                "  \"compilerOptions\": {\n",
                "    \"baseUrl\": \".\",\n",
                "    /* aliases */\n",
                "    \"paths\": {\n",
                "      \"@lib/*\": [\"libs/*/src/index.ts\"],\n",
                "      \"@/*\": [\"missing/*\", \"app/src/*\"],\n",
                "      \"@/special/*\": [\"app/special/*\"],\n",
                "      \"exact\": [\"app/src/exact.ts\"],\n",
                "    },\n",
                "  },\n",
                "}\n",
            ),
        ),
        (
            "app/tsconfig.json",
            "{\n  \"extends\": \"../tsconfig.base\",\n  \"compilerOptions\": { \"strict\": true, },\n}\n",
        ),
        (
            "other/tsconfig.json",
            "{ \"extends\": [\"@tsconfig/node20/tsconfig.json\"], \"compilerOptions\": { \"baseUrl\": \"src\" } }\n",
        ),
        (
            "app/src/main.ts",
            concat!(
                "import { a } from '@lib/alpha';\n",
                "import b from '@/b';\n",
                "import s from '@/special/s';\n",
                "import e from 'exact';\n",
                "import r from 'react';\n",
                "import m from '@/nowhere';\n",
                "import fromBase from 'app/src/b';\n",
            ),
        ),
        ("libs/alpha/src/index.ts", "export const a = 1;\n"),
        ("app/src/b.ts", ""),
        ("app/special/s.ts", ""),
        ("app/src/exact.ts", ""),
        ("other/src/main.ts", "import u from 'util/x';\n"),
        ("other/src/util/x.ts", ""),
    ]);
    assert_eq!(
        edges_from(&graph, "app/src/main.ts"),
        vec![
            ("app/special/s.ts", Import, 3),
            ("app/src/b.ts", Import, 2),
            ("app/src/b.ts", Import, 7),
            ("app/src/exact.ts", Import, 4),
            ("libs/alpha/src/index.ts", Import, 1),
        ]
    );
    assert_eq!(
        edges_from(&graph, "other/src/main.ts"),
        vec![("other/src/util/x.ts", Import, 1)]
    );
    assert_eq!(graph.external_imports, 1, "react");
    assert_eq!(
        diagnostics(&graph),
        vec![
            (DiagnosticCode::Unresolved, "app/src/main.ts", Some(6)),
            (DiagnosticCode::Unsupported, "other/tsconfig.json", None),
        ]
    );
}

const WORKSPACE: &[(&str, &str)] = &[
    (
        "package.json",
        "{ \"name\": \"root\", \"private\": true, \"workspaces\": [\"packages/*\"] }",
    ),
    (
        "packages/domain/package.json",
        r#"{
  "name": "@acme/domain",
  "exports": {
    ".": { "types": "./dist/index.d.ts", "import": "./src/index.ts" },
    "./money": { "import": "./src/money.js", "require": "./dist/money.cjs" },
    "./features/*": "./src/features/*.ts",
    "./package.json": "./package.json"
  }
}"#,
    ),
    ("packages/domain/src/index.ts", "export const domain = 1;\n"),
    ("packages/domain/src/money.ts", "export const money = 1;\n"),
    (
        "packages/domain/src/features/cart.ts",
        "export const cart = 1;\n",
    ),
    (
        "packages/built/package.json",
        "{ \"name\": \"@acme/built\", \"main\": \"./dist/index.js\", \"types\": \"./dist/index.d.ts\" }",
    ),
    ("packages/built/src/index.ts", "export const built = 1;\n"),
    (
        "packages/bare/package.json",
        "{ \"name\": \"bare\", \"exports\": \"./dist/bare.js\" }",
    ),
    (
        "packages/app/package.json",
        r##"{
  "name": "@acme/app",
  "main": "./src/main.ts",
  "bin": { "acme": "./src/cli.ts" },
  "imports": { "#internal/*": "./src/internal/*.ts", "#dep": "@acme/domain" },
  "dependencies": { "@acme/domain": "workspace:*", "@acme/built": "workspace:*", "react": "^19" },
  "devDependencies": { "vitest": "^3" }
}"##,
    ),
    (
        "packages/app/src/main.ts",
        concat!(
            "import { domain } from '@acme/domain';\n",
            "import { money } from '@acme/domain/money';\n",
            "import { cart } from '@acme/domain/features/cart';\n",
            "import { built } from '@acme/built';\n",
            "import b from 'bare';\n",
            "import { helper } from '#internal/helper';\n",
            "import dep from '#dep';\n",
            "import React from 'react';\n",
            "import fs from 'node:fs';\n",
            "import { domain as again } from '@acme/domain-extra';\n",
            "import pkg from '@acme/domain/package.json';\n",
        ),
    ),
    ("packages/app/src/cli.ts", "import './main';\n"),
    (
        "packages/app/src/internal/helper.ts",
        "export const helper = 1;\n",
    ),
    (
        "packages/app/test/main.test.ts",
        "import { domain } from '@acme/domain';\nimport '../src/main';\n",
    ),
];

#[test]
fn a_workspace_package_resolves_through_exports() {
    let graph = build_from_table(WORKSPACE);
    assert_eq!(
        edges_from(&graph, "packages/app/src/main.ts"),
        vec![
            ("packages/app/src/internal/helper.ts", Import, 6),
            ("packages/bare/package.json", Import, 5),
            ("packages/built/src/index.ts", Import, 4),
            ("packages/domain/package.json", Asset, 11),
            ("packages/domain/src/features/cart.ts", Import, 3),
            ("packages/domain/src/index.ts", Import, 1),
            ("packages/domain/src/index.ts", Import, 7),
            ("packages/domain/src/money.ts", Import, 2),
        ]
    );
    let precision = |to: &str| {
        graph
            .edges
            .iter()
            .find(|e| e.from == "packages/app/src/main.ts" && e.to == to)
            .map(|e| e.precision)
    };
    // `dist/` is not in scope: `src/index.*`, else the manifest, at
    // package precision.
    assert_eq!(
        precision("packages/built/src/index.ts"),
        Some(Precision::Package)
    );
    assert_eq!(
        precision("packages/bare/package.json"),
        Some(Precision::Package)
    );
    assert_eq!(
        precision("packages/domain/src/money.ts"),
        Some(Precision::Item)
    );
    // react, node:fs, and @acme/domain-extra (not @acme/domain by prefix).
    assert_eq!(graph.external_imports, 3);
    assert_eq!(
        edges_from(&graph, "packages/app/test/main.test.ts"),
        vec![
            ("packages/app/src/main.ts", Import, 2),
            ("packages/domain/src/index.ts", Import, 1),
        ]
    );
    assert!(
        graph
            .edges
            .iter()
            .filter(|e| e.from.contains("/test/"))
            .all(|e| e.scope == Test)
    );
    assert_eq!(
        graph.files["packages/app/test/main.test.ts"].class,
        FileClass::Test
    );

    let units: Vec<(&str, &str, &Vec<String>, &Vec<String>)> = graph
        .units
        .iter()
        .map(|u| {
            (
                u.id.as_str(),
                u.root.as_str(),
                &u.entry_files,
                &u.declared_units,
            )
        })
        .collect();
    let s = |items: &[&str]| items.iter().map(|i| (*i).to_string()).collect::<Vec<_>>();
    assert_eq!(
        units,
        vec![
            (
                "npm:@acme/app",
                "packages/app",
                &s(&["packages/app/src/cli.ts", "packages/app/src/main.ts"]),
                &s(&["npm:@acme/built", "npm:@acme/domain"]),
            ),
            (
                "npm:@acme/built",
                "packages/built",
                &s(&["packages/built/src/index.ts"]),
                &s(&[]),
            ),
            (
                "npm:@acme/domain",
                "packages/domain",
                // `*` patterns and non-code targets are no entry files.
                &s(&[
                    "packages/domain/src/index.ts",
                    "packages/domain/src/money.ts"
                ]),
                &s(&[]),
            ),
            ("npm:bare", "packages/bare", &s(&[]), &s(&[])),
            ("npm:root", "", &s(&[]), &s(&[])),
        ]
    );
    assert!(graph.units.iter().all(|u| u.kind == UnitKind::NpmPackage));
    assert_eq!(
        graph.files["packages/app/src/main.ts"].unit.as_deref(),
        Some("npm:@acme/app")
    );
    assert!(graph.files["packages/app/src/main.ts"].is_entry);
    assert!(graph.diagnostics.is_empty(), "{:?}", graph.diagnostics);
}

const BARREL: &[(&str, &str)] = &[
    (
        "src/main.ts",
        concat!(
            "import { Button, Card as C, Icon, Missing } from './ui';\n",
            "import * as ui from './ui';\n",
            "import Default from './ui';\n",
            "import type { Theme } from './ui';\n",
            "import { Label } from './ui/index.ts';\n",
        ),
    ),
    (
        "src/ui/index.ts",
        concat!(
            "export { Button } from './Button';\n",
            "export { default as Card } from './Card';\n",
            "export * from './icons';\n",
            "export type { Theme } from './theme';\n",
            "import { Label } from './Label';\n",
            "export { Label };\n",
            "export default 1;\n",
        ),
    ),
    ("src/ui/Button.tsx", "export function Button() {}\n"),
    ("src/ui/Card.tsx", "export default function Card() {}\n"),
    (
        "src/ui/icons/index.ts",
        "export * from './Icon';\nexport * from './Other';\n",
    ),
    ("src/ui/icons/Icon.tsx", "export const Icon = 1;\n"),
    ("src/ui/icons/Other.tsx", "export const Other = 1;\n"),
    ("src/ui/theme.ts", "export interface Theme {}\n"),
    ("src/ui/Label.tsx", "export class Label {}\n"),
];

#[test]
fn a_named_import_follows_the_barrel() {
    let graph = build_from_table(BARREL);
    let from_main: Vec<(&str, EdgeKind, u32, Option<&str>, Precision)> = graph
        .edges
        .iter()
        .filter(|e| e.from == "src/main.ts")
        .map(|e| (e.to.as_str(), e.kind, e.line, e.via.as_deref(), e.precision))
        .collect();
    let via = Some("src/ui/index.ts");
    assert_eq!(
        from_main,
        vec![
            ("src/ui/Button.tsx", Import, 1, via, Precision::Item),
            ("src/ui/Card.tsx", Import, 1, via, Precision::Item),
            ("src/ui/Label.tsx", Import, 5, via, Precision::Item),
            ("src/ui/icons/Icon.tsx", Import, 1, via, Precision::Item),
            // `Missing` is nowhere: the import stops at the barrel; so do the
            // namespace and default imports.
            ("src/ui/index.ts", Import, 1, None, Precision::Module),
            ("src/ui/index.ts", Import, 2, None, Precision::Module),
            ("src/ui/index.ts", Import, 3, None, Precision::Module),
            ("src/ui/theme.ts", ImportType, 4, via, Precision::Item),
        ]
    );
    // The barrel's own re-exports are structural.
    assert_eq!(
        edges_from(&graph, "src/ui/index.ts"),
        vec![
            ("src/ui/Button.tsx", Reexport, 1),
            ("src/ui/Card.tsx", Reexport, 2),
            ("src/ui/Label.tsx", Import, 5),
            ("src/ui/icons/index.ts", Reexport, 3),
            ("src/ui/theme.ts", ImportType, 4),
        ]
    );
}

/// `src/main.ts` imports `{ Deep }` through `hops` barrels, each
/// re-exporting the next.
fn barrel_chain(hops: usize) -> Vec<(String, String)> {
    let mut table = vec![(
        "src/main.ts".to_string(),
        "import { Deep } from './b0';\n".to_string(),
    )];
    for at in 0..hops {
        table.push((
            format!("src/b{at}.ts"),
            format!("export {{ Deep }} from './b{}';\n", at + 1),
        ));
    }
    table.push((
        format!("src/b{hops}.ts"),
        "export const Deep = 1;\n".to_string(),
    ));
    table
}

fn build_owned(table: &[(String, String)]) -> CodeGraph {
    let borrowed: Vec<(&str, &str)> = table
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    build_from_table(&borrowed)
}

#[test]
fn a_barrel_chain_past_the_depth_cap_is_a_diagnostic_not_a_guess() {
    let graph = build_owned(&barrel_chain(8));
    let edge = graph
        .edges
        .iter()
        .find(|e| e.from == "src/main.ts")
        .expect("an edge");
    assert_eq!(
        (edge.to.as_str(), edge.via.as_deref()),
        ("src/b8.ts", Some("src/b0.ts"))
    );

    let graph = build_owned(&barrel_chain(9));
    assert!(graph.edges.iter().all(|e| e.from != "src/main.ts"));
    assert_eq!(
        diagnostics(&graph),
        vec![(DiagnosticCode::ReexportTooDeep, "src/main.ts", Some(1))]
    );

    // A cycle of barrels ends at the barrel, never loops.
    let graph = build_from_table(&[
        ("src/main.ts", "import { X } from './a';\n"),
        ("src/a.ts", "export * from './b';\n"),
        ("src/b.ts", "export * from './a';\n"),
    ]);
    assert_eq!(
        edges_from(&graph, "src/main.ts"),
        vec![("src/a.ts", Import, 1)]
    );
}

#[test]
fn a_non_literal_dynamic_import_is_counted_not_guessed() {
    let graph = build_from_table(&[
        (
            "src/routes.ts",
            concat!(
                "const page = await import('./pages/home');\n",
                "const other = await import(`./pages/${name}`);\n",
                "const third = import(path);\n",
                "const plain = import(`./pages/about`);\n",
                "const meta = import.meta.url;\n",
            ),
        ),
        ("src/pages/home.ts", ""),
        ("src/pages/about.ts", ""),
    ]);
    assert_eq!(
        edges_from(&graph, "src/routes.ts"),
        vec![
            ("src/pages/about.ts", DynamicImport, 4),
            ("src/pages/home.ts", DynamicImport, 1),
        ]
    );
    assert_eq!(
        diagnostics(&graph),
        vec![
            (DiagnosticCode::DynamicUnresolved, "src/routes.ts", Some(2)),
            (DiagnosticCode::DynamicUnresolved, "src/routes.ts", Some(3)),
        ]
    );
}

#[test]
fn asset_imports_make_asset_edges() {
    let graph = build_from_table(&[
        (
            "src/App.tsx",
            concat!(
                "import './App.css';\n",
                "import logo from './logo.svg?url';\n",
                "import data from './data.json';\n",
                "import raw from './notes.md?raw';\n",
                "import katex from 'katex/dist/katex.css';\n",
                "import gone from './gone.png';\n",
                "const icon = new URL('./icon.png', import.meta.url);\n",
                "import App from './App.vue';\n",
            ),
        ),
        ("src/App.css", ""),
        ("src/logo.svg", ""),
        ("src/data.json", "{}"),
        ("src/notes.md", ""),
        ("src/App.vue", "<template></template>\n"),
    ]);
    assert_eq!(
        edges_from(&graph, "src/App.tsx"),
        vec![
            ("src/App.css", Asset, 1),
            ("src/App.vue", Import, 8),
            ("src/data.json", Asset, 3),
            ("src/logo.svg", Asset, 2),
            ("src/notes.md", Asset, 4),
        ]
    );
    assert_eq!(graph.external_imports, 1, "katex");
    assert_eq!(
        diagnostics(&graph),
        vec![
            (DiagnosticCode::Unresolved, "src/App.tsx", Some(6)),
            (DiagnosticCode::Unsupported, "src/App.vue", None),
        ]
    );
    assert!(!graph.files.contains_key("src/App.vue"), "never read");
}

#[test]
fn require_and_type_imports_and_test_scope() {
    let graph = build_from_table(&[
        (
            "lib/index.cjs",
            "const a = require('./a');\nconst b = require(name);\nmodule.exports = { a };\n",
        ),
        ("lib/a.cjs", ""),
        (
            "src/x.spec.ts",
            "import type { T } from './t';\nimport u = require('./u');\nexport type { T } from './t';\n",
        ),
        ("src/t.ts", "export type T = number;\n"),
        ("src/u.ts", ""),
        ("src/__tests__/y.js", "require('../u');\n"),
    ]);
    assert_eq!(
        edges(&graph),
        vec![
            ("lib/index.cjs", "lib/a.cjs", Require, Prod, 1),
            ("src/__tests__/y.js", "src/u.ts", Require, Test, 1),
            ("src/x.spec.ts", "src/t.ts", ImportType, Test, 1),
            ("src/x.spec.ts", "src/t.ts", ImportType, Test, 3),
            ("src/x.spec.ts", "src/u.ts", Require, Test, 2),
        ]
    );
    assert!(graph.diagnostics.is_empty(), "{:?}", graph.diagnostics);
}

#[test]
fn malformed_and_deeply_nested_input_never_panics() {
    let mut deep_template = String::from("const s = ");
    for _ in 0..40 {
        deep_template.push_str("`${");
    }
    deep_template.push('1');
    for _ in 0..40 {
        deep_template.push_str("}`");
    }
    deep_template.push_str(";\nimport a from './a';\n");
    let mut shallow_template = String::from("const s = ");
    for _ in 0..20 {
        shallow_template.push_str("`${");
    }
    for _ in 0..20 {
        shallow_template.push_str("}`");
    }
    shallow_template.push_str(";\nimport a from './a';\n");
    let brackets = format!(
        "{}{}\nimport a from './a';\n",
        "({[".repeat(50_000),
        "]})".repeat(50_000)
    );
    let unbalanced = format!("{}\nimport a from './a';\n", "}`)]".repeat(10_000));
    let table: Vec<(&str, &str)> = vec![
        ("src/deep.ts", deep_template.as_str()),
        ("src/shallow.ts", shallow_template.as_str()),
        ("src/brackets.ts", brackets.as_str()),
        ("src/unbalanced.ts", unbalanced.as_str()),
        (
            "src/open-comment.ts",
            "import a from './a';\n/* never closed",
        ),
        (
            "src/open-string.ts",
            "import a from './a';\nconst s = 'never closed\nimport b from './b';\n",
        ),
        (
            "src/open-template.ts",
            "import a from './a';\nconst t = `never ${ closed",
        ),
        (
            "src/open-regex.ts",
            "import a from './a';\nconst r = /never closed\n",
        ),
        (
            "src/garbage.ts",
            "\u{feff}#!/usr/bin/env node\nimport { from './a'; import * as from; export { ;\nimport('\n",
        ),
        ("src/nul.js", "\0\0\u{1}import\0from'./a'"),
        ("src/a.ts", ""),
        ("src/b.ts", ""),
    ];
    let graph = build_from_table(&table);
    assert!(!graph.files["src/deep.ts"].parsed);
    assert!(
        diagnostics(&graph).contains(&(DiagnosticCode::Unparsed, "src/deep.ts", None)),
        "{:?}",
        graph.diagnostics
    );
    assert!(edges_from(&graph, "src/deep.ts").is_empty());
    // A block comment or template that never closes would hide the rest of
    // the file: unparsed, visibly, never `parsed` with edges missing.
    for unparsed in ["src/open-comment.ts", "src/open-template.ts"] {
        assert!(!graph.files[unparsed].parsed, "{unparsed}");
        assert!(edges_from(&graph, unparsed).is_empty(), "{unparsed}");
        assert!(diagnostics(&graph).contains(&(DiagnosticCode::Unparsed, unparsed, None)));
    }
    for parsed in [
        "src/shallow.ts",
        "src/brackets.ts",
        "src/unbalanced.ts",
        "src/open-string.ts",
        "src/open-regex.ts",
    ] {
        assert!(graph.files[parsed].parsed, "{parsed}");
        assert!(
            edges_from(&graph, parsed).contains(&(
                "src/a.ts",
                Import,
                if parsed.contains("open-") { 1 } else { 2 }
            )),
            "{parsed}: {:?}",
            edges_from(&graph, parsed)
        );
    }
    // An unterminated string ends at its line: the next line is code.
    assert!(edges_from(&graph, "src/open-string.ts").contains(&("src/b.ts", Import, 3)));
    // Whatever it made of garbage, it made it twice the same way.
    assert_eq!(
        serde_json::to_string(&graph).expect("json"),
        serde_json::to_string(&build_from_table(&table)).expect("json")
    );
}

#[test]
fn rust_and_js_ts_share_one_graph() {
    let graph = build_from_table(&[
        (
            "Cargo.toml",
            "[package]\nname = \"poly\"\nversion = \"0.1.0\"\n",
        ),
        ("src/lib.rs", "pub mod util;\n"),
        ("src/util.rs", "pub fn f() {}\n"),
        ("web/package.json", "{ \"name\": \"poly-web\" }"),
        ("web/src/main.ts", "import { f } from './util';\n"),
        ("web/src/util.ts", "export function f() {}\n"),
    ]);
    let units: Vec<&str> = graph.units.iter().map(|u| u.id.as_str()).collect();
    assert_eq!(units, vec!["cargo:poly", "npm:poly-web"]);
    assert_eq!(graph.files["src/util.rs"].language, Language::Rust);
    assert_eq!(
        graph.files["web/src/util.ts"].language,
        Language::Typescript
    );
    assert!(edges(&graph).contains(&("src/lib.rs", "src/util.rs", EdgeKind::Mod, Prod, 1)));
    assert!(edges(&graph).contains(&("web/src/main.ts", "web/src/util.ts", Import, Prod, 1)));
}

#[test]
fn manifests_that_cannot_be_read_are_listed_not_guessed() {
    let graph = build_from_table(&[
        ("a/package.json", "{ \"name\": \"a\", }"),
        ("b/package.json", "{ \"name\": \"dup\" }"),
        ("c/package.json", "{ \"name\": \"dup\" }"),
        ("d/tsconfig.json", "{ \"compilerOptions\": { \"paths\": [ }"),
        ("e/tsconfig.json", "{ \"extends\": \"./missing.json\" }"),
        ("f/tsconfig.json", "{ \"extends\": \"./g.json\" }"),
        ("f/g.json", "{}"),
        ("h/tsconfig.json", "{ \"extends\": \"./tsconfig.json\" }"),
    ]);
    assert!(graph.units.is_empty(), "{:?}", graph.units);
    assert_eq!(
        diagnostics(&graph),
        vec![
            (DiagnosticCode::Unresolved, "e/tsconfig.json", None),
            (DiagnosticCode::Unresolved, "f/tsconfig.json", None),
            (DiagnosticCode::Unsupported, "h/tsconfig.json", None),
            (DiagnosticCode::ManifestUnsupported, "a/package.json", None),
            (DiagnosticCode::ManifestUnsupported, "b/package.json", None),
            (DiagnosticCode::ManifestUnsupported, "c/package.json", None),
            (DiagnosticCode::ManifestUnsupported, "d/tsconfig.json", None),
        ]
    );
}

#[test]
fn js_ts_graph_is_byte_identical_across_runs_and_input_orders() {
    let mut table: Vec<(&str, &str)> = WORKSPACE.iter().chain(BARREL).copied().collect();
    let first = serde_json::to_string(&build_from_table(&table)).expect("json");
    let second = serde_json::to_string(&build_from_table(&table)).expect("json");
    table.reverse();
    let third = serde_json::to_string(&build_from_table(&table)).expect("json");
    assert_eq!(first, second);
    assert_eq!(first, third, "input order does not matter");
}

/// Spawn's own `web/` sources, read from the repository: the graph of the
/// real front end is built twice to the same bytes, and every relative
/// import of `web/src` and `web/test` resolves.
#[test]
fn spawns_own_web_sources_make_a_byte_identical_graph() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut inventory = BTreeSet::new();
    let mut sources = BTreeMap::new();
    let mut stack = vec![repo.join("web")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if !matches!(name.as_str(), "node_modules" | "dist" | ".vite") {
                    stack.push(path);
                }
                continue;
            }
            let rel = path
                .strip_prefix(&repo)
                .expect("under the repository")
                .to_string_lossy()
                .replace('\\', "/");
            if spawn_codegraph::wants(&rel)
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                sources.insert(rel.clone(), text);
            }
            inventory.insert(rel);
        }
    }
    assert!(sources.contains_key("web/package.json"), "web/ is read");
    let first = build(&inventory, &sources);
    let second = build(&inventory, &sources);
    let first_json = serde_json::to_string(&first).expect("json");
    assert_eq!(first_json, serde_json::to_string(&second).expect("json"));
    assert!(first.units.iter().any(|u| u.id == "npm:spawn-web"));
    assert!(first.edges.len() > 100, "{} edges", first.edges.len());
    assert!(first.files.values().all(|info| info.parsed));
    let unresolved: Vec<_> = first
        .diagnostics
        .iter()
        .filter(|d| d.code == DiagnosticCode::Unresolved || d.code == DiagnosticCode::Unparsed)
        .collect();
    assert!(unresolved.is_empty(), "{unresolved:?}");
}

#[test]
fn dotted_file_names_resolve_like_any_module() {
    let graph = build_from_table(&[
        (
            "packages/shared/package.json",
            "{ \"name\": \"@fx/shared\", \"exports\": { \"./*\": \"./src/*.ts\" } }",
        ),
        (
            "packages/shared/src/date.utils.ts",
            "export const day = 1;\n",
        ),
        (
            "app/main.ts",
            concat!(
                "import { AppComponent } from './app.component';\n",
                "import { UserService } from './user.service';\n",
                "import config from './vite.config';\n",
                "import type { X } from './x.types';\n",
                "import v from './v1.2';\n",
                "import { day } from '@fx/shared/date.utils';\n",
                "import styles from './a.module.css';\n",
                "import data from './data.json';\n",
                "import raw from './notes.md?raw';\n",
                "import data2 from './data';\n",
                "import gone from './gone.service';\n",
            ),
        ),
        ("app/app.component.ts", "export class AppComponent {}\n"),
        ("app/user.service.tsx", "export class UserService {}\n"),
        ("app/vite.config.ts", "export default {};\n"),
        ("app/x.types.d.ts", "export type X = 1;\n"),
        ("app/v1.2/index.ts", "export default 1;\n"),
        ("app/a.module.css", ".a {}\n"),
        ("app/data.json", "{}"),
        ("app/notes.md", "# notes\n"),
    ]);
    assert_eq!(
        edges_from(&graph, "app/main.ts"),
        vec![
            ("app/a.module.css", Asset, 7),
            ("app/app.component.ts", Import, 1),
            ("app/data.json", Asset, 8),
            ("app/data.json", Asset, 10),
            ("app/notes.md", Asset, 9),
            ("app/user.service.tsx", Import, 2),
            ("app/v1.2/index.ts", Import, 5),
            ("app/vite.config.ts", Import, 3),
            ("app/x.types.d.ts", ImportType, 4),
            ("packages/shared/src/date.utils.ts", Import, 6),
        ]
    );
    assert_eq!(
        diagnostics(&graph),
        vec![(DiagnosticCode::Unresolved, "app/main.ts", Some(11))]
    );
}

#[test]
fn import_calls_are_types_in_declaration_files_and_type_positions_only() {
    let graph = build_from_table(&[
        (
            "src/routes.ts",
            concat!(
                "const Page = lazy(() => import('./page').then((m) => ({ default: m.Page })));\n",
                "import('./side').catch(report).finally(done);\n",
                "type P = typeof import('./page');\n",
                "type Q = import('./page').Props;\n",
                "const all = { ...require('./table') };\n",
                "const list = [...require('./list')];\n",
            ),
        ),
        (
            "src/env.d.ts",
            "declare const page: Promise<typeof import('./page')>;\ndeclare const later: import('./side').then;\n",
        ),
        ("src/page.tsx", "export function Page() {}\n"),
        ("src/side.ts", ""),
        ("src/table.js", ""),
        ("src/list.cjs", ""),
    ]);
    assert_eq!(
        edges(&graph),
        vec![
            ("src/env.d.ts", "src/page.tsx", ImportType, Prod, 1),
            ("src/env.d.ts", "src/side.ts", ImportType, Prod, 2),
            ("src/routes.ts", "src/list.cjs", Require, Prod, 6),
            ("src/routes.ts", "src/page.tsx", ImportType, Prod, 3),
            ("src/routes.ts", "src/page.tsx", ImportType, Prod, 4),
            ("src/routes.ts", "src/page.tsx", DynamicImport, Prod, 1),
            ("src/routes.ts", "src/side.ts", DynamicImport, Prod, 2),
            ("src/routes.ts", "src/table.js", Require, Prod, 5),
        ]
    );
}

#[test]
fn an_exports_condition_out_of_scope_falls_through_to_the_next_in_scope() {
    let graph = build_from_table(&[
        (
            "packages/lib/package.json",
            r#"{
  "name": "@fx/lib",
  "exports": {
    ".": { "import": "./dist/index.js", "default": "./dist/index.cjs", "types": "./src/index.ts" },
    "./utils": { "types": "./src/utils.ts", "import": "./dist/utils.js" },
    "./gone": { "import": "./dist/gone.js", "types": "./dist/gone.d.ts" },
    "./nested": { "node": { "import": "./dist/n.js", "default": "./src/n.ts" } }
  }
}"#,
        ),
        ("packages/lib/src/index.ts", "export const lib = 1;\n"),
        ("packages/lib/src/utils.ts", "export const util = 1;\n"),
        ("packages/lib/src/n.ts", "export const n = 1;\n"),
        (
            "app/main.ts",
            concat!(
                "import '@fx/lib';\n",
                "import '@fx/lib/utils';\n",
                "import '@fx/lib/gone';\n",
                "import '@fx/lib/nested';\n",
            ),
        ),
    ]);
    let found: Vec<(&str, u32, Precision)> = graph
        .edges
        .iter()
        .filter(|e| e.from == "app/main.ts")
        .map(|e| (e.to.as_str(), e.line, e.precision))
        .collect();
    assert_eq!(
        found,
        vec![
            // No listed condition is in scope: the package fallback.
            ("packages/lib/src/index.ts", 1, Precision::Module),
            ("packages/lib/src/index.ts", 3, Precision::Package),
            ("packages/lib/src/n.ts", 4, Precision::Module),
            ("packages/lib/src/utils.ts", 2, Precision::Module),
        ]
    );
}

/// Every config extends the next two, each written three times: without
/// memoising and deduplicating, the merge visits billions of configs (it
/// does not finish in minutes); with both, each config once.
#[test]
fn a_diamond_of_tsconfig_extends_stays_linear() {
    let name = |at: usize| {
        if at == 0 {
            "tsconfig.json".to_string()
        } else {
            format!("tsconfig.c{at}.json")
        }
    };
    let mut table: Vec<(String, String)> = (0..16)
        .map(|at| {
            let text = if at < 14 {
                let (a, b) = (name(at + 1), name(at + 2));
                format!("{{ \"extends\": [\"./{a}\", \"./{b}\", \"./{a}\", \"./{b}\", \"./{a}\", \"./{b}\"] }}")
            } else {
                "{ \"compilerOptions\": { \"baseUrl\": \".\", \"paths\": { \"@/*\": [\"src/*\"] } } }"
                    .to_string()
            };
            (name(at), text)
        })
        .collect();
    table.push((
        "src/main.ts".to_string(),
        "import x from '@/x';\n".to_string(),
    ));
    table.push(("src/x.ts".to_string(), String::new()));
    let started = std::time::Instant::now();
    let graph = build_owned(&table);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        edges_from(&graph, "src/main.ts"),
        vec![("src/x.ts", Import, 1)]
    );
    assert!(graph.diagnostics.is_empty(), "{:?}", graph.diagnostics);
}
