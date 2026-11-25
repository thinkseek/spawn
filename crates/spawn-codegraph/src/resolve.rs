//! Resolution. Rust: units and targets from `Cargo.toml`, module trees from
//! each target root, and every `use`, path and `include*!` resolved to a
//! file of the in-scope inventory. JavaScript / TypeScript: npm units from
//! `package.json`, `tsconfig.json` / `jsconfig.json` paths, and every
//! import, re-export, `import(...)` and `require(...)` resolved the same
//! way, named imports followed through barrels.
//!
//! A pure function of its inputs: it looks paths up in the inventory set it
//! is given and never probes a disk, so it cannot read an excluded or
//! secret file and is testable from string tables.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use spawn_core::feature_deps::{
    CodeGraph, Diagnostic, DiagnosticCode, EdgeKind, FileClass, FileEdge, FileInfo, Language,
    Precision, Scope, Unit, UnitKind,
};

use crate::manifest::{self, CargoManifest};
use crate::rust::{self, FileFacts, ModuleFacts, UseEntry};

/// Re-exports are followed at most this deep.
pub const MAX_REEXPORT_DEPTH: u8 = 8;

/// The directory of a repository-relative path (`""` at the root).
fn dir_of(path: &str) -> &str {
    path.rfind('/').map_or("", |at| &path[..at])
}

fn join(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        rel.to_string()
    } else {
        format!("{dir}/{rel}")
    }
}

/// `path` with `.` and `..` resolved; `None` when it leaves the repository.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

#[derive(Debug, Clone)]
struct Krate {
    unit: usize,
    root: String,
    scope: Scope,
    is_lib: bool,
    /// Crate names this target may use: in-repo dependencies (by alias) and
    /// its own package's library.
    externs: BTreeMap<String, usize>,
    root_module: usize,
}

#[derive(Debug, Clone)]
struct Module {
    krate: usize,
    file: String,
    inline: Vec<String>,
    parent: Option<usize>,
    children: BTreeMap<String, usize>,
    /// Where `mod name;` inside this module looks for `name.rs`.
    child_dir: String,
    test: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Hit {
    module: usize,
    item: bool,
    via: Option<String>,
    /// The walk stopped at a module with segments left that name nothing it
    /// declares (a missing item).
    leftover: bool,
    /// A guess: nothing declares the name, but an item-level macro call of
    /// `module` mentions it (precision `macro`).
    macro_guess: bool,
}

impl Hit {
    fn item(module: usize, via: Option<String>) -> Self {
        Self {
            module,
            item: true,
            via,
            leftover: false,
            macro_guess: false,
        }
    }

    fn macro_guess(module: usize, via: Option<String>) -> Self {
        Self {
            macro_guess: true,
            ..Self::item(module, via)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Lookup {
    Found(Hit),
    External,
    Unresolved,
    /// A re-export chain longer than [`MAX_REEXPORT_DEPTH`]: nothing is
    /// claimed.
    TooDeep,
}

/// The stack of the thread that parses: [`rust::MAX_SYN_TOKENS`] bounds
/// what any one `syn` call reads, and the costliest shapes at that budget
/// take about half of it in a debug build, so the bound holds whatever
/// stack the caller's thread has (a test or HTTP worker: 2 MiB).
const PARSE_STACK_BYTES: usize = 64 * 1024 * 1024;

/// Parse every Rust source on one thread with [`PARSE_STACK_BYTES`] of
/// stack. A panic while parsing one file lists only that file unparsed;
/// when the thread cannot be started, every file is listed unparsed rather
/// than parsed on the caller's smaller stack.
fn parse_all(sources: &BTreeMap<String, String>) -> Vec<(String, Result<rust::FileFacts, String>)> {
    let rust_sources = || {
        sources
            .iter()
            .filter(|(path, _)| crate::is_rust_source(path))
    };
    let work = || {
        rust_sources()
            .map(|(path, text)| {
                let facts = std::panic::catch_unwind(|| rust::parse(text))
                    .unwrap_or_else(|_| Err("the parser panicked on this file".to_string()));
                (path.clone(), facts)
            })
            .collect::<Vec<_>>()
    };
    let unparsed = |why: &str| {
        rust_sources()
            .map(|(path, _)| (path.clone(), Err(why.to_string())))
            .collect::<Vec<_>>()
    };
    std::thread::scope(|scope| {
        match std::thread::Builder::new()
            .name("codegraph-parse".to_string())
            .stack_size(PARSE_STACK_BYTES)
            .spawn_scoped(scope, work)
        {
            Ok(handle) => handle
                .join()
                .unwrap_or_else(|_| unparsed("the parser panicked")),
            Err(_) => unparsed("the parser's thread could not be started"),
        }
    })
}

struct PackageUnit {
    unit: Unit,
    manifest: CargoManifest,
    package: String,
}

struct Graph<'a> {
    inventory: &'a BTreeSet<String>,
    facts: BTreeMap<String, FileFacts>,
    units: Vec<PackageUnit>,
    crates: Vec<Krate>,
    modules: Vec<Module>,
    edges: BTreeSet<FileEdge>,
    diagnostics: Vec<Diagnostic>,
    external_imports: usize,
}

/// Build the code graph of every source and manifest in `sources` (path
/// to text): Rust and `Cargo.toml`, JavaScript / TypeScript and
/// `package.json` / `tsconfig.json`. References resolve only to paths in
/// `inventory`, the in-scope file set.
pub fn build(inventory: &BTreeSet<String>, sources: &BTreeMap<String, String>) -> CodeGraph {
    merge(
        build_rust(inventory, sources),
        build_ecma(inventory, sources),
    )
}

/// Build the code graph of the Rust sources and `Cargo.toml` manifests in
/// `sources`.
pub fn build_rust(inventory: &BTreeSet<String>, sources: &BTreeMap<String, String>) -> CodeGraph {
    let mut graph = Graph {
        inventory,
        facts: BTreeMap::new(),
        units: Vec::new(),
        crates: Vec::new(),
        modules: Vec::new(),
        edges: BTreeSet::new(),
        diagnostics: Vec::new(),
        external_imports: 0,
    };
    graph.read_sources(sources);
    graph.find_targets();
    graph.build_module_trees();
    graph.resolve_all();
    graph.finish(sources)
}

fn diagnostic(code: DiagnosticCode, path: &str, line: Option<u32>, message: String) -> Diagnostic {
    Diagnostic {
        code,
        path: path.to_string(),
        line,
        message,
    }
}

impl Graph<'_> {
    fn read_sources(&mut self, sources: &BTreeMap<String, String>) {
        let mut workspace_packages: BTreeMap<String, String> = BTreeMap::new();
        for (path, text) in sources {
            if crate::is_cargo_manifest(path) {
                match manifest::read_cargo(text) {
                    Ok(manifest) => {
                        for dep in &manifest.workspace_dependencies {
                            workspace_packages.insert(dep.alias.clone(), dep.package.clone());
                        }
                        if let Some(package) = manifest.package.clone() {
                            self.units.push(PackageUnit {
                                unit: Unit {
                                    id: format!("cargo:{package}"),
                                    kind: UnitKind::CargoPackage,
                                    root: dir_of(path).to_string(),
                                    entry_files: Vec::new(),
                                    declared_units: Vec::new(),
                                },
                                manifest,
                                package,
                            });
                        }
                    }
                    Err(error) => self.diagnostics.push(diagnostic(
                        DiagnosticCode::ManifestUnsupported,
                        path,
                        (error.line > 0).then_some(error.line),
                        format!("not read, no unit made: {}", error.message),
                    )),
                }
            }
        }
        for (path, parsed) in parse_all(sources) {
            match parsed {
                Ok(facts) => {
                    if facts.token_fallback {
                        self.diagnostics.push(diagnostic(
                            DiagnosticCode::TokenFallback,
                            &path,
                            None,
                            "part of the file was read with a token walk (syn refused it, or it \
                             could not be cut to the token budget): only its a::b paths"
                                .to_string(),
                        ));
                    }
                    self.facts.insert(path, facts);
                }
                Err(error) => {
                    self.diagnostics
                        .push(diagnostic(DiagnosticCode::Unparsed, &path, None, error));
                }
            }
        }
        // Dependencies inherited from the workspace name its package.
        for unit in &mut self.units {
            for dep in &mut unit.manifest.dependencies {
                if dep.from_workspace
                    && let Some(package) = workspace_packages.get(&dep.alias)
                {
                    dep.package.clone_from(package);
                }
            }
        }
    }

    /// The Rust files directly in `<root>/<dir>/` (`x.rs`) and their
    /// `<root>/<dir>/<x>/main.rs`.
    fn auto_targets(&self, root: &str, dir: &str) -> Vec<String> {
        let prefix = join(root, dir) + "/";
        self.facts
            .keys()
            .filter(|path| {
                path.strip_prefix(&prefix).is_some_and(|rest| {
                    (!rest.contains('/') && crate::is_rust_source(rest))
                        || rest
                            .strip_suffix("/main.rs")
                            .is_some_and(|name| !name.contains('/'))
                })
            })
            .cloned()
            .collect()
    }

    #[allow(clippy::too_many_lines)]
    fn find_targets(&mut self) {
        let by_package: BTreeMap<String, usize> = self
            .units
            .iter()
            .enumerate()
            .map(|(index, unit)| (unit.package.clone(), index))
            .collect();
        let mut lib_of: BTreeMap<usize, usize> = BTreeMap::new();
        let mut lib_ident: BTreeMap<usize, String> = BTreeMap::new();
        for unit_index in 0..self.units.len() {
            let unit = &self.units[unit_index];
            let root = unit.unit.root.clone();
            let manifest = unit.manifest.clone();
            let mut roots: Vec<(String, Scope, bool)> = Vec::new();
            let explicit = |decl: &manifest::TargetDecl| decl.path.as_ref().map(|p| join(&root, p));

            let lib_path = manifest
                .lib
                .as_ref()
                .and_then(explicit)
                .unwrap_or_else(|| join(&root, "src/lib.rs"));
            if self.facts.contains_key(&lib_path) {
                roots.push((lib_path, Scope::Prod, true));
                lib_ident.insert(
                    unit_index,
                    manifest
                        .lib
                        .as_ref()
                        .and_then(|lib| lib.name.clone())
                        .unwrap_or_else(|| unit.package.replace('-', "_")),
                );
            }
            let mut bins: Vec<String> = manifest.bins.iter().filter_map(explicit).collect();
            for bin in manifest.bins.iter().filter(|bin| bin.path.is_none()) {
                if let Some(name) = &bin.name {
                    bins.push(join(&root, &format!("src/bin/{name}.rs")));
                    bins.push(join(&root, &format!("src/bin/{name}/main.rs")));
                }
            }
            bins.push(join(&root, "src/main.rs"));
            bins.extend(self.auto_targets(&root, "src/bin"));
            roots.extend(bins.into_iter().map(|path| (path, Scope::Prod, false)));
            for (decls, dir) in [
                (&manifest.tests, "tests"),
                (&manifest.examples, "examples"),
                (&manifest.benches, "benches"),
            ] {
                let mut paths: Vec<String> = decls.iter().filter_map(explicit).collect();
                paths.extend(self.auto_targets(&root, dir));
                roots.extend(paths.into_iter().map(|path| (path, Scope::Test, false)));
            }
            match &manifest.build {
                Some(Some(path)) => roots.push((join(&root, path), Scope::Build, false)),
                Some(None) => {}
                None => roots.push((join(&root, "build.rs"), Scope::Build, false)),
            }

            let mut seen = BTreeSet::new();
            for (path, scope, is_lib) in roots {
                if !self.facts.contains_key(&path) || !seen.insert(path.clone()) {
                    continue;
                }
                if is_lib {
                    lib_of.insert(unit_index, self.crates.len());
                }
                self.crates.push(Krate {
                    unit: unit_index,
                    root: path,
                    scope,
                    is_lib,
                    externs: BTreeMap::new(),
                    root_module: 0,
                });
            }
        }

        // Crate names: in-repo dependencies and the package's own library.
        let mut declared: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
        for krate in &mut self.crates {
            let unit = &self.units[krate.unit];
            for dep in &unit.manifest.dependencies {
                let Some(&target_unit) = by_package.get(&dep.package) else {
                    continue;
                };
                if target_unit == krate.unit {
                    continue;
                }
                declared
                    .entry(krate.unit)
                    .or_default()
                    .insert(self.units[target_unit].unit.id.clone());
                if let Some(&lib) = lib_of.get(&target_unit) {
                    krate.externs.insert(dep.alias.replace('-', "_"), lib);
                }
            }
            if !krate.is_lib
                && let (Some(&lib), Some(ident)) =
                    (lib_of.get(&krate.unit), lib_ident.get(&krate.unit))
            {
                krate.externs.insert(ident.clone(), lib);
            }
        }
        for (index, unit) in self.units.iter_mut().enumerate() {
            unit.unit.declared_units = declared
                .remove(&index)
                .unwrap_or_default()
                .into_iter()
                .collect();
        }
    }

    fn build_module_trees(&mut self) {
        for krate_index in 0..self.crates.len() {
            let root = self.crates[krate_index].root.clone();
            let root_module = self.modules.len();
            self.crates[krate_index].root_module = root_module;
            self.modules.push(Module {
                krate: krate_index,
                child_dir: dir_of(&root).to_string(),
                file: root,
                inline: Vec::new(),
                parent: None,
                children: BTreeMap::new(),
                test: false,
            });
            let mut seen: BTreeSet<(String, Vec<String>)> = BTreeSet::new();
            let mut queue = VecDeque::from([root_module]);
            while let Some(at) = queue.pop_front() {
                let module = self.modules[at].clone();
                if !seen.insert((module.file.clone(), module.inline.clone())) {
                    continue;
                }
                let decls = self
                    .module_facts(at)
                    .map(|facts| facts.mods.clone())
                    .unwrap_or_default();
                for decl in decls {
                    let (file, inline, child_dir) = if decl.inline.is_some() {
                        let mut inline = module.inline.clone();
                        inline.push(decl.name.clone());
                        (
                            module.file.clone(),
                            inline,
                            join(&module.child_dir, &decl.name),
                        )
                    } else if let Some(file) = self.mod_file(&module, &decl) {
                        let child_dir = if decl.path_attr.is_some() {
                            dir_of(&file).to_string()
                        } else {
                            join(&module.child_dir, &decl.name)
                        };
                        let scope = self.scope_of(at, decl.test);
                        if file != module.file {
                            self.edges.insert(FileEdge {
                                from: module.file.clone(),
                                to: file.clone(),
                                kind: EdgeKind::Mod,
                                scope,
                                line: decl.line,
                                via: None,
                                precision: Precision::Module,
                            });
                        }
                        (file, Vec::new(), child_dir)
                    } else {
                        self.diagnostics.push(diagnostic(
                            DiagnosticCode::Unresolved,
                            &module.file,
                            Some(decl.line),
                            format!("mod {}; names no file of the in-scope inventory", decl.name),
                        ));
                        continue;
                    };
                    let child = self.modules.len();
                    self.modules.push(Module {
                        krate: krate_index,
                        file,
                        inline,
                        parent: Some(at),
                        children: BTreeMap::new(),
                        child_dir,
                        test: module.test || decl.test,
                    });
                    self.modules[at].children.insert(decl.name.clone(), child);
                    queue.push_back(child);
                }
            }
        }
    }

    /// The file `mod name;` in `module` loads, if the inventory holds it.
    fn mod_file(&self, module: &Module, decl: &rust::ModDecl) -> Option<String> {
        if let Some(path) = &decl.path_attr {
            let base = if module.inline.is_empty() {
                dir_of(&module.file)
            } else {
                module.child_dir.as_str()
            };
            return normalize(&join(base, path)).filter(|file| self.inventory.contains(file));
        }
        [
            join(&module.child_dir, &format!("{}.rs", decl.name)),
            join(&module.child_dir, &format!("{}/mod.rs", decl.name)),
        ]
        .into_iter()
        .find(|file| self.inventory.contains(file))
    }

    fn module_facts(&self, module: usize) -> Option<&ModuleFacts> {
        let module = &self.modules[module];
        self.facts
            .get(&module.file)
            .and_then(|facts| facts.root.nested(&module.inline))
    }

    fn scope_of(&self, module: usize, test: bool) -> Scope {
        let module = &self.modules[module];
        match self.crates[module.krate].scope {
            Scope::Prod if test || module.test => Scope::Test,
            scope => scope,
        }
    }

    fn uses_of(&self, module: usize) -> &[UseEntry] {
        self.module_facts(module)
            .map_or(&[][..], |facts| facts.uses.as_slice())
    }

    /// Resolve `segments` as written in `at`. `skip` is the alias the
    /// reference itself introduces (`use serde;` does not name itself).
    fn resolve(
        &self,
        at: usize,
        segments: &[String],
        leading_colon: bool,
        depth: u8,
        skip: Option<&str>,
    ) -> Lookup {
        self.resolve_in(at, segments, leading_colon, depth, skip, true)
    }

    /// [`Self::resolve`]; with `globs` false a first segment is not looked
    /// up through glob imports, which resolving a glob's own path needs
    /// (`use std::io::*;` would otherwise look `std` up through itself).
    fn resolve_in(
        &self,
        at: usize,
        segments: &[String],
        leading_colon: bool,
        depth: u8,
        skip: Option<&str>,
        globs: bool,
    ) -> Lookup {
        let Some(first) = segments.first() else {
            return Lookup::External;
        };
        if depth > MAX_REEXPORT_DEPTH {
            return Lookup::TooDeep;
        }
        let krate = &self.crates[self.modules[at].krate];
        let root = |krate: usize| self.crates[krate].root_module;
        if leading_colon {
            return match krate.externs.get(first) {
                Some(&lib) => self.walk(root(lib), &segments[1..], depth, None),
                None => Lookup::External,
            };
        }
        match first.as_str() {
            "crate" | "$crate" => self.walk(krate.root_module, &segments[1..], depth, None),
            "self" => self.walk(at, &segments[1..], depth, None),
            "super" => self.walk(at, segments, depth, None),
            name => {
                if let Some(entry) = self.uses_of(at).iter().find(|entry| {
                    !entry.glob && entry.name.as_deref() == Some(name) && skip != Some(name)
                }) {
                    let mut full = entry.segments.clone();
                    full.extend_from_slice(&segments[1..]);
                    return self.resolve_in(
                        at,
                        &full,
                        entry.leading_colon,
                        depth + 1,
                        Some(name),
                        globs,
                    );
                }
                if let Some(&child) = self.modules[at].children.get(name) {
                    return self.walk(child, &segments[1..], depth, None);
                }
                if let Some(&lib) = krate.externs.get(name) {
                    return self.walk(root(lib), &segments[1..], depth, None);
                }
                if self
                    .module_facts(at)
                    .is_some_and(|facts| facts.items.contains(name))
                {
                    // A local item: explicit names shadow glob imports.
                    return Lookup::Found(Hit::item(at, None));
                }
                if !globs {
                    return Lookup::External;
                }
                // A name a glob import of this module brings in
                // (`use super::*;`): a real declaration behind any glob
                // first, a macro mention only after none has one.
                let (globs, too_deep) = self.glob_modules(at, depth);
                if let Some(&glob) = globs.iter().find(|glob| self.provides(**glob, name, depth)) {
                    return self.walk(glob, segments, depth + 1, None);
                }
                if too_deep {
                    return Lookup::TooDeep;
                }
                if self.mentions(at, name) {
                    return Lookup::Found(Hit::macro_guess(at, None));
                }
                if let Some(&glob) = globs
                    .iter()
                    .find(|glob| self.reaches_mention(**glob, name, depth))
                {
                    return self.walk(glob, segments, depth + 1, None);
                }
                Lookup::External
            }
        }
    }

    /// Whether module `module` declares `name`: a child module, an item,
    /// or a named `use`. Macro mentions do not count.
    fn declares(&self, module: usize, name: &str) -> bool {
        self.modules[module].children.contains_key(name)
            || self.module_facts(module).is_some_and(|facts| {
                facts.items.contains(name)
                    || facts
                        .uses
                        .iter()
                        .any(|entry| !entry.glob && entry.name.as_deref() == Some(name))
            })
    }

    /// Whether an item-level macro call of `module` mentions `name`.
    fn mentions(&self, module: usize, name: &str) -> bool {
        self.module_facts(module)
            .is_some_and(|facts| facts.macro_items.contains(name))
    }

    /// The modules the glob imports of `module` bring in, in source order,
    /// and whether resolving one of them passed the re-export depth cap.
    fn glob_modules(&self, module: usize, depth: u8) -> (Vec<usize>, bool) {
        let mut found = Vec::new();
        let mut too_deep = false;
        for entry in self.uses_of(module).iter().filter(|entry| entry.glob) {
            match self.resolve_in(
                module,
                &entry.segments,
                entry.leading_colon,
                depth + 1,
                None,
                false,
            ) {
                Lookup::TooDeep => too_deep = true,
                Lookup::Found(hit) if !hit.item && hit.module != module => found.push(hit.module),
                _ => {}
            }
        }
        (found, too_deep)
    }

    /// Whether `test` holds for `module` or for a module its glob imports
    /// reach, transitively (`pub use inner::*;` chains). Each module is
    /// visited once.
    fn glob_reach(&self, module: usize, depth: u8, test: impl Fn(usize) -> bool) -> bool {
        let mut seen = BTreeSet::from([module]);
        let mut queue = vec![module];
        while let Some(next) = queue.pop() {
            if test(next) {
                return true;
            }
            for glob in self.glob_modules(next, depth).0 {
                if seen.insert(glob) {
                    queue.push(glob);
                }
            }
        }
        false
    }

    /// Whether `module`, or a module its globs reach, declares `name`.
    fn provides(&self, module: usize, name: &str, depth: u8) -> bool {
        self.glob_reach(module, depth, |next| self.declares(next, name))
    }

    /// Whether `module`, or a module its globs reach, has a macro call that
    /// mentions `name`.
    fn reaches_mention(&self, module: usize, name: &str, depth: u8) -> bool {
        self.glob_reach(module, depth, |next| self.mentions(next, name))
    }

    /// Follow `rest` from `start` down the module tree, through named and
    /// glob imports, to the file that declares the item (precision `item`)
    /// or the last module reached (precision `module`). Real declarations
    /// win everywhere: a module whose item-level macro call mentions the
    /// name (precision `macro`) is consulted only after this module's items,
    /// named imports and every glob import's declarations have none. Imports of any
    /// visibility are followed: code that compiles names a private import
    /// only from the importing module's descendants (`super::Flags`).
    fn walk(&self, start: usize, rest: &[String], depth: u8, via: Option<String>) -> Lookup {
        let mut at = start;
        let mut leftover = false;
        for (index, segment) in rest.iter().enumerate() {
            match segment.as_str() {
                "self" => continue,
                "super" => {
                    let Some(parent) = self.modules[at].parent else {
                        return Lookup::Unresolved;
                    };
                    at = parent;
                    continue;
                }
                _ => {}
            }
            if let Some(&child) = self.modules[at].children.get(segment) {
                at = child;
                continue;
            }
            leftover = true;
            let Some(facts) = self.module_facts(at) else {
                break;
            };
            if facts.items.contains(segment) {
                return Lookup::Found(Hit::item(at, via));
            }
            let first_via = || via.clone().or_else(|| Some(self.modules[at].file.clone()));
            if let Some(entry) = facts
                .uses
                .iter()
                .find(|entry| !entry.glob && entry.name.as_deref() == Some(segment.as_str()))
            {
                return match self.resolve(
                    at,
                    &entry.segments,
                    entry.leading_colon,
                    depth + 1,
                    Some(segment),
                ) {
                    Lookup::Found(hit) if hit.item || index + 1 == rest.len() => {
                        Lookup::Found(Hit {
                            via: first_via(),
                            ..hit
                        })
                    }
                    Lookup::Found(hit) => {
                        self.walk(hit.module, &rest[index + 1..], depth + 1, first_via())
                    }
                    Lookup::TooDeep => Lookup::TooDeep,
                    // Imported from outside the repository: this file.
                    Lookup::External | Lookup::Unresolved => Lookup::Found(Hit::item(at, via)),
                };
            }
            let (globs, too_deep) = self.glob_modules(at, depth);
            if let Some(&glob) = globs
                .iter()
                .find(|glob| self.provides(**glob, segment, depth))
            {
                return self.walk(glob, &rest[index..], depth + 1, first_via());
            }
            if too_deep {
                return Lookup::TooDeep;
            }
            if facts.macro_items.contains(segment) {
                return Lookup::Found(Hit::macro_guess(at, via));
            }
            if let Some(&glob) = globs
                .iter()
                .find(|glob| self.reaches_mention(**glob, segment, depth))
            {
                return self.walk(glob, &rest[index..], depth + 1, first_via());
            }
            break;
        }
        Lookup::Found(Hit {
            module: at,
            item: false,
            via,
            leftover,
            macro_guess: false,
        })
    }

    fn add_edge(&mut self, from: &str, hit: &Hit, kind: EdgeKind, scope: Scope, line: u32) {
        let to = &self.modules[hit.module].file;
        if to == from {
            return;
        }
        self.edges.insert(FileEdge {
            from: from.to_string(),
            to: to.clone(),
            kind,
            scope,
            line,
            via: hit.via.clone().filter(|via| via != to),
            precision: if hit.macro_guess {
                Precision::Macro
            } else if hit.item {
                Precision::Item
            } else {
                Precision::Module
            },
        });
    }

    fn too_deep(&mut self, file: &str, line: u32, written: &str) {
        self.diagnostics.push(diagnostic(
            DiagnosticCode::ReexportTooDeep,
            file,
            Some(line),
            format!(
                "{written} passes through more than {MAX_REEXPORT_DEPTH} re-exports; no edge is \
                 claimed"
            ),
        ));
    }

    fn resolve_all(&mut self) {
        for at in 0..self.modules.len() {
            let Some(facts) = self.module_facts(at).cloned() else {
                continue;
            };
            let file = self.modules[at].file.clone();
            for entry in &facts.uses {
                let kind = if entry.public {
                    EdgeKind::Reexport
                } else {
                    EdgeKind::Use
                };
                let scope = self.scope_of(at, entry.test);
                let written = entry.segments.join("::");
                match self.resolve(
                    at,
                    &entry.segments,
                    entry.leading_colon,
                    0,
                    entry.name.as_deref(),
                ) {
                    Lookup::Found(hit) => {
                        if hit.leftover && !entry.glob {
                            self.diagnostics.push(diagnostic(
                                DiagnosticCode::UnresolvedItem,
                                &file,
                                Some(entry.line),
                                format!(
                                    "use {written}: {} declares no such item, not even through an \
                                     item-level macro call; the edge stops at that module",
                                    self.modules[hit.module].file
                                ),
                            ));
                        }
                        self.add_edge(&file, &hit, kind, scope, entry.line);
                        // A re-exported name the module also uses itself is
                        // a real dependency besides the structural re-export.
                        if let (true, Some(name)) = (entry.public, &entry.name)
                            && let Some(&(line, test)) = facts.local_refs.get(name)
                        {
                            let scope = self.scope_of(at, test);
                            self.add_edge(&file, &hit, EdgeKind::Use, scope, line);
                        }
                    }
                    Lookup::External => self.external_imports += 1,
                    Lookup::Unresolved => self.diagnostics.push(diagnostic(
                        DiagnosticCode::Unresolved,
                        &file,
                        Some(entry.line),
                        format!("use {written} resolves to nothing"),
                    )),
                    Lookup::TooDeep => self.too_deep(&file, entry.line, &written),
                }
            }
            for path in &facts.paths {
                let scope = self.scope_of(at, path.test);
                match self.resolve(at, &path.segments, path.leading_colon, 0, None) {
                    Lookup::Found(hit) => {
                        self.add_edge(&file, &hit, EdgeKind::Path, scope, path.line);
                    }
                    Lookup::TooDeep => self.too_deep(&file, path.line, &path.segments.join("::")),
                    Lookup::External | Lookup::Unresolved => {}
                }
            }
            for include in &facts.includes {
                let scope = self.scope_of(at, include.test);
                let base = if include.from_manifest_dir {
                    self.units[self.crates[self.modules[at].krate].unit]
                        .unit
                        .root
                        .clone()
                } else {
                    dir_of(&file).to_string()
                };
                match normalize(&join(&base, &include.path)) {
                    Some(target) if self.inventory.contains(&target) => {
                        if target != file {
                            self.edges.insert(FileEdge {
                                from: file.clone(),
                                to: target,
                                kind: EdgeKind::Include,
                                scope,
                                line: include.line,
                                via: None,
                                precision: Precision::Item,
                            });
                        }
                    }
                    _ => self.diagnostics.push(diagnostic(
                        DiagnosticCode::Unresolved,
                        &file,
                        Some(include.line),
                        format!(
                            "include of {} names no file of the in-scope inventory",
                            include.path
                        ),
                    )),
                }
            }
        }
    }

    fn unit_of(&self, path: &str) -> Option<usize> {
        self.units
            .iter()
            .enumerate()
            .filter(|(_, unit)| {
                unit.unit.root.is_empty() || path.starts_with(&format!("{}/", unit.unit.root))
            })
            .max_by_key(|(_, unit)| unit.unit.root.len())
            .map(|(index, _)| index)
    }

    fn finish(mut self, sources: &BTreeMap<String, String>) -> CodeGraph {
        let mut contexts: BTreeMap<&str, Vec<Scope>> = BTreeMap::new();
        for at in 0..self.modules.len() {
            let scope = self.scope_of(at, false);
            contexts
                .entry(self.modules[at].file.as_str())
                .or_default()
                .push(scope);
        }
        let entries: BTreeSet<&str> = self.crates.iter().map(|k| k.root.as_str()).collect();
        let mut files = BTreeMap::new();
        let mut orphans = Vec::new();
        for path in sources.keys().filter(|path| crate::is_rust_source(path)) {
            let unit = self.unit_of(path);
            let unit_root = unit.map_or("", |unit| self.units[unit].unit.root.as_str());
            let relative = path.strip_prefix(&format!("{unit_root}/")).unwrap_or(path);
            let scopes = contexts.get(path.as_str());
            let test_dir = ["tests/", "benches/", "examples/"]
                .iter()
                .any(|dir| relative.starts_with(dir));
            let only_test = scopes.is_some_and(|scopes| scopes.iter().all(|s| *s == Scope::Test));
            // An unparsed file is already listed as such, not as an orphan.
            if unit.is_some() && scopes.is_none() && self.facts.contains_key(path) {
                orphans.push(path.clone());
            }
            files.insert(
                path.clone(),
                FileInfo {
                    language: Language::Rust,
                    unit: unit.map(|unit| self.units[unit].unit.id.clone()),
                    class: if test_dir || only_test {
                        FileClass::Test
                    } else {
                        FileClass::Source
                    },
                    is_entry: entries.contains(path.as_str()),
                    in_module_tree: scopes.is_some(),
                    parsed: self.facts.contains_key(path),
                },
            );
        }
        for path in orphans {
            self.diagnostics.push(diagnostic(
                DiagnosticCode::OrphanSource,
                &path,
                None,
                "no module tree of its unit reaches it: the compiler never sees it".to_string(),
            ));
        }
        for krate in &self.crates {
            if krate.scope == Scope::Prod {
                self.units[krate.unit]
                    .unit
                    .entry_files
                    .push(krate.root.clone());
            }
        }
        let mut units: Vec<Unit> = self
            .units
            .into_iter()
            .map(|mut unit| {
                unit.unit.entry_files.sort();
                unit.unit.entry_files.dedup();
                unit.unit
            })
            .collect();
        units.sort_by(|a, b| (&a.id, &a.root).cmp(&(&b.id, &b.root)));
        self.diagnostics.sort();
        self.diagnostics.dedup();
        CodeGraph {
            units,
            edges: self.edges.into_iter().collect(),
            files,
            diagnostics: self.diagnostics,
            external_imports: self.external_imports,
        }
    }
}

// ---------------------------------------------------------------------
// JavaScript / TypeScript resolution: npm units from `package.json`,
// `tsconfig.json` / `jsconfig.json` paths, and every import, re-export,
// `import(...)` and `require(...)` resolved to a file of the in-scope
// inventory, the way TypeScript's `bundler` resolution and Node's package
// `exports` / `imports` would, without a `node_modules` (external packages
// are counted, never followed).

/// The suffixes tried after a specifier without one, in order.
const ECMA_SUFFIXES: [&str; 10] = [
    ".ts", ".tsx", ".d.ts", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".json",
];

/// `extends` chains of `tsconfig.json` files are followed at most this deep.
const MAX_TSCONFIG_DEPTH: usize = 16;

/// Conditions of a package's `exports` / `imports` tried first, in order.
const EXPORT_CONDITIONS: [&str; 4] = ["import", "default", "require", "types"];

/// Nested condition objects are read at most this deep.
const MAX_CONDITION_DEPTH: usize = 16;

/// The TypeScript files a `.js`-family specifier may stand for.
fn ts_counterparts(path: &str) -> Vec<String> {
    let pairs: [(&str, &[&str]); 4] = [
        (".js", &[".ts", ".tsx", ".d.ts"]),
        (".jsx", &[".tsx"]),
        (".mjs", &[".mts", ".d.mts"]),
        (".cjs", &[".cts", ".d.cts"]),
    ];
    for (ext, targets) in pairs {
        if let Some(stem) = path.strip_suffix(ext) {
            return targets.iter().map(|t| format!("{stem}{t}")).collect();
        }
    }
    Vec::new()
}

/// The part of `text` a single-`*` pattern matches, if it does.
fn pattern_match<'t>(pattern: &str, text: &'t str) -> Option<&'t str> {
    let (prefix, suffix) = pattern.split_once('*')?;
    if suffix.contains('*') || text.len() < prefix.len() + suffix.len() {
        return None;
    }
    text.strip_prefix(prefix)?.strip_suffix(suffix)
}

/// A named `package.json` (an npm unit) or a nameless one (its `imports`
/// and directory entry still count).
struct NpmManifest {
    manifest: manifest::PackageJson,
    /// The unit id when the package has a name no other package has.
    unit: Option<String>,
}

/// The `paths` and `baseUrl` a `tsconfig.json` ends with after its
/// `extends` chain, as repository-relative directories.
#[derive(Debug, Clone, Default)]
struct TsPaths {
    base_url: Option<String>,
    paths: Vec<(String, Vec<String>)>,
    /// The directory `paths` targets are relative to without a `baseUrl`.
    paths_dir: String,
}

impl TsPaths {
    fn paths_base(&self) -> &str {
        self.base_url.as_deref().unwrap_or(&self.paths_dir)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    File(String, Precision),
    External,
    Unresolved,
}

enum Follow {
    /// The file that declares the name (`true`), or the file a named
    /// re-export points at without declaring it (`false`).
    Found(String, bool),
    NotFound,
    TooDeep,
}

struct EcmaGraph<'a> {
    inventory: &'a BTreeSet<String>,
    facts: BTreeMap<String, crate::ecma::EcmaFacts>,
    /// Package directory to its manifest.
    packages: BTreeMap<String, NpmManifest>,
    /// Package name to its directory (unique names only).
    by_name: BTreeMap<String, String>,
    /// Directory of each `tsconfig.json` / `jsconfig.json` to its merged
    /// paths.
    tsconfigs: BTreeMap<String, TsPaths>,
    edges: BTreeSet<FileEdge>,
    diagnostics: Vec<Diagnostic>,
    external_imports: usize,
}

/// Build the code graph of the JavaScript and TypeScript sources and their
/// manifests in `sources`; `.vue` and `.svelte` files of `inventory` are
/// listed `unsupported`.
pub fn build_ecma(inventory: &BTreeSet<String>, sources: &BTreeMap<String, String>) -> CodeGraph {
    let mut graph = EcmaGraph {
        inventory,
        facts: BTreeMap::new(),
        packages: BTreeMap::new(),
        by_name: BTreeMap::new(),
        tsconfigs: BTreeMap::new(),
        edges: BTreeSet::new(),
        diagnostics: Vec::new(),
        external_imports: 0,
    };
    for path in inventory
        .iter()
        .filter(|path| crate::ecma::is_unsupported_component(path))
    {
        graph.diagnostics.push(diagnostic(
            DiagnosticCode::Unsupported,
            path,
            None,
            "a single-file component: no extractor reads it, its imports miss an edge".to_string(),
        ));
    }
    graph.read_manifests(sources);
    for (path, text) in sources
        .iter()
        .filter(|(path, _)| crate::ecma::is_ecma_source(path))
    {
        let parsed = std::panic::catch_unwind(|| crate::ecma::parse_at(path, text))
            .unwrap_or_else(|_| Err("the lexer panicked on this file".to_string()));
        match parsed {
            Ok(facts) => {
                graph.facts.insert(path.clone(), facts);
            }
            Err(error) => {
                graph
                    .diagnostics
                    .push(diagnostic(DiagnosticCode::Unparsed, path, None, error));
            }
        }
    }
    graph.resolve_all();
    graph.finish(sources)
}

impl EcmaGraph<'_> {
    fn read_manifests(&mut self, sources: &BTreeMap<String, String>) {
        let mut names: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (path, text) in sources
            .iter()
            .filter(|(path, _)| crate::is_package_json(path))
        {
            match manifest::read_package_json(text) {
                Ok(manifest) => {
                    if let Some(name) = &manifest.name {
                        names
                            .entry(name.clone())
                            .or_default()
                            .push(dir_of(path).to_string());
                    }
                    self.packages.insert(
                        dir_of(path).to_string(),
                        NpmManifest {
                            manifest,
                            unit: None,
                        },
                    );
                }
                Err(error) => self.diagnostics.push(diagnostic(
                    DiagnosticCode::ManifestUnsupported,
                    path,
                    None,
                    format!("not read, no unit made: {error}"),
                )),
            }
        }
        for (name, dirs) in names {
            if let [dir] = dirs.as_slice() {
                if let Some(package) = self.packages.get_mut(dir) {
                    package.unit = Some(format!("npm:{name}"));
                }
                self.by_name.insert(name, dir.clone());
            } else {
                for dir in dirs {
                    self.diagnostics.push(diagnostic(
                        DiagnosticCode::ManifestUnsupported,
                        &join(&dir, "package.json"),
                        None,
                        format!(
                            "package name {name} is not unique in the repository: no unit made"
                        ),
                    ));
                }
            }
        }
        let mut configs: BTreeMap<String, manifest::TsConfig> = BTreeMap::new();
        for (path, text) in sources.iter().filter(|(path, _)| crate::is_tsconfig(path)) {
            match manifest::read_tsconfig(text) {
                Ok(config) => {
                    configs.insert(path.clone(), config);
                }
                Err(error) => self.diagnostics.push(diagnostic(
                    DiagnosticCode::ManifestUnsupported,
                    path,
                    None,
                    format!("not read, its paths are not used: {error}"),
                )),
            }
        }
        let mut merged = BTreeMap::new();
        let mut memo = BTreeMap::new();
        for path in configs.keys() {
            let name = path.rsplit('/').next().unwrap_or(path);
            if name == "tsconfig.json" || name == "jsconfig.json" {
                let dir = dir_of(path).to_string();
                // `tsconfig.json` wins over `jsconfig.json` in one directory.
                if name == "jsconfig.json" && merged.contains_key(&dir) {
                    continue;
                }
                let paths = self.merge_tsconfig(&configs, path, 0, &mut Vec::new(), &mut memo);
                merged.insert(dir, paths);
            }
        }
        self.tsconfigs = merged;
    }

    /// The paths `path` ends with after its `extends` chain. Each config
    /// is merged once (`memo`) and each `extends` entry read once, so a
    /// diamond of configs extending the same parents stays linear.
    fn merge_tsconfig(
        &mut self,
        configs: &BTreeMap<String, manifest::TsConfig>,
        path: &str,
        depth: usize,
        chain: &mut Vec<String>,
        memo: &mut BTreeMap<String, TsPaths>,
    ) -> TsPaths {
        if let Some(done) = memo.get(path) {
            return done.clone();
        }
        let mut merged = TsPaths {
            paths_dir: dir_of(path).to_string(),
            ..TsPaths::default()
        };
        let Some(config) = configs.get(path) else {
            return merged;
        };
        chain.push(path.to_string());
        let mut seen = BTreeSet::new();
        for extends in config
            .extends
            .iter()
            .filter(|extends| seen.insert(extends.as_str()))
        {
            if !(extends.starts_with("./") || extends.starts_with("../")) {
                self.diagnostics.push(diagnostic(
                    DiagnosticCode::Unsupported,
                    path,
                    None,
                    format!("extends {extends}, a package config: skipped"),
                ));
                continue;
            }
            let Some(target) = normalize(&join(dir_of(path), extends)) else {
                continue;
            };
            let target = if configs.contains_key(&target)
                || std::path::Path::new(&target)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            {
                target
            } else {
                format!("{target}.json")
            };
            if !configs.contains_key(&target) {
                self.diagnostics.push(diagnostic(
                    DiagnosticCode::Unresolved,
                    path,
                    None,
                    format!("extends {extends}: no in-scope config there"),
                ));
                continue;
            }
            if chain.contains(&target) || depth >= MAX_TSCONFIG_DEPTH {
                self.diagnostics.push(diagnostic(
                    DiagnosticCode::Unsupported,
                    path,
                    None,
                    format!("extends {extends}: a cycle or a chain past {MAX_TSCONFIG_DEPTH} configs, not followed"),
                ));
                continue;
            }
            let parent = self.merge_tsconfig(configs, &target, depth + 1, chain, memo);
            if parent.base_url.is_some() {
                merged.base_url = parent.base_url;
            }
            if !parent.paths.is_empty() {
                merged.paths = parent.paths;
                merged.paths_dir = parent.paths_dir;
            }
        }
        chain.pop();
        if let Some(base_url) = &config.base_url {
            merged.base_url = normalize(&join(dir_of(path), base_url));
        }
        if let Some(paths) = &config.paths {
            merged.paths.clone_from(paths);
            merged.paths_dir = dir_of(path).to_string();
        }
        memo.insert(path.to_string(), merged.clone());
        merged
    }

    /// The nearest directory at or above `dir` that has an entry in `map`.
    fn nearest<'m, T>(map: &'m BTreeMap<String, T>, dir: &str) -> Option<(&'m str, &'m T)> {
        let mut dir = dir;
        loop {
            if let Some((key, value)) = map.get_key_value(dir) {
                return Some((key.as_str(), value));
            }
            if dir.is_empty() {
                return None;
            }
            dir = dir_of(dir);
        }
    }

    /// The in-scope file `base` names, the way a bundler resolves a path
    /// without a `node_modules` step: exact, the TypeScript file a `.js`
    /// name stands for, each suffix, then the directory's `package.json`
    /// or `index`. Whatever the specifier's spelling (`./app.component`,
    /// `./v1.2`), it is tried as code once the exact path is not in scope.
    fn file(&self, base: &str) -> Option<(String, Precision)> {
        if self.inventory.contains(base) {
            return Some((base.to_string(), Precision::Module));
        }
        let candidates = ts_counterparts(base)
            .into_iter()
            .chain(ECMA_SUFFIXES.iter().map(|suffix| format!("{base}{suffix}")));
        for candidate in candidates {
            if !base.is_empty() && self.inventory.contains(&candidate) {
                return Some((candidate, Precision::Module));
            }
        }
        if self.packages.contains_key(base) {
            return self.package_target(base, None);
        }
        self.index(base).map(|index| (index, Precision::Module))
    }

    /// `<dir>/index.*`.
    fn index(&self, dir: &str) -> Option<String> {
        ECMA_SUFFIXES
            .iter()
            .map(|suffix| join(dir, &format!("index{suffix}")))
            .find(|candidate| self.inventory.contains(candidate))
    }

    /// A target written in a package's manifest (`./dist/index.js`),
    /// resolved from its directory.
    fn in_package(&self, root: &str, target: &str) -> Option<String> {
        let path = normalize(&join(root, target))?;
        if path == root {
            return None;
        }
        if self.inventory.contains(&path) {
            return Some(path);
        }
        ts_counterparts(&path)
            .into_iter()
            .chain(ECMA_SUFFIXES.iter().map(|suffix| format!("{path}{suffix}")))
            .find(|candidate| self.inventory.contains(candidate))
            .or_else(|| self.index(&path))
    }

    /// The file package `root` gives for `subpath` (`None`: the package
    /// itself). A target that is not in scope (a `dist/` build) falls back
    /// to `<root>/src/index.*`, then to the package's `package.json`, both
    /// at `package` precision.
    fn package_target(&self, root: &str, subpath: Option<&str>) -> Option<(String, Precision)> {
        let package = &self.packages.get(root)?.manifest;
        let found = if let Some(exports) = &package.exports {
            let key = subpath.map_or_else(|| ".".to_string(), |sub| format!("./{sub}"));
            // Every condition's target in order: the first in scope wins.
            export_targets(exports, &key)
                .iter()
                .find_map(|target| self.in_package(root, target))
                .or_else(|| subpath.and_then(|sub| self.in_package(root, sub)))
        } else if let Some(sub) = subpath {
            self.in_package(root, sub)
        } else {
            [&package.module, &package.main, &package.types]
                .into_iter()
                .flatten()
                .find_map(|target| self.in_package(root, target))
                .or_else(|| self.index(root))
        };
        if let Some(file) = found {
            return Some((file, Precision::Module));
        }
        if let Some(index) = self.index(&join(root, "src")) {
            return Some((index, Precision::Package));
        }
        Some((join(root, "package.json"), Precision::Package))
            .filter(|(manifest, _)| self.inventory.contains(manifest))
    }

    /// Where `specifier`, written in `from`, leads.
    fn resolve(&self, from: &str, specifier: &str) -> Target {
        let spec = crate::ecma::strip_query(specifier);
        let dir = dir_of(from);
        let found = |hit: Option<(String, Precision)>| {
            hit.map_or(Target::Unresolved, |(file, precision)| {
                Target::File(file, precision)
            })
        };
        if spec == "." || spec == ".." || spec.starts_with("./") || spec.starts_with("../") {
            return found(normalize(&join(dir, spec)).and_then(|base| self.file(&base)));
        }
        if spec.starts_with('#') {
            let Some((root, package)) = Self::nearest(&self.packages, dir) else {
                return Target::Unresolved;
            };
            let targets = package
                .manifest
                .imports
                .as_ref()
                .map(|imports| export_targets(imports, spec))
                .unwrap_or_default();
            // The first target that is in scope, in condition order.
            for target in &targets {
                if target.starts_with("./") {
                    if let Some(file) = self.in_package(root, target) {
                        return Target::File(file, Precision::Module);
                    }
                } else if let hit @ (Target::File(..) | Target::External) = self.bare(dir, target) {
                    return hit;
                }
            }
            return Target::Unresolved;
        }
        if spec.starts_with('/') || spec.contains(':') || spec.is_empty() {
            return Target::External;
        }
        self.bare(dir, spec)
    }

    /// A bare specifier: `paths`, then `baseUrl`, then an in-repo package;
    /// anything else is external.
    fn bare(&self, dir: &str, spec: &str) -> Target {
        let mut matched = false;
        if let Some((_, config)) = Self::nearest(&self.tsconfigs, dir) {
            let mut best: Option<(usize, &Vec<String>, &str)> = None;
            for (pattern, targets) in &config.paths {
                let candidate = if pattern == spec {
                    Some((usize::MAX, ""))
                } else {
                    pattern_match(pattern, spec).map(|star| (pattern.find('*').unwrap_or(0), star))
                };
                if let Some((rank, star)) = candidate
                    && best.is_none_or(|(best_rank, _, _)| rank > best_rank)
                {
                    best = Some((rank, targets, star));
                }
            }
            if let Some((rank, targets, star)) = best {
                // A lone `*` maps every bare name: a miss there is a package.
                matched = rank > 0;
                for target in targets {
                    let target = target.replace('*', star);
                    if let Some(hit) = normalize(&join(config.paths_base(), &target))
                        .and_then(|base| self.file(&base))
                    {
                        return Target::File(hit.0, hit.1);
                    }
                }
            }
            if let Some(base_url) = &config.base_url
                && let Some(hit) =
                    normalize(&join(base_url, spec)).and_then(|base| self.file(&base))
            {
                return Target::File(hit.0, hit.1);
            }
        }
        let package = self
            .by_name
            .iter()
            .filter(|(name, _)| spec == name.as_str() || spec.starts_with(&format!("{name}/")))
            .max_by_key(|(name, _)| name.len());
        if let Some((name, root)) = package {
            let subpath = spec[name.len()..].strip_prefix('/');
            return self
                .package_target(root, subpath)
                .map_or(Target::Unresolved, |(file, precision)| {
                    Target::File(file, precision)
                });
        }
        if matched {
            Target::Unresolved
        } else {
            Target::External
        }
    }

    /// Where the name `name`, exported by `file`, is declared, following
    /// named and star re-exports at most [`MAX_REEXPORT_DEPTH`] hops.
    fn follow(
        &self,
        file: &str,
        name: &str,
        depth: u8,
        seen: &mut BTreeSet<(String, String)>,
    ) -> Follow {
        let Some(facts) = self.facts.get(file) else {
            return Follow::NotFound;
        };
        if facts.exports.contains(name) {
            return Follow::Found(file.to_string(), true);
        }
        if !seen.insert((file.to_string(), name.to_string())) {
            return Follow::NotFound;
        }
        let mut too_deep = false;
        for reexport in &facts.reexports {
            match reexport {
                crate::ecma::Reexport::Named {
                    name: exported,
                    orig,
                    specifier,
                } if exported == name => {
                    if depth >= MAX_REEXPORT_DEPTH {
                        return Follow::TooDeep;
                    }
                    let Target::File(target, _) = self.resolve(file, specifier) else {
                        return Follow::NotFound;
                    };
                    return match self.follow(&target, orig, depth + 1, seen) {
                        Follow::NotFound => Follow::Found(target, false),
                        other => other,
                    };
                }
                crate::ecma::Reexport::Star { specifier } => {
                    if depth >= MAX_REEXPORT_DEPTH {
                        too_deep = true;
                        continue;
                    }
                    let Target::File(target, _) = self.resolve(file, specifier) else {
                        continue;
                    };
                    match self.follow(&target, name, depth + 1, seen) {
                        found @ Follow::Found(_, true) => return found,
                        Follow::TooDeep => too_deep = true,
                        _ => {}
                    }
                }
                crate::ecma::Reexport::Named { .. } => {}
            }
        }
        if too_deep {
            Follow::TooDeep
        } else {
            Follow::NotFound
        }
    }

    fn edge(
        &mut self,
        from: &str,
        to: String,
        kind: EdgeKind,
        line: u32,
        via: Option<String>,
        precision: Precision,
    ) {
        self.edges.insert(FileEdge {
            from: from.to_string(),
            to,
            kind,
            scope: crate::ecma::scope_of(from),
            line,
            via,
            precision,
        });
    }

    fn resolve_all(&mut self) {
        let facts = std::mem::take(&mut self.facts);
        for (from, file_facts) in &facts {
            for line in &file_facts.dynamic_unresolved {
                self.diagnostics.push(diagnostic(
                    DiagnosticCode::DynamicUnresolved,
                    from,
                    Some(*line),
                    "import() of a non-literal: counted, not guessed".to_string(),
                ));
            }
        }
        self.facts = facts;
        let mut pending: Vec<(String, crate::ecma::ModuleRef)> = Vec::new();
        for (from, file_facts) in &self.facts {
            pending.extend(file_facts.refs.iter().map(|r| (from.clone(), r.clone())));
        }
        for (from, reference) in pending {
            let (to, precision) = match self.resolve(&from, &reference.specifier) {
                Target::External => {
                    self.external_imports += 1;
                    continue;
                }
                Target::Unresolved => {
                    self.diagnostics.push(diagnostic(
                        DiagnosticCode::Unresolved,
                        &from,
                        Some(reference.line),
                        format!("{} names no in-scope file", reference.specifier),
                    ));
                    continue;
                }
                Target::File(to, precision) => (to, precision),
            };
            // An asset is what the specifier reaches, not how it is spelled:
            // `./app.component` is code, `./data` reaching `data.json` is
            // not. A package fallback to its manifest is no asset import.
            let kind = if precision != Precision::Package && !crate::ecma::is_code_file(&to) {
                EdgeKind::Asset
            } else {
                reference.kind
            };
            let named: Vec<&str> = reference
                .imported
                .iter()
                .filter_map(|imported| match imported {
                    crate::ecma::Imported::Named(name) => Some(name.as_str()),
                    _ => None,
                })
                .collect();
            // A side-effect, default or namespace import stops at the file.
            let stops = named.len() < reference.imported.len() || named.is_empty();
            for name in named {
                match self.follow(&to, name, 0, &mut BTreeSet::new()) {
                    Follow::Found(file, item) => {
                        let via = (file != to).then(|| to.clone());
                        // A package fallback stays a guess, whatever it declares.
                        let precision = if item && precision == Precision::Module {
                            Precision::Item
                        } else {
                            precision
                        };
                        self.edge(&from, file, kind, reference.line, via, precision);
                    }
                    Follow::NotFound => self.edge(&from, to.clone(), kind, reference.line, None, precision),
                    Follow::TooDeep => self.diagnostics.push(diagnostic(
                        DiagnosticCode::ReexportTooDeep,
                        &from,
                        Some(reference.line),
                        format!("{name} from {}: re-exports past {MAX_REEXPORT_DEPTH} hops, no edge claimed", reference.specifier),
                    )),
                }
            }
            if stops {
                self.edge(&from, to, kind, reference.line, None, precision);
            }
        }
    }

    fn finish(mut self, sources: &BTreeMap<String, String>) -> CodeGraph {
        let mut units = Vec::new();
        let names: BTreeSet<&str> = self.by_name.keys().map(String::as_str).collect();
        for (root, package) in &self.packages {
            let Some(id) = &package.unit else {
                continue;
            };
            let manifest = &package.manifest;
            let mut entries: BTreeSet<String> = BTreeSet::new();
            let mut targets: Vec<String> = [&manifest.main, &manifest.module, &manifest.types]
                .into_iter()
                .flatten()
                .cloned()
                .chain(manifest.bin.iter().cloned())
                .collect();
            if let Some(exports) = &manifest.exports {
                export_leaves(exports, 0, &mut targets);
            }
            for target in targets.iter().filter(|target| !target.contains('*')) {
                if let Some(file) = self.in_package(root, target)
                    && crate::ecma::is_ecma_source(&file)
                {
                    entries.insert(file);
                }
            }
            if entries.is_empty()
                && let Some((file, _)) = self.package_target(root, None)
                && file != join(root, "package.json")
            {
                entries.insert(file);
            }
            units.push(Unit {
                id: id.clone(),
                kind: UnitKind::NpmPackage,
                root: root.clone(),
                entry_files: entries.into_iter().collect(),
                declared_units: manifest
                    .dependencies
                    .iter()
                    .filter(|dep| {
                        names.contains(dep.as_str()) && Some(format!("npm:{dep}")) != package.unit
                    })
                    .map(|dep| format!("npm:{dep}"))
                    .collect(),
            });
        }
        let entry_files: BTreeSet<&String> =
            units.iter().flat_map(|unit| &unit.entry_files).collect();
        let mut files = BTreeMap::new();
        for path in sources
            .keys()
            .filter(|path| crate::ecma::is_ecma_source(path))
        {
            let unit = Self::nearest(&self.packages, dir_of(path))
                .and_then(|(_, package)| package.unit.clone());
            files.insert(
                path.clone(),
                FileInfo {
                    language: crate::ecma::language_of(path),
                    unit,
                    class: if crate::ecma::is_test_path(path) {
                        FileClass::Test
                    } else {
                        FileClass::Source
                    },
                    is_entry: entry_files.contains(path),
                    in_module_tree: false,
                    parsed: self.facts.contains_key(path),
                },
            );
        }
        units.sort_by(|a, b| (&a.id, &a.root).cmp(&(&b.id, &b.root)));
        self.diagnostics.sort();
        self.diagnostics.dedup();
        CodeGraph {
            units,
            edges: self.edges.into_iter().collect(),
            files,
            diagnostics: self.diagnostics,
            external_imports: self.external_imports,
        }
    }
}

/// The targets `exports` (or `imports`) gives `key` (`.`, `./sub`,
/// `#name`), in the order to try them: an exact key, else the `*` pattern
/// with the longest prefix; within it, every condition's target, the
/// [`EXPORT_CONDITIONS`] first, then the others as written. The caller
/// takes the first that is in scope.
fn export_targets(map: &manifest::Json, key: &str) -> Vec<String> {
    use manifest::Json;
    let mut out = Vec::new();
    let Json::Obj(entries) = map else {
        if key == "." {
            conditions(map, 0, &mut out);
        }
        return out;
    };
    let is_subpaths = entries
        .iter()
        .any(|(k, _)| k.starts_with('.') || k.starts_with('#'));
    if !is_subpaths {
        if key == "." {
            conditions(map, 0, &mut out);
        }
        return out;
    }
    if let Some(value) = map.get(key) {
        conditions(value, 0, &mut out);
        return out;
    }
    let Some((_, value, star)) = entries
        .iter()
        .filter_map(|(pattern, value)| {
            pattern_match(pattern, key).map(|star| (pattern.find('*').unwrap_or(0), value, star))
        })
        .max_by_key(|(rank, _, _)| *rank)
    else {
        return out;
    };
    conditions(value, 0, &mut out);
    out.iter().map(|target| target.replace('*', star)).collect()
}

/// Every target a condition value gives, in the order to try them; each
/// value is visited once.
fn conditions(value: &manifest::Json, depth: usize, out: &mut Vec<String>) {
    use manifest::Json;
    if depth > MAX_CONDITION_DEPTH {
        return;
    }
    match value {
        Json::Str(target) if !out.contains(target) => out.push(target.clone()),
        Json::Arr(items) => items
            .iter()
            .for_each(|item| conditions(item, depth + 1, out)),
        Json::Obj(entries) => {
            for condition in EXPORT_CONDITIONS {
                if let Some(v) = value.get(condition) {
                    conditions(v, depth + 1, out);
                }
            }
            for (key, v) in entries {
                if !EXPORT_CONDITIONS.contains(&key.as_str()) {
                    conditions(v, depth + 1, out);
                }
            }
        }
        _ => {}
    }
}

/// Every string target in an `exports` value.
fn export_leaves(value: &manifest::Json, depth: usize, out: &mut Vec<String>) {
    use manifest::Json;
    if depth > MAX_CONDITION_DEPTH {
        return;
    }
    match value {
        Json::Str(target) => out.push(target.clone()),
        Json::Arr(items) => items
            .iter()
            .for_each(|item| export_leaves(item, depth + 1, out)),
        Json::Obj(entries) => entries
            .iter()
            .for_each(|(_, v)| export_leaves(v, depth + 1, out)),
        _ => {}
    }
}

/// The Rust and the JavaScript / TypeScript graphs as one.
fn merge(mut rust: CodeGraph, ecma: CodeGraph) -> CodeGraph {
    rust.units.extend(ecma.units);
    rust.units
        .sort_by(|a, b| (&a.id, &a.root).cmp(&(&b.id, &b.root)));
    let edges: BTreeSet<FileEdge> = rust.edges.into_iter().chain(ecma.edges).collect();
    rust.edges = edges.into_iter().collect();
    rust.files.extend(ecma.files);
    rust.diagnostics.extend(ecma.diagnostics);
    rust.diagnostics.sort();
    rust.diagnostics.dedup();
    rust.external_imports += ecma.external_imports;
    rust
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_normalize_inside_the_repository_only() {
        assert_eq!(normalize("a/b/../c/./d.rs").as_deref(), Some("a/c/d.rs"));
        assert_eq!(normalize("a/../../x"), None);
        assert_eq!(dir_of("a/b/c.rs"), "a/b");
        assert_eq!(dir_of("c.rs"), "");
        assert_eq!(join("", "x"), "x");
    }
}
