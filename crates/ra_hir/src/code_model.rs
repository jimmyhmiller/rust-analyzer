//! FIXME: write short doc here

pub(crate) mod src;

use std::sync::Arc;

use hir_def::{
    adt::VariantData,
    builtin_type::BuiltinType,
    docs::Documentation,
    per_ns::PerNs,
    resolver::HasResolver,
    type_ref::{Mutability, TypeRef},
    AdtId, AstItemDef, ConstId, ContainerId, DefWithBodyId, EnumId, FunctionId, GenericDefId,
    HasModule, ImplId, LocalEnumVariantId, LocalImportId, LocalModuleId, LocalStructFieldId,
    Lookup, ModuleId, StaticId, StructId, TraitId, TypeAliasId, UnionId,
};
use hir_expand::{
    diagnostics::DiagnosticSink,
    name::{self, AsName},
    AstId, MacroDefId,
};
use ra_db::{CrateId, Edition, FileId, FilePosition};
use ra_syntax::{ast, AstNode, SyntaxNode};

use crate::{
    db::{DefDatabase, HirDatabase},
    expr::{BindingAnnotation, Body, BodySourceMap, ExprValidator, Pat, PatId},
    ty::display::HirFormatter,
    ty::{
        self, utils::all_super_traits, InEnvironment, InferenceResult, Namespace, TraitEnvironment,
        TraitRef, Ty, TypeCtor, TypeWalk,
    },
    CallableDef, Either, HirDisplay, Name, Source,
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
    pub fn crate_id(self) -> CrateId {
        self.crate_id
    }

    pub fn dependencies(self, db: &impl DefDatabase) -> Vec<CrateDependency> {
        db.crate_graph()
            .dependencies(self.crate_id)
            .map(|dep| {
                let krate = Crate { crate_id: dep.crate_id() };
                let name = dep.as_name();
                CrateDependency { krate, name }
            })
            .collect()
    }

    pub fn root_module(self, db: &impl DefDatabase) -> Option<Module> {
        let module_id = db.crate_def_map(self.crate_id).root;
        Some(Module::new(self, module_id))
    }

    pub fn edition(self, db: &impl DefDatabase) -> Edition {
        let crate_graph = db.crate_graph();
        crate_graph.edition(self.crate_id)
    }

    pub fn all(db: &impl DefDatabase) -> Vec<Crate> {
        db.crate_graph().iter().map(|crate_id| Crate { crate_id }).collect()
    }
}

pub enum ModuleSource {
    SourceFile(ast::SourceFile),
    Module(ast::Module),
}

impl ModuleSource {
    pub fn new(
        db: &impl DefDatabase,
        file_id: Option<FileId>,
        decl_id: Option<AstId<ast::Module>>,
    ) -> ModuleSource {
        match (file_id, decl_id) {
            (Some(file_id), _) => {
                let source_file = db.parse(file_id).tree();
                ModuleSource::SourceFile(source_file)
            }
            (None, Some(item_id)) => {
                let module = item_id.to_node(db);
                assert!(module.item_list().is_some(), "expected inline module");
                ModuleSource::Module(module)
            }
            (None, None) => panic!(),
        }
    }

    // FIXME: this methods do not belong here
    pub fn from_position(db: &impl DefDatabase, position: FilePosition) -> ModuleSource {
        let parse = db.parse(position.file_id);
        match &ra_syntax::algo::find_node_at_offset::<ast::Module>(
            parse.tree().syntax(),
            position.offset,
        ) {
            Some(m) if !m.has_semi() => ModuleSource::Module(m.clone()),
            _ => {
                let source_file = parse.tree();
                ModuleSource::SourceFile(source_file)
            }
        }
    }

    pub fn from_child_node(db: &impl DefDatabase, child: Source<&SyntaxNode>) -> ModuleSource {
        if let Some(m) =
            child.value.ancestors().filter_map(ast::Module::cast).find(|it| !it.has_semi())
        {
            ModuleSource::Module(m)
        } else {
            let file_id = child.file_id.original_file(db);
            let source_file = db.parse(file_id).tree();
            ModuleSource::SourceFile(source_file)
        }
    }

    pub fn from_file_id(db: &impl DefDatabase, file_id: FileId) -> ModuleSource {
        let source_file = db.parse(file_id).tree();
        ModuleSource::SourceFile(source_file)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Module {
    pub(crate) id: ModuleId,
}

/// The defs which can be visible in the module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleDef {
    Module(Module),
    Function(Function),
    Adt(Adt),
    // Can't be directly declared, but can be imported.
    EnumVariant(EnumVariant),
    Const(Const),
    Static(Static),
    Trait(Trait),
    TypeAlias(TypeAlias),
    BuiltinType(BuiltinType),
}
impl_froms!(
    ModuleDef: Module,
    Function,
    Adt(Struct, Enum, Union),
    EnumVariant,
    Const,
    Static,
    Trait,
    TypeAlias,
    BuiltinType
);

pub use hir_def::attr::Attrs;

impl Module {
    pub(crate) fn new(krate: Crate, crate_module_id: LocalModuleId) -> Module {
        Module { id: ModuleId { krate: krate.crate_id, module_id: crate_module_id } }
    }

    /// Name of this module.
    pub fn name(self, db: &impl DefDatabase) -> Option<Name> {
        let def_map = db.crate_def_map(self.id.krate);
        let parent = def_map[self.id.module_id].parent?;
        def_map[parent].children.iter().find_map(|(name, module_id)| {
            if *module_id == self.id.module_id {
                Some(name.clone())
            } else {
                None
            }
        })
    }

    /// Returns the crate this module is part of.
    pub fn krate(self) -> Crate {
        Crate { crate_id: self.id.krate }
    }

    /// Topmost parent of this module. Every module has a `crate_root`, but some
    /// might be missing `krate`. This can happen if a module's file is not included
    /// in the module tree of any target in `Cargo.toml`.
    pub fn crate_root(self, db: &impl DefDatabase) -> Module {
        let def_map = db.crate_def_map(self.id.krate);
        self.with_module_id(def_map.root)
    }

    /// Finds a child module with the specified name.
    pub fn child(self, db: &impl DefDatabase, name: &Name) -> Option<Module> {
        let def_map = db.crate_def_map(self.id.krate);
        let child_id = def_map[self.id.module_id].children.get(name)?;
        Some(self.with_module_id(*child_id))
    }

    /// Iterates over all child modules.
    pub fn children(self, db: &impl DefDatabase) -> impl Iterator<Item = Module> {
        let def_map = db.crate_def_map(self.id.krate);
        let children = def_map[self.id.module_id]
            .children
            .iter()
            .map(|(_, module_id)| self.with_module_id(*module_id))
            .collect::<Vec<_>>();
        children.into_iter()
    }

    /// Finds a parent module.
    pub fn parent(self, db: &impl DefDatabase) -> Option<Module> {
        let def_map = db.crate_def_map(self.id.krate);
        let parent_id = def_map[self.id.module_id].parent?;
        Some(self.with_module_id(parent_id))
    }

    pub fn path_to_root(self, db: &impl HirDatabase) -> Vec<Module> {
        let mut res = vec![self];
        let mut curr = self;
        while let Some(next) = curr.parent(db) {
            res.push(next);
            curr = next
        }
        res
    }

    /// Returns a `ModuleScope`: a set of items, visible in this module.
    pub fn scope(self, db: &impl HirDatabase) -> Vec<(Name, ScopeDef, Option<Import>)> {
        db.crate_def_map(self.id.krate)[self.id.module_id]
            .scope
            .entries()
            .map(|(name, res)| {
                (name.clone(), res.def.into(), res.import.map(|id| Import { parent: self, id }))
            })
            .collect()
    }

    pub fn diagnostics(self, db: &impl HirDatabase, sink: &mut DiagnosticSink) {
        db.crate_def_map(self.id.krate).add_diagnostics(db, self.id.module_id, sink);
        for decl in self.declarations(db) {
            match decl {
                crate::ModuleDef::Function(f) => f.diagnostics(db, sink),
                crate::ModuleDef::Module(m) => {
                    // Only add diagnostics from inline modules
                    if let ModuleSource::Module(_) = m.definition_source(db).value {
                        m.diagnostics(db, sink)
                    }
                }
                _ => (),
            }
        }

        for impl_block in self.impl_blocks(db) {
            for item in impl_block.items(db) {
                if let AssocItem::Function(f) = item {
                    f.diagnostics(db, sink);
                }
            }
        }
    }

    pub fn declarations(self, db: &impl DefDatabase) -> Vec<ModuleDef> {
        let def_map = db.crate_def_map(self.id.krate);
        def_map[self.id.module_id].scope.declarations().map(ModuleDef::from).collect()
    }

    pub fn impl_blocks(self, db: &impl DefDatabase) -> Vec<ImplBlock> {
        let def_map = db.crate_def_map(self.id.krate);
        def_map[self.id.module_id].impls.iter().copied().map(ImplBlock::from).collect()
    }

    fn with_module_id(self, module_id: LocalModuleId) -> Module {
        Module::new(self.krate(), module_id)
    }
}

pub struct Import {
    pub(crate) parent: Module,
    pub(crate) id: LocalImportId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StructField {
    pub(crate) parent: VariantDef,
    pub(crate) id: LocalStructFieldId,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FieldSource {
    Named(ast::RecordFieldDef),
    Pos(ast::TupleFieldDef),
}

impl StructField {
    pub fn name(&self, db: &impl HirDatabase) -> Name {
        self.parent.variant_data(db).fields()[self.id].name.clone()
    }

    pub fn ty(&self, db: &impl HirDatabase) -> Ty {
        db.field_types(self.parent.into())[self.id].clone()
    }

    pub fn parent_def(&self, _db: &impl HirDatabase) -> VariantDef {
        self.parent
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Struct {
    pub(crate) id: StructId,
}

impl Struct {
    pub fn module(self, db: &impl DefDatabase) -> Module {
        Module { id: self.id.module(db) }
    }

    pub fn krate(self, db: &impl DefDatabase) -> Option<Crate> {
        Some(self.module(db).krate())
    }

    pub fn name(self, db: &impl DefDatabase) -> Option<Name> {
        db.struct_data(self.id.into()).name.clone()
    }

    pub fn fields(self, db: &impl HirDatabase) -> Vec<StructField> {
        db.struct_data(self.id.into())
            .variant_data
            .fields()
            .iter()
            .map(|(id, _)| StructField { parent: self.into(), id })
            .collect()
    }

    pub fn field(self, db: &impl HirDatabase, name: &Name) -> Option<StructField> {
        db.struct_data(self.id.into())
            .variant_data
            .fields()
            .iter()
            .find(|(_id, data)| data.name == *name)
            .map(|(id, _)| StructField { parent: self.into(), id })
    }

    pub fn ty(self, db: &impl HirDatabase) -> Ty {
        db.type_for_def(self.into(), Namespace::Types)
    }

    pub fn constructor_ty(self, db: &impl HirDatabase) -> Ty {
        db.type_for_def(self.into(), Namespace::Values)
    }

    fn variant_data(self, db: &impl DefDatabase) -> Arc<VariantData> {
        db.struct_data(self.id.into()).variant_data.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Union {
    pub(crate) id: UnionId,
}

impl Union {
    pub fn name(self, db: &impl DefDatabase) -> Option<Name> {
        db.union_data(self.id).name.clone()
    }

    pub fn module(self, db: &impl DefDatabase) -> Module {
        Module { id: self.id.module(db) }
    }

    pub fn ty(self, db: &impl HirDatabase) -> Ty {
        db.type_for_def(self.into(), Namespace::Types)
    }

    pub fn fields(self, db: &impl HirDatabase) -> Vec<StructField> {
        db.union_data(self.id)
            .variant_data
            .fields()
            .iter()
            .map(|(id, _)| StructField { parent: self.into(), id })
            .collect()
    }

    pub fn field(self, db: &impl HirDatabase, name: &Name) -> Option<StructField> {
        db.union_data(self.id)
            .variant_data
            .fields()
            .iter()
            .find(|(_id, data)| data.name == *name)
            .map(|(id, _)| StructField { parent: self.into(), id })
    }

    fn variant_data(self, db: &impl DefDatabase) -> Arc<VariantData> {
        db.union_data(self.id).variant_data.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Enum {
    pub(crate) id: EnumId,
}

impl Enum {
    pub fn module(self, db: &impl DefDatabase) -> Module {
        Module { id: self.id.module(db) }
    }

    pub fn krate(self, db: &impl DefDatabase) -> Option<Crate> {
        Some(self.module(db).krate())
    }

    pub fn name(self, db: &impl DefDatabase) -> Option<Name> {
        db.enum_data(self.id).name.clone()
    }

    pub fn variants(self, db: &impl DefDatabase) -> Vec<EnumVariant> {
        db.enum_data(self.id)
            .variants
            .iter()
            .map(|(id, _)| EnumVariant { parent: self, id })
            .collect()
    }

    pub fn variant(self, db: &impl DefDatabase, name: &Name) -> Option<EnumVariant> {
        db.enum_data(self.id)
            .variants
            .iter()
            .find(|(_id, data)| data.name.as_ref() == Some(name))
            .map(|(id, _)| EnumVariant { parent: self, id })
    }

    pub fn ty(self, db: &impl HirDatabase) -> Ty {
        db.type_for_def(self.into(), Namespace::Types)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EnumVariant {
    pub(crate) parent: Enum,
    pub(crate) id: LocalEnumVariantId,
}

impl EnumVariant {
    pub fn module(self, db: &impl HirDatabase) -> Module {
        self.parent.module(db)
    }
    pub fn parent_enum(self, _db: &impl DefDatabase) -> Enum {
        self.parent
    }

    pub fn name(self, db: &impl DefDatabase) -> Option<Name> {
        db.enum_data(self.parent.id).variants[self.id].name.clone()
    }

    pub fn fields(self, db: &impl HirDatabase) -> Vec<StructField> {
        self.variant_data(db)
            .fields()
            .iter()
            .map(|(id, _)| StructField { parent: self.into(), id })
            .collect()
    }

    pub fn field(self, db: &impl HirDatabase, name: &Name) -> Option<StructField> {
        self.variant_data(db)
            .fields()
            .iter()
            .find(|(_id, data)| data.name == *name)
            .map(|(id, _)| StructField { parent: self.into(), id })
    }

    pub(crate) fn variant_data(self, db: &impl DefDatabase) -> Arc<VariantData> {
        db.enum_data(self.parent.id).variants[self.id].variant_data.clone()
    }
}

/// A Data Type
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Adt {
    Struct(Struct),
    Union(Union),
    Enum(Enum),
}
impl_froms!(Adt: Struct, Union, Enum);

impl Adt {
    pub fn has_non_default_type_params(self, db: &impl HirDatabase) -> bool {
        let subst = db.generic_defaults(self.into());
        subst.iter().any(|ty| ty == &Ty::Unknown)
    }
    pub fn ty(self, db: &impl HirDatabase) -> Ty {
        match self {
            Adt::Struct(it) => it.ty(db),
            Adt::Union(it) => it.ty(db),
            Adt::Enum(it) => it.ty(db),
        }
    }

    pub fn module(self, db: &impl DefDatabase) -> Module {
        match self {
            Adt::Struct(s) => s.module(db),
            Adt::Union(s) => s.module(db),
            Adt::Enum(e) => e.module(db),
        }
    }

    pub fn krate(self, db: &impl HirDatabase) -> Option<Crate> {
        Some(self.module(db).krate())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VariantDef {
    Struct(Struct),
    Union(Union),
    EnumVariant(EnumVariant),
}
impl_froms!(VariantDef: Struct, Union, EnumVariant);

impl VariantDef {
    pub fn fields(self, db: &impl HirDatabase) -> Vec<StructField> {
        match self {
            VariantDef::Struct(it) => it.fields(db),
            VariantDef::Union(it) => it.fields(db),
            VariantDef::EnumVariant(it) => it.fields(db),
        }
    }

    pub(crate) fn field(self, db: &impl HirDatabase, name: &Name) -> Option<StructField> {
        match self {
            VariantDef::Struct(it) => it.field(db, name),
            VariantDef::Union(it) => it.field(db, name),
            VariantDef::EnumVariant(it) => it.field(db, name),
        }
    }

    pub fn module(self, db: &impl HirDatabase) -> Module {
        match self {
            VariantDef::Struct(it) => it.module(db),
            VariantDef::Union(it) => it.module(db),
            VariantDef::EnumVariant(it) => it.module(db),
        }
    }

    pub(crate) fn variant_data(self, db: &impl DefDatabase) -> Arc<VariantData> {
        match self {
            VariantDef::Struct(it) => it.variant_data(db),
            VariantDef::Union(it) => it.variant_data(db),
            VariantDef::EnumVariant(it) => it.variant_data(db),
        }
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
    pub fn module(self, db: &impl HirDatabase) -> Module {
        match self {
            DefWithBody::Const(c) => c.module(db),
            DefWithBody::Function(f) => f.module(db),
            DefWithBody::Static(s) => s.module(db),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Function {
    pub(crate) id: FunctionId,
}

impl Function {
    pub fn module(self, db: &impl DefDatabase) -> Module {
        self.id.lookup(db).module(db).into()
    }

    pub fn krate(self, db: &impl DefDatabase) -> Option<Crate> {
        Some(self.module(db).krate())
    }

    pub fn name(self, db: &impl HirDatabase) -> Name {
        db.function_data(self.id).name.clone()
    }

    pub fn has_self_param(self, db: &impl HirDatabase) -> bool {
        db.function_data(self.id).has_self_param
    }

    pub fn params(self, db: &impl HirDatabase) -> Vec<TypeRef> {
        db.function_data(self.id).params.clone()
    }

    pub fn body_source_map(self, db: &impl HirDatabase) -> Arc<BodySourceMap> {
        db.body_with_source_map(self.id.into()).1
    }

    pub fn body(self, db: &impl HirDatabase) -> Arc<Body> {
        db.body(self.id.into())
    }

    pub fn ty(self, db: &impl HirDatabase) -> Ty {
        db.type_for_def(self.into(), Namespace::Values)
    }

    pub fn infer(self, db: &impl HirDatabase) -> Arc<InferenceResult> {
        db.infer(self.into())
    }

    /// The containing impl block, if this is a method.
    pub fn impl_block(self, db: &impl DefDatabase) -> Option<ImplBlock> {
        match self.container(db) {
            Some(Container::ImplBlock(it)) => Some(it),
            _ => None,
        }
    }

    /// The containing trait, if this is a trait method definition.
    pub fn parent_trait(self, db: &impl DefDatabase) -> Option<Trait> {
        match self.container(db) {
            Some(Container::Trait(it)) => Some(it),
            _ => None,
        }
    }

    pub fn container(self, db: &impl DefDatabase) -> Option<Container> {
        match self.id.lookup(db).container {
            ContainerId::TraitId(it) => Some(Container::Trait(it.into())),
            ContainerId::ImplId(it) => Some(Container::ImplBlock(it.into())),
            ContainerId::ModuleId(_) => None,
        }
    }

    pub fn diagnostics(self, db: &impl HirDatabase, sink: &mut DiagnosticSink) {
        let infer = self.infer(db);
        infer.add_diagnostics(db, self, sink);
        let mut validator = ExprValidator::new(self, infer, sink);
        validator.validate_body(db);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Const {
    pub(crate) id: ConstId,
}

impl Const {
    pub fn module(self, db: &impl DefDatabase) -> Module {
        Module { id: self.id.lookup(db).module(db) }
    }

    pub fn krate(self, db: &impl DefDatabase) -> Option<Crate> {
        Some(self.module(db).krate())
    }

    pub fn name(self, db: &impl HirDatabase) -> Option<Name> {
        db.const_data(self.id).name.clone()
    }

    pub fn infer(self, db: &impl HirDatabase) -> Arc<InferenceResult> {
        db.infer(self.into())
    }

    /// The containing impl block, if this is a type alias.
    pub fn impl_block(self, db: &impl DefDatabase) -> Option<ImplBlock> {
        match self.container(db) {
            Some(Container::ImplBlock(it)) => Some(it),
            _ => None,
        }
    }

    /// The containing trait, if this is a trait type alias definition.
    pub fn parent_trait(self, db: &impl DefDatabase) -> Option<Trait> {
        match self.container(db) {
            Some(Container::Trait(it)) => Some(it),
            _ => None,
        }
    }

    pub fn container(self, db: &impl DefDatabase) -> Option<Container> {
        match self.id.lookup(db).container {
            ContainerId::TraitId(it) => Some(Container::Trait(it.into())),
            ContainerId::ImplId(it) => Some(Container::ImplBlock(it.into())),
            ContainerId::ModuleId(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Static {
    pub(crate) id: StaticId,
}

impl Static {
    pub fn module(self, db: &impl DefDatabase) -> Module {
        Module { id: self.id.lookup(db).module(db) }
    }

    pub fn krate(self, db: &impl DefDatabase) -> Option<Crate> {
        Some(self.module(db).krate())
    }

    pub fn infer(self, db: &impl HirDatabase) -> Arc<InferenceResult> {
        db.infer(self.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Trait {
    pub(crate) id: TraitId,
}

impl Trait {
    pub fn module(self, db: &impl DefDatabase) -> Module {
        Module { id: self.id.module(db) }
    }

    pub fn name(self, db: &impl DefDatabase) -> Option<Name> {
        db.trait_data(self.id).name.clone()
    }

    pub fn items(self, db: &impl DefDatabase) -> Vec<AssocItem> {
        db.trait_data(self.id).items.iter().map(|(_name, it)| (*it).into()).collect()
    }

    pub fn associated_type_by_name_including_super_traits(
        self,
        db: &impl HirDatabase,
        name: &Name,
    ) -> Option<TypeAlias> {
        all_super_traits(db, self.id)
            .into_iter()
            .find_map(|t| db.trait_data(t).associated_type_by_name(name))
            .map(TypeAlias::from)
    }

    pub fn trait_ref(self, db: &impl HirDatabase) -> TraitRef {
        TraitRef::for_trait(db, self)
    }

    pub fn is_auto(self, db: &impl DefDatabase) -> bool {
        db.trait_data(self.id).auto
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeAlias {
    pub(crate) id: TypeAliasId,
}

impl TypeAlias {
    pub fn has_non_default_type_params(self, db: &impl HirDatabase) -> bool {
        let subst = db.generic_defaults(self.id.into());
        subst.iter().any(|ty| ty == &Ty::Unknown)
    }

    pub fn module(self, db: &impl DefDatabase) -> Module {
        Module { id: self.id.lookup(db).module(db) }
    }

    pub fn krate(self, db: &impl DefDatabase) -> Option<Crate> {
        Some(self.module(db).krate())
    }

    /// The containing impl block, if this is a type alias.
    pub fn impl_block(self, db: &impl DefDatabase) -> Option<ImplBlock> {
        match self.container(db) {
            Some(Container::ImplBlock(it)) => Some(it),
            _ => None,
        }
    }

    /// The containing trait, if this is a trait type alias definition.
    pub fn parent_trait(self, db: &impl DefDatabase) -> Option<Trait> {
        match self.container(db) {
            Some(Container::Trait(it)) => Some(it),
            _ => None,
        }
    }

    pub fn container(self, db: &impl DefDatabase) -> Option<Container> {
        match self.id.lookup(db).container {
            ContainerId::TraitId(it) => Some(Container::Trait(it.into())),
            ContainerId::ImplId(it) => Some(Container::ImplBlock(it.into())),
            ContainerId::ModuleId(_) => None,
        }
    }

    pub fn type_ref(self, db: &impl DefDatabase) -> Option<TypeRef> {
        db.type_alias_data(self.id).type_ref.clone()
    }

    pub fn ty(self, db: &impl HirDatabase) -> Ty {
        db.type_for_def(self.into(), Namespace::Types)
    }

    pub fn name(self, db: &impl DefDatabase) -> Name {
        db.type_alias_data(self.id).name.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacroDef {
    pub(crate) id: MacroDefId,
}

impl MacroDef {}

pub enum Container {
    Trait(Trait),
    ImplBlock(ImplBlock),
}
impl_froms!(Container: Trait, ImplBlock);

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum AssocItem {
    Function(Function),
    Const(Const),
    TypeAlias(TypeAlias),
}
// FIXME: not every function, ... is actually an assoc item. maybe we should make
// sure that you can only turn actual assoc items into AssocItems. This would
// require not implementing From, and instead having some checked way of
// casting them, and somehow making the constructors private, which would be annoying.
impl_froms!(AssocItem: Function, Const, TypeAlias);

impl AssocItem {
    pub fn module(self, db: &impl DefDatabase) -> Module {
        match self {
            AssocItem::Function(f) => f.module(db),
            AssocItem::Const(c) => c.module(db),
            AssocItem::TypeAlias(t) => t.module(db),
        }
    }

    pub fn container(self, db: &impl DefDatabase) -> Container {
        match self {
            AssocItem::Function(f) => f.container(db),
            AssocItem::Const(c) => c.container(db),
            AssocItem::TypeAlias(t) => t.container(db),
        }
        .expect("AssocItem without container")
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum GenericDef {
    Function(Function),
    Adt(Adt),
    Trait(Trait),
    TypeAlias(TypeAlias),
    ImplBlock(ImplBlock),
    // enum variants cannot have generics themselves, but their parent enums
    // can, and this makes some code easier to write
    EnumVariant(EnumVariant),
    // consts can have type parameters from their parents (i.e. associated consts of traits)
    Const(Const),
}
impl_froms!(
    GenericDef: Function,
    Adt(Struct, Enum, Union),
    Trait,
    TypeAlias,
    ImplBlock,
    EnumVariant,
    Const
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Local {
    pub(crate) parent: DefWithBody,
    pub(crate) pat_id: PatId,
}

impl Local {
    pub fn name(self, db: &impl HirDatabase) -> Option<Name> {
        let body = db.body(self.parent.into());
        match &body[self.pat_id] {
            Pat::Bind { name, .. } => Some(name.clone()),
            _ => None,
        }
    }

    pub fn is_self(self, db: &impl HirDatabase) -> bool {
        self.name(db) == Some(name::SELF_PARAM)
    }

    pub fn is_mut(self, db: &impl HirDatabase) -> bool {
        let body = db.body(self.parent.into());
        match &body[self.pat_id] {
            Pat::Bind { mode, .. } => match mode {
                BindingAnnotation::Mutable | BindingAnnotation::RefMut => true,
                _ => false,
            },
            _ => false,
        }
    }

    pub fn parent(self, _db: &impl HirDatabase) -> DefWithBody {
        self.parent
    }

    pub fn module(self, db: &impl HirDatabase) -> Module {
        self.parent.module(db)
    }

    pub fn ty(self, db: &impl HirDatabase) -> Type {
        let infer = db.infer(self.parent);
        let ty = infer[self.pat_id].clone();
        let def = DefWithBodyId::from(self.parent);
        let resolver = def.resolver(db);
        let krate = def.module(db).krate;
        let environment = TraitEnvironment::lower(db, &resolver);
        Type { krate, ty: InEnvironment { value: ty, environment } }
    }

    pub fn source(self, db: &impl HirDatabase) -> Source<Either<ast::BindPat, ast::SelfParam>> {
        let (_body, source_map) = db.body_with_source_map(self.parent.into());
        let src = source_map.pat_syntax(self.pat_id).unwrap(); // Hmm...
        let root = src.file_syntax(db);
        src.map(|ast| ast.map(|it| it.cast().unwrap().to_node(&root), |it| it.to_node(&root)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GenericParam {
    pub(crate) parent: GenericDefId,
    pub(crate) idx: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImplBlock {
    pub(crate) id: ImplId,
}

impl ImplBlock {
    pub fn all_in_crate(db: &impl HirDatabase, krate: Crate) -> Vec<ImplBlock> {
        let impls = db.impls_in_crate(krate.crate_id);
        impls.all_impls().map(Self::from).collect()
    }
    pub fn for_trait(db: &impl HirDatabase, krate: Crate, trait_: Trait) -> Vec<ImplBlock> {
        let impls = db.impls_in_crate(krate.crate_id);
        impls.lookup_impl_blocks_for_trait(trait_).map(Self::from).collect()
    }

    pub fn target_trait(&self, db: &impl DefDatabase) -> Option<TypeRef> {
        db.impl_data(self.id).target_trait.clone()
    }

    pub fn target_type(&self, db: &impl DefDatabase) -> TypeRef {
        db.impl_data(self.id).target_type.clone()
    }

    pub fn target_ty(&self, db: &impl HirDatabase) -> Ty {
        Ty::from_hir(db, &self.id.resolver(db), &self.target_type(db))
    }

    pub fn target_trait_ref(&self, db: &impl HirDatabase) -> Option<TraitRef> {
        let target_ty = self.target_ty(db);
        TraitRef::from_hir(db, &self.id.resolver(db), &self.target_trait(db)?, Some(target_ty))
    }

    pub fn items(&self, db: &impl DefDatabase) -> Vec<AssocItem> {
        db.impl_data(self.id).items.iter().map(|it| (*it).into()).collect()
    }

    pub fn is_negative(&self, db: &impl DefDatabase) -> bool {
        db.impl_data(self.id).is_negative
    }

    pub fn module(&self, db: &impl DefDatabase) -> Module {
        self.id.module(db).into()
    }

    pub fn krate(&self, db: &impl DefDatabase) -> Crate {
        Crate { crate_id: self.module(db).id.krate }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Type {
    pub(crate) krate: CrateId,
    pub(crate) ty: InEnvironment<Ty>,
}

impl Type {
    pub fn is_bool(&self) -> bool {
        match &self.ty.value {
            Ty::Apply(a_ty) => match a_ty.ctor {
                TypeCtor::Bool => true,
                _ => false,
            },
            _ => false,
        }
    }

    pub fn is_mutable_reference(&self) -> bool {
        match &self.ty.value {
            Ty::Apply(a_ty) => match a_ty.ctor {
                TypeCtor::Ref(Mutability::Mut) => true,
                _ => false,
            },
            _ => false,
        }
    }

    pub fn is_unknown(&self) -> bool {
        match &self.ty.value {
            Ty::Unknown => true,
            _ => false,
        }
    }

    // FIXME: this method is broken, as it doesn't take closures into account.
    pub fn as_callable(&self) -> Option<CallableDef> {
        Some(self.ty.value.as_callable()?.0)
    }

    pub fn contains_unknown(&self) -> bool {
        return go(&self.ty.value);

        fn go(ty: &Ty) -> bool {
            match ty {
                Ty::Unknown => true,
                Ty::Apply(a_ty) => a_ty.parameters.iter().any(go),
                _ => false,
            }
        }
    }

    pub fn fields(&self, db: &impl HirDatabase) -> Vec<(StructField, Type)> {
        if let Ty::Apply(a_ty) = &self.ty.value {
            match a_ty.ctor {
                ty::TypeCtor::Adt(AdtId::StructId(s)) => {
                    let var_def = s.into();
                    return db
                        .field_types(var_def)
                        .iter()
                        .map(|(local_id, ty)| {
                            let def = StructField { parent: var_def.into(), id: local_id };
                            let ty = ty.clone().subst(&a_ty.parameters);
                            (def, self.derived(ty))
                        })
                        .collect();
                }
                _ => {}
            }
        };
        Vec::new()
    }

    pub fn tuple_fields(&self, _db: &impl HirDatabase) -> Vec<Type> {
        let mut res = Vec::new();
        if let Ty::Apply(a_ty) = &self.ty.value {
            match a_ty.ctor {
                ty::TypeCtor::Tuple { .. } => {
                    for ty in a_ty.parameters.iter() {
                        let ty = ty.clone().subst(&a_ty.parameters);
                        res.push(self.derived(ty));
                    }
                }
                _ => {}
            }
        };
        res
    }

    pub fn variant_fields(
        &self,
        db: &impl HirDatabase,
        def: VariantDef,
    ) -> Vec<(StructField, Type)> {
        // FIXME: check that ty and def match
        match &self.ty.value {
            Ty::Apply(a_ty) => def
                .fields(db)
                .into_iter()
                .map(|it| (it, self.derived(it.ty(db).subst(&a_ty.parameters))))
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn autoderef<'a>(&'a self, db: &'a impl HirDatabase) -> impl Iterator<Item = Type> + 'a {
        // There should be no inference vars in types passed here
        // FIXME check that?
        let canonical = crate::ty::Canonical { value: self.ty.value.clone(), num_vars: 0 };
        let environment = self.ty.environment.clone();
        let ty = InEnvironment { value: canonical, environment: environment.clone() };
        ty::autoderef(db, Some(self.krate), ty)
            .map(|canonical| canonical.value)
            .map(move |ty| self.derived(ty))
    }

    // FIXME: remove
    pub fn into_ty(self) -> Ty {
        self.ty.value
    }

    pub fn as_adt(&self) -> Option<Adt> {
        let (adt, _subst) = self.ty.value.as_adt()?;
        Some(adt)
    }

    fn derived(&self, ty: Ty) -> Type {
        Type {
            krate: self.krate,
            ty: InEnvironment { value: ty, environment: self.ty.environment.clone() },
        }
    }
}

impl HirDisplay for Type {
    fn hir_fmt(&self, f: &mut HirFormatter<impl HirDatabase>) -> std::fmt::Result {
        self.ty.value.hir_fmt(f)
    }
}

/// For IDE only
pub enum ScopeDef {
    ModuleDef(ModuleDef),
    MacroDef(MacroDef),
    GenericParam(GenericParam),
    ImplSelfType(ImplBlock),
    AdtSelfType(Adt),
    Local(Local),
    Unknown,
}

impl From<PerNs> for ScopeDef {
    fn from(def: PerNs) -> Self {
        def.take_types()
            .or_else(|| def.take_values())
            .map(|module_def_id| ScopeDef::ModuleDef(module_def_id.into()))
            .or_else(|| {
                def.take_macros().map(|macro_def_id| ScopeDef::MacroDef(macro_def_id.into()))
            })
            .unwrap_or(ScopeDef::Unknown)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttrDef {
    Module(Module),
    StructField(StructField),
    Adt(Adt),
    Function(Function),
    EnumVariant(EnumVariant),
    Static(Static),
    Const(Const),
    Trait(Trait),
    TypeAlias(TypeAlias),
    MacroDef(MacroDef),
}

impl_froms!(
    AttrDef: Module,
    StructField,
    Adt(Struct, Enum, Union),
    EnumVariant,
    Static,
    Const,
    Function,
    Trait,
    TypeAlias,
    MacroDef
);

pub trait HasAttrs {
    fn attrs(self, db: &impl DefDatabase) -> Attrs;
}

impl<T: Into<AttrDef>> HasAttrs for T {
    fn attrs(self, db: &impl DefDatabase) -> Attrs {
        let def: AttrDef = self.into();
        db.attrs(def.into())
    }
}

pub trait Docs {
    fn docs(&self, db: &impl HirDatabase) -> Option<Documentation>;
}
impl<T: Into<AttrDef> + Copy> Docs for T {
    fn docs(&self, db: &impl HirDatabase) -> Option<Documentation> {
        let def: AttrDef = (*self).into();
        db.documentation(def.into())
    }
}
