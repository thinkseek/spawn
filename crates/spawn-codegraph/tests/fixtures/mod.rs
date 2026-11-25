//! Fixture repositories as string tables of `(path, text)`, never as
//! directories: a real fixture tree inside Spawn would be parsed as Spawn's
//! own source and would need registry entries of its own. The pure API takes
//! a table directly.

/// The Rust half of the design's `POLYGLOT` fixture: a library crate whose
/// root re-exports an item, a binary that uses it by crate name (once in a
/// macro body), an unused module, and an integration test.
pub const POLYGLOT_RUST: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[workspace]\nresolver = \"3\"\nmembers = [\"crates/core\", \"crates/app\"]\n",
    ),
    (
        "crates/core/Cargo.toml",
        "[package]\nname = \"fx-core\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    ),
    (
        "crates/core/src/lib.rs",
        "pub mod model;\npub mod util;\npub use model::Item;\n",
    ),
    (
        "crates/core/src/model.rs",
        "use crate::util::clamp;\n\npub struct Item;\n\npub fn make() -> Item {\n    let _ = clamp(1);\n    Item\n}\n",
    ),
    (
        "crates/core/src/util.rs",
        "pub fn clamp(value: i32) -> i32 {\n    value.max(0)\n}\n",
    ),
    (
        "crates/app/Cargo.toml",
        "[package]\nname = \"fx-app\"\nversion = \"0.1.0\"\n\n[dependencies]\nfx-core = { path = \"../core\" }\nserde_json = \"1\"\n",
    ),
    (
        "crates/app/src/main.rs",
        "mod cli;\nmod island;\n\nfn main() {\n    cli::run();\n}\n",
    ),
    (
        "crates/app/src/cli.rs",
        "use fx_core::Item;\nuse serde_json::json;\n\npub fn run() {\n    let _ = json!({ \"k\": fx_core::util::clamp(1), \"s\": \"fx_core::model::make\" });\n    let _item: Item = Item;\n}\n",
    ),
    ("crates/app/src/island.rs", "pub fn never_called() {}\n"),
    (
        "crates/app/tests/smoke.rs",
        "use fx_core::model::Item;\n\n#[test]\nfn smoke() {\n    let _ = Item;\n}\n",
    ),
    ("README.md", "# fixture\n"),
    ("docs/guide.md", "# guide\n"),
];

/// One library crate exercising the module tree: `#[path]`, inline
/// modules, `mod.rs` and non-`mod.rs` directories.
pub const MODULE_TREE: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"tree\"\nversion = \"0.1.0\"\n",
    ),
    (
        "src/lib.rs",
        "#[path = \"impls/real.rs\"]\nmod alias;\nmod outer {\n    pub mod inner;\n    mod nested {\n        mod deep;\n    }\n}\nmod dir;\nmod a;\nmod missing;\n",
    ),
    ("src/impls/real.rs", "pub fn real() {}\n"),
    ("src/outer/inner.rs", "pub fn inner() {}\n"),
    ("src/outer/nested/deep.rs", "pub fn deep() {}\n"),
    ("src/dir/mod.rs", "mod leaf;\n"),
    ("src/dir/leaf.rs", "pub fn leaf() {}\n"),
    ("src/a.rs", "mod b;\n"),
    ("src/a/b.rs", "pub fn b() {}\n"),
];

/// `use` trees with groups, `self`, renames, `super`, and paths through
/// an alias.
pub const USE_TREES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"uses\"\nversion = \"0.1.0\"\n",
    ),
    ("src/lib.rs", "mod a;\nmod b;\n"),
    (
        "src/a.rs",
        "use crate::b::{self as bee, Thing as T};\n\nfn f() {\n    bee::g();\n    let _t: Option<T> = None;\n}\n",
    ),
    (
        "src/b.rs",
        "pub struct Thing;\n\npub fn g() {}\n\npub mod c;\n",
    ),
    (
        "src/b/c.rs",
        "use super::Thing;\nuse super::super::a;\n\npub fn h(_: Thing) {}\n",
    ),
];

/// Re-exports through a glob and a chain of named re-exports.
pub const REEXPORTS: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[workspace]\nmembers = [\"lib\", \"bin\"]\n\n[workspace.dependencies]\nshapes = { package = \"fx-shapes\", path = \"lib\" }\n",
    ),
    (
        "lib/Cargo.toml",
        "[package]\nname = \"fx-shapes\"\nversion = \"0.1.0\"\n",
    ),
    (
        "lib/src/lib.rs",
        "mod geometry;\nmod api;\npub use geometry::*;\npub use api::Facade;\n",
    ),
    (
        "lib/src/geometry.rs",
        "pub struct Circle;\npub mod square;\n",
    ),
    ("lib/src/geometry/square.rs", "pub struct Square;\n"),
    (
        "lib/src/api.rs",
        "pub use crate::geometry::square::Square as Facade;\n",
    ),
    (
        "bin/Cargo.toml",
        "[package]\nname = \"fx-bin\"\nversion = \"0.1.0\"\n\n[dependencies]\nshapes.workspace = true\n",
    ),
    (
        "bin/src/main.rs",
        "use shapes::Circle;\n\nfn main() {\n    let _ = shapes::Facade;\n    let _ = Circle;\n    let _ = shapes::square::Square;\n}\n",
    ),
];

/// Test-only code: `#[cfg(test)]` modules, `#[test]` functions, a module
/// file only tests declare.
pub const TEST_SCOPE: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"scoped\"\nversion = \"0.1.0\"\n",
    ),
    (
        "src/lib.rs",
        "mod a;\nmod b;\n#[cfg(test)]\nmod helpers;\n#[cfg(not(test))]\nmod live;\n",
    ),
    (
        "src/a.rs",
        "pub fn f() {\n    crate::b::g();\n}\n\n#[cfg(test)]\nmod tests {\n    use crate::b::g;\n}\n\n#[test]\nfn t() {\n    crate::b::g();\n}\n",
    ),
    ("src/b.rs", "pub fn g() {}\n"),
    (
        "src/helpers.rs",
        "pub fn fixture() {\n    crate::b::g();\n}\n",
    ),
    ("src/live.rs", "pub fn live() {\n    crate::b::g();\n}\n"),
];

/// `include_str!` relative to the file and to the package root.
pub const INCLUDES: &[(&str, &str)] = &[
    (
        "db/Cargo.toml",
        "[package]\nname = \"db\"\nversion = \"0.1.0\"\n",
    ),
    (
        "db/src/lib.rs",
        "const SCHEMA: &str = include_str!(\"../sql/schema.sql\");\nconst DATA: &[u8] = include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/tests/data/rows.bin\"));\nconst GONE: &str = include_str!(\"../sql/gone.sql\");\n",
    ),
    ("db/sql/schema.sql", "create table t (id int);\n"),
    ("db/tests/data/rows.bin", "rows"),
];

/// A parent's private import named from a child module, directly
/// (`super::Flags`) and through `use super::*`.
pub const PRIVATE_IMPORTS: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"imports\"\nversion = \"0.1.0\"\n",
    ),
    (
        "src/main.rs",
        "mod config;\nmod nodes;\npub(crate) use config::Flags;\n\nfn main() {}\n",
    ),
    (
        "src/config.rs",
        "pub struct Flags;\n\nimpl Flags {\n    pub fn parse() {}\n}\n",
    ),
    (
        "src/nodes.rs",
        "use crate::Flags;\n\nmod commands;\n\npub fn helper() {}\n",
    ),
    (
        "src/nodes/commands.rs",
        "use super::*;\n\nfn run() {\n    super::Flags::parse();\n    helper();\n    Flags::parse();\n}\n",
    ),
];

/// A `pub use` name the re-exporting module also uses itself.
pub const REEXPORT_USED_LOCALLY: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"local\"\nversion = \"0.1.0\"\n",
    ),
    ("src/lib.rs", "mod thing;\npub mod api;\n"),
    ("src/thing.rs", "pub struct Thing;\n"),
    (
        "src/api.rs",
        "pub use crate::thing::Thing;\n\npub fn make() -> Thing {\n    Thing\n}\n",
    ),
];

/// An explicit import of an item its module does not declare.
pub const MISSING_LEAF: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"leaf\"\nversion = \"0.1.0\"\n",
    ),
    (
        "src/lib.rs",
        "mod util;\nuse crate::util::Missing;\nuse crate::util::clamp;\nuse crate::util::Generated;\n",
    ),
    ("src/util.rs", "pub fn clamp() {}\nmake_type!(Generated);\n"),
];

/// A facade re-exports a real item and also names it in a macro call: the
/// import follows the re-export to the declaring file.
pub const MACRO_FACADE: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"facade\"\nversion = \"0.1.0\"\n",
    ),
    ("src/lib.rs", "pub mod real;\npub mod facade;\nmod app;\n"),
    ("src/real.rs", "pub struct Thing;\n"),
    (
        "src/facade.rs",
        "pub use crate::real::Thing;\nassert_send!(Thing);\n",
    ),
    (
        "src/app.rs",
        "use crate::facade::Thing;\npub fn f(_: Thing) {}\n",
    ),
];

/// Two glob imports: one module only mentions `Config` and `Deep` in a
/// macro call, the other declares `Config` and glob re-exports the module
/// that declares `Deep`. The declarations win over the mentions.
pub const MACRO_GLOBS: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"globs\"\nversion = \"0.1.0\"\n",
    ),
    ("src/lib.rs", "mod a;\nmod b;\nmod deep;\nmod app;\n"),
    ("src/a.rs", "register!(Config, Deep);\npub fn helper() {}\n"),
    (
        "src/b.rs",
        "pub use crate::deep::*;\npub struct Config;\nimpl Config {\n    pub fn new() -> Self {\n        Config\n    }\n}\n",
    ),
    (
        "src/deep.rs",
        "pub struct Deep;\nimpl Deep {\n    pub fn new() -> Self {\n        Deep\n    }\n}\n",
    ),
    (
        "src/app.rs",
        "use crate::a::*;\nuse crate::b::*;\npub fn f() {\n    let _ = Config::new();\n    let _ = Deep::new();\n}\n",
    ),
];

/// Names that only a macro call mentions are a guess (`macro`); a name
/// nothing mentions is still a missing item.
pub const MACRO_TABLE: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"table\"\nversion = \"0.1.0\"\n",
    ),
    (
        "src/lib.rs",
        "mod m;\nuse crate::m::Missing;\nuse crate::m::Integer;\nuse crate::m::Absent;\n",
    ),
    (
        "src/m.rs",
        "table! {\n    posts (id) {\n        Missing -> Text,\n        id -> Integer,\n    }\n}\n",
    ),
];
