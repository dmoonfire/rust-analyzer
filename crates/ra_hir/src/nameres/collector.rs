use arrayvec::ArrayVec;
use rustc_hash::FxHashMap;
use relative_path::RelativePathBuf;
use test_utils::tested_by;
use ra_db::FileId;
use ra_syntax::ast;

use crate::{
    Function, Module, Struct, Union, Enum, Const, Static, Trait, TypeAlias,
    DefDatabase, HirFileId, Name, Path,
    KnownName,
    nameres::{
        Resolution, PerNs, ModuleDef, ReachedFixedPoint, ResolveMode,
        CrateDefMap, CrateModuleId, ModuleData,
        diagnostics::DefDiagnostic,
        raw,
    },
    ids::{AstItemDef, LocationCtx, MacroCallLoc, MacroCallId, MacroDefId, MacroFileKind},
    AstId,
};

pub(super) fn collect_defs(db: &impl DefDatabase, mut def_map: CrateDefMap) -> CrateDefMap {
    // populate external prelude
    for dep in def_map.krate.dependencies(db) {
        log::debug!("crate dep {:?} -> {:?}", dep.name, dep.krate);
        if let Some(module) = dep.krate.root_module(db) {
            def_map.extern_prelude.insert(dep.name.clone(), module.into());
        }
        // look for the prelude
        if def_map.prelude.is_none() {
            let map = db.crate_def_map(dep.krate);
            if map.prelude.is_some() {
                def_map.prelude = map.prelude;
            }
        }
    }

    let mut collector = DefCollector {
        db,
        def_map,
        glob_imports: FxHashMap::default(),
        unresolved_imports: Vec::new(),
        unexpanded_macros: Vec::new(),
        global_macro_scope: FxHashMap::default(),
        macro_stack_monitor: MacroStackMonitor::default(),
    };
    collector.collect();
    collector.finish()
}

#[derive(Default)]
struct MacroStackMonitor {
    counts: FxHashMap<MacroDefId, u32>,

    /// Mainly use for test
    validator: Option<Box<dyn Fn(u32) -> bool>>,
}

impl MacroStackMonitor {
    fn increase(&mut self, macro_def_id: MacroDefId) {
        *self.counts.entry(macro_def_id).or_default() += 1;
    }

    fn decrease(&mut self, macro_def_id: MacroDefId) {
        *self.counts.entry(macro_def_id).or_default() -= 1;
    }

    fn is_poison(&self, macro_def_id: MacroDefId) -> bool {
        let cur = *self.counts.get(&macro_def_id).unwrap_or(&0);

        if let Some(validator) = &self.validator {
            validator(cur)
        } else {
            cur > 100
        }
    }
}

/// Walks the tree of module recursively
struct DefCollector<DB> {
    db: DB,
    def_map: CrateDefMap,
    glob_imports: FxHashMap<CrateModuleId, Vec<(CrateModuleId, raw::ImportId)>>,
    unresolved_imports: Vec<(CrateModuleId, raw::ImportId, raw::ImportData)>,
    unexpanded_macros: Vec<(CrateModuleId, AstId<ast::MacroCall>, Path)>,
    global_macro_scope: FxHashMap<Name, MacroDefId>,

    /// Some macro use `$tt:tt which mean we have to handle the macro perfectly
    /// To prevent stackoverflow, we add a deep counter here for prevent that.
    macro_stack_monitor: MacroStackMonitor,
}

impl<'a, DB> DefCollector<&'a DB>
where
    DB: DefDatabase,
{
    fn collect(&mut self) {
        let crate_graph = self.db.crate_graph();
        let file_id = crate_graph.crate_root(self.def_map.krate.crate_id());
        let raw_items = self.db.raw_items(file_id.into());
        let module_id = self.def_map.root;
        self.def_map.modules[module_id].definition = Some(file_id);
        ModCollector {
            def_collector: &mut *self,
            module_id,
            file_id: file_id.into(),
            raw_items: &raw_items,
        }
        .collect(raw_items.items());

        // main name resolution fixed-point loop.
        let mut i = 0;
        loop {
            match (self.resolve_imports(), self.resolve_macros()) {
                (ReachedFixedPoint::Yes, ReachedFixedPoint::Yes) => break,
                _ => i += 1,
            }
            if i == 1000 {
                log::error!("diverging name resolution");
                break;
            }
        }

        let unresolved_imports = std::mem::replace(&mut self.unresolved_imports, Vec::new());
        // show unresolved imports in completion, etc
        for (module_id, import, import_data) in unresolved_imports {
            self.record_resolved_import(module_id, PerNs::none(), import, &import_data)
        }
    }

    fn define_macro(&mut self, name: Name, macro_id: MacroDefId, export: bool) {
        if export {
            self.def_map.public_macros.insert(name.clone(), macro_id);
        } else {
            self.def_map.local_macros.insert(name.clone(), macro_id);
        }
        self.global_macro_scope.insert(name, macro_id);
    }

    fn resolve_imports(&mut self) -> ReachedFixedPoint {
        let mut imports = std::mem::replace(&mut self.unresolved_imports, Vec::new());
        let mut resolved = Vec::new();
        imports.retain(|(module_id, import, import_data)| {
            let (def, fp) = self.resolve_import(*module_id, import_data);
            if fp == ReachedFixedPoint::Yes {
                resolved.push((*module_id, def, *import, import_data.clone()))
            }
            fp == ReachedFixedPoint::No
        });
        self.unresolved_imports = imports;
        // Resolves imports, filling-in module scopes
        let result =
            if resolved.is_empty() { ReachedFixedPoint::Yes } else { ReachedFixedPoint::No };
        for (module_id, def, import, import_data) in resolved {
            self.record_resolved_import(module_id, def, import, &import_data)
        }
        result
    }

    fn resolve_import(
        &self,
        module_id: CrateModuleId,
        import: &raw::ImportData,
    ) -> (PerNs<ModuleDef>, ReachedFixedPoint) {
        log::debug!("resolving import: {:?} ({:?})", import, self.def_map.edition);
        if import.is_extern_crate {
            let res = self.def_map.resolve_name_in_extern_prelude(
                &import
                    .path
                    .as_ident()
                    .expect("extern crate should have been desugared to one-element path"),
            );
            (res, ReachedFixedPoint::Yes)
        } else {
            let res =
                self.def_map.resolve_path_fp(self.db, ResolveMode::Import, module_id, &import.path);

            (res.resolved_def, res.reached_fixedpoint)
        }
    }

    fn record_resolved_import(
        &mut self,
        module_id: CrateModuleId,
        def: PerNs<ModuleDef>,
        import_id: raw::ImportId,
        import: &raw::ImportData,
    ) {
        if import.is_glob {
            log::debug!("glob import: {:?}", import);
            match def.take_types() {
                Some(ModuleDef::Module(m)) => {
                    if import.is_prelude {
                        tested_by!(std_prelude);
                        self.def_map.prelude = Some(m);
                    } else if m.krate != self.def_map.krate {
                        tested_by!(glob_across_crates);
                        // glob import from other crate => we can just import everything once
                        let item_map = self.db.crate_def_map(m.krate);
                        let scope = &item_map[m.module_id].scope;
                        let items = scope
                            .items
                            .iter()
                            .map(|(name, res)| (name.clone(), res.clone()))
                            .collect::<Vec<_>>();
                        self.update(module_id, Some(import_id), &items);
                    } else {
                        // glob import from same crate => we do an initial
                        // import, and then need to propagate any further
                        // additions
                        let scope = &self.def_map[m.module_id].scope;
                        let items = scope
                            .items
                            .iter()
                            .map(|(name, res)| (name.clone(), res.clone()))
                            .collect::<Vec<_>>();
                        self.update(module_id, Some(import_id), &items);
                        // record the glob import in case we add further items
                        self.glob_imports
                            .entry(m.module_id)
                            .or_default()
                            .push((module_id, import_id));
                    }
                }
                Some(ModuleDef::Enum(e)) => {
                    tested_by!(glob_enum);
                    // glob import from enum => just import all the variants
                    let variants = e.variants(self.db);
                    let resolutions = variants
                        .into_iter()
                        .filter_map(|variant| {
                            let res = Resolution {
                                def: PerNs::both(variant.into(), variant.into()),
                                import: Some(import_id),
                            };
                            let name = variant.name(self.db)?;
                            Some((name, res))
                        })
                        .collect::<Vec<_>>();
                    self.update(module_id, Some(import_id), &resolutions);
                }
                Some(d) => {
                    log::debug!("glob import {:?} from non-module/enum {:?}", import, d);
                }
                None => {
                    log::debug!("glob import {:?} didn't resolve as type", import);
                }
            }
        } else {
            match import.path.segments.last() {
                Some(last_segment) => {
                    let name = import.alias.clone().unwrap_or_else(|| last_segment.name.clone());
                    log::debug!("resolved import {:?} ({:?}) to {:?}", name, import, def);

                    // extern crates in the crate root are special-cased to insert entries into the extern prelude: rust-lang/rust#54658
                    if import.is_extern_crate && module_id == self.def_map.root {
                        if let Some(def) = def.take_types() {
                            self.def_map.extern_prelude.insert(name.clone(), def);
                        }
                    }
                    let resolution = Resolution { def, import: Some(import_id) };
                    self.update(module_id, Some(import_id), &[(name, resolution)]);
                }
                None => tested_by!(bogus_paths),
            }
        }
    }

    fn update(
        &mut self,
        module_id: CrateModuleId,
        import: Option<raw::ImportId>,
        resolutions: &[(Name, Resolution)],
    ) {
        self.update_recursive(module_id, import, resolutions, 0)
    }

    fn update_recursive(
        &mut self,
        module_id: CrateModuleId,
        import: Option<raw::ImportId>,
        resolutions: &[(Name, Resolution)],
        depth: usize,
    ) {
        if depth > 100 {
            // prevent stack overflows (but this shouldn't be possible)
            panic!("infinite recursion in glob imports!");
        }
        let module_items = &mut self.def_map.modules[module_id].scope;
        let mut changed = false;
        for (name, res) in resolutions {
            let existing = module_items.items.entry(name.clone()).or_default();
            if existing.def.types.is_none() && res.def.types.is_some() {
                existing.def.types = res.def.types;
                existing.import = import.or(res.import);
                changed = true;
            }
            if existing.def.values.is_none() && res.def.values.is_some() {
                existing.def.values = res.def.values;
                existing.import = import.or(res.import);
                changed = true;
            }
            if existing.def.is_none()
                && res.def.is_none()
                && existing.import.is_none()
                && res.import.is_some()
            {
                existing.import = res.import;
            }
        }
        if !changed {
            return;
        }
        let glob_imports = self
            .glob_imports
            .get(&module_id)
            .into_iter()
            .flat_map(|v| v.iter())
            .cloned()
            .collect::<Vec<_>>();
        for (glob_importing_module, glob_import) in glob_imports {
            // We pass the glob import so that the tracked import in those modules is that glob import
            self.update_recursive(glob_importing_module, Some(glob_import), resolutions, depth + 1);
        }
    }

    // XXX: this is just a pile of hacks now, because `PerNs` does not handle
    // macro namespace.
    fn resolve_macros(&mut self) -> ReachedFixedPoint {
        let mut macros = std::mem::replace(&mut self.unexpanded_macros, Vec::new());
        let mut resolved = Vec::new();
        let mut res = ReachedFixedPoint::Yes;
        macros.retain(|(module_id, ast_id, path)| {
            if path.segments.len() != 2 {
                return true;
            }
            let crate_name = &path.segments[0].name;
            let krate = match self.def_map.resolve_name_in_extern_prelude(crate_name).take_types() {
                Some(ModuleDef::Module(m)) => m.krate(self.db),
                _ => return true,
            };
            let krate = match krate {
                Some(it) => it,
                _ => return true,
            };
            res = ReachedFixedPoint::No;
            let def_map = self.db.crate_def_map(krate);
            if let Some(macro_id) = def_map.public_macros.get(&path.segments[1].name).cloned() {
                let call_id = MacroCallLoc { def: macro_id, ast_id: *ast_id }.id(self.db);
                resolved.push((*module_id, call_id, macro_id));
            }
            false
        });

        for (module_id, macro_call_id, macro_def_id) in resolved {
            self.collect_macro_expansion(module_id, macro_call_id, macro_def_id);
        }
        res
    }

    fn collect_macro_expansion(
        &mut self,
        module_id: CrateModuleId,
        macro_call_id: MacroCallId,
        macro_def_id: MacroDefId,
    ) {
        if self.def_map.poison_macros.contains(&macro_def_id) {
            return;
        }

        self.macro_stack_monitor.increase(macro_def_id);

        if !self.macro_stack_monitor.is_poison(macro_def_id) {
            let file_id: HirFileId = macro_call_id.as_file(MacroFileKind::Items);
            let raw_items = self.db.raw_items(file_id);
            ModCollector { def_collector: &mut *self, file_id, module_id, raw_items: &raw_items }
                .collect(raw_items.items());
        } else {
            log::error!("Too deep macro expansion: {}", macro_call_id.debug_dump(self.db));
            self.def_map.poison_macros.insert(macro_def_id);
        }

        self.macro_stack_monitor.decrease(macro_def_id);
    }

    fn finish(self) -> CrateDefMap {
        self.def_map
    }
}

/// Walks a single module, populating defs, imports and macros
struct ModCollector<'a, D> {
    def_collector: D,
    module_id: CrateModuleId,
    file_id: HirFileId,
    raw_items: &'a raw::RawItems,
}

impl<DB> ModCollector<'_, &'_ mut DefCollector<&'_ DB>>
where
    DB: DefDatabase,
{
    fn collect(&mut self, items: &[raw::RawItem]) {
        for item in items {
            match *item {
                raw::RawItem::Module(m) => self.collect_module(&self.raw_items[m]),
                raw::RawItem::Import(import) => self.def_collector.unresolved_imports.push((
                    self.module_id,
                    import,
                    self.raw_items[import].clone(),
                )),
                raw::RawItem::Def(def) => self.define_def(&self.raw_items[def]),
                raw::RawItem::Macro(mac) => self.collect_macro(&self.raw_items[mac]),
            }
        }
    }

    fn collect_module(&mut self, module: &raw::ModuleData) {
        match module {
            // inline module, just recurse
            raw::ModuleData::Definition { name, items, ast_id } => {
                let module_id =
                    self.push_child_module(name.clone(), ast_id.with_file_id(self.file_id), None);
                ModCollector {
                    def_collector: &mut *self.def_collector,
                    module_id,
                    file_id: self.file_id,
                    raw_items: self.raw_items,
                }
                .collect(&*items);
            }
            // out of line module, resovle, parse and recurse
            raw::ModuleData::Declaration { name, ast_id } => {
                let ast_id = ast_id.with_file_id(self.file_id);
                let is_root = self.def_collector.def_map.modules[self.module_id].parent.is_none();
                match resolve_submodule(self.def_collector.db, self.file_id, name, is_root) {
                    Ok(file_id) => {
                        let module_id = self.push_child_module(name.clone(), ast_id, Some(file_id));
                        let raw_items = self.def_collector.db.raw_items(file_id.into());
                        ModCollector {
                            def_collector: &mut *self.def_collector,
                            module_id,
                            file_id: file_id.into(),
                            raw_items: &raw_items,
                        }
                        .collect(raw_items.items())
                    }
                    Err(candidate) => self.def_collector.def_map.diagnostics.push(
                        DefDiagnostic::UnresolvedModule {
                            module: self.module_id,
                            declaration: ast_id,
                            candidate,
                        },
                    ),
                };
            }
        }
    }

    fn push_child_module(
        &mut self,
        name: Name,
        declaration: AstId<ast::Module>,
        definition: Option<FileId>,
    ) -> CrateModuleId {
        let modules = &mut self.def_collector.def_map.modules;
        let res = modules.alloc(ModuleData::default());
        modules[res].parent = Some(self.module_id);
        modules[res].declaration = Some(declaration);
        modules[res].definition = definition;
        modules[self.module_id].children.insert(name.clone(), res);
        let resolution = Resolution {
            def: PerNs::types(
                Module { krate: self.def_collector.def_map.krate, module_id: res }.into(),
            ),
            import: None,
        };
        self.def_collector.update(self.module_id, None, &[(name, resolution)]);
        res
    }

    fn define_def(&mut self, def: &raw::DefData) {
        let module = Module { krate: self.def_collector.def_map.krate, module_id: self.module_id };
        let ctx = LocationCtx::new(self.def_collector.db, module, self.file_id.into());

        macro_rules! def {
            ($kind:ident, $ast_id:ident) => {
                $kind { id: AstItemDef::from_ast_id(ctx, $ast_id) }.into()
            };
        }
        let name = def.name.clone();
        let def: PerNs<ModuleDef> = match def.kind {
            raw::DefKind::Function(ast_id) => PerNs::values(def!(Function, ast_id)),
            raw::DefKind::Struct(ast_id) => {
                let s = def!(Struct, ast_id);
                PerNs::both(s, s)
            }
            raw::DefKind::Union(ast_id) => {
                let s = def!(Union, ast_id);
                PerNs::both(s, s)
            }
            raw::DefKind::Enum(ast_id) => PerNs::types(def!(Enum, ast_id)),
            raw::DefKind::Const(ast_id) => PerNs::values(def!(Const, ast_id)),
            raw::DefKind::Static(ast_id) => PerNs::values(def!(Static, ast_id)),
            raw::DefKind::Trait(ast_id) => PerNs::types(def!(Trait, ast_id)),
            raw::DefKind::TypeAlias(ast_id) => PerNs::types(def!(TypeAlias, ast_id)),
        };
        let resolution = Resolution { def, import: None };
        self.def_collector.update(self.module_id, None, &[(name, resolution)])
    }

    fn collect_macro(&mut self, mac: &raw::MacroData) {
        // Case 1: macro rules, define a macro in crate-global mutable scope
        if is_macro_rules(&mac.path) {
            if let Some(name) = &mac.name {
                let macro_id = MacroDefId(mac.ast_id.with_file_id(self.file_id));
                self.def_collector.define_macro(name.clone(), macro_id, mac.export)
            }
            return;
        }

        let ast_id = mac.ast_id.with_file_id(self.file_id);

        // Case 2: try to expand macro_rules from this crate, triggering
        // recursive item collection.
        if let Some(macro_id) =
            mac.path.as_ident().and_then(|name| self.def_collector.global_macro_scope.get(&name))
        {
            let def = *macro_id;
            let macro_call_id = MacroCallLoc { def, ast_id }.id(self.def_collector.db);

            self.def_collector.collect_macro_expansion(self.module_id, macro_call_id, def);
            return;
        }

        // Case 3: path to a macro from another crate, expand during name resolution
        self.def_collector.unexpanded_macros.push((self.module_id, ast_id, mac.path.clone()))
    }
}

fn is_macro_rules(path: &Path) -> bool {
    path.as_ident().and_then(Name::as_known_name) == Some(KnownName::MacroRules)
}

fn resolve_submodule(
    db: &impl DefDatabase,
    file_id: HirFileId,
    name: &Name,
    is_root: bool,
) -> Result<FileId, RelativePathBuf> {
    // FIXME: handle submodules of inline modules properly
    let file_id = file_id.original_file(db);
    let source_root_id = db.file_source_root(file_id);
    let path = db.file_relative_path(file_id);
    let root = RelativePathBuf::default();
    let dir_path = path.parent().unwrap_or(&root);
    let mod_name = path.file_stem().unwrap_or("unknown");
    let is_dir_owner = is_root || mod_name == "mod";

    let file_mod = dir_path.join(format!("{}.rs", name));
    let dir_mod = dir_path.join(format!("{}/mod.rs", name));
    let file_dir_mod = dir_path.join(format!("{}/{}.rs", mod_name, name));
    let mut candidates = ArrayVec::<[_; 2]>::new();
    if is_dir_owner {
        candidates.push(file_mod.clone());
        candidates.push(dir_mod);
    } else {
        candidates.push(file_dir_mod.clone());
    };
    let sr = db.source_root(source_root_id);
    let mut points_to = candidates.into_iter().filter_map(|path| sr.files.get(&path)).map(|&it| it);
    // FIXME: handle ambiguity
    match points_to.next() {
        Some(file_id) => Ok(file_id),
        None => Err(if is_dir_owner { file_mod } else { file_dir_mod }),
    }
}

#[cfg(test)]
mod tests {
    use ra_db::SourceDatabase;

    use crate::{Crate, mock::MockDatabase, DefDatabase};
    use ra_arena::{Arena};
    use super::*;
    use rustc_hash::FxHashSet;

    fn do_collect_defs(
        db: &impl DefDatabase,
        def_map: CrateDefMap,
        monitor: MacroStackMonitor,
    ) -> CrateDefMap {
        let mut collector = DefCollector {
            db,
            def_map,
            glob_imports: FxHashMap::default(),
            unresolved_imports: Vec::new(),
            unexpanded_macros: Vec::new(),
            global_macro_scope: FxHashMap::default(),
            macro_stack_monitor: monitor,
        };
        collector.collect();
        collector.finish()
    }

    fn do_limited_resolve(code: &str, limit: u32, poison_limit: u32) -> CrateDefMap {
        let (db, _source_root, _) = MockDatabase::with_single_file(&code);
        let crate_id = db.crate_graph().iter().next().unwrap();
        let krate = Crate { crate_id };

        let def_map = {
            let edition = krate.edition(&db);
            let mut modules: Arena<CrateModuleId, ModuleData> = Arena::default();
            let root = modules.alloc(ModuleData::default());
            CrateDefMap {
                krate,
                edition,
                extern_prelude: FxHashMap::default(),
                prelude: None,
                root,
                modules,
                public_macros: FxHashMap::default(),
                poison_macros: FxHashSet::default(),
                local_macros: FxHashMap::default(),
                diagnostics: Vec::new(),
            }
        };

        let mut monitor = MacroStackMonitor::default();
        monitor.validator = Some(Box::new(move |count| {
            assert!(count < limit);
            count >= poison_limit
        }));

        do_collect_defs(&db, def_map, monitor)
    }

    #[test]
    fn test_macro_expand_limit_width() {
        do_limited_resolve(
            r#"
        macro_rules! foo {
            ($($ty:ty)*) => { foo!($($ty)*, $($ty)*); }
        }
foo!(KABOOM);
        "#,
            16,
            1000,
        );
    }

    #[test]
    fn test_macro_expand_poisoned() {
        let def = do_limited_resolve(
            r#"
        macro_rules! foo {
            ($ty:ty) => { foo!($ty); }
        }
foo!(KABOOM);
        "#,
            100,
            16,
        );

        assert_eq!(def.poison_macros.len(), 1);
    }

    #[test]
    fn test_macro_expand_normal() {
        let def = do_limited_resolve(
            r#"
        macro_rules! foo {
            ($ident:ident) => { struct $ident {} }
        }
foo!(Bar);
        "#,
            16,
            16,
        );

        assert_eq!(def.poison_macros.len(), 0);
    }
}
