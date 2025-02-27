// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    diag,
    diagnostics::codes::*,
    expansion::{
        ast::{self as E, AbilitySet, ModuleIdent},
        translate::is_valid_struct_constant_or_schema_name as is_constant_name,
    },
    naming::ast::{self as N, Neighbor_},
    parser::ast::{self as P, Ability_, ConstantName, Field, FunctionName, StructName},
    shared::{unique_map::UniqueMap, *},
    FullyCompiledProgram,
};
use move_ir_types::location::*;
use move_symbol_pool::Symbol;
use std::collections::{BTreeMap, BTreeSet};

use super::{
    ast::{Neighbor, TParamID},
    fake_natives,
};

//**************************************************************************************************
// Context
//**************************************************************************************************

#[derive(Debug, Clone)]
enum ResolvedType {
    TParam(Loc, N::TParam),
    BuiltinType,
}

impl ResolvedType {
    fn error_msg(&self, n: &Name) -> (Loc, String) {
        match self {
            ResolvedType::TParam(loc, _) => (
                *loc,
                format!("But '{}' was declared as a type parameter here", n),
            ),
            ResolvedType::BuiltinType => (n.loc, format!("But '{}' is a builtin type", n)),
        }
    }
}

struct Context<'env> {
    env: &'env mut CompilationEnv,
    current_module: Option<ModuleIdent>,
    scoped_types: BTreeMap<ModuleIdent, BTreeMap<Symbol, (Loc, ModuleIdent, AbilitySet, usize)>>,
    unscoped_types: BTreeMap<Symbol, ResolvedType>,
    scoped_functions: BTreeMap<ModuleIdent, BTreeMap<Symbol, Loc>>,
    unscoped_constants: BTreeMap<Symbol, Loc>,
    scoped_constants: BTreeMap<ModuleIdent, BTreeMap<Symbol, Loc>>,
    local_scopes: Vec<BTreeMap<Symbol, u16>>,
    local_count: BTreeMap<Symbol, u16>,
    used_locals: BTreeSet<N::Var_>,
    /// Type parameters used in a function (they have to be cleared after processing each function).
    used_fun_tparams: BTreeSet<TParamID>,
    /// Indicates if the compiler is currently translating a function (set to true before starting
    /// to translate a function and to false after translation is over).
    translating_fun: bool,
}

impl<'env> Context<'env> {
    fn new(
        compilation_env: &'env mut CompilationEnv,
        pre_compiled_lib: Option<&FullyCompiledProgram>,
        prog: &E::Program,
    ) -> Self {
        use ResolvedType as RT;
        let all_modules = || {
            prog.modules
                .key_cloned_iter()
                .chain(pre_compiled_lib.iter().flat_map(|pre_compiled| {
                    pre_compiled
                        .expansion
                        .modules
                        .key_cloned_iter()
                        .filter(|(mident, _m)| !prog.modules.contains_key(mident))
                }))
        };
        let scoped_types = all_modules()
            .map(|(mident, mdef)| {
                let mems = mdef
                    .structs
                    .key_cloned_iter()
                    .map(|(s, sdef)| {
                        let abilities = sdef.abilities.clone();
                        let arity = sdef.type_parameters.len();
                        let sname = s.value();
                        (sname, (s.loc(), mident, abilities, arity))
                    })
                    .collect();
                (mident, mems)
            })
            .collect();
        let scoped_functions = all_modules()
            .map(|(mident, mdef)| {
                let mems = mdef
                    .functions
                    .iter()
                    .map(|(nloc, n, _)| (*n, nloc))
                    .collect();
                (mident, mems)
            })
            .collect();
        let scoped_constants = all_modules()
            .map(|(mident, mdef)| {
                let mems = mdef
                    .constants
                    .iter()
                    .map(|(nloc, n, _)| (*n, nloc))
                    .collect();
                (mident, mems)
            })
            .collect();
        let unscoped_types = N::BuiltinTypeName_::all_names()
            .iter()
            .map(|s| (*s, RT::BuiltinType))
            .collect();
        Self {
            env: compilation_env,
            current_module: None,
            scoped_types,
            scoped_functions,
            scoped_constants,
            unscoped_types,
            unscoped_constants: BTreeMap::new(),
            local_scopes: vec![],
            local_count: BTreeMap::new(),
            used_locals: BTreeSet::new(),
            used_fun_tparams: BTreeSet::new(),
            translating_fun: false,
        }
    }

    fn resolve_module(&mut self, m: &ModuleIdent) -> bool {
        // NOTE: piggybacking on `scoped_functions` to provide a set of modules in the context。
        // TODO: a better solution would be to have a single `BTreeMap<ModuleIdent, ModuleInfo>`
        // in the context that can be used to resolve modules, types, and functions.
        let resolved = self.scoped_functions.contains_key(m);
        if !resolved {
            self.env.add_diag(diag!(
                NameResolution::UnboundModule,
                (m.loc, format!("Unbound module '{}'", m))
            ))
        }
        resolved
    }

    fn resolve_module_type(
        &mut self,
        loc: Loc,
        m: &ModuleIdent,
        n: &Name,
    ) -> Option<(Loc, StructName, AbilitySet, usize)> {
        let types = match self.scoped_types.get(m) {
            None => {
                self.env.add_diag(diag!(
                    NameResolution::UnboundModule,
                    (m.loc, format!("Unbound module '{}'", m)),
                ));
                return None;
            }
            Some(members) => members,
        };
        match types.get(&n.value) {
            None => {
                let msg = format!(
                    "Invalid module access. Unbound struct '{}' in module '{}'",
                    n, m
                );
                self.env
                    .add_diag(diag!(NameResolution::UnboundModuleMember, (loc, msg)));
                None
            }
            Some((decl_loc, _, abilities, arity)) => {
                Some((*decl_loc, StructName(*n), abilities.clone(), *arity))
            }
        }
    }

    fn resolve_module_function(
        &mut self,
        loc: Loc,
        m: &ModuleIdent,
        n: &Name,
    ) -> Option<FunctionName> {
        let functions = match self.scoped_functions.get(m) {
            None => {
                self.env.add_diag(diag!(
                    NameResolution::UnboundModule,
                    (m.loc, format!("Unbound module '{}'", m)),
                ));
                return None;
            }
            Some(members) => members,
        };
        match functions.get(&n.value).cloned() {
            None => {
                let msg = format!(
                    "Invalid module access. Unbound function '{}' in module '{}'",
                    n, m
                );
                self.env
                    .add_diag(diag!(NameResolution::UnboundModuleMember, (loc, msg)));
                None
            }
            Some(_) => Some(FunctionName(*n)),
        }
    }

    fn resolve_module_constant(
        &mut self,
        loc: Loc,
        m: &ModuleIdent,
        n: Name,
    ) -> Option<ConstantName> {
        let constants = match self.scoped_constants.get(m) {
            None => {
                self.env.add_diag(diag!(
                    NameResolution::UnboundModule,
                    (m.loc, format!("Unbound module '{}'", m)),
                ));
                return None;
            }
            Some(members) => members,
        };
        match constants.get(&n.value).cloned() {
            None => {
                let msg = format!(
                    "Invalid module access. Unbound constant '{}' in module '{}'",
                    n, m
                );
                self.env
                    .add_diag(diag!(NameResolution::UnboundModuleMember, (loc, msg)));
                None
            }
            Some(_) => Some(ConstantName(n)),
        }
    }

    fn resolve_unscoped_type(&mut self, n: &Name) -> Option<ResolvedType> {
        match self.unscoped_types.get(&n.value) {
            None => {
                let msg = format!("Unbound type '{}' in current scope", n);
                self.env
                    .add_diag(diag!(NameResolution::UnboundType, (n.loc, msg)));
                None
            }
            Some(rn) => Some(rn.clone()),
        }
    }

    fn resolve_struct_name(
        &mut self,
        loc: Loc,
        verb: &str,
        sp!(nloc, ma_): E::ModuleAccess,
        etys_opt: Option<Vec<E::Type>>,
    ) -> Option<(ModuleIdent, StructName, Option<Vec<N::Type>>)> {
        use E::ModuleAccess_ as EA;

        match ma_ {
            EA::Name(n) => match self.resolve_unscoped_type(&n) {
                None => {
                    assert!(self.env.has_errors());
                    None
                }
                Some(rt) => {
                    self.env.add_diag(diag!(
                        NameResolution::NamePositionMismatch,
                        (nloc, format!("Invalid {}. Expected a struct name", verb)),
                        rt.error_msg(&n),
                    ));
                    None
                }
            },
            EA::ModuleAccess(m, n) => match self.resolve_module_type(nloc, &m, &n) {
                None => {
                    assert!(self.env.has_errors());
                    None
                }
                Some((_, _, _, arity)) => {
                    let tys_opt = etys_opt.map(|etys| {
                        let tys = types(self, etys);
                        let name_f = || format!("{}::{}", &m, &n);
                        check_type_argument_arity(self, loc, name_f, tys, arity)
                    });
                    Some((m, StructName(n), tys_opt))
                }
            },
        }
    }

    fn resolve_constant(
        &mut self,
        sp!(loc, ma_): E::ModuleAccess,
    ) -> Option<(Option<ModuleIdent>, ConstantName)> {
        use E::ModuleAccess_ as EA;
        match ma_ {
            EA::Name(n) => match self.unscoped_constants.get(&n.value) {
                None => {
                    self.env.add_diag(diag!(
                        NameResolution::UnboundUnscopedName,
                        (loc, format!("Unbound constant '{}'", n)),
                    ));
                    None
                }
                Some(_) => Some((None, ConstantName(n))),
            },
            EA::ModuleAccess(m, n) => match self.resolve_module_constant(loc, &m, n) {
                None => {
                    assert!(self.env.has_errors());
                    None
                }
                Some(cname) => Some((Some(m), cname)),
            },
        }
    }

    fn bind_type(&mut self, s: Symbol, rt: ResolvedType) {
        self.unscoped_types.insert(s, rt);
    }

    fn bind_constant(&mut self, s: Symbol, loc: Loc) {
        self.unscoped_constants.insert(s, loc);
    }

    fn save_unscoped(&self) -> (BTreeMap<Symbol, ResolvedType>, BTreeMap<Symbol, Loc>) {
        (self.unscoped_types.clone(), self.unscoped_constants.clone())
    }

    fn restore_unscoped(
        &mut self,
        (types, constants): (BTreeMap<Symbol, ResolvedType>, BTreeMap<Symbol, Loc>),
    ) {
        self.unscoped_types = types;
        self.unscoped_constants = constants;
    }

    fn new_local_scope(&mut self) {
        let cur = self.local_scopes.last().unwrap().clone();
        self.local_scopes.push(cur)
    }

    fn close_local_scope(&mut self) {
        self.local_scopes.pop();
    }

    fn declare_local(&mut self, is_parameter: bool, sp!(vloc, name): Name) -> N::Var {
        let default = if is_parameter { 0 } else { 1 };
        let id = *self
            .local_count
            .entry(name)
            .and_modify(|c| *c += 1)
            .or_insert(default);
        self.local_scopes.last_mut().unwrap().insert(name, id);
        // all locals start at color zero
        // they will be incremented when substituted for macros
        let nvar_ = N::Var_ { name, id, color: 0 };
        sp(vloc, nvar_)
    }

    fn resolve_local(&mut self, loc: Loc, verb: &str, sp!(vloc, name): Name) -> Option<N::Var> {
        let id_opt = self.local_scopes.last().unwrap().get(&name).copied();
        match id_opt {
            None => {
                let msg = format!("Invalid {}. Unbound variable '{}'", verb, name);
                self.env
                    .add_diag(diag!(NameResolution::UnboundVariable, (loc, msg)));
                None
            }
            Some(id) => {
                // all locals start at color zero
                // they will be incremented when substituted for macros
                let nvar_ = N::Var_ { name, id, color: 0 };
                self.used_locals.insert(nvar_);
                Some(sp(vloc, nvar_))
            }
        }
    }
}

//**************************************************************************************************
// Entry
//**************************************************************************************************

pub fn program(
    compilation_env: &mut CompilationEnv,
    pre_compiled_lib: Option<&FullyCompiledProgram>,
    prog: E::Program,
) -> N::Program {
    let mut context = Context::new(compilation_env, pre_compiled_lib, &prog);
    let E::Program {
        modules: emodules,
        scripts: escripts,
    } = prog;
    let modules = modules(&mut context, emodules);
    let scripts = scripts(&mut context, escripts);
    N::Program { modules, scripts }
}

fn modules(
    context: &mut Context,
    modules: UniqueMap<ModuleIdent, E::ModuleDefinition>,
) -> UniqueMap<ModuleIdent, N::ModuleDefinition> {
    modules.map(|ident, mdef| module(context, ident, mdef))
}

fn module(
    context: &mut Context,
    ident: ModuleIdent,
    mdef: E::ModuleDefinition,
) -> N::ModuleDefinition {
    context.current_module = Some(ident);
    let E::ModuleDefinition {
        loc,
        warning_filter,
        package_name,
        attributes,
        is_source_module,
        friends: efriends,
        structs: estructs,
        functions: efunctions,
        constants: econstants,
        specs,
    } = mdef;
    context.env.add_warning_filter_scope(warning_filter.clone());
    let mut spec_dependencies = BTreeSet::new();
    spec_blocks(&mut spec_dependencies, &specs);
    let friends = efriends.filter_map(|mident, f| friend(context, mident, f));
    let unscoped = context.save_unscoped();
    let structs = estructs.map(|name, s| {
        context.restore_unscoped(unscoped.clone());
        struct_def(context, name, s)
    });
    let functions = efunctions.map(|name, f| {
        context.restore_unscoped(unscoped.clone());
        function(context, &mut spec_dependencies, Some(ident), name, f)
    });
    let constants = econstants.map(|name, c| {
        context.restore_unscoped(unscoped.clone());
        constant(context, name, c)
    });
    context.restore_unscoped(unscoped);
    context.env.pop_warning_filter_scope();
    N::ModuleDefinition {
        loc,
        warning_filter,
        package_name,
        attributes,
        is_source_module,
        friends,
        structs,
        constants,
        functions,
        spec_dependencies,
    }
}

fn scripts(
    context: &mut Context,
    escripts: BTreeMap<Symbol, E::Script>,
) -> BTreeMap<Symbol, N::Script> {
    escripts
        .into_iter()
        .map(|(n, s)| (n, script(context, s)))
        .collect()
}

fn script(context: &mut Context, escript: E::Script) -> N::Script {
    let E::Script {
        warning_filter,
        package_name,
        attributes,
        loc,
        constants: econstants,
        function_name,
        function: efunction,
        specs,
    } = escript;
    context.env.add_warning_filter_scope(warning_filter.clone());
    let mut spec_dependencies = BTreeSet::new();
    spec_blocks(&mut spec_dependencies, &specs);
    let outer_unscoped = context.save_unscoped();
    for (loc, s, _) in &econstants {
        context.bind_constant(*s, loc)
    }
    let inner_unscoped = context.save_unscoped();
    let constants = econstants.map(|name, c| {
        context.restore_unscoped(inner_unscoped.clone());
        constant(context, name, c)
    });
    context.restore_unscoped(inner_unscoped);
    let function = function(
        context,
        &mut spec_dependencies,
        None,
        function_name,
        efunction,
    );
    context.restore_unscoped(outer_unscoped);
    context.env.pop_warning_filter_scope();
    N::Script {
        warning_filter,
        package_name,
        attributes,
        loc,
        constants,
        function_name,
        function,
        spec_dependencies,
    }
}

//**************************************************************************************************
// Friends
//**************************************************************************************************

fn friend(context: &mut Context, mident: ModuleIdent, friend: E::Friend) -> Option<E::Friend> {
    let current_mident = context.current_module.as_ref().unwrap();
    if mident.value.address != current_mident.value.address {
        // NOTE: in alignment with the bytecode verifier, this constraint is a policy decision
        // rather than a technical requirement. The compiler, VM, and bytecode verifier DO NOT
        // rely on the assumption that friend modules must reside within the same account address.
        let msg = "Cannot declare modules out of the current address as a friend";
        context.env.add_diag(diag!(
            Declarations::InvalidFriendDeclaration,
            (friend.loc, "Invalid friend declaration"),
            (mident.loc, msg),
        ));
        None
    } else if &mident == current_mident {
        context.env.add_diag(diag!(
            Declarations::InvalidFriendDeclaration,
            (friend.loc, "Invalid friend declaration"),
            (mident.loc, "Cannot declare the module itself as a friend"),
        ));
        None
    } else if context.resolve_module(&mident) {
        Some(friend)
    } else {
        assert!(context.env.has_errors());
        None
    }
}

//**************************************************************************************************
// Functions
//**************************************************************************************************

fn function(
    context: &mut Context,
    spec_dependencies: &mut BTreeSet<(ModuleIdent, Neighbor)>,
    module_opt: Option<ModuleIdent>,
    name: FunctionName,
    ef: E::Function,
) -> N::Function {
    let E::Function {
        warning_filter,
        index,
        attributes,
        loc: _,
        visibility,
        entry,
        signature,
        acquires,
        body,
        specs,
    } = ef;
    assert!(context.local_scopes.is_empty());
    assert!(context.local_count.is_empty());
    assert!(context.used_locals.is_empty());
    assert!(context.used_fun_tparams.is_empty());
    assert!(!context.translating_fun);
    context.env.add_warning_filter_scope(warning_filter.clone());
    spec_blocks(spec_dependencies, specs.values());
    context.local_scopes = vec![BTreeMap::new()];
    context.local_count = BTreeMap::new();
    context.translating_fun = true;
    let signature = function_signature(context, signature);
    let acquires = function_acquires(context, acquires);
    let body = function_body(context, body);

    if !matches!(body.value, N::FunctionBody_::Native) {
        for tparam in &signature.type_parameters {
            if !context.used_fun_tparams.contains(&tparam.id) {
                let sp!(loc, n) = tparam.user_specified_name;
                let msg = format!("Unused type parameter '{}'.", n);
                context
                    .env
                    .add_diag(diag!(UnusedItem::FunTypeParam, (loc, msg)))
            }
        }
    }

    let mut f = N::Function {
        warning_filter,
        index,
        attributes,
        visibility,
        entry,
        signature,
        acquires,
        body,
    };
    fake_natives::function(context.env, module_opt, name, &f);
    let used_locals = std::mem::take(&mut context.used_locals);
    remove_unused_bindings_function(context, &used_locals, &mut f);
    context.local_scopes = vec![];
    context.local_count = BTreeMap::new();
    context.used_locals = BTreeSet::new();
    context.used_fun_tparams = BTreeSet::new();
    context.env.pop_warning_filter_scope();
    context.translating_fun = false;
    f
}

fn function_signature(context: &mut Context, sig: E::FunctionSignature) -> N::FunctionSignature {
    let type_parameters = fun_type_parameters(context, sig.type_parameters);

    let mut declared = UniqueMap::new();
    let parameters = sig
        .parameters
        .into_iter()
        .map(|(param, param_ty)| {
            if let Err((param, prev_loc)) = declared.add(param, ()) {
                if !param.is_underscore() {
                    let msg = format!("Duplicate parameter with name '{}'", param);
                    context.env.add_diag(diag!(
                        Declarations::DuplicateItem,
                        (param.loc(), msg),
                        (prev_loc, "Previously declared here"),
                    ))
                }
            }
            let is_parameter = true;
            let nparam = context.declare_local(is_parameter, param.0);
            let nparam_ty = type_(context, param_ty);
            (nparam, nparam_ty)
        })
        .collect();
    let return_type = type_(context, sig.return_type);
    N::FunctionSignature {
        type_parameters,
        parameters,
        return_type,
    }
}

fn function_body(context: &mut Context, sp!(loc, b_): E::FunctionBody) -> N::FunctionBody {
    match b_ {
        E::FunctionBody_::Native => sp(loc, N::FunctionBody_::Native),
        E::FunctionBody_::Defined(es) => sp(loc, N::FunctionBody_::Defined(sequence(context, es))),
    }
}

fn function_acquires(
    context: &mut Context,
    eacquires: Vec<E::ModuleAccess>,
) -> BTreeMap<StructName, Loc> {
    let mut acquires = BTreeMap::new();
    for eacquire in eacquires {
        let new_loc = eacquire.loc;
        let sn = match acquires_type(context, eacquire) {
            None => continue,
            Some(sn) => sn,
        };
        if let Some(old_loc) = acquires.insert(sn, new_loc) {
            context.env.add_diag(diag!(
                Declarations::DuplicateItem,
                (new_loc, "Duplicate acquires item"),
                (old_loc, "Item previously listed here"),
            ))
        }
    }
    acquires
}

fn acquires_type(context: &mut Context, sp!(loc, en_): E::ModuleAccess) -> Option<StructName> {
    use ResolvedType as RT;
    use E::ModuleAccess_ as EN;
    match en_ {
        EN::Name(n) => {
            let case = match context.resolve_unscoped_type(&n)? {
                RT::BuiltinType => "builtin type",
                RT::TParam(_, _) => "type parameter",
            };
            let msg = format!(
                "Invalid acquires item. Expected a struct name, but got a {}",
                case
            );
            context
                .env
                .add_diag(diag!(NameResolution::NamePositionMismatch, (loc, msg)));
            None
        }
        EN::ModuleAccess(m, n) => {
            let (decl_loc, _, abilities, _) = context.resolve_module_type(loc, &m, &n)?;
            acquires_type_struct(context, loc, decl_loc, m, StructName(n), &abilities)
        }
    }
}

fn acquires_type_struct(
    context: &mut Context,
    loc: Loc,
    decl_loc: Loc,
    declared_module: ModuleIdent,
    n: StructName,
    abilities: &AbilitySet,
) -> Option<StructName> {
    let declared_in_current = match &context.current_module {
        Some(current_module) => current_module == &declared_module,
        None => false,
    };

    let mut has_errors = false;

    if !abilities.has_ability_(Ability_::Key) {
        let msg = format!(
            "Invalid acquires item. Expected a struct with the '{}' ability.",
            Ability_::KEY
        );
        let decl_msg = format!("Declared without the '{}' ability here", Ability_::KEY);
        context.env.add_diag(diag!(
            Declarations::InvalidAcquiresItem,
            (loc, msg),
            (decl_loc, decl_msg),
        ));
        has_errors = true;
    }

    if !declared_in_current {
        let tmsg = format!(
            "The struct '{}' was not declared in the current module. Global storage access is \
             internal to the module'",
            n
        );
        context.env.add_diag(diag!(
            Declarations::InvalidAcquiresItem,
            (loc, "Invalid acquires item"),
            (decl_loc, tmsg),
        ));
        has_errors = true;
    }

    if has_errors {
        None
    } else {
        Some(n)
    }
}

//**************************************************************************************************
// Structs
//**************************************************************************************************

fn struct_def(
    context: &mut Context,
    _name: StructName,
    sdef: E::StructDefinition,
) -> N::StructDefinition {
    let E::StructDefinition {
        warning_filter,
        index,
        attributes,
        loc: _loc,
        abilities,
        type_parameters,
        fields,
    } = sdef;
    context.env.add_warning_filter_scope(warning_filter.clone());
    let type_parameters = struct_type_parameters(context, type_parameters);
    let fields = struct_fields(context, fields);
    context.env.pop_warning_filter_scope();
    N::StructDefinition {
        warning_filter,
        index,
        attributes,
        abilities,
        type_parameters,
        fields,
    }
}

fn struct_fields(context: &mut Context, efields: E::StructFields) -> N::StructFields {
    match efields {
        E::StructFields::Native(loc) => N::StructFields::Native(loc),
        E::StructFields::Defined(em) => {
            N::StructFields::Defined(em.map(|_f, (idx, t)| (idx, type_(context, t))))
        }
    }
}

//**************************************************************************************************
// Constants
//**************************************************************************************************

fn constant(context: &mut Context, _name: ConstantName, econstant: E::Constant) -> N::Constant {
    let E::Constant {
        warning_filter,
        index,
        attributes,
        loc,
        signature: esignature,
        value: evalue,
    } = econstant;
    assert!(context.local_scopes.is_empty());
    assert!(context.local_count.is_empty());
    assert!(context.used_locals.is_empty());
    context.env.add_warning_filter_scope(warning_filter.clone());
    context.local_scopes = vec![BTreeMap::new()];
    let signature = type_(context, esignature);
    let value = exp_(context, evalue);
    context.local_scopes = vec![];
    context.local_count = BTreeMap::new();
    context.used_locals = BTreeSet::new();
    context.env.pop_warning_filter_scope();
    N::Constant {
        warning_filter,
        index,
        attributes,
        loc,
        signature,
        value,
    }
}

//**************************************************************************************************
// Types
//**************************************************************************************************

fn fun_type_parameters(
    context: &mut Context,
    type_parameters: Vec<(Name, AbilitySet)>,
) -> Vec<N::TParam> {
    let mut unique_tparams = UniqueMap::new();
    type_parameters
        .into_iter()
        .map(|(name, abilities)| type_parameter(context, &mut unique_tparams, name, abilities))
        .collect()
}

fn struct_type_parameters(
    context: &mut Context,
    type_parameters: Vec<E::StructTypeParameter>,
) -> Vec<N::StructTypeParameter> {
    let mut unique_tparams = UniqueMap::new();
    type_parameters
        .into_iter()
        .map(|param| {
            let is_phantom = param.is_phantom;
            let param = type_parameter(context, &mut unique_tparams, param.name, param.constraints);
            N::StructTypeParameter { param, is_phantom }
        })
        .collect()
}

fn type_parameter(
    context: &mut Context,
    unique_tparams: &mut UniqueMap<Name, ()>,
    name: Name,
    abilities: AbilitySet,
) -> N::TParam {
    let id = N::TParamID::next();
    let user_specified_name = name;
    let tp = N::TParam {
        id,
        user_specified_name,
        abilities,
    };
    let loc = name.loc;
    context.bind_type(name.value, ResolvedType::TParam(loc, tp.clone()));
    if let Err((name, old_loc)) = unique_tparams.add(name, ()) {
        let msg = format!("Duplicate type parameter declared with name '{}'", name);
        context.env.add_diag(diag!(
            Declarations::DuplicateItem,
            (loc, msg),
            (old_loc, "Type parameter previously defined here"),
        ))
    }
    tp
}

fn types(context: &mut Context, tys: Vec<E::Type>) -> Vec<N::Type> {
    tys.into_iter().map(|t| type_(context, t)).collect()
}

fn type_(context: &mut Context, sp!(loc, ety_): E::Type) -> N::Type {
    use ResolvedType as RT;
    use E::{ModuleAccess_ as EN, Type_ as ET};
    use N::{TypeName_ as NN, Type_ as NT};
    let ty_ = match ety_ {
        ET::Unit => NT::Unit,
        ET::Multiple(tys) => {
            NT::multiple_(loc, tys.into_iter().map(|t| type_(context, t)).collect())
        }
        ET::Ref(mut_, inner) => NT::Ref(mut_, Box::new(type_(context, *inner))),
        ET::UnresolvedError => {
            assert!(context.env.has_errors());
            NT::UnresolvedError
        }
        ET::Apply(sp!(_, EN::Name(n)), tys) => match context.resolve_unscoped_type(&n) {
            None => {
                assert!(context.env.has_errors());
                NT::UnresolvedError
            }
            Some(RT::BuiltinType) => {
                let bn_ = N::BuiltinTypeName_::resolve(&n.value).unwrap();
                let name_f = || format!("{}", &bn_);
                let arity = bn_.tparam_constraints(loc).len();
                let tys = types(context, tys);
                let tys = check_type_argument_arity(context, loc, name_f, tys, arity);
                NT::builtin_(sp(loc, bn_), tys)
            }
            Some(RT::TParam(_, tp)) => {
                if !tys.is_empty() {
                    context.env.add_diag(diag!(
                        NameResolution::NamePositionMismatch,
                        (loc, "Generic type parameters cannot take type arguments"),
                    ));
                    NT::UnresolvedError
                } else {
                    if context.translating_fun {
                        context.used_fun_tparams.insert(tp.id);
                    }
                    NT::Param(tp)
                }
            }
        },
        ET::Apply(sp!(nloc, EN::ModuleAccess(m, n)), tys) => {
            match context.resolve_module_type(nloc, &m, &n) {
                None => {
                    assert!(context.env.has_errors());
                    NT::UnresolvedError
                }
                Some((_, _, _, arity)) => {
                    let tn = sp(nloc, NN::ModuleType(m, StructName(n)));
                    let tys = types(context, tys);
                    let name_f = || format!("{}", tn);
                    let tys = check_type_argument_arity(context, loc, name_f, tys, arity);
                    NT::Apply(None, tn, tys)
                }
            }
        }
        ET::Fun(_, _) => panic!("ICE only allowed in spec context"),
    };
    sp(loc, ty_)
}

fn check_type_argument_arity<F: FnOnce() -> String>(
    context: &mut Context,
    loc: Loc,
    name_f: F,
    mut ty_args: Vec<N::Type>,
    arity: usize,
) -> Vec<N::Type> {
    let args_len = ty_args.len();
    if args_len != arity {
        let diag_code = if args_len > arity {
            NameResolution::TooManyTypeArguments
        } else {
            NameResolution::TooFewTypeArguments
        };
        let msg = format!(
            "Invalid instantiation of '{}'. Expected {} type argument(s) but got {}",
            name_f(),
            arity,
            args_len
        );
        context.env.add_diag(diag!(diag_code, (loc, msg)));
    }

    while ty_args.len() > arity {
        ty_args.pop();
    }

    while ty_args.len() < arity {
        ty_args.push(sp(loc, N::Type_::UnresolvedError))
    }

    ty_args
}

//**************************************************************************************************
// Exp
//**************************************************************************************************

fn sequence(context: &mut Context, seq: E::Sequence) -> N::Sequence {
    context.new_local_scope();
    let nseq = seq.into_iter().map(|s| sequence_item(context, s)).collect();
    context.close_local_scope();
    nseq
}

fn sequence_item(context: &mut Context, sp!(loc, ns_): E::SequenceItem) -> N::SequenceItem {
    use E::SequenceItem_ as ES;
    use N::SequenceItem_ as NS;

    let s_ = match ns_ {
        ES::Seq(e) => NS::Seq(exp_(context, e)),
        ES::Declare(b, ty_opt) => {
            let bind_opt = bind_list(context, b);
            let tys = ty_opt.map(|t| type_(context, t));
            match bind_opt {
                None => {
                    assert!(context.env.has_errors());
                    NS::Seq(sp(loc, N::Exp_::UnresolvedError))
                }
                Some(bind) => NS::Declare(bind, tys),
            }
        }
        ES::Bind(b, e) => {
            let e = exp_(context, e);
            let bind_opt = bind_list(context, b);
            match bind_opt {
                None => {
                    assert!(context.env.has_errors());
                    NS::Seq(sp(loc, N::Exp_::UnresolvedError))
                }
                Some(bind) => NS::Bind(bind, e),
            }
        }
    };
    sp(loc, s_)
}

fn call_args(context: &mut Context, sp!(loc, es): Spanned<Vec<E::Exp>>) -> Spanned<Vec<N::Exp>> {
    sp(loc, exps(context, es))
}

fn exps(context: &mut Context, es: Vec<E::Exp>) -> Vec<N::Exp> {
    es.into_iter().map(|e| exp_(context, e)).collect()
}

fn exp(context: &mut Context, e: E::Exp) -> Box<N::Exp> {
    Box::new(exp_(context, e))
}

fn exp_(context: &mut Context, e: E::Exp) -> N::Exp {
    use E::Exp_ as EE;
    use N::Exp_ as NE;
    let sp!(eloc, e_) = e;
    let ne_ = match e_ {
        EE::Unit { trailing } => NE::Unit { trailing },
        EE::Value(val) => NE::Value(val),
        EE::Move(v) => match context.resolve_local(eloc, "move", v.0) {
            None => {
                debug_assert!(context.env.has_errors());
                NE::UnresolvedError
            }
            Some(nv) => NE::Move(nv),
        },
        EE::Copy(v) => match context.resolve_local(eloc, "copy", v.0) {
            None => {
                debug_assert!(context.env.has_errors());
                NE::UnresolvedError
            }
            Some(nv) => NE::Copy(nv),
        },
        EE::Name(sp!(aloc, E::ModuleAccess_::Name(v)), None) => {
            if is_constant_name(&v.value) {
                access_constant(context, sp(aloc, E::ModuleAccess_::Name(v)))
            } else {
                match context.resolve_local(eloc, "variable usage", v) {
                    None => {
                        debug_assert!(context.env.has_errors());
                        NE::UnresolvedError
                    }
                    Some(nv) => NE::Use(nv),
                }
            }
        }
        EE::Name(ma, None) => access_constant(context, ma),

        EE::IfElse(eb, et, ef) => {
            NE::IfElse(exp(context, *eb), exp(context, *et), exp(context, *ef))
        }
        EE::While(eb, el) => NE::While(exp(context, *eb), exp(context, *el)),
        EE::Loop(el) => NE::Loop(exp(context, *el)),
        EE::Block(seq) => NE::Block(sequence(context, seq)),

        EE::Assign(a, e) => {
            let na_opt = assign_list(context, a);
            let ne = exp(context, *e);
            match na_opt {
                None => {
                    assert!(context.env.has_errors());
                    NE::UnresolvedError
                }
                Some(na) => NE::Assign(na, ne),
            }
        }
        EE::FieldMutate(edotted, er) => {
            let ndot_opt = dotted(context, *edotted);
            let ner = exp(context, *er);
            match ndot_opt {
                None => {
                    assert!(context.env.has_errors());
                    NE::UnresolvedError
                }
                Some(ndot) => NE::FieldMutate(ndot, ner),
            }
        }
        EE::Mutate(el, er) => {
            let nel = exp(context, *el);
            let ner = exp(context, *er);
            NE::Mutate(nel, ner)
        }

        EE::Return(es) => NE::Return(exp(context, *es)),
        EE::Abort(es) => NE::Abort(exp(context, *es)),
        EE::Break => NE::Break,
        EE::Continue => NE::Continue,

        EE::Dereference(e) => NE::Dereference(exp(context, *e)),
        EE::UnaryExp(uop, e) => NE::UnaryExp(uop, exp(context, *e)),
        EE::BinopExp(e1, bop, e2) => NE::BinopExp(exp(context, *e1), bop, exp(context, *e2)),

        EE::Pack(tn, etys_opt, efields) => {
            match context.resolve_struct_name(eloc, "construction", tn, etys_opt) {
                None => {
                    assert!(context.env.has_errors());
                    NE::UnresolvedError
                }
                Some((m, sn, tys_opt)) => NE::Pack(
                    m,
                    sn,
                    tys_opt,
                    efields.map(|_, (idx, e)| (idx, exp_(context, e))),
                ),
            }
        }
        EE::ExpList(es) => {
            assert!(es.len() > 1);
            NE::ExpList(exps(context, es))
        }

        EE::Borrow(mut_, inner) => match *inner {
            sp!(_, EE::ExpDotted(edot)) => match dotted(context, *edot) {
                None => {
                    assert!(context.env.has_errors());
                    NE::UnresolvedError
                }
                Some(d) => NE::Borrow(mut_, d),
            },
            e => {
                let ne = exp(context, e);
                NE::Borrow(mut_, sp(ne.loc, N::ExpDotted_::Exp(ne)))
            }
        },

        EE::ExpDotted(edot) => match dotted(context, *edot) {
            None => {
                assert!(context.env.has_errors());
                NE::UnresolvedError
            }
            Some(d) => NE::DerefBorrow(d),
        },

        EE::Cast(e, t) => NE::Cast(exp(context, *e), type_(context, t)),
        EE::Annotate(e, t) => NE::Annotate(exp(context, *e), type_(context, t)),

        EE::Call(sp!(mloc, ma_), true, tys_opt, rhs) => {
            use E::ModuleAccess_ as EA;
            use N::BuiltinFunction_ as BF;
            assert!(tys_opt.is_none(), "ICE macros do not have type arguments");
            let nes = call_args(context, rhs);
            match ma_ {
                EA::Name(n) if n.value.as_str() == BF::ASSERT_MACRO => {
                    NE::Builtin(sp(mloc, BF::Assert(true)), nes)
                }
                ma_ => {
                    context.env.add_diag(diag!(
                        NameResolution::UnboundMacro,
                        (mloc, format!("Unbound macro '{}'", ma_)),
                    ));
                    NE::UnresolvedError
                }
            }
        }
        EE::Call(sp!(mloc, ma_), false, tys_opt, rhs) => {
            use E::ModuleAccess_ as EA;
            let ty_args = tys_opt.map(|tys| types(context, tys));
            let nes = call_args(context, rhs);
            match ma_ {
                EA::Name(n) if N::BuiltinFunction_::all_names().contains(&n.value) => {
                    match resolve_builtin_function(context, eloc, &n, ty_args) {
                        None => {
                            assert!(context.env.has_errors());
                            NE::UnresolvedError
                        }
                        Some(f) => NE::Builtin(sp(mloc, f), nes),
                    }
                }

                EA::Name(n) => {
                    context.env.add_diag(diag!(
                        NameResolution::UnboundUnscopedName,
                        (n.loc, format!("Unbound function '{}' in current scope", n)),
                    ));
                    NE::UnresolvedError
                }
                EA::ModuleAccess(m, n) => match context.resolve_module_function(mloc, &m, &n) {
                    None => {
                        assert!(context.env.has_errors());
                        NE::UnresolvedError
                    }
                    Some(_) => NE::ModuleCall(m, FunctionName(n), ty_args, nes),
                },
            }
        }
        EE::Vector(vec_loc, tys_opt, rhs) => {
            let ty_args = tys_opt.map(|tys| types(context, tys));
            let nes = call_args(context, rhs);
            let ty_opt = check_builtin_ty_args_impl(
                context,
                vec_loc,
                || "Invalid 'vector' instantation".to_string(),
                eloc,
                1,
                ty_args,
            )
            .map(|mut v| {
                assert!(v.len() == 1);
                v.pop().unwrap()
            });
            NE::Vector(vec_loc, ty_opt, nes)
        }

        EE::Spec(u, unbound_names) => {
            // Vars currently aren't shadowable by types/functions
            let used_locals = unbound_names
                .into_iter()
                .filter_map(|v| {
                    if context.local_scopes.last()?.contains_key(&v.value) {
                        let nv = context
                            .resolve_local(v.loc, "ICE should always resolve", v)
                            .unwrap();
                        Some(nv)
                    } else {
                        None
                    }
                })
                .collect();
            NE::Spec(u, used_locals)
        }
        EE::UnresolvedError => {
            assert!(context.env.has_errors());
            NE::UnresolvedError
        }
        // `Name` matches name variants only allowed in specs (we handle the allowed ones above)
        EE::Index(..) | EE::Lambda(..) | EE::Quant(..) | EE::Name(_, Some(_)) => {
            panic!("ICE unexpected specification construct")
        }
    };
    sp(eloc, ne_)
}

fn access_constant(context: &mut Context, ma: E::ModuleAccess) -> N::Exp_ {
    match context.resolve_constant(ma) {
        None => {
            assert!(context.env.has_errors());
            N::Exp_::UnresolvedError
        }
        Some((m, c)) => N::Exp_::Constant(m, c),
    }
}

fn dotted(context: &mut Context, edot: E::ExpDotted) -> Option<N::ExpDotted> {
    let sp!(loc, edot_) = edot;
    let nedot_ = match edot_ {
        E::ExpDotted_::Exp(e) => {
            let ne = exp(context, e);
            match &ne.value {
                N::Exp_::UnresolvedError => return None,
                _ => N::ExpDotted_::Exp(ne),
            }
        }
        E::ExpDotted_::Dot(d, f) => N::ExpDotted_::Dot(Box::new(dotted(context, *d)?), Field(f)),
    };
    Some(sp(loc, nedot_))
}

#[derive(Clone, Copy)]
enum LValueCase {
    Bind,
    Assign,
}

fn lvalue(
    context: &mut Context,
    seen_locals: &mut UniqueMap<Name, ()>,
    case: LValueCase,
    sp!(loc, l_): E::LValue,
) -> Option<N::LValue> {
    use LValueCase as C;
    use E::LValue_ as EL;
    use N::LValue_ as NL;
    let nl_ = match l_ {
        EL::Var(sp!(_, E::ModuleAccess_::Name(n)), None) => {
            let v = P::Var(n);
            if v.is_underscore() {
                NL::Ignore
            } else {
                if let Err((var, prev_loc)) = seen_locals.add(n, ()) {
                    let (primary, secondary) = match case {
                        C::Bind => {
                            let msg = format!(
                                "Duplicate declaration for local '{}' in a given 'let'",
                                &var
                            );
                            ((var.loc, msg), (prev_loc, "Previously declared here"))
                        }
                        C::Assign => {
                            let msg = format!(
                                "Duplicate usage of local '{}' in a given assignment",
                                &var
                            );
                            ((var.loc, msg), (prev_loc, "Previously assigned here"))
                        }
                    };
                    context
                        .env
                        .add_diag(diag!(Declarations::DuplicateItem, primary, secondary));
                }
                let nv = match case {
                    C::Bind => {
                        let is_parameter = false;
                        context.declare_local(is_parameter, n)
                    }
                    C::Assign => context.resolve_local(loc, "assignment", n)?,
                };
                NL::Var {
                    var: nv,
                    // set later
                    unused_binding: false,
                }
            }
        }
        EL::Unpack(tn, etys_opt, efields) => {
            let msg = match case {
                C::Bind => "deconstructing binding",
                C::Assign => "deconstructing assignment",
            };
            let (m, sn, tys_opt) = context.resolve_struct_name(loc, msg, tn, etys_opt)?;
            let nfields =
                UniqueMap::maybe_from_opt_iter(efields.into_iter().map(|(k, (idx, inner))| {
                    Some((k, (idx, lvalue(context, seen_locals, case, inner)?)))
                }))?;
            NL::Unpack(
                m,
                sn,
                tys_opt,
                nfields.expect("ICE fields were already unique"),
            )
        }
        EL::Var(_, _) => panic!("unexpected specification construct"),
    };
    Some(sp(loc, nl_))
}

fn bind_list(context: &mut Context, ls: E::LValueList) -> Option<N::LValueList> {
    lvalue_list(context, &mut UniqueMap::new(), LValueCase::Bind, ls)
}

fn assign_list(context: &mut Context, ls: E::LValueList) -> Option<N::LValueList> {
    lvalue_list(context, &mut UniqueMap::new(), LValueCase::Assign, ls)
}

fn lvalue_list(
    context: &mut Context,
    seen_locals: &mut UniqueMap<Name, ()>,
    case: LValueCase,
    sp!(loc, b_): E::LValueList,
) -> Option<N::LValueList> {
    Some(sp(
        loc,
        b_.into_iter()
            .map(|inner| lvalue(context, seen_locals, case, inner))
            .collect::<Option<_>>()?,
    ))
}

fn resolve_builtin_function(
    context: &mut Context,
    loc: Loc,
    b: &Name,
    ty_args: Option<Vec<N::Type>>,
) -> Option<N::BuiltinFunction_> {
    use N::{BuiltinFunction_ as B, BuiltinFunction_::*};
    Some(match b.value.as_str() {
        B::MOVE_TO => MoveTo(check_builtin_ty_arg(context, loc, b, ty_args)),
        B::MOVE_FROM => MoveFrom(check_builtin_ty_arg(context, loc, b, ty_args)),
        B::BORROW_GLOBAL => BorrowGlobal(false, check_builtin_ty_arg(context, loc, b, ty_args)),
        B::BORROW_GLOBAL_MUT => BorrowGlobal(true, check_builtin_ty_arg(context, loc, b, ty_args)),
        B::EXISTS => Exists(check_builtin_ty_arg(context, loc, b, ty_args)),
        B::FREEZE => Freeze(check_builtin_ty_arg(context, loc, b, ty_args)),
        B::ASSERT_MACRO => {
            let dep_msg = format!(
                "'{}' function syntax has been deprecated and will be removed",
                B::ASSERT_MACRO
            );
            // TODO make this a tip/hint?
            let help_msg = format!(
                "Replace with '{0}!'. '{0}' has been replaced with a '{0}!' built-in macro so \
                 that arguments are no longer eagerly evaluated",
                B::ASSERT_MACRO
            );
            context.env.add_diag(diag!(
                Uncategorized::DeprecatedWillBeRemoved,
                (b.loc, dep_msg),
                (b.loc, help_msg),
            ));
            check_builtin_ty_args(context, loc, b, 0, ty_args);
            Assert(false)
        }
        _ => {
            context.env.add_diag(diag!(
                NameResolution::UnboundUnscopedName,
                (b.loc, format!("Unbound function: '{}'", b)),
            ));
            return None;
        }
    })
}

fn check_builtin_ty_arg(
    context: &mut Context,
    loc: Loc,
    b: &Name,
    ty_args: Option<Vec<N::Type>>,
) -> Option<N::Type> {
    let res = check_builtin_ty_args(context, loc, b, 1, ty_args);
    res.map(|mut v| {
        assert!(v.len() == 1);
        v.pop().unwrap()
    })
}

fn check_builtin_ty_args(
    context: &mut Context,
    loc: Loc,
    b: &Name,
    arity: usize,
    ty_args: Option<Vec<N::Type>>,
) -> Option<Vec<N::Type>> {
    check_builtin_ty_args_impl(
        context,
        b.loc,
        || format!("Invalid call to builtin function: '{}'", b),
        loc,
        arity,
        ty_args,
    )
}

fn check_builtin_ty_args_impl(
    context: &mut Context,
    msg_loc: Loc,
    fmsg: impl Fn() -> String,
    targs_loc: Loc,
    arity: usize,
    ty_args: Option<Vec<N::Type>>,
) -> Option<Vec<N::Type>> {
    let mut msg_opt = None;
    ty_args.map(|mut args| {
        let args_len = args.len();
        if args_len != arity {
            let diag_code = if args_len > arity {
                NameResolution::TooManyTypeArguments
            } else {
                NameResolution::TooFewTypeArguments
            };
            let msg = msg_opt.get_or_insert_with(fmsg);
            let targs_msg = format!("Expected {} type argument(s) but got {}", arity, args_len);
            context
                .env
                .add_diag(diag!(diag_code, (msg_loc, msg), (targs_loc, targs_msg)));
        }

        while args.len() > arity {
            args.pop();
        }

        while args.len() < arity {
            args.push(sp(targs_loc, N::Type_::UnresolvedError));
        }

        args
    })
}

//**************************************************************************************************
// Unused locals
//**************************************************************************************************

fn remove_unused_bindings_function(
    context: &mut Context,
    used: &BTreeSet<N::Var_>,
    f: &mut N::Function,
) {
    match &mut f.body.value {
        N::FunctionBody_::Defined(seq) => remove_unused_bindings_seq(context, used, seq),
        // no warnings for natives
        N::FunctionBody_::Native => return,
    }
    for (v, _) in &mut f.signature.parameters {
        if !used.contains(&v.value) {
            report_unused_local(context, v);
        }
    }
}

fn remove_unused_bindings_seq(
    context: &mut Context,
    used: &BTreeSet<N::Var_>,
    seq: &mut N::Sequence,
) {
    for sp!(_, item_) in seq {
        match item_ {
            N::SequenceItem_::Seq(e) => remove_unused_bindings_exp(context, used, e),
            N::SequenceItem_::Declare(lvalues, _) => {
                // unused bindings will be reported as unused assignments
                remove_unused_bindings_lvalues(
                    context, used, lvalues, /* report unused */ true,
                )
            }
            N::SequenceItem_::Bind(lvalues, e) => {
                remove_unused_bindings_lvalues(
                    context, used, lvalues, /* report unused */ false,
                );
                remove_unused_bindings_exp(context, used, e)
            }
        }
    }
}

fn remove_unused_bindings_lvalues(
    context: &mut Context,
    used: &BTreeSet<N::Var_>,
    sp!(_, lvalues): &mut N::LValueList,
    report: bool,
) {
    for lvalue in lvalues {
        remove_unused_bindings_lvalue(context, used, lvalue, report)
    }
}

fn remove_unused_bindings_lvalue(
    context: &mut Context,
    used: &BTreeSet<N::Var_>,
    sp!(_, lvalue_): &mut N::LValue,
    report: bool,
) {
    match lvalue_ {
        N::LValue_::Ignore => (),
        N::LValue_::Var {
            var,
            unused_binding,
        } if used.contains(&var.value) => {
            debug_assert!(!*unused_binding);
        }
        N::LValue_::Var {
            var,
            unused_binding,
        } => {
            debug_assert!(!*unused_binding);
            if report {
                report_unused_local(context, var);
            }
            *unused_binding = true;
        }
        N::LValue_::Unpack(_, _, _, lvalues) => {
            for (_, _, (_, lvalue)) in lvalues {
                remove_unused_bindings_lvalue(context, used, lvalue, report)
            }
        }
    }
}

fn remove_unused_bindings_exp(
    context: &mut Context,
    used: &BTreeSet<N::Var_>,
    sp!(_, e_): &mut N::Exp,
) {
    match e_ {
        N::Exp_::Value(_)
        | N::Exp_::Move(_)
        | N::Exp_::Copy(_)
        | N::Exp_::Use(_)
        | N::Exp_::Constant(_, _)
        | N::Exp_::Break
        | N::Exp_::Continue
        | N::Exp_::Unit { .. }
        | N::Exp_::Spec(_, _)
        | N::Exp_::UnresolvedError => (),
        N::Exp_::Return(e)
        | N::Exp_::Abort(e)
        | N::Exp_::Dereference(e)
        | N::Exp_::UnaryExp(_, e)
        | N::Exp_::Cast(e, _)
        | N::Exp_::Assign(_, e)
        | N::Exp_::Loop(e)
        | N::Exp_::Annotate(e, _) => remove_unused_bindings_exp(context, used, e),
        N::Exp_::IfElse(econd, et, ef) => {
            remove_unused_bindings_exp(context, used, econd);
            remove_unused_bindings_exp(context, used, et);
            remove_unused_bindings_exp(context, used, ef);
        }
        N::Exp_::While(econd, ebody) => {
            remove_unused_bindings_exp(context, used, econd);
            remove_unused_bindings_exp(context, used, ebody)
        }
        N::Exp_::Block(s) => remove_unused_bindings_seq(context, used, s),
        N::Exp_::FieldMutate(ed, e) => {
            remove_unused_bindings_exp_dotted(context, used, ed);
            remove_unused_bindings_exp(context, used, e)
        }
        N::Exp_::Mutate(el, er) | N::Exp_::BinopExp(el, _, er) => {
            remove_unused_bindings_exp(context, used, el);
            remove_unused_bindings_exp(context, used, er)
        }
        N::Exp_::Pack(_, _, _, fields) => {
            for (_, _, (_, e)) in fields {
                remove_unused_bindings_exp(context, used, e)
            }
        }
        N::Exp_::Builtin(_, sp!(_, es))
        | N::Exp_::Vector(_, _, sp!(_, es))
        | N::Exp_::ModuleCall(_, _, _, sp!(_, es))
        | N::Exp_::ExpList(es) => {
            for e in es {
                remove_unused_bindings_exp(context, used, e)
            }
        }

        N::Exp_::DerefBorrow(ed) | N::Exp_::Borrow(_, ed) => {
            remove_unused_bindings_exp_dotted(context, used, ed)
        }
    }
}

fn remove_unused_bindings_exp_dotted(
    context: &mut Context,
    used: &BTreeSet<N::Var_>,
    sp!(_, ed_): &mut N::ExpDotted,
) {
    match ed_ {
        N::ExpDotted_::Exp(e) => remove_unused_bindings_exp(context, used, e),
        N::ExpDotted_::Dot(ed, _) => remove_unused_bindings_exp_dotted(context, used, ed),
    }
}

fn report_unused_local(context: &mut Context, sp!(loc, unused_): &N::Var) {
    if !unused_.name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return;
    }
    let N::Var_ { name, id, color } = unused_;
    debug_assert!(*color == 0);
    let is_parameter = *id == 0;
    let kind = if is_parameter {
        "parameter"
    } else {
        "local variable"
    };
    let msg = format!(
        "Unused {kind} '{name}'. Consider removing or prefixing with an underscore: '_{name}'",
    );
    context
        .env
        .add_diag(diag!(UnusedItem::Variable, (*loc, msg)));
}

//**************************************************************************************************
// Specs
//**************************************************************************************************

fn spec_blocks<'a>(
    used: &mut BTreeSet<(ModuleIdent, Neighbor)>,
    specs: impl IntoIterator<Item = &'a E::SpecBlock>,
) {
    for spec in specs {
        spec_block(used, spec)
    }
}

fn spec_block(used: &mut BTreeSet<(ModuleIdent, Neighbor)>, sp!(_, sb_): &E::SpecBlock) {
    sb_.members
        .iter()
        .for_each(|sbm| spec_block_member(used, sbm))
}

fn spec_block_member(
    used: &mut BTreeSet<(ModuleIdent, Neighbor)>,
    sp!(_, sbm_): &E::SpecBlockMember,
) {
    use E::SpecBlockMember_ as M;
    match sbm_ {
        M::Condition {
            exp: e,
            additional_exps: es,
            ..
        } => {
            spec_exp(used, e);
            es.iter().for_each(|e| spec_exp(used, e))
        }
        M::Function { body, .. } => {
            if let E::FunctionBody_::Defined(seq) = &body.value {
                spec_sequence(used, seq)
            }
        }
        M::Let { def: e, .. } | M::Include { exp: e, .. } | M::Apply { exp: e, .. } => {
            spec_exp(used, e)
        }
        M::Update { lhs, rhs } => {
            spec_exp(used, lhs);
            spec_exp(used, rhs);
        }
        // A special treatment to the `pragma friend` declarations.
        //
        // The `pragma friend = <address::module_name::function_name>` notion exists before the
        // `friend` feature is implemented as a language feature. And it may still have a use case,
        // that is, to friend a module that is compiled with other modules but not published.
        //
        // To illustrate, suppose we have module `A` and `B` compiled and proved together locally,
        // but for some reason, module `A` is not published on-chain. In this case, we cannot
        // declare `friend A;` in module `B` because that will lead to a linking error (the loader
        // is unable to find module `A`). But the prover side still needs to know that `A` is a
        // friend of `B` (e.g., to verify global invariants). So, the `pragma friend = ...` syntax
        // might need to stay for this purpose. And for that, we need to add the module that is
        // declared as a friend in the `immediate_neighbors`.
        M::Pragma { properties } => {
            for prop in properties {
                let pragma = &prop.value;
                if pragma.name.value.as_str() == "friend" {
                    match &pragma.value {
                        None => (),
                        Some(E::PragmaValue::Literal(_)) => (),
                        Some(E::PragmaValue::Ident(maccess)) => match &maccess.value {
                            E::ModuleAccess_::Name(_) => (),
                            E::ModuleAccess_::ModuleAccess(mident, _) => {
                                used.insert((*mident, sp(maccess.loc, Neighbor_::Friend)));
                            }
                        },
                    }
                }
            }
        }
        M::Variable { .. } => (),
    }
}

fn spec_sequence(used: &mut BTreeSet<(ModuleIdent, Neighbor)>, seq: &E::Sequence) {
    for item in seq {
        spec_sequence_item(used, item)
    }
}

fn spec_sequence_item(
    used: &mut BTreeSet<(ModuleIdent, Neighbor)>,
    sp!(_, item_): &E::SequenceItem,
) {
    match item_ {
        E::SequenceItem_::Declare(lvs, _) => spec_lvalues(used, lvs),
        E::SequenceItem_::Bind(lvs, e) => {
            spec_lvalues(used, lvs);
            spec_exp(used, e);
        }
        E::SequenceItem_::Seq(e) => spec_exp(used, e),
    }
}

fn spec_lvalues(used: &mut BTreeSet<(ModuleIdent, Neighbor)>, sp!(_, lvs_): &E::LValueList) {
    for lv in lvs_ {
        spec_lvalue(used, lv)
    }
}

fn spec_lvalue(used: &mut BTreeSet<(ModuleIdent, Neighbor)>, sp!(_, lv_): &E::LValue) {
    match lv_ {
        E::LValue_::Var(m, tys_opt) => {
            spec_module_access(used, m);
            if let Some(tys) = tys_opt {
                spec_types(used, tys)
            }
        }
        E::LValue_::Unpack(m, tys_opt, fields) => {
            spec_module_access(used, m);
            if let Some(tys) = tys_opt {
                spec_types(used, tys)
            }
            for (_, _, (_, field_lv)) in fields {
                spec_lvalue(used, field_lv)
            }
        }
    }
}

fn spec_types(used: &mut BTreeSet<(ModuleIdent, Neighbor)>, tys: &[E::Type]) {
    for ty in tys {
        spec_type(used, ty)
    }
}

fn spec_type(used: &mut BTreeSet<(ModuleIdent, Neighbor)>, sp!(_, ty_): &E::Type) {
    match ty_ {
        E::Type_::Unit | E::Type_::UnresolvedError => (),
        E::Type_::Multiple(tys) => spec_types(used, tys),
        E::Type_::Apply(ma, tys) => {
            spec_module_access(used, ma);
            spec_types(used, tys)
        }
        E::Type_::Ref(_, inner) => spec_type(used, inner),
        E::Type_::Fun(ty_params, ty_ret) => {
            spec_types(used, ty_params);
            spec_type(used, ty_ret);
        }
    }
}

fn spec_module_access(
    used: &mut BTreeSet<(ModuleIdent, Neighbor)>,
    sp!(loc, ma_): &E::ModuleAccess,
) {
    match ma_ {
        E::ModuleAccess_::Name(_) => (),
        E::ModuleAccess_::ModuleAccess(m, _) => {
            used.insert((*m, sp(*loc, Neighbor_::Dependency)));
        }
    }
}

fn spec_exp(used: &mut BTreeSet<(ModuleIdent, Neighbor)>, sp!(_, e_): &E::Exp) {
    match e_ {
        E::Exp_::Value(_)
        | E::Exp_::Move(_)
        | E::Exp_::Copy(_)
        | E::Exp_::Break
        | E::Exp_::Continue
        | E::Exp_::Unit { .. }
        | E::Exp_::Spec(_, _)
        | E::Exp_::UnresolvedError => (),

        E::Exp_::Loop(einner)
        | E::Exp_::Return(einner)
        | E::Exp_::Abort(einner)
        | E::Exp_::Dereference(einner)
        | E::Exp_::UnaryExp(_, einner)
        | E::Exp_::Borrow(_, einner) => spec_exp(used, einner),

        E::Exp_::Mutate(el, er) | E::Exp_::BinopExp(el, _, er) | E::Exp_::Index(el, er) => {
            spec_exp(used, el);
            spec_exp(used, er)
        }

        E::Exp_::Name(ma, tys_opt) => {
            spec_module_access(used, ma);
            if let Some(tys) = tys_opt {
                spec_types(used, tys)
            }
        }
        E::Exp_::Call(ma, _, tys_opt, sp!(_, args_)) => {
            spec_module_access(used, ma);
            if let Some(tys) = tys_opt {
                spec_types(used, tys)
            }
            for arg in args_ {
                spec_exp(used, arg)
            }
        }
        E::Exp_::Pack(ma, tys_opt, fields) => {
            spec_module_access(used, ma);
            if let Some(tys) = tys_opt {
                spec_types(used, tys)
            }
            for (_, _, (_, arg)) in fields {
                spec_exp(used, arg)
            }
        }
        E::Exp_::Vector(_, tys_opt, sp!(_, args_)) => {
            if let Some(tys) = tys_opt {
                spec_types(used, tys)
            }
            for arg in args_ {
                spec_exp(used, arg)
            }
        }
        E::Exp_::IfElse(econd, etrue, efalse) => {
            spec_exp(used, econd);
            spec_exp(used, etrue);
            spec_exp(used, efalse);
        }
        E::Exp_::While(econd, ebody) => {
            spec_exp(used, econd);
            spec_exp(used, ebody)
        }
        E::Exp_::Block(seq) => spec_sequence(used, seq),
        E::Exp_::Lambda(lvs, ebody) => {
            spec_lvalues(used, lvs);
            spec_exp(used, ebody)
        }
        E::Exp_::Quant(_, sp!(_, lvs_es_), ess, e_opt, inner) => {
            for sp!(_, (lv, e)) in lvs_es_ {
                spec_lvalue(used, lv);
                spec_exp(used, e);
            }
            for es in ess {
                for e in es {
                    spec_exp(used, e)
                }
            }
            if let Some(e) = e_opt {
                spec_exp(used, e)
            }
            spec_exp(used, inner)
        }
        E::Exp_::Assign(lvs, er) => {
            spec_lvalues(used, lvs);
            spec_exp(used, er)
        }
        E::Exp_::FieldMutate(edotted, er) => {
            spec_exp_dotted(used, edotted);
            spec_exp(used, er)
        }

        E::Exp_::ExpList(es) => {
            for e in es {
                spec_exp(used, e)
            }
        }
        E::Exp_::ExpDotted(edotted) => spec_exp_dotted(used, edotted),
        E::Exp_::Cast(e, ty) | E::Exp_::Annotate(e, ty) => {
            spec_exp(used, e);
            spec_type(used, ty)
        }
    }
}

fn spec_exp_dotted(used: &mut BTreeSet<(ModuleIdent, Neighbor)>, sp!(_, edotted_): &E::ExpDotted) {
    match edotted_ {
        E::ExpDotted_::Exp(e) => spec_exp(used, e),
        E::ExpDotted_::Dot(edotted, _) => spec_exp_dotted(used, edotted),
    }
}
