//! Rust resolution: units and targets from `Cargo.toml`, module trees from
//! each target root, and every `use`, path and `include*!` resolved to a
//! file of the in-scope inventory.
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

/// Build the code graph of the Rust sources and `Cargo.toml` manifests in
/// `sources` (path to text). References resolve only to paths in
/// `inventory`, the in-scope file set.
pub fn build(inventory: &BTreeSet<String>, sources: &BTreeMap<String, String>) -> CodeGraph {
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
