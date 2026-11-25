//! The Rust extractor end to end, on string-table fixtures
//! (`fixtures/mod.rs`): module trees, `use` trees, macro bodies,
//! re-exports, test scope, includes, orphans, manifests, determinism.

mod fixtures;

use spawn_codegraph::{build_from_table, wants};
use spawn_core::feature_deps::{
    CodeGraph, DiagnosticCode, EdgeKind, FileClass, FileEdge, Precision, Scope,
};

/// The edges as `(from, to, kind, scope, line)`.
fn edges(graph: &CodeGraph) -> Vec<(&str, &str, EdgeKind, Scope, u32)> {
    graph
        .edges
        .iter()
        .map(|e| (e.from.as_str(), e.to.as_str(), e.kind, e.scope, e.line))
        .collect()
}

fn edge<'g>(graph: &'g CodeGraph, from: &str, to: &str, kind: EdgeKind) -> Vec<&'g FileEdge> {
    graph
        .edges
        .iter()
        .filter(|e| e.from == from && e.to == to && e.kind == kind)
        .collect()
}

fn diagnostics(graph: &CodeGraph) -> Vec<(DiagnosticCode, &str, Option<u32>)> {
    graph
        .diagnostics
        .iter()
        .map(|d| (d.code, d.path.as_str(), d.line))
        .collect()
}

use EdgeKind::{Include, Mod, Path, Reexport, Use};
use Scope::{Prod, Test};

#[test]
fn polyglot_rust_graph_has_exactly_the_expected_edges() {
    let graph = build_from_table(fixtures::POLYGLOT_RUST);
    assert_eq!(
        edges(&graph),
        vec![
            (
                "crates/app/src/cli.rs",
                "crates/core/src/model.rs",
                Use,
                Prod,
                1
            ),
            (
                "crates/app/src/cli.rs",
                "crates/core/src/util.rs",
                Path,
                Prod,
                5
            ),
            (
                "crates/app/src/main.rs",
                "crates/app/src/cli.rs",
                Path,
                Prod,
                5
            ),
            (
                "crates/app/src/main.rs",
                "crates/app/src/cli.rs",
                Mod,
                Prod,
                1
            ),
            (
                "crates/app/src/main.rs",
                "crates/app/src/island.rs",
                Mod,
                Prod,
                2
            ),
            (
                "crates/app/tests/smoke.rs",
                "crates/core/src/model.rs",
                Use,
                Test,
                1
            ),
            (
                "crates/core/src/lib.rs",
                "crates/core/src/model.rs",
                Mod,
                Prod,
                1
            ),
            (
                "crates/core/src/lib.rs",
                "crates/core/src/model.rs",
                Reexport,
                Prod,
                3
            ),
            (
                "crates/core/src/lib.rs",
                "crates/core/src/util.rs",
                Mod,
                Prod,
                2
            ),
            (
                "crates/core/src/model.rs",
                "crates/core/src/util.rs",
                Use,
                Prod,
                1
            ),
        ]
    );
}

#[test]
fn units_carry_entry_files_and_declared_in_repo_dependencies() {
    let graph = build_from_table(fixtures::POLYGLOT_RUST);
    let units: Vec<_> = graph
        .units
        .iter()
        .map(|u| {
            (
                u.id.as_str(),
                u.root.as_str(),
                u.entry_files.clone(),
                u.declared_units.clone(),
            )
        })
        .collect();
    assert_eq!(
        units,
        vec![
            (
                "cargo:fx-app",
                "crates/app",
                vec!["crates/app/src/main.rs".to_string()],
                vec!["cargo:fx-core".to_string()],
            ),
            (
                "cargo:fx-core",
                "crates/core",
                vec!["crates/core/src/lib.rs".to_string()],
                Vec::new(),
            ),
        ]
    );
    // serde_json and serde_json::json are outside the repository.
    assert_eq!(graph.external_imports, 1);
    assert!(graph.diagnostics.is_empty(), "{:?}", graph.diagnostics);
    let smoke = &graph.files["crates/app/tests/smoke.rs"];
    assert_eq!(smoke.class, FileClass::Test);
    assert!(smoke.is_entry && smoke.in_module_tree && smoke.parsed);
    assert_eq!(smoke.unit.as_deref(), Some("cargo:fx-app"));
    assert_eq!(
        graph.files.len(),
        7,
        "only Rust sources are files of the graph"
    );
}

#[test]
fn module_tree_follows_mod_path_attr_and_inline_mods() {
    let graph = build_from_table(fixtures::MODULE_TREE);
    let mods: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.kind == Mod)
        .map(|e| (e.from.as_str(), e.to.as_str(), e.line))
        .collect();
    assert_eq!(
        mods,
        vec![
            ("src/a.rs", "src/a/b.rs", 1),
            ("src/dir/mod.rs", "src/dir/leaf.rs", 1),
            ("src/lib.rs", "src/a.rs", 10),
            ("src/lib.rs", "src/dir/mod.rs", 9),
            ("src/lib.rs", "src/impls/real.rs", 2),
            ("src/lib.rs", "src/outer/inner.rs", 4),
            ("src/lib.rs", "src/outer/nested/deep.rs", 6),
        ]
    );
    assert!(graph.files.values().all(|info| info.in_module_tree));
    assert_eq!(
        diagnostics(&graph),
        vec![(DiagnosticCode::Unresolved, "src/lib.rs", Some(11))]
    );
    assert!(graph.diagnostics[0].message.contains("mod missing;"));
}

#[test]
fn use_trees_aliases_and_super_resolve() {
    let graph = build_from_table(fixtures::USE_TREES);
    let found: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.kind != Mod)
        .map(|e| (e.from.as_str(), e.to.as_str(), e.kind, e.line, e.precision))
        .collect();
    assert_eq!(
        found,
        vec![
            // `Thing as T` names the item; `self as bee` the module.
            ("src/a.rs", "src/b.rs", Use, 1, Precision::Item),
            ("src/a.rs", "src/b.rs", Use, 1, Precision::Module),
            // `bee::g()` goes through the alias.
            ("src/a.rs", "src/b.rs", Path, 4, Precision::Item),
            ("src/b/c.rs", "src/a.rs", Use, 2, Precision::Module),
            ("src/b/c.rs", "src/b.rs", Use, 1, Precision::Item),
        ]
    );
}

#[test]
fn private_imports_and_glob_names_resolve_from_child_modules() {
    let graph = build_from_table(fixtures::PRIVATE_IMPORTS);
    let found: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.from == "src/nodes/commands.rs")
        .map(|e| (e.to.as_str(), e.kind, e.line, e.via.as_deref(), e.precision))
        .collect();
    assert_eq!(
        found,
        vec![
            (
                "src/config.rs",
                Path,
                4,
                Some("src/nodes.rs"),
                Precision::Item
            ),
            (
                "src/config.rs",
                Path,
                6,
                Some("src/nodes.rs"),
                Precision::Item
            ),
            ("src/nodes.rs", Use, 1, None, Precision::Module),
        ]
    );
}

#[test]
fn a_reexport_also_used_locally_keeps_its_dependency_edge() {
    let graph = build_from_table(fixtures::REEXPORT_USED_LOCALLY);
    let found: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.from == "src/api.rs")
        .map(|e| (e.to.as_str(), e.kind, e.line, e.precision))
        .collect();
    assert_eq!(
        found,
        vec![
            ("src/thing.rs", Use, 3, Precision::Item),
            ("src/thing.rs", Reexport, 1, Precision::Item),
        ]
    );
}

/// `src/lib.rs` declares `m0` .. `m{hops}`; each `m<i>` re-exports `X`
/// from the next, and the last defines it.
fn reexport_chain(hops: usize) -> Vec<(String, String)> {
    let mut lib: String = (0..=hops).fold(String::new(), |mut lib, i| {
        lib.push_str("mod m");
        lib.push_str(&i.to_string());
        lib.push_str(";\n");
        lib
    });
    lib.push_str("pub fn user() {\n    let _ = crate::m0::X;\n    let _ = crate::m5::X;\n}\n");
    let mut table = vec![
        (
            "Cargo.toml".to_string(),
            "[package]\nname = \"chain\"\nversion = \"0.1.0\"\n".to_string(),
        ),
        ("src/lib.rs".to_string(), lib),
    ];
    for i in 0..hops {
        table.push((
            format!("src/m{i}.rs"),
            format!("pub use crate::m{}::X;\n", i + 1),
        ));
    }
    table.push((format!("src/m{hops}.rs"), "pub struct X;\n".to_string()));
    table
}

#[test]
fn a_reexport_chain_past_the_depth_cap_is_a_diagnostic_not_a_guess() {
    assert_eq!(spawn_codegraph::resolve::MAX_REEXPORT_DEPTH, 8);
    let owned = reexport_chain(11);
    let table: Vec<(&str, &str)> = owned
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    let graph = build_from_table(&table);
    // m0 is 11 re-exports from X: no edge is claimed, a diagnostic says so.
    let too_deep: Vec<_> = graph
        .diagnostics
        .iter()
        .filter(|d| d.code == DiagnosticCode::ReexportTooDeep)
        .map(|d| (d.path.as_str(), d.line))
        .collect();
    assert_eq!(
        too_deep,
        vec![
            ("src/lib.rs", Some(14)),
            ("src/m0.rs", Some(1)),
            ("src/m1.rs", Some(1)),
        ]
    );
    let from_lib: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.from == "src/lib.rs" && e.kind == Path)
        .map(|e| (e.to.as_str(), e.line, e.via.as_deref(), e.precision))
        .collect();
    // m5 is 6 re-exports from X: followed to the defining file.
    assert_eq!(
        from_lib,
        vec![("src/m11.rs", 15, Some("src/m5.rs"), Precision::Item)]
    );
}

#[test]
fn an_import_of_a_missing_item_is_flagged_not_hidden() {
    let graph = build_from_table(fixtures::MISSING_LEAF);
    let uses: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.kind == Use)
        .map(|e| (e.to.as_str(), e.line, e.precision))
        .collect();
    assert_eq!(
        uses,
        vec![
            ("src/util.rs", 2, Precision::Module),
            ("src/util.rs", 3, Precision::Item),
            // Mentioned by an item-level macro call: a guess, no diagnostic.
            ("src/util.rs", 4, Precision::Macro),
        ]
    );
    assert_eq!(
        diagnostics(&graph),
        vec![(DiagnosticCode::UnresolvedItem, "src/lib.rs", Some(2))]
    );
    assert!(
        graph.diagnostics[0]
            .message
            .contains("crate::util::Missing")
    );
}

/// The edges of `kind` from `from` as `(to, line, precision, via)`.
fn edges_from<'g>(
    graph: &'g CodeGraph,
    from: &str,
    kind: EdgeKind,
) -> Vec<(&'g str, u32, Precision, Option<&'g str>)> {
    graph
        .edges
        .iter()
        .filter(|e| e.from == from && e.kind == kind)
        .map(|e| (e.to.as_str(), e.line, e.precision, e.via.as_deref()))
        .collect()
}

#[test]
fn a_reexport_outranks_a_macro_mention_of_the_same_name() {
    let graph = build_from_table(fixtures::MACRO_FACADE);
    assert_eq!(
        edges_from(&graph, "src/app.rs", Use),
        vec![("src/real.rs", 1, Precision::Item, Some("src/facade.rs"))]
    );
    assert_eq!(diagnostics(&graph), vec![]);
}

#[test]
fn a_declaration_behind_any_glob_outranks_a_macro_mention_behind_another() {
    let graph = build_from_table(fixtures::MACRO_GLOBS);
    assert_eq!(
        edges_from(&graph, "src/app.rs", Path),
        vec![
            ("src/b.rs", 4, Precision::Item, None),
            ("src/deep.rs", 5, Precision::Item, Some("src/b.rs")),
        ]
    );
    assert!(
        graph.edges.iter().all(|e| e.precision != Precision::Macro),
        "{:?}",
        graph.edges
    );
    assert_eq!(diagnostics(&graph), vec![]);
}

#[test]
fn names_only_a_macro_call_mentions_are_macro_guesses_and_absent_ones_unresolved() {
    let graph = build_from_table(fixtures::MACRO_TABLE);
    assert_eq!(
        edges_from(&graph, "src/lib.rs", Use),
        vec![
            ("src/m.rs", 2, Precision::Macro, None),
            ("src/m.rs", 3, Precision::Macro, None),
            ("src/m.rs", 4, Precision::Module, None),
        ]
    );
    assert_eq!(
        diagnostics(&graph),
        vec![(DiagnosticCode::UnresolvedItem, "src/lib.rs", Some(4))]
    );
    assert!(graph.diagnostics[0].message.contains("crate::m::Absent"));
}

#[test]
fn pathological_nesting_is_unparsed_not_a_stack_overflow() {
    let deep = format!(
        "fn main() {{\n    let _ = {}1{};\n}}\n",
        "(".repeat(200_000),
        ")".repeat(200_000)
    );
    let hidden = format!(
        "// {open}\n/* {open} /* {open} */ */\nconst S: &str = \"{open}\";\nconst R: &str = r#\"{open}\"#;\nfn main() {{\n    deep::b::g();\n}}\n",
        open = "(".repeat(200_000)
    );
    let at_limit = format!(
        "fn main() {{\n    let _ = {}1{};\n}}\n",
        "(".repeat(spawn_codegraph::rust::MAX_NESTING - 1),
        ")".repeat(spawn_codegraph::rust::MAX_NESTING - 1)
    );
    let table = [
        (
            "Cargo.toml",
            "[package]\nname = \"deep\"\nversion = \"0.1.0\"\n",
        ),
        ("src/lib.rs", "pub mod b;\n"),
        ("src/b.rs", "pub fn g() {}\n"),
        ("src/main.rs", deep.as_str()),
        ("src/bin/hidden.rs", hidden.as_str()),
        ("src/bin/limit.rs", at_limit.as_str()),
    ];
    let graph = build_from_table(&table);
    assert_eq!(
        graph
            .diagnostics
            .iter()
            .map(|d| (d.code, d.path.as_str()))
            .collect::<Vec<_>>(),
        vec![(DiagnosticCode::Unparsed, "src/main.rs")]
    );
    assert!(
        graph.diagnostics[0]
            .message
            .contains("over the limit of 256")
    );
    assert!(!graph.files["src/main.rs"].parsed);
    assert!(graph.files["src/bin/limit.rs"].parsed);
    // Brackets in comments and strings do not count: the file parses and
    // its path resolves.
    assert!(graph.files["src/bin/hidden.rs"].parsed);
    assert_eq!(
        edge(&graph, "src/bin/hidden.rs", "src/b.rs", Path).len(),
        1,
        "{:?}",
        graph.edges
    );
}

/// Every construct `syn` recurses on without brackets, at the size the
/// review saw abort a debug build (or larger), as `(name, source)`.
fn pathological() -> Vec<(&'static str, String)> {
    let n = |count: usize, unit: &str| unit.repeat(count);
    let body = |count: usize, unit: &str, end: &str| {
        format!(
            "fn f(c: bool, y: u8) {{ let _ = {}{end}; }}\n",
            unit.repeat(count)
        )
    };
    vec![
        (
            "generics",
            format!("type T = {}u8{};\n", n(2_500, "Vec<"), n(2_500, ">")),
        ),
        (
            "qualified_paths",
            format!("type T = {}u8{};\n", n(2_500, "<"), n(2_500, " as A>::B")),
        ),
        ("references", body(40_000, "&", "c")),
        ("spaced_references", body(40_000, "& ", "c")),
        ("mutable_references", body(40_000, "&mut ", "c")),
        ("nots", body(40_000, "!", "c")),
        ("negations", body(40_000, "-", "y")),
        ("derefs", body(40_000, "*", "y")),
        (
            "pointer_types",
            format!("type T = {}u8;\n", n(40_000, "*const ")),
        ),
        (
            "lifetime_references",
            format!("fn f<'a>(_: {}u8) {{}}\n", n(40_000, "&'a ")),
        ),
        ("closures", body(40_000, "|| ", "0")),
        ("move_closures", body(40_000, "move || ", "0")),
        (
            "reference_patterns",
            format!("fn f(y: u8) {{ let {}x = y; }}\n", n(40_000, "&")),
        ),
        ("tries", body(1, "y", &n(40_000, "?"))),
        ("ranges", body(40_000, ".. ", "y")),
        ("method_calls", body(1, "y", &n(450_000, ".a()"))),
        ("fields", body(1, "y", &n(450_000, ".f"))),
        ("tuple_fields", body(1, "y", &n(450_000, ".0"))),
        ("sums", body(1, "1", &n(900_000, "+1"))),
        ("shifts", body(1, "1", &n(400_000, "<<1"))),
        ("returns", body(100_000, "return ", "0")),
        ("casts", body(1, "0", &n(100_000, " as u8"))),
        (
            "else_if_ladders",
            body(1, "if c {}", &n(100_000, " else if c {}")),
        ),
        ("indexing", body(1, "y", &n(100_000, "[0]"))),
        ("calls", body(1, "y", &n(100_000, "()"))),
        ("closures_with_parameters", body(100_000, "|a| ", "0")),
        (
            "bindings",
            format!("fn f(y: u8) {{ let {}x = y; }}\n", n(100_000, "x @ ")),
        ),
        (
            "function_pointers",
            format!("type T = {}u8;\n", n(100_000, "fn() -> ")),
        ),
    ]
    .into_iter()
    .chain(review_bypasses())
    .chain(rev4_bypasses())
    .chain(rev4_lifetime_bypasses())
    .collect()
}

/// The review's bypasses (Rev 2), at or past the size that aborted a debug
/// build, bare and wrapped in a bracket, and the compound chains beside
/// them.
fn review_bypasses() -> Vec<(&'static str, String)> {
    let n = |count: usize, unit: &str| unit.repeat(count);
    let body = |count: usize, unit: &str, end: &str| {
        format!(
            "fn f(c: bool, y: u8) {{ let _ = {}{end}; }}\n",
            unit.repeat(count)
        )
    };
    vec![
        ("closures_with_two_parameters", body(6_000, "|a,b| ", "1")),
        (
            "wrapped_closures_with_two_parameters",
            body(1, "(", &format!("{}1)", n(6_000, "|a,b| "))),
        ),
        (
            "higher_ranked_function_pointers",
            format!("type T = {}u8;\n", n(3_000, "for<'a,'b> fn() -> ")),
        ),
        (
            "wrapped_higher_ranked_function_pointers",
            format!("fn f(_: {}u8) {{}}\n", n(3_000, "for<'a,'b> fn() -> ")),
        ),
        (
            "turbofish_assignments",
            body(1, "", &format!("x{}", n(12_000, "::<A,B> = x"))),
        ),
        (
            "wrapped_turbofish_assignments",
            body(1, "(", &format!("x{})", n(12_000, "::<A,B> = x"))),
        ),
        (
            "brackets_after_a_shebang_with_a_quote",
            format!(
                "#!/bin/x \"\n{}",
                body(10_000, "(", &format!("1{}", n(10_000, ")")))
            ),
        ),
        (
            "brackets_after_a_byte_order_mark_and_shebang",
            format!(
                "\u{feff}#!/bin/x \"\n{}",
                body(10_000, "(", &format!("1{}", n(10_000, ")")))
            ),
        ),
        ("calls_with_arguments", body(1, "y", &n(12_000, ".f(1, 2)"))),
        (
            "else_if_ladders_with_values",
            body(1, "if c { 1 }", &n(6_000, " else if c { 2 }")),
        ),
        ("block_sums", body(1, "{ 1 }", &n(6_000, " + { 1 }"))),
        // Rev 3's, and unary bombs.
        ("attributed_closures", body(6_000, "#[a] |a,b| ", "1")),
        ("struct_pattern_closures", body(6_000, "|S{x},b| ", "1")),
        ("inline_const_closures", body(6_000, "|const{0},b| ", "1")),
        (
            "const_block_turbofish_assignments",
            body(1, "", &format!("x{}", n(12_000, "::<{0},B> = x"))),
        ),
        ("reference_bomb", body(6_000, "&", "c")),
        ("not_bomb", body(6_000, "!", "c")),
    ]
}

/// Rev 4's attribute-value bypasses, each past the size that aborted.
fn rev4_bypasses() -> Vec<(&'static str, String)> {
    let n = |count: usize, unit: &str| unit.repeat(count);
    vec![
        // Rev 4's: an attribute's `= value`, read as an expression, in
        // every position an outer attribute takes.
        (
            "attribute_value",
            format!("#[a = x as {}u8] fn f() {{}}\n", n(3_000, "&")),
        ),
        (
            "statement_attribute_value",
            format!("fn f() {{ #[a = x as {}u8] let y = 1; }}\n", n(3_000, "&")),
        ),
        (
            "field_attribute_value",
            format!("struct S {{ #[a = x as {}u8] f: u8 }}\n", n(3_000, "&")),
        ),
        (
            "cfg_attribute_value",
            format!("#[a] #[cfg = x as {}u8] fn f() {{}}\n", n(3_000, "&")),
        ),
        (
            "path_attribute_value",
            format!(
                "#[doc = \"x\"] #[path = x as {}u8] mod m {{}}\n",
                n(3_000, "&")
            ),
        ),
        (
            "test_attribute_value",
            format!("/// d\n#[x::test = x as {}u8] fn f() {{}}\n", n(3_000, "&")),
        ),
        (
            "variant_attribute_value",
            format!("enum E {{ #[a = x as {}u8] V }}\n", n(3_000, "&")),
        ),
        (
            "arm_attribute_value",
            format!(
                "fn f(c: u8) {{ match c {{ #[a = x as {}u8] _ => {{}} }} }}\n",
                n(3_000, "&")
            ),
        ),
        (
            "method_attribute_value",
            format!(
                "struct S;\nimpl S {{ #[a = x as {}u8] fn m() {{}} }}\n",
                n(3_000, "&")
            ),
        ),
        (
            "foreign_attribute_value",
            format!(
                "extern \"C\" {{ #[a = x as {}u8] fn c(); }}\n",
                n(3_000, "&")
            ),
        ),
        (
            "reference_attribute_value",
            format!("#[a = {}x] fn f() {{}}\n", n(30_000, "&")),
        ),
    ]
}

/// Rev 4's lifetime and first-block bypasses.
fn rev4_lifetime_bypasses() -> Vec<(&'static str, String)> {
    let n = |count: usize, unit: &str| unit.repeat(count);
    vec![
        // Rev 4's: a lifetime or label before `!` is no macro's name.
        (
            "lifetime_before_never",
            format!("fn f<'a>() -> &'a ! {{ let _: {}u8; }}\n", n(3_000, "&")),
        ),
        (
            "label_before_not",
            format!(
                "fn main() {{ 'a: loop {{ break 'a !(x as {}u8); }} }}\n",
                n(3_000, "&")
            ),
        ),
        (
            "label_before_not_block",
            format!(
                "fn main() {{ 'a: loop {{ break 'a !{{ x as {}u8 }}; }} }}\n",
                n(3_000, "&")
            ),
        ),
        (
            "label_before_not_array",
            format!(
                "fn main() {{ 'a: loop {{ break 'a ![x as {}u8]; }} }}\n",
                n(3_000, "&")
            ),
        ),
        (
            "macro_rules_label",
            format!(
                "fn main() {{ loop {{ break 'macro_rules !name(x as {}u8); }} }}\n",
                n(3_000, "&")
            ),
        ),
        (
            "impl_for_never_reference",
            format!(
                "trait T {{}}\nimpl<'a> T for &'a ! {{ fn f() {{ let _: {}u8; }} }}\n",
                n(3_000, "&")
            ),
        ),
        (
            "closure_returning_never_reference",
            format!(
                "fn main() {{ let _ = || -> &'a ! {{ x as {}u8 }}; }}\n",
                n(3_000, "&")
            ),
        ),
        // Rev 4's: a block read before anything else in the file.
        (
            "and_chain_first",
            format!(
                "const A: bool = std::hint::black_box(true){};\nfn main() {{}}\n",
                n(100, " && std::hint::black_box(true)")
            ),
        ),
        (
            "where_bound_first",
            format!("struct S where {}u8: A;\nfn main() {{}}\n", n(1_200, "&")),
        ),
    ]
}

#[test]
fn no_input_aborts_the_parser() {
    // Every shape that ever overflowed the parser's stack, at or past the
    // size that did. The graph is built on the parser thread, exactly as
    // `spawn features graph` builds it: an overflow aborts this binary.
    // Each `syn` call gets at most `MAX_SYN_TOKENS` tokens, so a file is
    // read in pieces, with a token walk for a piece that cannot be cut
    // (`token_fallback`), or is unparsed when its brackets nest too deep.
    let bombs = pathological();
    let mut table: Vec<(String, &str)> = vec![
        (
            "Cargo.toml".to_owned(),
            "[package]\nname = \"deep\"\nversion = \"0.1.0\"\n",
        ),
        ("src/lib.rs".to_owned(), ""),
    ];
    for (name, text) in &bombs {
        table.push((format!("src/bin/{name}.rs"), text.as_str()));
    }
    let table: Vec<(&str, &str)> = table.iter().map(|(p, t)| (p.as_str(), *t)).collect();
    let graph = build_from_table(&table);
    for (name, _) in &bombs {
        let path = format!("src/bin/{name}.rs");
        let codes: Vec<DiagnosticCode> = graph
            .diagnostics
            .iter()
            .filter(|d| d.path == path)
            .map(|d| d.code)
            .collect();
        if name.starts_with("brackets_after") {
            assert_eq!(codes, vec![DiagnosticCode::Unparsed], "{name}");
        } else {
            assert!(graph.files[&path].parsed, "{name}");
            assert!(
                codes
                    .iter()
                    .all(|code| *code == DiagnosticCode::TokenFallback),
                "{name}: {codes:?}"
            );
        }
    }
}

#[test]
fn valid_code_past_the_token_budget_is_read_without_a_token_walk() {
    let n = |count: usize, f: &dyn Fn(usize) -> String| (0..count).map(f).collect::<String>();
    let files = [
        (
            "src/bin/bit_table.rs",
            format!(
                "const M: [u64; 200] = [{}];\nfn main() {{ deep::b::g(); }}\n",
                n(200, &|i| format!("1 << {i}, "))
            ),
        ),
        (
            "src/bin/shift_arms.rs",
            format!(
                "fn f(x: u8) -> u64 {{\n    match x {{\n{}        _ => 0,\n    }}\n}}\nfn main() {{ deep::b::g(); }}\n",
                n(200, &|i| format!("        {i} => 1 << {i},\n"))
            ),
        ),
        (
            "src/bin/guards.rs",
            format!(
                "fn f(x: u8) -> u8 {{\n    match x {{\n{}        _ => 0,\n    }}\n}}\nfn main() {{ deep::b::g(); }}\n",
                n(200, &|i| format!("        v if v < {i} => {i},\n"))
            ),
        ),
        (
            "src/bin/else_if.rs",
            format!(
                "fn f(a: u8) -> u8 {{\n    if a == 0 {{ 0 }}{} else {{ deep::b::g(); 1 }}\n}}\nfn main() {{}}\n",
                n(150, &|i| format!(" else if a == {i} {{ {i} }}"))
            ),
        ),
        (
            "src/bin/or_terms.rs",
            format!(
                "fn f(a: u8) -> bool {{\n    let b = a == 0{};\n    deep::b::g();\n    b\n}}\nfn main() {{}}\n",
                n(300, &|i| format!(" || a == {i}"))
            ),
        ),
        (
            "src/bin/where_bounds.rs",
            format!(
                "fn f<{}>() where {}{{ deep::b::g(); }}\nfn main() {{}}\n",
                n(200, &|i| format!("T{i}, ")),
                n(200, &|i| format!("T{i}: Clone, "))
            ),
        ),
        (
            "src/bin/attribute.rs",
            concat!("#!", "[allow(unused)]\nfn main() {\n    deep::b::g();\n}\n").to_owned(),
        ),
        (
            "src/bin/closure_list.rs",
            format!(
                "fn main() {{\n    let _ = vec![{}];\n    deep::b::g();\n}}\n",
                n(600, &|_| "|a, b| 1, |c, d| 2, ".to_owned())
            ),
        ),
        (
            "src/bin/comparison_list.rs",
            format!(
                "fn main() {{\n    let (a, b) = (1, 2);\n    let _ = [{}];\n    deep::b::g();\n}}\n",
                n(600, &|_| "a < b, ".to_owned())
            ),
        ),
        (
            "src/bin/items.rs",
            n(600, &|i| format!("fn f{i}() {{}}\n")) + "fn main() { deep::b::g(); }\n",
        ),
    ];
    assert_read_without_a_token_walk(&files);
}

#[test]
fn long_attribute_runs_fields_patterns_and_inner_modules_are_read_without_a_token_walk() {
    let n = |count: usize, f: &dyn Fn(usize) -> String| (0..count).map(f).collect::<String>();
    let files = [
        (
            "src/bin/outer_attributes.rs",
            n(300, &|i| {
                format!("/// Line {i} of the docs.\n#[allow(unused)]\n")
            }) + "fn main() {\n    deep::b::g();\n}\n",
        ),
        (
            "src/bin/generic_fields.rs",
            format!(
                "struct S<'a, T> {{\n{}    t: &'a T,\n}}\nfn main() {{ deep::b::g(); }}\n",
                n(200, &|i| format!(
                    "    f{i}: std::collections::HashMap<u8, Vec<&'a T>>,\n"
                ))
            ),
        ),
        (
            "src/bin/pattern_alternatives.rs",
            format!(
                "fn f(c: char) -> bool {{\n    match c {{\n        'a'{} => deep::b::g() == (),\n        _ => false,\n    }}\n}}\nfn main() {{}}\n",
                n(800, &|i| format!(
                    " | '\\u{{{:04X}}}'..='\\u{{{:04X}}}'",
                    256 + 2 * i,
                    257 + 2 * i
                ))
            ),
        ),
        (
            "src/bin/module_in_block.rs",
            format!(
                concat!(
                    "fn main() {{\n    mod rules {{\n        #!",
                    "[allow(unused)]\n{}    }}\n    ::std::mem::drop(0);\n    deep::b::g();\n}}\n"
                ),
                n(600, &|i| format!("        pub fn h{i}() {{}}\n"))
            ),
        ),
        (
            "src/bin/foreign_block.rs",
            format!(
                "unsafe extern \"C\" {{\n{}    fn last(f: deep::b::g);\n}}\nfn main() {{}}\n",
                n(400, &|i| format!("    fn c{i}(x: *const u8) -> i32;\n"))
            ),
        ),
    ];
    assert_read_without_a_token_walk(&files);
}

/// Each file calls `deep::b::g` once; none may need the token walk.
fn assert_read_without_a_token_walk(files: &[(&str, String)]) {
    let mut table: Vec<(&str, &str)> = vec![
        (
            "Cargo.toml",
            "[package]\nname = \"deep\"\nversion = \"0.1.0\"\n",
        ),
        ("src/lib.rs", "pub mod b;\n"),
        ("src/b.rs", "pub fn g() {}\n"),
    ];
    table.extend(files.iter().map(|(path, text)| (*path, text.as_str())));
    let graph = build_from_table(&table);
    assert_eq!(diagnostics(&graph), vec![]);
    for (path, _) in files {
        assert!(graph.files[*path].parsed, "{path}");
        assert_eq!(edge(&graph, path, "src/b.rs", Path).len(), 1, "{path}");
    }
}

#[test]
fn a_use_group_past_the_token_budget_keeps_every_import() {
    // 502 names: the group alone is past the budget. Each big file must
    // give what its 7-name twin gives, the padding names each an
    // unresolved_item, and need no token walk.
    let names = |count: usize| {
        (0..count)
            .flat_map(|i| [", n".to_owned(), i.to_string()])
            .collect::<String>()
    };
    let files = |padding: &str| {
        [
            (
                "plain",
                format!("use crate::target::{{hit, T{padding}}};\n"),
            ),
            (
                "public",
                format!("pub use crate::target::{{hit, T{padding}}};\n"),
            ),
            (
                "nested",
                format!("use crate::{{target::{{hit, T{padding}}}}};\n"),
            ),
            (
                "in_block",
                format!(
                    "pub fn f() {{\n    use crate::target::{{hit, T{padding}}};\n    hit();\n}}\n"
                ),
            ),
        ]
    };
    let big = files(&names(500));
    let small = files(&names(5));
    let mut table: Vec<(String, String)> = vec![
        (
            "Cargo.toml".to_owned(),
            "[package]\nname = \"deep\"\nversion = \"0.1.0\"\n".to_owned(),
        ),
        (
            "src/target.rs".to_owned(),
            "pub fn hit() {}\npub struct T;\n".to_owned(),
        ),
    ];
    let mut lib = "pub mod target;\n".to_owned();
    for (name, text) in &big {
        lib.extend(["pub mod ", name, ";\npub mod ", name, "_small;\n"]);
        table.push((format!("src/{name}.rs"), text.clone()));
    }
    for (name, text) in &small {
        table.push((format!("src/{name}_small.rs"), text.clone()));
    }
    table.push(("src/lib.rs".to_owned(), lib));
    let table: Vec<(&str, &str)> = table
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    let graph = build_from_table(&table);
    let to_target = |from: &str| -> Vec<(EdgeKind, Scope, Precision)> {
        let mut found: Vec<_> = graph
            .edges
            .iter()
            .filter(|e| e.from == from && e.to == "src/target.rs")
            .map(|e| (e.kind, e.scope, e.precision))
            .collect();
        found.sort_by_key(|edge| format!("{edge:?}"));
        found
    };
    let codes = |from: &str, code: DiagnosticCode| {
        graph
            .diagnostics
            .iter()
            .filter(|d| d.path == from && d.code == code)
            .count()
    };
    for (name, _) in &big {
        let (path, twin) = (format!("src/{name}.rs"), format!("src/{name}_small.rs"));
        assert!(!to_target(&twin).is_empty(), "{name}");
        assert_eq!(to_target(&path), to_target(&twin), "{name}");
        assert_eq!(codes(&twin, DiagnosticCode::UnresolvedItem), 5, "{name}");
        assert_eq!(codes(&path, DiagnosticCode::UnresolvedItem), 500, "{name}");
        assert_eq!(codes(&path, DiagnosticCode::TokenFallback), 0, "{name}");
    }
}

#[test]
fn paths_inside_macro_bodies_are_found() {
    let graph = build_from_table(fixtures::POLYGLOT_RUST);
    let found = edge(
        &graph,
        "crates/app/src/cli.rs",
        "crates/core/src/util.rs",
        Path,
    );
    assert_eq!(found.len(), 1, "{:?}", graph.edges);
    assert_eq!(found[0].line, 5);
    assert_eq!(found[0].precision, Precision::Item);
    // The string literal "fx_core::model::make" in the same macro is not a
    // path: cli.rs reaches model.rs only through its `use`.
    let to_model: Vec<_> = edge(
        &graph,
        "crates/app/src/cli.rs",
        "crates/core/src/model.rs",
        Path,
    );
    assert!(to_model.is_empty(), "{to_model:?}");
}

#[test]
fn a_root_reexport_resolves_to_the_defining_file() {
    let graph = build_from_table(fixtures::POLYGLOT_RUST);
    let found = edge(
        &graph,
        "crates/app/src/cli.rs",
        "crates/core/src/model.rs",
        Use,
    );
    assert_eq!(found.len(), 1, "{:?}", graph.edges);
    assert_eq!(found[0].via.as_deref(), Some("crates/core/src/lib.rs"));
    assert_eq!(found[0].precision, Precision::Item);
    assert!(
        edge(
            &graph,
            "crates/app/src/cli.rs",
            "crates/core/src/lib.rs",
            Use
        )
        .is_empty(),
        "never the re-exporting lib.rs"
    );

    // Through a glob, a chain of named re-exports, and a workspace-inherited
    // renamed dependency.
    let graph = build_from_table(fixtures::REEXPORTS);
    let from_main: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.from == "bin/src/main.rs")
        .map(|e| (e.to.as_str(), e.kind, e.line, e.via.as_deref()))
        .collect();
    assert_eq!(
        from_main,
        vec![
            ("lib/src/geometry.rs", Use, 1, Some("lib/src/lib.rs")),
            (
                "lib/src/geometry/square.rs",
                Path,
                4,
                Some("lib/src/lib.rs")
            ),
            (
                "lib/src/geometry/square.rs",
                Path,
                6,
                Some("lib/src/lib.rs")
            ),
        ]
    );
    let reexports: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.kind == Reexport)
        .map(|e| (e.from.as_str(), e.to.as_str(), e.via.as_deref()))
        .collect();
    assert_eq!(
        reexports,
        vec![
            ("lib/src/api.rs", "lib/src/geometry/square.rs", None),
            ("lib/src/lib.rs", "lib/src/geometry.rs", None),
            (
                "lib/src/lib.rs",
                "lib/src/geometry/square.rs",
                Some("lib/src/api.rs")
            ),
        ]
    );
    assert_eq!(
        graph.units[0].declared_units,
        vec!["cargo:fx-shapes".to_string()]
    );
}

#[test]
fn cfg_test_edges_are_test_scope() {
    let graph = build_from_table(fixtures::TEST_SCOPE);
    let to_b: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.to == "src/b.rs")
        .map(|e| (e.from.as_str(), e.kind, e.scope, e.line))
        .collect();
    assert_eq!(
        to_b,
        vec![
            ("src/a.rs", Use, Test, 7),
            ("src/a.rs", Path, Prod, 2),
            ("src/a.rs", Path, Test, 12),
            ("src/helpers.rs", Path, Test, 2),
            ("src/lib.rs", Mod, Prod, 2),
            ("src/live.rs", Path, Prod, 2),
        ]
    );
    assert_eq!(
        edge(&graph, "src/lib.rs", "src/helpers.rs", Mod)[0].scope,
        Test
    );
    assert_eq!(graph.files["src/helpers.rs"].class, FileClass::Test);
    assert_eq!(graph.files["src/live.rs"].class, FileClass::Source);
}

#[test]
fn include_str_makes_an_include_edge() {
    let graph = build_from_table(fixtures::INCLUDES);
    let includes: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.kind == Include)
        .map(|e| (e.to.as_str(), e.line))
        .collect();
    assert_eq!(
        includes,
        vec![("db/sql/schema.sql", 1), ("db/tests/data/rows.bin", 2)]
    );
    assert_eq!(
        diagnostics(&graph),
        vec![(DiagnosticCode::Unresolved, "db/src/lib.rs", Some(3))]
    );
    assert!(graph.diagnostics[0].message.contains("../sql/gone.sql"));
}

#[test]
fn an_undeclared_rs_file_is_an_orphan_source() {
    let mut table = fixtures::POLYGLOT_RUST.to_vec();
    table.push(("crates/app/src/stray.rs", "pub fn stray() {}\n"));
    table.push(("scripts/tool.rs", "fn main() {}\n"));
    let graph = build_from_table(&table);
    assert_eq!(
        diagnostics(&graph),
        vec![(
            DiagnosticCode::OrphanSource,
            "crates/app/src/stray.rs",
            None
        )]
    );
    assert!(!graph.files["crates/app/src/stray.rs"].in_module_tree);
    // island.rs is declared (in the tree) even though nothing calls it.
    assert!(graph.files["crates/app/src/island.rs"].in_module_tree);
    // A file outside every unit is no orphan: no unit claims it.
    assert_eq!(graph.files["scripts/tool.rs"].unit, None);
}

#[test]
fn unsupported_toml_is_a_diagnostic_not_a_guess() {
    let mut table: Vec<(&str, &str)> = fixtures::POLYGLOT_RUST
        .iter()
        .copied()
        .filter(|(path, _)| *path != "crates/core/Cargo.toml")
        .collect();
    table.push((
        "crates/core/Cargo.toml",
        "[package]\nname = \"fx-core\"\nversion = 0.1.0 + 1\n",
    ));
    let graph = build_from_table(&table);
    assert_eq!(
        graph
            .diagnostics
            .iter()
            .filter(|d| d.code == DiagnosticCode::ManifestUnsupported)
            .map(|d| (d.path.as_str(), d.line))
            .collect::<Vec<_>>(),
        vec![("crates/core/Cargo.toml", Some(3))]
    );
    // No unit is guessed for fx-core: its files belong to no unit, the app
    // cannot name it, and nothing resolves into it.
    assert_eq!(
        graph
            .units
            .iter()
            .map(|u| u.id.as_str())
            .collect::<Vec<_>>(),
        vec!["cargo:fx-app"]
    );
    assert_eq!(graph.files["crates/core/src/lib.rs"].unit, None);
    assert!(
        graph
            .edges
            .iter()
            .all(|e| !e.to.starts_with("crates/core/"))
    );
}

#[test]
fn cross_crate_imports_need_a_declared_dependency() {
    let table: Vec<(&str, &str)> = fixtures::POLYGLOT_RUST
        .iter()
        .map(|(path, text)| {
            if *path == "crates/app/Cargo.toml" {
                (
                    *path,
                    "[package]\nname = \"fx-app\"\nversion = \"0.1.0\"\n\n[dev-dependencies]\nrenamed = { package = \"fx-core\", path = \"../core\" }\n",
                )
            } else if *path == "crates/app/tests/smoke.rs" {
                (*path, "use renamed::util::clamp;\n")
            } else {
                (*path, *text)
            }
        })
        .collect();
    let graph = build_from_table(&table);
    // `fx_core` is no longer a name the app declares; `renamed` is.
    let into_core: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.from.starts_with("crates/app/") && e.to.starts_with("crates/core/"))
        .map(|e| (e.from.as_str(), e.to.as_str()))
        .collect();
    assert_eq!(
        into_core,
        vec![("crates/app/tests/smoke.rs", "crates/core/src/util.rs")]
    );
}

#[test]
fn graph_output_is_byte_identical_across_runs() {
    let first = serde_json::to_string(&build_from_table(fixtures::POLYGLOT_RUST)).expect("json");
    let second = serde_json::to_string(&build_from_table(fixtures::POLYGLOT_RUST)).expect("json");
    assert_eq!(first, second);
    let mut reversed = fixtures::POLYGLOT_RUST.to_vec();
    reversed.reverse();
    let third = serde_json::to_string(&build_from_table(&reversed)).expect("json");
    assert_eq!(first, third, "input order does not matter");
}

#[test]
fn only_rust_sources_and_cargo_manifests_are_read() {
    assert!(wants("crates/a/src/lib.rs") && wants("Cargo.toml") && wants("crates/a/Cargo.toml"));
    assert!(!wants("Cargo.lock") && !wants("web/src/main.ts") && !wants("docs/Cargo.toml.md"));
}
