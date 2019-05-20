use std::sync::Arc;

use ra_db::{CrateId, SourceRootId, Edition};
use ra_syntax::{ast::self, TreeArc};

use crate::{
    Name, Ty, HirFileId, Either,
    HirDatabase, DefDatabase,
    type_ref::TypeRef,
    nameres::{ModuleScope, Namespace, ImportId, CrateModuleId},
    expr::{Body, BodySourceMap, validation::ExprValidator},
    ty::{ TraitRef, InferenceResult},
    adt::{EnumVariantId, StructFieldId, VariantDef},
    generics::HasGenericParams,
    docs::{Documentation, Docs, docs_from_ast},
    ids::{FunctionId, StructId, EnumId, AstItemDef, ConstId, StaticId, TraitId, TypeAliasId},
    impl_block::ImplBlock,
    resolve::Resolver,
    diagnostics::{DiagnosticSink},
    traits::{TraitItem, TraitData},
};

/// hir::Crate describes a single crate. It's the main interface with which
/// a crate's dependencies interact. Mostly, it should be just a proxy for the
/// root module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Crate {
    pub(crate) crate_id: CrateId,
}

#[derive(Debug)]
pub struct CrateDependency {
    pub krate: Crate,
    pub name: Name,
}

impl Crate {
    pub fn crate_id(&self) -> CrateId {
        self.crate_id
    }

    pub fn dependencies(&self, db: &impl DefDatabase) -> Vec<CrateDependency> {
        self.dependencies_impl(db)
    }

    pub fn root_module(&self, db: &impl DefDatabase) -> Option<Module> {
        self.root_module_impl(db)
    }

    pub fn edition(&self, db: &impl DefDatabase) -> Edition {
        let crate_graph = db.crate_graph();
        crate_graph.edition(self.crate_id)
    }

    // FIXME: should this be in source_binder?
    pub fn source_root_crates(db: &impl DefDatabase, source_root: SourceRootId) -> Vec<Crate> {
        let crate_ids = db.source_root_crates(source_root);
        crate_ids.iter().map(|&crate_id| Crate { crate_id }).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Module {
    pub(crate) krate: Crate,
    pub(crate) module_id: CrateModuleId,
}

/// The defs which can be visible in the module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleDef {
    Module(Module),
    Function(Function),
    Struct(Struct),
    Union(Union),
    Enum(Enum),
    // Can't be directly declared, but can be imported.
    EnumVariant(EnumVariant),
    Const(Const),
    Static(Static),
    Trait(Trait),
    TypeAlias(TypeAlias),
}
impl_froms!(
    ModuleDef: Module,
    Function,
    Struct,
    Union,
    Enum,
    EnumVariant,
    Const,
    Static,
    Trait,
    TypeAlias
);

pub enum ModuleSource {
    SourceFile(TreeArc<ast::SourceFile>),
    Module(TreeArc<ast::Module>),
}

impl Module {
    /// Name of this module.
    pub fn name(&self, db: &impl HirDatabase) -> Option<Name> {
        self.name_impl(db)
    }

    /// Returns a node which defines this module. That is, a file or a `mod foo {}` with items.
    pub fn definition_source(&self, db: &impl DefDatabase) -> (HirFileId, ModuleSource) {
        self.definition_source_impl(db)
    }

    /// Returns a node which declares this module, either a `mod foo;` or a `mod foo {}`.
    /// `None` for the crate root.
    pub fn declaration_source(
        &self,
        db: &impl HirDatabase,
    ) -> Option<(HirFileId, TreeArc<ast::Module>)> {
        self.declaration_source_impl(db)
    }

    /// Returns the syntax of the last path segment corresponding to this import
    pub fn import_source(
        &self,
        db: &impl HirDatabase,
        import: ImportId,
    ) -> Either<TreeArc<ast::UseTree>, TreeArc<ast::ExternCrateItem>> {
        let (file_id, source) = self.definition_source(db);
        let (_, source_map) = db.raw_items_with_source_map(file_id);
        source_map.get(&source, import)
    }

    /// Returns the crate this module is part of.
    pub fn krate(&self, _db: &impl DefDatabase) -> Option<Crate> {
        Some(self.krate)
    }

    /// Topmost parent of this module. Every module has a `crate_root`, but some
    /// might be missing `krate`. This can happen if a module's file is not included
    /// in the module tree of any target in `Cargo.toml`.
    pub fn crate_root(&self, db: &impl DefDatabase) -> Module {
        self.crate_root_impl(db)
    }

    /// Finds a child module with the specified name.
    pub fn child(&self, db: &impl HirDatabase, name: &Name) -> Option<Module> {
        self.child_impl(db, name)
    }

    /// Iterates over all child modules.
    pub fn children(&self, db: &impl DefDatabase) -> impl Iterator<Item = Module> {
        self.children_impl(db)
    }

    /// Finds a parent module.
    pub fn parent(&self, db: &impl DefDatabase) -> Option<Module> {
        self.parent_impl(db)
    }

    pub fn path_to_root(&self, db: &impl HirDatabase) -> Vec<Module> {
        let mut res = vec![self.clone()];
        let mut curr = self.clone();
        while let Some(next) = curr.parent(db) {
            res.push(next.clone());
            curr = next
        }
        res
    }

    /// Returns a `ModuleScope`: a set of items, visible in this module.
    pub fn scope(&self, db: &impl HirDatabase) -> ModuleScope {
        db.crate_def_map(self.krate)[self.module_id].scope.clone()
    }

    pub fn diagnostics(&self, db: &impl HirDatabase, sink: &mut DiagnosticSink) {
        db.crate_def_map(self.krate).add_diagnostics(db, self.module_id, sink);
        for decl in self.declarations(db) {
            match decl {
                crate::ModuleDef::Function(f) => f.diagnostics(db, sink),
                crate::ModuleDef::Module(f) => f.diagnostics(db, sink),
                _ => (),
            }
        }

        for impl_block in self.impl_blocks(db) {
            for item in impl_block.items(db) {
                match item {
                    crate::ImplItem::Method(f) => f.diagnostics(db, sink),
                    _ => (),
                }
            }
        }
    }

    pub(crate) fn resolver(&self, db: &impl DefDatabase) -> Resolver {
        let def_map = db.crate_def_map(self.krate);
        Resolver::default().push_module_scope(def_map, self.module_id)
    }

    pub fn declarations(self, db: &impl DefDatabase) -> Vec<ModuleDef> {
        let def_map = db.crate_def_map(self.krate);
        def_map[self.module_id]
            .scope
            .entries()
            .filter_map(|(_name, res)| if res.import.is_none() { Some(res.def) } else { None })
            .flat_map(|per_ns| {
                per_ns.take_types().into_iter().chain(per_ns.take_values().into_iter())
            })
            .collect()
    }

    pub fn impl_blocks(self, db: &impl HirDatabase) -> Vec<ImplBlock> {
        let module_impl_blocks = db.impls_in_module(self);
        module_impl_blocks
            .impls
            .iter()
            .map(|(impl_id, _)| ImplBlock::from_id(self, impl_id))
            .collect()
    }
}

impl Docs for Module {
    fn docs(&self, db: &impl HirDatabase) -> Option<Documentation> {
        self.declaration_source(db).and_then(|it| docs_from_ast(&*it.1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StructField {
    pub(crate) parent: VariantDef,
    pub(crate) id: StructFieldId,
}

#[derive(Debug)]
pub enum FieldSource {
    Named(TreeArc<ast::NamedFieldDef>),
    Pos(TreeArc<ast::PosFieldDef>),
}

impl StructField {
    pub fn name(&self, db: &impl HirDatabase) -> Name {
        self.parent.variant_data(db).fields().unwrap()[self.id].name.clone()
    }

    pub fn source(&self, db: &impl DefDatabase) -> (HirFileId, FieldSource) {
        self.source_impl(db)
    }

    pub fn ty(&self, db: &impl HirDatabase) -> Ty {
        db.type_for_field(*self)
    }

    pub fn parent_def(&self, _db: &impl HirDatabase) -> VariantDef {
        self.parent
    }
}

impl Docs for StructField {
    fn docs(&self, db: &impl HirDatabase) -> Option<Documentation> {
        match self.source(db).1 {
            FieldSource::Named(named) => docs_from_ast(&*named),
            FieldSource::Pos(..) => return None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Struct {
    pub(crate) id: StructId,
}

impl Struct {
    pub fn source(&self, db: &impl DefDatabase) -> (HirFileId, TreeArc<ast::StructDef>) {
        self.id.source(db)
    }

    pub fn module(&self, db: &impl HirDatabase) -> Module {
        self.id.module(db)
    }

    pub fn name(&self, db: &impl HirDatabase) -> Option<Name> {
        db.struct_data(*self).name.clone()
    }

    pub fn fields(&self, db: &impl HirDatabase) -> Vec<StructField> {
        db.struct_data(*self)
            .variant_data
            .fields()
            .into_iter()
            .flat_map(|it| it.iter())
            .map(|(id, _)| StructField { parent: (*self).into(), id })
            .collect()
    }

    pub fn field(&self, db: &impl HirDatabase, name: &Name) -> Option<StructField> {
        db.struct_data(*self)
            .variant_data
            .fields()
            .into_iter()
            .flat_map(|it| it.iter())
            .find(|(_id, data)| data.name == *name)
            .map(|(id, _)| StructField { parent: (*self).into(), id })
    }

    pub fn ty(&self, db: &impl HirDatabase) -> Ty {
        db.type_for_def((*self).into(), Namespace::Types)
    }

    pub fn constructor_ty(&self, db: &impl HirDatabase) -> Ty {
        db.type_for_def((*self).into(), Namespace::Values)
    }

    // FIXME move to a more general type
    /// Builds a resolver for type references inside this struct.
    pub(crate) fn resolver(&self, db: &impl HirDatabase) -> Resolver {
        // take the outer scope...
        let r = self.module(db).resolver(db);
        // ...and add generic params, if present
        let p = self.generic_params(db);
        let r = if !p.params.is_empty() { r.push_generic_params_scope(p) } else { r };
        r
    }
}

impl Docs for Struct {
    fn docs(&self, db: &impl HirDatabase) -> Option<Documentation> {
        docs_from_ast(&*self.source(db).1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Union {
    pub(crate) id: StructId,
}

impl Union {
    pub fn source(&self, db: &impl DefDatabase) -> (HirFileId, TreeArc<ast::StructDef>) {
        self.id.source(db)
    }

    pub fn name(&self, db: &impl HirDatabase) -> Option<Name> {
        db.struct_data(Struct { id: self.id }).name.clone()
    }

    pub fn module(&self, db: &impl HirDatabase) -> Module {
        self.id.module(db)
    }

    // FIXME move to a more general type
    /// Builds a resolver for type references inside this union.
    pub(crate) fn resolver(&self, db: &impl HirDatabase) -> Resolver {
        // take the outer scope...
        let r = self.module(db).resolver(db);
        // ...and add generic params, if present
        let p = self.generic_params(db);
        let r = if !p.params.is_empty() { r.push_generic_params_scope(p) } else { r };
        r
    }
}

impl Docs for Union {
    fn docs(&self, db: &impl HirDatabase) -> Option<Documentation> {
        docs_from_ast(&*self.source(db).1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Enum {
    pub(crate) id: EnumId,
}

impl Enum {
    pub fn source(&self, db: &impl DefDatabase) -> (HirFileId, TreeArc<ast::EnumDef>) {
        self.id.source(db)
    }

    pub fn module(&self, db: &impl HirDatabase) -> Module {
        self.id.module(db)
    }

    pub fn name(&self, db: &impl HirDatabase) -> Option<Name> {
        db.enum_data(*self).name.clone()
    }

    pub fn variants(&self, db: &impl DefDatabase) -> Vec<EnumVariant> {
        db.enum_data(*self)
            .variants
            .iter()
            .map(|(id, _)| EnumVariant { parent: *self, id })
            .collect()
    }

    pub fn variant(&self, db: &impl DefDatabase, name: &Name) -> Option<EnumVariant> {
        db.enum_data(*self)
            .variants
            .iter()
            .find(|(_id, data)| data.name.as_ref() == Some(name))
            .map(|(id, _)| EnumVariant { parent: *self, id })
    }

    pub fn ty(&self, db: &impl HirDatabase) -> Ty {
        db.type_for_def((*self).into(), Namespace::Types)
    }

    // FIXME: move to a more general type
    /// Builds a resolver for type references inside this struct.
    pub(crate) fn resolver(&self, db: &impl HirDatabase) -> Resolver {
        // take the outer scope...
        let r = self.module(db).resolver(db);
        // ...and add generic params, if present
        let p = self.generic_params(db);
        let r = if !p.params.is_empty() { r.push_generic_params_scope(p) } else { r };
        r
    }
}

impl Docs for Enum {
    fn docs(&self, db: &impl HirDatabase) -> Option<Documentation> {
        docs_from_ast(&*self.source(db).1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EnumVariant {
    pub(crate) parent: Enum,
    pub(crate) id: EnumVariantId,
}

impl EnumVariant {
    pub fn source(&self, db: &impl DefDatabase) -> (HirFileId, TreeArc<ast::EnumVariant>) {
        self.source_impl(db)
    }
    pub fn module(&self, db: &impl HirDatabase) -> Module {
        self.parent.module(db)
    }
    pub fn parent_enum(&self, _db: &impl DefDatabase) -> Enum {
        self.parent
    }

    pub fn name(&self, db: &impl DefDatabase) -> Option<Name> {
        db.enum_data(self.parent).variants[self.id].name.clone()
    }

    pub fn fields(&self, db: &impl HirDatabase) -> Vec<StructField> {
        self.variant_data(db)
            .fields()
            .into_iter()
            .flat_map(|it| it.iter())
            .map(|(id, _)| StructField { parent: (*self).into(), id })
            .collect()
    }

    pub fn field(&self, db: &impl HirDatabase, name: &Name) -> Option<StructField> {
        self.variant_data(db)
            .fields()
            .into_iter()
            .flat_map(|it| it.iter())
            .find(|(_id, data)| data.name == *name)
            .map(|(id, _)| StructField { parent: (*self).into(), id })
    }
}

impl Docs for EnumVariant {
    fn docs(&self, db: &impl HirDatabase) -> Option<Documentation> {
        docs_from_ast(&*self.source(db).1)
    }
}

/// The defs which have a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefWithBody {
    Function(Function),
    Static(Static),
    Const(Const),
}

impl_froms!(DefWithBody: Function, Const, Static);

impl DefWithBody {
    pub fn infer(&self, db: &impl HirDatabase) -> Arc<InferenceResult> {
        db.infer(*self)
    }

    pub fn body(&self, db: &impl HirDatabase) -> Arc<Body> {
        db.body_hir(*self)
    }

    pub fn body_source_map(&self, db: &impl HirDatabase) -> Arc<BodySourceMap> {
        db.body_with_source_map(*self).1
    }

    /// Builds a resolver for code inside this item.
    pub(crate) fn resolver(&self, db: &impl HirDatabase) -> Resolver {
        match *self {
            DefWithBody::Const(ref c) => c.resolver(db),
            DefWithBody::Function(ref f) => f.resolver(db),
            DefWithBody::Static(ref s) => s.resolver(db),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Function {
    pub(crate) id: FunctionId,
}

/// The declared signature of a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FnSignature {
    pub(crate) name: Name,
    pub(crate) params: Vec<TypeRef>,
    pub(crate) ret_type: TypeRef,
    /// True if the first param is `self`. This is relevant to decide whether this
    /// can be called as a method.
    pub(crate) has_self_param: bool,
}

impl FnSignature {
    pub fn name(&self) -> &Name {
        &self.name
    }

    pub fn params(&self) -> &[TypeRef] {
        &self.params
    }

    pub fn ret_type(&self) -> &TypeRef {
        &self.ret_type
    }

    /// True if the first arg is `self`. This is relevant to decide whether this
    /// can be called as a method.
    pub fn has_self_param(&self) -> bool {
        self.has_self_param
    }
}

impl Function {
    pub fn source(&self, db: &impl DefDatabase) -> (HirFileId, TreeArc<ast::FnDef>) {
        self.id.source(db)
    }

    pub fn module(&self, db: &impl DefDatabase) -> Module {
        self.id.module(db)
    }

    pub fn name(&self, db: &impl HirDatabase) -> Name {
        self.signature(db).name.clone()
    }

    pub(crate) fn body_source_map(&self, db: &impl HirDatabase) -> Arc<BodySourceMap> {
        db.body_with_source_map((*self).into()).1
    }

    pub fn body(&self, db: &impl HirDatabase) -> Arc<Body> {
        db.body_hir((*self).into())
    }

    pub fn ty(&self, db: &impl HirDatabase) -> Ty {
        db.type_for_def((*self).into(), Namespace::Values)
    }

    pub fn signature(&self, db: &impl HirDatabase) -> Arc<FnSignature> {
        db.fn_signature(*self)
    }

    pub fn infer(&self, db: &impl HirDatabase) -> Arc<InferenceResult> {
        db.infer((*self).into())
    }

    /// The containing impl block, if this is a method.
    pub fn impl_block(&self, db: &impl DefDatabase) -> Option<ImplBlock> {
        let module_impls = db.impls_in_module(self.module(db));
        ImplBlock::containing(module_impls, (*self).into())
    }

    /// The containing trait, if this is a trait method definition.
    pub fn parent_trait(&self, db: &impl DefDatabase) -> Option<Trait> {
        db.trait_items_index(self.module(db)).get_parent_trait((*self).into())
    }

    pub fn container(&self, db: &impl DefDatabase) -> Option<Container> {
        if let Some(impl_block) = self.impl_block(db) {
            Some(impl_block.into())
        } else if let Some(trait_) = self.parent_trait(db) {
            Some(trait_.into())
        } else {
            None
        }
    }

    // FIXME: move to a more general type for 'body-having' items
    /// Builds a resolver for code inside this item.
    pub(crate) fn resolver(&self, db: &impl HirDatabase) -> Resolver {
        // take the outer scope...
        let r = self.container(db).map_or_else(|| self.module(db).resolver(db), |c| c.resolver(db));
        // ...and add generic params, if present
        let p = self.generic_params(db);
        let r = if !p.params.is_empty() { r.push_generic_params_scope(p) } else { r };
        r
    }

    pub fn diagnostics(&self, db: &impl HirDatabase, sink: &mut DiagnosticSink) {
        let infer = self.infer(db);
        infer.add_diagnostics(db, *self, sink);
        let mut validator = ExprValidator::new(*self, infer, sink);
        validator.validate_body(db);
    }
}

impl Docs for Function {
    fn docs(&self, db: &impl HirDatabase) -> Option<Documentation> {
        docs_from_ast(&*self.source(db).1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Const {
    pub(crate) id: ConstId,
}

impl Const {
    pub fn source(&self, db: &impl DefDatabase) -> (HirFileId, TreeArc<ast::ConstDef>) {
        self.id.source(db)
    }

    pub fn module(&self, db: &impl DefDatabase) -> Module {
        self.id.module(db)
    }

    pub fn signature(&self, db: &impl HirDatabase) -> Arc<ConstSignature> {
        db.const_signature(*self)
    }

    pub fn infer(&self, db: &impl HirDatabase) -> Arc<InferenceResult> {
        db.infer((*self).into())
    }

    /// The containing impl block, if this is a method.
    pub fn impl_block(&self, db: &impl DefDatabase) -> Option<ImplBlock> {
        let module_impls = db.impls_in_module(self.module(db));
        ImplBlock::containing(module_impls, (*self).into())
    }

    // FIXME: move to a more general type for 'body-having' items
    /// Builds a resolver for code inside this item.
    pub(crate) fn resolver(&self, db: &impl HirDatabase) -> Resolver {
        // take the outer scope...
        let r = self
            .impl_block(db)
            .map(|ib| ib.resolver(db))
            .unwrap_or_else(|| self.module(db).resolver(db));
        r
    }
}

impl Docs for Const {
    fn docs(&self, db: &impl HirDatabase) -> Option<Documentation> {
        docs_from_ast(&*self.source(db).1)
    }
}

/// The declared signature of a const.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstSignature {
    pub(crate) name: Name,
    pub(crate) type_ref: TypeRef,
}

impl ConstSignature {
    pub fn name(&self) -> &Name {
        &self.name
    }

    pub fn type_ref(&self) -> &TypeRef {
        &self.type_ref
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Static {
    pub(crate) id: StaticId,
}

impl Static {
    pub fn source(&self, db: &impl DefDatabase) -> (HirFileId, TreeArc<ast::StaticDef>) {
        self.id.source(db)
    }

    pub fn module(&self, db: &impl DefDatabase) -> Module {
        self.id.module(db)
    }

    pub fn signature(&self, db: &impl HirDatabase) -> Arc<ConstSignature> {
        db.static_signature(*self)
    }

    /// Builds a resolver for code inside this item.
    pub(crate) fn resolver(&self, db: &impl HirDatabase) -> Resolver {
        // take the outer scope...
        self.module(db).resolver(db)
    }

    pub fn infer(&self, db: &impl HirDatabase) -> Arc<InferenceResult> {
        db.infer((*self).into())
    }
}

impl Docs for Static {
    fn docs(&self, db: &impl HirDatabase) -> Option<Documentation> {
        docs_from_ast(&*self.source(db).1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Trait {
    pub(crate) id: TraitId,
}

impl Trait {
    pub fn source(&self, db: &impl DefDatabase) -> (HirFileId, TreeArc<ast::TraitDef>) {
        self.id.source(db)
    }

    pub fn module(&self, db: &impl DefDatabase) -> Module {
        self.id.module(db)
    }

    pub fn name(self, db: &impl DefDatabase) -> Option<Name> {
        self.trait_data(db).name().clone()
    }

    pub fn items(self, db: &impl DefDatabase) -> Vec<TraitItem> {
        self.trait_data(db).items().to_vec()
    }

    pub(crate) fn trait_data(self, db: &impl DefDatabase) -> Arc<TraitData> {
        db.trait_data(self)
    }

    pub fn trait_ref(self, db: &impl HirDatabase) -> TraitRef {
        TraitRef::for_trait(db, self)
    }

    pub fn is_auto(self, db: &impl DefDatabase) -> bool {
        self.trait_data(db).is_auto()
    }

    pub(crate) fn resolver(&self, db: &impl DefDatabase) -> Resolver {
        let r = self.module(db).resolver(db);
        // add generic params, if present
        let p = self.generic_params(db);
        let r = if !p.params.is_empty() { r.push_generic_params_scope(p) } else { r };
        r
    }
}

impl Docs for Trait {
    fn docs(&self, db: &impl HirDatabase) -> Option<Documentation> {
        docs_from_ast(&*self.source(db).1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeAlias {
    pub(crate) id: TypeAliasId,
}

impl TypeAlias {
    pub fn source(&self, db: &impl DefDatabase) -> (HirFileId, TreeArc<ast::TypeAliasDef>) {
        self.id.source(db)
    }

    pub fn module(&self, db: &impl DefDatabase) -> Module {
        self.id.module(db)
    }

    /// The containing impl block, if this is a method.
    pub fn impl_block(&self, db: &impl DefDatabase) -> Option<ImplBlock> {
        let module_impls = db.impls_in_module(self.module(db));
        ImplBlock::containing(module_impls, (*self).into())
    }

    /// The containing trait, if this is a trait method definition.
    pub fn parent_trait(&self, db: &impl DefDatabase) -> Option<Trait> {
        db.trait_items_index(self.module(db)).get_parent_trait((*self).into())
    }

    pub fn container(&self, db: &impl DefDatabase) -> Option<Container> {
        if let Some(impl_block) = self.impl_block(db) {
            Some(impl_block.into())
        } else if let Some(trait_) = self.parent_trait(db) {
            Some(trait_.into())
        } else {
            None
        }
    }

    pub fn type_ref(self, db: &impl DefDatabase) -> Arc<TypeRef> {
        db.type_alias_ref(self)
    }

    /// Builds a resolver for the type references in this type alias.
    pub(crate) fn resolver(&self, db: &impl HirDatabase) -> Resolver {
        // take the outer scope...
        let r = self
            .impl_block(db)
            .map(|ib| ib.resolver(db))
            .unwrap_or_else(|| self.module(db).resolver(db));
        // ...and add generic params, if present
        let p = self.generic_params(db);
        let r = if !p.params.is_empty() { r.push_generic_params_scope(p) } else { r };
        r
    }
}

impl Docs for TypeAlias {
    fn docs(&self, db: &impl HirDatabase) -> Option<Documentation> {
        docs_from_ast(&*self.source(db).1)
    }
}

pub enum Container {
    Trait(Trait),
    ImplBlock(ImplBlock),
}
impl_froms!(Container: Trait, ImplBlock);

impl Container {
    pub(crate) fn resolver(&self, db: &impl DefDatabase) -> Resolver {
        match self {
            Container::Trait(trait_) => trait_.resolver(db),
            Container::ImplBlock(impl_block) => impl_block.resolver(db),
        }
    }
}
