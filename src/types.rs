//! Static type system: the type lattice, a unification engine, and a bidirectional
//! checker that infers parameter/local types from usage (`PLAN.md` Phase 3, `SPEC.md` §5).
//!
//! Consumes the [`crate::ast::Module`] and the [`crate::resolve::Resolution`] from the
//! previous phases plus a [`TypeConfig`] describing host functions and memory. Produces a
//! [`TypeInfo`] carrying the module's **export signature** (each exported name → its type),
//! or a batch of [`Diagnostics`].
//!
//! ## What is and isn't inferred (v1)
//!
//! * Numbers are a single `f64` type ([`Type::Number`]).
//! * Parameter types are inferred from how they're *used* (operators, calls, indexing), or
//!   pinned by a `---@param` annotation (§5.6). A parameter that is neither used in a
//!   type-determining way nor annotated is an error ("ambiguous, annotate").
//! * Table literals get a concrete shape: all-positional → [`Type::Array`], all-`name =`
//!   → [`Type::Record`], string-keyed → [`Type::Map`].
//! * Conditions must be `bool`; equality needs matching operand types; arithmetic/relational
//!   operators are numeric (relational also allows `string`); `..` is `string`.
//! * **`and`/`or`** yield one of their operands, which may differ in type: the result has the
//!   one type that covers each operand it can yield (`x or 0` with `x : number?` is a `number`),
//!   recorded in [`TypeInfo::logic`] for lowering. The left operand must be able to be `nil` or
//!   `false` (§3.5). A **conditional expression** (which only the Ruby surface spells) joins its
//!   two branches the same way, and its condition narrows them as an `if`'s would.
//!
//! * **Multi-value returns** produce a [`Type::Tuple`], consumable only by a matching-arity
//!   parallel binding or a pass-through `return` (§5.5); anywhere else is `E0415`.
//! * **A call to a function that returns nothing** (`()`) may be a statement or returned with
//!   `return f()`; used as a value it is `E0435` (§5.5). A function returns nothing when none of
//!   its `return`s gives a value, which is settled before any body is checked, or when it
//!   returns such a call.
//! * **Method calls** (`mem:m(...)`) are typed against the host memory's declared method
//!   signatures (§7); any other receiver is `E0417`.
//! * **EmmyLua `---@` annotations** (§5.6) are supported for top-level functions: `---@param
//!   <name> <type>` and `---@return <type>` pin a parameter/return type (parsed by
//!   [`crate::annotations`], applied in Pass 2 before bodies are checked). This is what lets a
//!   record/array-typed *parameter* be expressed.
//! * **Every path returns a value** in a function whose return type isn't optional: a bare
//!   `return`, or control reaching the closing `end`, is `E0480` (§5.5). Checked after every
//!   body, once return types are settled. A function whose [`FuncBody::fallthrough`] is
//!   [`Fallthrough::Nil`] (the Ruby surface's) returns `nil` there instead, which makes its
//!   return type optional.
//! * **Optional returns**: where the return type is `T?` — declared by `---@return`, or inferred
//!   because the function returns a literal `nil` beside a value — a `return` may give a `T` or
//!   `nil`, checked like an assignment. Decided before the body is checked, so it doesn't depend
//!   on the order of the `return`s (§5.5).
//! * **A local initialized to `nil`**, or not initialized at all, is `T?`, with `T` fixed by
//!   what is later assigned to it (§5.2), or `nil` if nothing else ever is.
//! * **Uses before the code that types a value**: a field read or write, or a call, on a value
//!   whose type is still unknown (a `local best = nil` read under a guard above the assignment
//!   that types it) stands for a fresh variable and is checked once every body has been, so the
//!   outcome doesn't depend on the order of the code. One whose value never gets a type is
//!   `E0410`.
//! * **Flow narrowing**: inside `if <place> ~= nil then`, and in the `else` branch and later
//!   `elseif` arms of `if <place> == nil then`, a `T?` place reads as `T`, where the place is a
//!   local, an upvalue, a memory root, or a field path over one (§5.3). A write to the place or
//!   a prefix of it ends the narrowing. Each narrowed read is recorded in
//!   [`TypeInfo::narrowed`] so lowering unwraps it.
//!
//! ## Deliberately deferred (documented gaps)
//!
//! * `---@type` annotations on locals.
//! * Narrowing from an early return, or from either operand of `and`/`or`.
//! * Iterating a record with `pairs` (its fields don't share a type).

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::ast::*;
use crate::diagnostics::{Diagnostic, Diagnostics, Span};
use crate::resolve::{Binding, Resolution, SymbolId};
use crate::runtime::builtins;
use crate::surface::Surface;

/// A Grindlang type (`SPEC.md` §5.1).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Type {
    Number,
    Bool,
    String,
    /// The type of the literal `nil`; only inhabits optionals.
    Nil,
    /// "No value" — the result of a `return`-less function used in value position.
    Unit,
    /// `T?`
    Optional(Box<Type>),
    /// Homogeneous, 1-based array.
    Array(Box<Type>),
    /// Homogeneous string-keyed map.
    Map(Box<Type>),
    /// Fixed, known string keys.
    Record(BTreeMap<String, Type>),
    /// A fixed, heterogeneous tuple of length ≥ 2 — the type of a multi-value `return`
    /// (`SPEC.md` §5.5). Only ever inhabits a function's return position and is consumed by a
    /// matching-arity parallel binding; the checker rejects it in every other position.
    Tuple(Vec<Type>),
    Function(FnType),
    /// Opaque named host type.
    Userdata(String),
    /// An inference variable (resolved by the [`Unifier`]).
    Var(u32),
    /// Poison: suppresses cascading diagnostics after an error.
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FnType {
    pub params: Vec<Type>,
    /// Single return type, or [`Type::Unit`] for a value-less function.
    pub ret: Box<Type>,
}

impl Type {
    pub fn optional(inner: Type) -> Type {
        Type::Optional(Box::new(inner))
    }
    pub fn array(inner: Type) -> Type {
        Type::Array(Box::new(inner))
    }
    pub fn map(inner: Type) -> Type {
        Type::Map(Box::new(inner))
    }
    pub fn function(params: Vec<Type>, ret: Type) -> Type {
        Type::Function(FnType {
            params,
            ret: Box::new(ret),
        })
    }
    /// A tuple type. Tuples always carry ≥ 2 elements (a single `return e` is not a tuple).
    pub fn tuple(elems: Vec<Type>) -> Type {
        Type::Tuple(elems)
    }
}

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Number => f.write_str("number"),
            Type::Bool => f.write_str("bool"),
            Type::String => f.write_str("string"),
            Type::Nil => f.write_str("nil"),
            Type::Unit => f.write_str("()"),
            Type::Optional(t) => write!(f, "{t}?"),
            Type::Array(t) => write!(f, "array<{t}>"),
            Type::Map(t) => write!(f, "map<string, {t}>"),
            Type::Record(fields) => {
                f.write_str("record { ")?;
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                f.write_str(" }")
            }
            Type::Tuple(elems) => {
                f.write_str("(")?;
                for (i, t) in elems.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{t}")?;
                }
                f.write_str(")")
            }
            Type::Function(ft) => {
                f.write_str("fn(")?;
                for (i, p) in ft.params.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{p}")?;
                }
                write!(f, ") -> {}", ft.ret)
            }
            Type::Userdata(name) => write!(f, "userdata<{name}>"),
            Type::Var(_) => f.write_str("?"),
            Type::Error => f.write_str("<error>"),
        }
    }
}

/// Host environment types the checker needs (the typed counterpart of
/// [`crate::resolve::ResolveConfig`]).
#[derive(Clone, Debug, Default)]
pub struct TypeConfig {
    /// Host-registered function name → signature.
    pub host_functions: BTreeMap<String, FnType>,
    /// Host memory binding name → its type (commonly a [`Type::Record`]).
    pub memory: BTreeMap<String, Type>,
    /// Host memory methods (`mem:method(args)`, `SPEC.md` §7): memory binding name → method
    /// name → the method's signature. A method's `params` are the **call-argument** types (the
    /// receiver is implicit and typed by the memory binding); `ret` is the result type.
    pub methods: BTreeMap<String, BTreeMap<String, FnType>>,
}

impl TypeConfig {
    /// Derive the name-only [`crate::resolve::ResolveConfig`] implied by this config so a
    /// single source of truth drives both resolution and type checking.
    pub fn to_resolve_config(&self) -> crate::resolve::ResolveConfig {
        crate::resolve::ResolveConfig {
            host_functions: self.host_functions.keys().cloned().collect(),
            memory: self.memory.keys().cloned().collect(),
        }
    }
}

/// The result of type checking: the module's export signature plus the resolved types of
/// every in-function symbol and every top-level declaration. Later phases (the IR lowering
/// in Phase 5, codegen in Phase 7) consume the per-symbol/declaration types so they don't
/// re-run inference.
#[derive(Clone, Debug)]
pub struct TypeInfo {
    /// Each exported name mapped to its (fully resolved) type.
    pub exports: BTreeMap<String, Type>,
    /// Resolved type of each in-function symbol, indexed by [`crate::resolve::SymbolId`].
    pub symbol_types: Vec<Type>,
    /// Resolved signature of each top-level function, by declared name.
    pub functions: BTreeMap<String, FnType>,
    /// Resolved type of each top-level constant, by declared name.
    pub constants: BTreeMap<String, Type>,
    /// Resolved signature of each nested function literal (anonymous `function … end` and
    /// `local function`), keyed by its [`FuncBody`] span. IR lowering reads these to recover
    /// a closure's parameter/return types (the checker computes them but, unlike top-level
    /// functions, they aren't otherwise persisted).
    pub function_literals: HashMap<Span, FnType>,
    /// The refined type of every name or field read inside a `~= nil` guard (`SPEC.md` §5.3),
    /// keyed by the read expression's span. The value still *holds* an optional at runtime, so
    /// IR lowering unwraps each of these reads to the narrowed type — without that, a narrowed
    /// `number?` would reach native code as a boxed handle where an `f64` is expected.
    pub narrowed: HashMap<Span, Type>,
    /// The type of every `and`/`or` expression that isn't an operand of another, and of every
    /// conditional expression (`SPEC.md` §3.5), keyed by its span. Its operands can differ from
    /// it and from each other (`x or 0` with `x` a `number?` is a `number`), so lowering can't
    /// take it from them.
    pub logic: HashMap<Span, Type>,
}

/// Type-check `module` (already resolved into `resolution`) against `cfg`.
pub fn typecheck(
    module: &Module,
    resolution: &Resolution,
    cfg: &TypeConfig,
) -> Result<TypeInfo, Diagnostics> {
    let mut c = Checker::new(module, resolution, cfg);
    let exports = c.run(module);
    if c.diags.has_errors() {
        return Err(c.diags);
    }

    let symbol_types = c.sym_types.iter().map(|t| c.u.deep(t)).collect();
    let functions = c
        .func_sigs
        .iter()
        .map(|(name, sig)| {
            let deep = match c.u.deep(&Type::Function(sig.clone())) {
                Type::Function(ft) => ft,
                _ => unreachable!("deep of a function type is a function type"),
            };
            (name.clone(), deep)
        })
        .collect();
    let constants = c
        .const_types
        .iter()
        .map(|(name, t)| (name.clone(), c.u.deep(t)))
        .collect();
    let function_literals = c
        .func_literals
        .iter()
        .map(|(span, sig)| {
            let deep = match c.u.deep(&Type::Function(sig.clone())) {
                Type::Function(ft) => ft,
                _ => unreachable!("deep of a function type is a function type"),
            };
            (*span, deep)
        })
        .collect();
    let narrowed = c
        .narrowed_reads
        .iter()
        .map(|(span, t)| (*span, c.u.deep(t)))
        .collect();
    let logic = c
        .logic_types
        .iter()
        .map(|(span, t)| (*span, c.u.deep(t)))
        .collect();

    Ok(TypeInfo {
        exports,
        symbol_types,
        functions,
        constants,
        function_literals,
        narrowed,
        logic,
    })
}

// ---- unification ------------------------------------------------------------

#[derive(Default)]
struct Unifier {
    subst: Vec<Option<Type>>,
}

impl Unifier {
    fn fresh(&mut self) -> Type {
        let id = self.subst.len() as u32;
        self.subst.push(None);
        Type::Var(id)
    }

    /// Follow variable links at the top level only.
    fn shallow(&self, t: &Type) -> Type {
        let mut cur = t.clone();
        while let Type::Var(id) = cur {
            match &self.subst[id as usize] {
                Some(next) => cur = next.clone(),
                None => break,
            }
        }
        cur
    }

    /// Fully substitute all variables, recursively. Unresolved vars stay [`Type::Var`].
    fn deep(&self, t: &Type) -> Type {
        match self.shallow(t) {
            Type::Optional(i) => match self.deep(&i) {
                // Holds nothing but `nil` (a `local x = nil` that nothing else is assigned to).
                Type::Nil => Type::Nil,
                // There is one `nil`: `x or nil` with an `x` of a type found to be `T?` later.
                inner @ Type::Optional(_) => inner,
                inner => Type::optional(inner),
            },
            Type::Array(i) => Type::array(self.deep(&i)),
            Type::Map(i) => Type::map(self.deep(&i)),
            Type::Record(fields) => Type::Record(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), self.deep(v)))
                    .collect(),
            ),
            Type::Tuple(elems) => Type::Tuple(elems.iter().map(|e| self.deep(e)).collect()),
            Type::Function(ft) => Type::Function(FnType {
                params: ft.params.iter().map(|p| self.deep(p)).collect(),
                ret: Box::new(self.deep(&ft.ret)),
            }),
            other => other,
        }
    }

    fn occurs(&self, var: u32, t: &Type) -> bool {
        match self.shallow(t) {
            Type::Var(id) => id == var,
            Type::Optional(i) | Type::Array(i) | Type::Map(i) => self.occurs(var, &i),
            Type::Record(fields) => fields.values().any(|v| self.occurs(var, v)),
            Type::Tuple(elems) => elems.iter().any(|e| self.occurs(var, e)),
            Type::Function(ft) => {
                ft.params.iter().any(|p| self.occurs(var, p)) || self.occurs(var, &ft.ret)
            }
            _ => false,
        }
    }

    fn bind(&mut self, var: u32, t: Type) -> Result<(), ()> {
        if self.occurs(var, &t) {
            return Err(());
        }
        self.subst[var as usize] = Some(t);
        Ok(())
    }

    /// Unify two types, recording variable bindings. Returns `Err` on a hard mismatch.
    fn unify(&mut self, a: &Type, b: &Type) -> Result<(), ()> {
        let a = self.shallow(a);
        let b = self.shallow(b);
        match (a, b) {
            (Type::Var(i), Type::Var(j)) if i == j => Ok(()),
            (Type::Var(i), t) | (t, Type::Var(i)) => self.bind(i, t),
            (Type::Error, _) | (_, Type::Error) => Ok(()),
            (Type::Number, Type::Number)
            | (Type::Bool, Type::Bool)
            | (Type::String, Type::String)
            | (Type::Nil, Type::Nil)
            | (Type::Unit, Type::Unit) => Ok(()),
            (Type::Optional(x), Type::Optional(y))
            | (Type::Array(x), Type::Array(y))
            | (Type::Map(x), Type::Map(y)) => self.unify(&x, &y),
            (Type::Record(x), Type::Record(y)) => {
                if x.len() != y.len() || x.keys().ne(y.keys()) {
                    return Err(());
                }
                for (k, xv) in &x {
                    self.unify(xv, &y[k])?;
                }
                Ok(())
            }
            (Type::Tuple(x), Type::Tuple(y)) => {
                if x.len() != y.len() {
                    return Err(());
                }
                for (xv, yv) in x.iter().zip(&y) {
                    self.unify(xv, yv)?;
                }
                Ok(())
            }
            (Type::Function(f), Type::Function(g)) => {
                if f.params.len() != g.params.len() {
                    return Err(());
                }
                for (p, q) in f.params.iter().zip(&g.params) {
                    self.unify(p, q)?;
                }
                self.unify(&f.ret, &g.ret)
            }
            (Type::Userdata(x), Type::Userdata(y)) if x == y => Ok(()),
            _ => Err(()),
        }
    }
}

// ---- checker ----------------------------------------------------------------

struct ReturnFrame {
    ret: Type,
    /// Whether each `return` is checked like an assignment to the return slot, so a `T` or
    /// `nil` may be returned where the return type is `T?` — decided once, before the body is
    /// checked, so the outcome can't depend on which `return` comes first (§5.5).
    widen: bool,
    saw_value: bool,
    /// Each bare `return` in the function, which returns no value.
    bare_returns: Vec<Span>,
    /// Whether a `return` of a call that gives nothing counts as a bare `return`, as it does
    /// where a path without a value returns `nil` ([`Fallthrough::Nil`]).
    nil_fallthrough: bool,
}

/// The ways a value-returning function can return no value, checked once every body has been
/// checked and its return type is fully known (see [`Checker::check_valueless_exits`]).
struct ValuelessExits {
    ret: Type,
    bare_returns: Vec<Span>,
    /// The function's closing `end`, if control can reach it.
    falls_off: Option<Span>,
}

/// A use of a value whose type wasn't known yet when the checker reached it: a field read or
/// write, or a call, on a value only later code gives a type (`local best = nil`, assigned
/// further down the loop that reads `best.score`). Checked once every body has been
/// ([`Checker::check_deferred_uses`]); until then the use stands for a fresh variable.
struct DeferredUse {
    /// The used value's type: an unbound variable when the use was reached.
    base: Type,
    kind: DeferredKind,
    /// The used value, where an error about it (rather than a field) is reported.
    span: Span,
}

enum DeferredKind {
    /// `base.name`. A read stands for `ty`; a write assigns a value of type `ty`. Errors about
    /// the field itself are reported at `name`, as they are when the type is known.
    Field { name: Ident, ty: Type, write: bool },
    /// `base(args)`, standing for `result`.
    Call {
        args: Vec<(Type, Span)>,
        result: Type,
        span: Span,
    },
}

/// What an operand of `and`/`or` can yield: the type of its truthy values, and of its `nil` or
/// `false` ones. `None` where it has none: a number is never falsy, `nil` never truthy.
struct Outcomes {
    truthy: Option<Type>,
    falsy: Option<Type>,
}

/// A **narrowable location**: a bare local/upvalue symbol, a host-memory root, or a field
/// path rooted at one of those. Keying the narrowing overlay by `Place` (rather than a bare
/// [`SymbolId`]) is what lets `if p.name ~= nil then … p.name` refine a *field*, not just a
/// whole variable (`SPEC.md` §5.3).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Place {
    Symbol(SymbolId),
    Memory(String),
    Field(Box<Place>, String),
}

impl Place {
    /// Whether `self` is a strict descendant of `prefix` (e.g. `p.name` is under `p`). Used to
    /// invalidate a field's narrowing when its container is reassigned.
    fn has_ancestor(&self, prefix: &Place) -> bool {
        match self {
            Place::Field(base, _) => base.as_ref() == prefix || base.has_ancestor(prefix),
            _ => false,
        }
    }
}

/// The refinements an `if` condition implies: one for where it holds, one for where it
/// doesn't (`Checker::narrowing_from`).
struct Narrowing {
    then: Option<(Place, Type)>,
    els: Option<(Place, Type)>,
}

struct Checker<'a> {
    res: &'a Resolution,
    cfg: &'a TypeConfig,
    /// The syntax diagnostics quote code in.
    surface: Surface,
    u: Unifier,
    /// Type of each symbol, indexed by [`SymbolId`].
    sym_types: Vec<Type>,
    /// Narrowing overlay: a [`Place`] temporarily refined to a non-optional type.
    narrowed: BTreeMap<Place, Type>,
    /// Each read that resolved through [`narrowed`](Self::narrowed), by span (deep-resolved into
    /// [`TypeInfo::narrowed`] at the end).
    narrowed_reads: HashMap<Span, Type>,
    /// Top-level function signatures, by name.
    func_sigs: BTreeMap<String, FnType>,
    /// Which parameters of each top-level function a `---@param` declares, by name: a function
    /// literal passed for one takes its type ([`Self::check_arg`]).
    declared_params: HashMap<String, Vec<bool>>,
    /// Top-level constant types, by name.
    const_types: BTreeMap<String, Type>,
    /// Nested function-literal signatures, by [`FuncBody`] span (deep-resolved at the end).
    func_literals: HashMap<Span, FnType>,
    return_stack: Vec<ReturnFrame>,
    valueless_exits: Vec<ValuelessExits>,
    deferred: Vec<DeferredUse>,
    /// The `T` of the type `T?` of each `local x = nil` or uninitialized `local x`, which is
    /// `nil` if nothing else is ever assigned to `x` ([`Checker::settle_nil_locals`]).
    nil_locals: Vec<Type>,
    /// The type of each `and`/`or` expression that isn't an operand of another, and of each
    /// conditional expression, by span (deep-resolved into [`TypeInfo::logic`] at the end).
    logic_types: HashMap<Span, Type>,
    /// The `T` of each `T?` operand of `and`/`or` typed before `T` was known, with the operand's
    /// span ([`Checker::check_logic_optionals`]).
    logic_optionals: Vec<(Type, Span)>,
    /// The type of each call used as a value before its callee's return type was known, with
    /// the call's span ([`Checker::check_valueless_calls`]).
    valueless_calls: Vec<(Type, Span)>,
    /// The top-level functions known before any body is checked to return nothing
    /// ([`Checker::valueless_functions`]).
    valueless_fns: HashSet<String>,
    diags: Diagnostics,
}

impl<'a> Checker<'a> {
    fn new(module: &Module, res: &'a Resolution, cfg: &'a TypeConfig) -> Self {
        Checker {
            res,
            cfg,
            surface: module.surface,
            u: Unifier::default(),
            sym_types: vec![Type::Error; res.symbols.len()],
            narrowed: BTreeMap::new(),
            narrowed_reads: HashMap::new(),
            func_sigs: BTreeMap::new(),
            declared_params: HashMap::new(),
            const_types: BTreeMap::new(),
            func_literals: HashMap::new(),
            return_stack: Vec::new(),
            valueless_exits: Vec::new(),
            deferred: Vec::new(),
            nil_locals: Vec::new(),
            logic_types: HashMap::new(),
            logic_optionals: Vec::new(),
            valueless_calls: Vec::new(),
            valueless_fns: HashSet::new(),
            diags: Diagnostics::new(),
        }
    }

    fn error(&mut self, code: &'static str, msg: impl Into<String>, span: Span) {
        self.diags.push(Diagnostic::error(code, msg, span));
    }

    fn run(&mut self, module: &Module) -> BTreeMap<String, Type> {
        self.valueless_fns = self.valueless_functions(module);
        // Pass 1: pre-create a signature (fresh vars) for every top-level function so
        // calls — including mutual recursion — resolve before bodies are checked. `---@`
        // annotations pin parameter/return vars here, *before* Pass 3, so the body sees a
        // record/array parameter's shape and an otherwise-untypeable parameter is no longer
        // ambiguous (`SPEC.md` §5.6). Each constant gets a fresh type too, since it is in
        // scope everywhere, above its declaration included.
        for decl in &module.decls {
            match decl {
                TopDecl::Function(f) => {
                    let sig = self.make_sig(&f.body.params);
                    if !f.annotations.is_empty() {
                        self.apply_fn_annotations(f, &sig);
                    }
                    let declared = f
                        .body
                        .params
                        .iter()
                        .map(|p| f.annotations.params.iter().any(|a| a.name.node == p.node))
                        .collect();
                    self.declared_params.insert(f.name.node.clone(), declared);
                    // A declared return type is the author's to give; `nil` against a
                    // non-optional one is then reported at the `return`, and a missing value
                    // at the exit that gives none (`E0480`).
                    if f.annotations.ret.is_empty() {
                        self.infer_return_up_front(&f.body, &sig);
                    }
                    self.func_sigs.insert(f.name.node.clone(), sig);
                }
                TopDecl::Const(c) => {
                    let t = self.u.fresh();
                    self.const_types.insert(c.name.node.clone(), t);
                }
                TopDecl::Stat(_) => {}
            }
        }

        // Pass 2: check top-level code in order — the constants' initializers and the
        // top-level statements. It returns nothing (the resolver rejects a `return` in it).
        self.return_stack.push(ReturnFrame {
            ret: Type::Unit,
            widen: false,
            saw_value: false,
            bare_returns: Vec::new(),
            nil_fallthrough: false,
        });
        for decl in &module.decls {
            match decl {
                TopDecl::Const(c) => {
                    let t = self.check_expr(&c.value);
                    let declared = self.const_types[&c.name.node].clone();
                    if self.u.unify(&declared, &t).is_err() {
                        let msg = self.mismatch("constant type mismatch", &t, &declared);
                        self.error("E0402", msg, c.value.span);
                    }
                }
                TopDecl::Stat(s) => self.check_stat(s),
                TopDecl::Function(_) => {}
            }
        }
        self.return_stack.pop();

        // Pass 3: check function bodies against their pre-created signatures.
        for decl in &module.decls {
            if let TopDecl::Function(f) = decl {
                let sig = self.func_sigs[&f.name.node].clone();
                self.check_func_body(&f.body, &sig);
            }
        }
        self.check_deferred_uses();
        self.check_logic_optionals();
        self.check_valueless_exits();
        self.check_valueless_calls();

        // Pass 4: flag parameters whose type inference couldn't pin.
        let mut unknown_params = std::collections::HashSet::new();
        for sym in self.res.symbols.iter() {
            if sym.kind == crate::resolve::SymbolKind::Param
                && let Some(id) = self.res.def(sym.def_span)
            {
                let t = self.u.deep(&self.sym_types[id as usize]);
                if let Type::Var(var) = t {
                    unknown_params.insert(var);
                    self.error(
                        "E0410",
                        format!(
                            "cannot infer the type of parameter `{}`; it is never used \
                             in a way that determines its type",
                            sym.name
                        ),
                        sym.def_span,
                    );
                }
            }
        }
        self.report_unknown_uses(&unknown_params);
        self.settle_nil_locals();

        self.build_exports(module)
    }

    /// Build the module export signature (`SPEC.md` §4).
    fn build_exports(&mut self, module: &Module) -> BTreeMap<String, Type> {
        let mut exports = BTreeMap::new();
        if let Some(export) = &module.export {
            for field in &export.node {
                match field {
                    // An entry names what it exports; it doesn't compute a value (`SPEC.md` §3.1).
                    Field::Named { name, value } => {
                        let t = match (&value.kind, self.res.binding(value.span)) {
                            (ExprKind::Name(_), Some(Binding::TopFunction(f))) => {
                                Type::Function(self.func_sigs[f].clone())
                            }
                            (ExprKind::Name(_), Some(Binding::TopConst(c))) => {
                                self.const_types[c].clone()
                            }
                            (ExprKind::Name(_), Some(Binding::Module(id))) => {
                                self.sym_types[*id as usize].clone()
                            }
                            _ => {
                                self.error(
                                    "E0411",
                                    "a module export entry must name a top-level function, \
                                     constant, or `local`",
                                    value.span,
                                );
                                Type::Error
                            }
                        };
                        exports.insert(name.node.clone(), self.u.deep(&t));
                    }
                    Field::Positional(e) => self.error(
                        "E0411",
                        "module export table entries must be `name = value`",
                        e.span,
                    ),
                    Field::Keyed { key, .. } => self.error(
                        "E0411",
                        "module export table entries must be `name = value`",
                        key.span,
                    ),
                }
            }
        } else {
            // Everything but what `local` makes private: a `local function` and a top-level
            // `local` are the module's own.
            for decl in &module.decls {
                match decl {
                    TopDecl::Function(f) if !f.local => {
                        let sig = self.func_sigs[&f.name.node].clone();
                        exports.insert(f.name.node.clone(), self.u.deep(&Type::Function(sig)));
                    }
                    TopDecl::Const(c) => {
                        let t = self.const_types[&c.name.node].clone();
                        exports.insert(c.name.node.clone(), self.u.deep(&t));
                    }
                    TopDecl::Function(_) | TopDecl::Stat(_) => {}
                }
            }
        }
        exports
    }

    /// Create a function signature of fresh variables, binding each parameter symbol's
    /// type to its fresh var so the body and the signature share inference state.
    fn make_sig(&mut self, params: &[Ident]) -> FnType {
        let mut ptypes = Vec::with_capacity(params.len());
        for p in params {
            let v = self.u.fresh();
            if let Some(id) = self.res.def(p.span) {
                self.sym_types[id as usize] = v.clone();
            }
            ptypes.push(v);
        }
        FnType {
            params: ptypes,
            ret: Box::new(self.u.fresh()),
        }
    }

    /// Pin parameter/return types from `---@param`/`---@return` annotations onto the (still
    /// fresh) signature vars. Applied in Pass 2, before bodies are checked, so a record/array
    /// parameter's shape is visible to the body and an unused parameter becomes typed. A
    /// direct conflict *here* is rare (the vars are fresh); the usual contradiction —
    /// annotation vs. how the body uses the value — surfaces as an ordinary type error at the
    /// conflicting use site.
    fn apply_fn_annotations(&mut self, f: &FuncDecl, sig: &FnType) {
        for pa in &f.annotations.params {
            match f.body.params.iter().find(|p| p.node == pa.name.node) {
                Some(param) => {
                    if let Some(id) = self.res.def(param.span) {
                        let ann = self.ann_to_type(&pa.ty.node, pa.ty.span);
                        let target = self.sym_types[id as usize].clone();
                        if self.u.unify(&ann, &target).is_err() {
                            self.error(
                                "E0460",
                                format!(
                                    "annotation for parameter `{}` conflicts with its type",
                                    pa.name.node
                                ),
                                pa.ty.span,
                            );
                        }
                    }
                }
                None => self.error(
                    "E0461",
                    format!("`@param` refers to unknown parameter `{}`", pa.name.node),
                    pa.name.span,
                ),
            }
        }
        // Zero `@return` lines: nothing to pin. One: a single return type. Two or more: a
        // tuple return (`SPEC.md` §5.5), in source order.
        if !f.annotations.ret.is_empty() {
            let span = f.annotations.ret[0].span;
            let ann = if f.annotations.ret.len() == 1 {
                self.ann_to_type(&f.annotations.ret[0].node, span)
            } else {
                let elems = f
                    .annotations
                    .ret
                    .iter()
                    .map(|r| self.ann_to_type(&r.node, r.span))
                    .collect();
                Type::Tuple(elems)
            };
            let target = (*sig.ret).clone();
            if self.u.unify(&ann, &target).is_err() {
                self.error(
                    "E0460",
                    "`@return` annotation conflicts with the inferred return type",
                    span,
                );
            }
        }
    }

    /// Resolve a parsed [`TypeAnn`] shape to a concrete [`Type`]. `span` locates an
    /// unknown-type-name error back at the annotation.
    fn ann_to_type(&mut self, ann: &TypeAnn, span: Span) -> Type {
        match ann {
            TypeAnn::Named(n) => match n.as_str() {
                "number" => Type::Number,
                "bool" => Type::Bool,
                "string" => Type::String,
                "nil" => Type::Nil,
                other => {
                    self.error(
                        "E0462",
                        format!("unknown type `{other}` in annotation"),
                        span,
                    );
                    Type::Error
                }
            },
            TypeAnn::Optional(t) => Type::optional(self.ann_to_type(t, span)),
            TypeAnn::Array(t) => Type::array(self.ann_to_type(t, span)),
            TypeAnn::Map(t) => Type::map(self.ann_to_type(t, span)),
            TypeAnn::Record(fields) => {
                let mut m = BTreeMap::new();
                for (k, v) in fields {
                    m.insert(k.clone(), self.ann_to_type(v, span));
                }
                Type::Record(m)
            }
            TypeAnn::Function { params, ret } => {
                let params = params.iter().map(|p| self.ann_to_type(p, span)).collect();
                let ret = match ret {
                    Some(r) => self.ann_to_type(r, span),
                    None => Type::Unit,
                };
                Type::function(params, ret)
            }
        }
    }

    fn check_func_body(&mut self, body: &FuncBody, sig: &FnType) {
        let widen = match self.u.shallow(&sig.ret) {
            Type::Optional(_) => true,
            Type::Tuple(elems) => elems
                .iter()
                .any(|e| matches!(self.u.shallow(e), Type::Optional(_))),
            _ => false,
        };
        self.return_stack.push(ReturnFrame {
            ret: (*sig.ret).clone(),
            widen,
            saw_value: false,
            bare_returns: Vec::new(),
            nil_fallthrough: body.fallthrough == Fallthrough::Nil,
        });
        self.check_block(&body.block);
        let frame = self.return_stack.pop().expect("return frame");
        // A function with no value-returning `return` is value-less — unless an annotation
        // already gave it a return type, which its exits are then checked against below.
        if !frame.saw_value && self.u.unify(&frame.ret, &Type::Unit).is_ok() {
            return;
        }
        let falls_off = (!always_returns(&body.block)).then(|| end_keyword(body));
        if falls_off.is_some() || !frame.bare_returns.is_empty() {
            self.valueless_exits.push(ValuelessExits {
                ret: frame.ret,
                bare_returns: frame.bare_returns,
                falls_off,
            });
        }
    }

    /// Reject every way a function whose return type can't hold `nil` returns without a value
    /// (`E0480`, `SPEC.md` §5.5): a bare `return`, or reaching the closing `end`.
    ///
    /// Such an exit has no value to give the caller. The interpreters would hand back `nil` and
    /// the JIT the zero of the return type (`0`, `false`), so this is where the three backends
    /// would otherwise disagree. A function returning an optional may exit without a value,
    /// which returns `nil` in all three.
    ///
    /// Run after every body is checked, because a return type can be fixed by code checked
    /// later — a call from another function, or the use of a closure after it is built.
    fn check_valueless_exits(&mut self) {
        for exits in std::mem::take(&mut self.valueless_exits) {
            let ret = self.u.deep(&exits.ret);
            // `Var`/`Error`: the return type couldn't be determined, which is reported elsewhere.
            if matches!(
                ret,
                Type::Optional(_) | Type::Nil | Type::Unit | Type::Var(_) | Type::Error
            ) {
                continue;
            }
            for span in exits.bare_returns {
                self.error(
                    "E0480",
                    format!(
                        "missing return value: this function returns {ret}, but this `return` \
                         gives none"
                    ),
                    span,
                );
            }
            if let Some(span) = exits.falls_off {
                self.error(
                    "E0480",
                    format!(
                        "missing `return`: this function returns {ret}, but can reach its end \
                         without returning a value"
                    ),
                    span,
                );
            }
        }
    }

    /// Check each [`DeferredUse`] whose value now has a type, as the use would have been had
    /// the type been known when the checker reached it.
    ///
    /// Repeated until nothing more resolves, since checking one use can fix the type of another's
    /// value: in `best.inner.x`, the type of `best.inner` is only known once `best.inner` is
    /// checked. A use whose value still has no type is left for [`Self::report_unknown_uses`].
    fn check_deferred_uses(&mut self) {
        loop {
            let pending = std::mem::take(&mut self.deferred);
            let count = pending.len();
            for use_ in pending {
                if matches!(self.u.shallow(&use_.base), Type::Var(_)) {
                    self.deferred.push(use_);
                } else {
                    self.check_deferred_use(use_);
                }
            }
            if self.deferred.len() == count {
                return;
            }
        }
    }

    fn check_deferred_use(&mut self, use_: DeferredUse) {
        let base = self.u.shallow(&use_.base);
        match use_.kind {
            DeferredKind::Field { name, ty, write } => {
                let field = match base {
                    Type::Record(ref fields) => match fields.get(&name.node) {
                        Some(t) => t.clone(),
                        None => {
                            let added = if write {
                                " (fields cannot be added)"
                            } else {
                                ""
                            };
                            self.error(
                                "E0420",
                                format!(
                                    "no field `{}` on {}{added}",
                                    name.node,
                                    self.u.deep(&base)
                                ),
                                name.span,
                            );
                            return;
                        }
                    },
                    // A write stores the element; a read may find it missing.
                    Type::Map(v) if write => *v,
                    Type::Map(v) => Type::optional(*v),
                    Type::Error => return,
                    other => {
                        self.no_fields(&other, &name.node, name.span);
                        return;
                    }
                };
                if write {
                    self.require_assignable(name.span, &ty, &field, "assignment type mismatch");
                } else if self.u.unify(&field, &ty).is_err() {
                    self.error(
                        "E0402",
                        format!(
                            "field `{}` has type {}, but is used here as {}",
                            name.node,
                            self.u.deep(&field),
                            self.u.deep(&ty)
                        ),
                        name.span,
                    );
                }
            }
            DeferredKind::Call { args, result, span } => match base {
                Type::Function(ft) => {
                    if args.len() != ft.params.len() {
                        self.error(
                            "E0430",
                            format!(
                                "this function takes {} argument(s) but {} were supplied",
                                ft.params.len(),
                                args.len()
                            ),
                            span,
                        );
                    }
                    for ((at, arg_span), pt) in args.iter().zip(&ft.params) {
                        self.require_assignable(*arg_span, at, pt, "argument type mismatch");
                    }
                    if self.u.unify(&ft.ret, &result).is_err() {
                        self.error(
                            "E0402",
                            format!(
                                "this function returns {}, but its result is used here as {}",
                                self.u.deep(&ft.ret),
                                self.u.deep(&result)
                            ),
                            span,
                        );
                    }
                }
                Type::Error => {}
                other => self.error(
                    "E0431",
                    format!("type {} is not callable", self.u.deep(&other)),
                    use_.span,
                ),
            },
        }
    }

    /// Reject each [`DeferredUse`] whose value never got a type (`E0410`), except where that value
    /// is a parameter already reported as such.
    fn report_unknown_uses(&mut self, unknown_params: &std::collections::HashSet<u32>) {
        for use_ in std::mem::take(&mut self.deferred) {
            if let Type::Var(var) = self.u.shallow(&use_.base)
                && unknown_params.contains(&var)
            {
                continue;
            }
            let msg = match use_.kind {
                DeferredKind::Field { name, .. } => format!(
                    "cannot infer the type of this value, so its field `{}` can't be checked",
                    name.node
                ),
                DeferredKind::Call { .. } => {
                    "cannot infer the type of this value, so calling it can't be checked"
                        .to_string()
                }
            };
            self.error("E0410", msg, use_.span);
        }
    }

    /// Give each `local x = nil` that is never assigned anything but `nil` the type `nil`: its
    /// type is `T?` with nothing to fix `T`, and [`Unifier::deep`] reads `nil?` as `nil`.
    fn settle_nil_locals(&mut self) {
        for inner in std::mem::take(&mut self.nil_locals) {
            if matches!(self.u.shallow(&inner), Type::Var(_)) {
                let _ = self.u.unify(&inner, &Type::Nil);
            }
        }
    }

    fn check_block(&mut self, block: &Block) {
        for stat in &block.stats {
            self.check_stat(stat);
        }
        if let Some(ret) = &block.ret {
            self.check_return(ret);
        }
    }

    fn check_return(&mut self, ret: &RetStat) {
        let frame_ret = self.return_stack.last().map(|f| (f.ret.clone(), f.widen));
        // 0 exprs → value-less; 1 → a single value (which may itself be a tuple-returning call,
        // i.e. a pass-through); ≥ 2 → a tuple built from single-valued elements (no flattening).
        let (value_ty, span) = if ret.exprs.is_empty() {
            if let Some(frame) = self.return_stack.last_mut() {
                frame.bare_returns.push(ret.span);
            }
            return;
        } else if ret.exprs.len() == 1 {
            let e = &ret.exprs[0];
            let t = self.check_expr_multi(e);
            // Where a path without a value returns `nil`, so does returning a call that gives
            // none: it is a bare `return` after the call.
            if let Some(frame) = self.return_stack.last_mut()
                && frame.nil_fallthrough
                && matches!(self.u.shallow(&t), Type::Unit)
            {
                frame.bare_returns.push(ret.span);
                return;
            }
            (t, e.span)
        } else {
            // Each element is typed through `check_expr` (the guard), so a nested multi-value
            // call inside the list is rejected as E0415 rather than silently flattened.
            let elems = ret.exprs.iter().map(|e| self.check_expr(e)).collect();
            (Type::Tuple(elems), ret.span)
        };
        if let Some((r, widen)) = frame_ret {
            if !widen {
                self.require(span, &value_ty, &r, "E0413", "return type mismatch");
            } else if !self.is_assignable(&value_ty, &r) {
                let msg = self.mismatch("return type mismatch", &value_ty, &r);
                self.error("E0413", msg, span);
            }
        }
        if let Some(frame) = self.return_stack.last_mut() {
            frame.saw_value = true;
        }
    }

    /// Settle what the shape of `body`'s returns decides about `sig`'s return type, before any
    /// body is checked, so the outcome doesn't depend on the order of the code (`SPEC.md` §5.5):
    ///
    /// * No `return` gives a value: the function returns nothing, `()`. A call to it used as a
    ///   value is then `E0435` wherever the call is, above the function included, instead of
    ///   leaving the use to fix its return type.
    /// * A literal `nil` on some paths and a value on others: it is optional, so every `return`
    ///   is checked against `T?` whichever comes first. Otherwise the first `return` would fix
    ///   the type: `return 1` makes it `number` and a later `return nil` a mismatch, while the
    ///   same two returns the other way round would infer `nil` and reject the `1`.
    ///
    /// Where a path without a value returns `nil` ([`Fallthrough::Nil`]), such a path — a bare
    /// `return`, reaching the end, or returning a call to a function that returns nothing — counts
    /// as returning `nil` beside a value, and the function returns nothing when all its paths are
    /// such. Which calls return nothing is decided for every top-level function first
    /// ([`Self::valueless_functions`]), so that returning one doesn't make a function optional.
    fn infer_return_up_front(&mut self, body: &FuncBody, sig: &FnType) {
        let nil_fallthrough = body.fallthrough == Fallthrough::Nil;
        let (mut returns_nil, mut returns_value, mut returns_any) = (false, false, false);
        let mut no_value = nil_fallthrough && !always_returns(&body.block);
        visit_returns(&body.block, &mut |ret| match ret.exprs.as_slice() {
            [] => no_value = true,
            [e] if is_literal_nil(e) => returns_nil = true,
            [e] if nil_fallthrough && self.gives_nothing(e, &self.valueless_fns) => no_value = true,
            [_] => returns_value = true,
            _ => returns_any = true,
        });
        returns_any |= returns_nil || returns_value;
        if !returns_any {
            let _ = self.u.unify(&sig.ret, &Type::Unit);
        } else if returns_value && (returns_nil || (nil_fallthrough && no_value)) {
            let inner = self.u.fresh();
            let _ = self.u.unify(&sig.ret, &Type::optional(inner));
        }
    }

    /// The top-level functions that return nothing, as far as their `return`s show before any
    /// body is checked: those none of whose `return`s gives a value. Where a path without a value
    /// returns `nil` ([`Fallthrough::Nil`]), returning a call to one of these gives no value
    /// either, so the set grows until it stops changing.
    fn valueless_functions(&self, module: &Module) -> HashSet<String> {
        let mut valueless = HashSet::new();
        loop {
            let before = valueless.len();
            for decl in &module.decls {
                let TopDecl::Function(f) = decl else { continue };
                if !f.annotations.ret.is_empty() || valueless.contains(&f.name.node) {
                    continue;
                }
                let nil_fallthrough = f.body.fallthrough == Fallthrough::Nil;
                let mut gives_value = false;
                visit_returns(&f.body.block, &mut |ret| match ret.exprs.as_slice() {
                    [] => {}
                    [e] if nil_fallthrough && self.gives_nothing(e, &valueless) => {}
                    _ => gives_value = true,
                });
                if !gives_value {
                    valueless.insert(f.name.node.clone());
                }
            }
            if valueless.len() == before {
                return valueless;
            }
        }
    }

    /// Whether `e` is a call known before any body is checked to return nothing: of a top-level
    /// function in `valueless`, or of a host function or memory method declared to return `()`.
    fn gives_nothing(&self, e: &Expr, valueless: &HashSet<String>) -> bool {
        match &e.kind {
            ExprKind::Paren(inner) => self.gives_nothing(inner, valueless),
            ExprKind::Call { callee, .. } => match &callee.kind {
                ExprKind::Name(_) => match self.res.binding(callee.span) {
                    Some(Binding::TopFunction(f)) => valueless.contains(f),
                    Some(Binding::HostFunction(f)) => self
                        .cfg
                        .host_functions
                        .get(f)
                        .is_some_and(|ft| *ft.ret == Type::Unit),
                    _ => false,
                },
                _ => false,
            },
            ExprKind::MethodCall {
                receiver, method, ..
            } => match self.res.binding(receiver.span) {
                Some(Binding::Memory(m)) => self
                    .cfg
                    .methods
                    .get(m)
                    .and_then(|methods| methods.get(&method.node))
                    .is_some_and(|ft| *ft.ret == Type::Unit),
                _ => false,
            },
            _ => false,
        }
    }

    // ---- statements ----------------------------------------------------------

    fn check_stat(&mut self, stat: &Stat) {
        match &stat.kind {
            StatKind::Empty | StatKind::Break | StatKind::Continue => {}
            StatKind::Local { names, exprs } => self.check_local(names, exprs, stat.span),
            StatKind::LocalFunction { name, body } => {
                let sig = self.make_sig(&body.params);
                self.infer_return_up_front(body, &sig);
                if let Some(id) = self.res.def(name.span) {
                    self.sym_types[id as usize] = Type::Function(sig.clone());
                }
                self.func_literals.insert(body.span, sig.clone());
                self.check_func_body(body, &sig);
            }
            StatKind::Assign { targets, exprs } => self.check_assign(targets, exprs, stat.span),
            // A call statement discards whatever the call returns: nothing, a value, or a tuple.
            StatKind::Call(e) => {
                self.check_expr_multi(e);
            }
            StatKind::Do(block) => self.check_block(block),
            StatKind::While { cond, body } => {
                self.forget_loop_writes(body);
                self.check_condition(cond);
                self.check_block(body);
            }
            StatKind::If { arms, else_block } => self.check_if(arms, else_block),
            StatKind::NumericFor {
                var,
                start,
                end,
                step,
                body,
            } => {
                self.require_number(start);
                self.require_number(end);
                if let Some(step) = step {
                    self.require_number(step);
                }
                if let Some(id) = self.res.def(var.span) {
                    self.sym_types[id as usize] = Type::Number;
                }
                self.forget_loop_writes(body);
                self.check_block(body);
            }
            StatKind::GenericFor { names, iter, body } => self.check_generic_for(names, iter, body),
        }
    }

    /// Type the right-hand side of a parallel binding/assignment against `n` targets, applying
    /// the Tier-A tuple rule (`SPEC.md` §5.5): a *single* multi-return call spreads across all
    /// `n` targets; otherwise it is a one-value-per-expression list whose count must equal `n`.
    /// Always returns exactly `n` types (padded with [`Type::Error`] on an arity error).
    fn check_multi_rhs(&mut self, exprs: &[Expr], n: usize, span: Span) -> Vec<Type> {
        // A single expression may be a multi-return call whose values spread across the targets.
        if exprs.len() == 1 {
            let t = self.check_expr_multi(&exprs[0]);
            if let Type::Tuple(elems) = self.u.deep(&t) {
                if elems.len() != n {
                    self.error(
                        "E0414",
                        format!(
                            "assignment arity mismatch: {n} target(s) but the call returns {} value(s)",
                            elems.len()
                        ),
                        span,
                    );
                    return vec![Type::Error; n];
                }
                return elems;
            }
            // A single ordinary value, or none, from a call to a function that returns nothing.
            let valueless = matches!(self.u.shallow(&t), Type::Unit);
            let t = self.require_value(&exprs[0], t);
            if n != 1 && !valueless {
                self.error(
                    "E0414",
                    format!("assignment arity mismatch: {n} target(s) but 1 value"),
                    span,
                );
            }
            let mut vals = vec![t];
            vals.resize(n, Type::Error);
            return vals;
        }
        // Multiple expressions: each must be single-valued (the guard forbids a tuple), 1:1.
        let mut vals: Vec<Type> = exprs.iter().map(|e| self.check_expr(e)).collect();
        if vals.len() != n {
            self.error(
                "E0414",
                format!(
                    "assignment arity mismatch: {n} target(s) but {} value(s)",
                    vals.len()
                ),
                span,
            );
        }
        vals.resize(n, Type::Error);
        vals
    }

    fn check_local(&mut self, names: &[Ident], exprs: &[Expr], span: Span) {
        // An uninitialized `local a, b` holds `nil` until assigned, so it is typed as if
        // initialized to `nil` (`SPEC.md` §5.2).
        let value_types = if exprs.is_empty() {
            vec![Type::Nil; names.len()]
        } else {
            self.check_multi_rhs(exprs, names.len(), span)
        };
        for (name, vt) in names.iter().zip(value_types) {
            // A local that starts out `nil` is optional, holding whatever is later assigned to
            // it (`SPEC.md` §5.2); it would otherwise be typed `nil`, which nothing else fits.
            let vt = if matches!(self.u.shallow(&vt), Type::Nil) {
                let inner = self.u.fresh();
                self.nil_locals.push(inner.clone());
                Type::optional(inner)
            } else {
                vt
            };
            if let Some(id) = self.res.def(name.span) {
                self.sym_types[id as usize] = vt;
            }
        }
    }

    fn check_assign(&mut self, targets: &[Expr], exprs: &[Expr], span: Span) {
        let value_types = self.check_multi_rhs(exprs, targets.len(), span);
        for (target, vt) in targets.iter().zip(value_types) {
            let place = self.place_type(target);
            self.require_assignable(target.span, &vt, &place, "assignment type mismatch");
        }
        // A write invalidates any narrowing of the assigned place or a sub-place of it — the
        // guarded value may no longer hold. Conservative: syntactic only, no alias tracking
        // (matching the trust model; scripts are trusted dev code).
        for target in targets {
            if let Some(place) = self.place_of(target) {
                self.end_narrowing(&place);
            }
        }
    }

    /// Drop the narrowing of `place` and of every field path under it.
    fn end_narrowing(&mut self, place: &Place) {
        self.narrowed
            .retain(|k, _| k != place && !k.has_ancestor(place));
    }

    /// Before a loop's condition and body: end the narrowing of every place the body assigns.
    /// A write late in one iteration runs before the next iteration reads, which the guard
    /// outside the loop can't account for.
    fn forget_loop_writes(&mut self, body: &Block) {
        let mut places = Vec::new();
        self.assigned_places(body, &mut places);
        for place in &places {
            self.end_narrowing(place);
        }
    }

    /// Every place `block` assigns, in it or in a block nested in it (not in a function
    /// literal, which is a closure writing an upvalue; see `check_assign`).
    fn assigned_places(&self, block: &Block, out: &mut Vec<Place>) {
        for stat in &block.stats {
            match &stat.kind {
                StatKind::Assign { targets, .. } => {
                    out.extend(targets.iter().filter_map(|t| self.place_of(t)))
                }
                StatKind::Do(body)
                | StatKind::While { body, .. }
                | StatKind::NumericFor { body, .. }
                | StatKind::GenericFor { body, .. } => self.assigned_places(body, out),
                StatKind::If { arms, else_block } => {
                    for (_, body) in arms {
                        self.assigned_places(body, out);
                    }
                    if let Some(body) = else_block {
                        self.assigned_places(body, out);
                    }
                }
                _ => {}
            }
        }
    }

    fn check_if(&mut self, arms: &[(Expr, Block)], else_block: &Option<Block>) {
        // What is narrowed on entering each arm: what was before the `if`, plus what each
        // earlier condition being false implies (`x == nil` false narrows `x`). Branches are
        // separate paths, so a write in one doesn't end a narrowing in the next; but after the
        // `if`, a place stays narrowed only if no branch wrote to it.
        let mut entry = self.narrowed.clone();
        let mut after = self.narrowed.clone();
        for (cond, block) in arms {
            self.narrowed = entry.clone();
            self.check_condition(cond);
            let Narrowing { then, els } = self.narrowing_from(cond);
            self.narrowed.extend(then);
            self.check_block(block);
            after.retain(|place, _| self.narrowed.contains_key(place));
            entry.extend(els);
        }
        self.narrowed = entry;
        if let Some(block) = else_block {
            self.check_block(block);
        }
        after.retain(|place, _| self.narrowed.contains_key(place));
        self.narrowed = after;
    }

    fn check_generic_for(&mut self, names: &[Ident], iter: &IterExpr, body: &Block) {
        let (key_t, val_t) = match iter {
            IterExpr::IPairs { arg, .. } => {
                let at = self.check_expr(arg);
                let elem = match self.u.shallow(&at) {
                    Type::Array(e) => *e,
                    Type::Error => Type::Error,
                    // The arg type is still unknown — `ipairs` forces it to be an array,
                    // so unify it with `array<fresh>` and adopt the fresh element type.
                    Type::Var(_) => {
                        let e = self.u.fresh();
                        let _ = self.u.unify(&at, &Type::array(e.clone()));
                        e
                    }
                    other => {
                        self.error(
                            "E0412",
                            format!(
                                "{} expects an array, found {}",
                                self.surface.array_loop(),
                                self.u.deep(&other)
                            ),
                            arg.span,
                        );
                        Type::Error
                    }
                };
                (Type::Number, elem)
            }
            IterExpr::Pairs { arg, .. } => {
                let at = self.check_expr(arg);
                let val = match self.u.shallow(&at) {
                    Type::Map(v) => *v,
                    Type::Error => Type::Error,
                    // Unknown arg type — `pairs` forces a map; unify with `map<fresh>`.
                    Type::Var(_) => {
                        let v = self.u.fresh();
                        let _ = self.u.unify(&at, &Type::map(v.clone()));
                        v
                    }
                    other => {
                        self.error(
                            "E0412",
                            format!(
                                "{} expects a map, found {} (iterating records is not yet \
                                 supported)",
                                self.surface.map_loop(),
                                self.u.deep(&other)
                            ),
                            arg.span,
                        );
                        Type::Error
                    }
                };
                (Type::String, val)
            }
        };
        if names.len() > 2 {
            self.error(
                "E0416",
                "a generic `for` binds at most two variables (key, value)",
                names[2].span,
            );
        }
        let assign = [key_t, val_t];
        for (i, name) in names.iter().enumerate() {
            if let Some(id) = self.res.def(name.span) {
                self.sym_types[id as usize] = assign.get(i).cloned().unwrap_or(Type::Error);
            }
        }
        self.forget_loop_writes(body);
        self.check_block(body);
    }

    // ---- expressions ---------------------------------------------------------

    /// Type an expression that **must** yield a single value. A multi-value (tuple) call in
    /// this position is an error (`E0415`): tuples are confined to the two legal spots — the
    /// sole expression of a `return` and the sole right-hand side of a parallel binding — which
    /// call [`Checker::check_expr_multi`] directly. Every recursive/operand position routes
    /// through here, so a tuple can never silently unify into an operand, argument, container
    /// element, or parameter var. Nor can a call that returns nothing ([`Checker::require_value`]).
    fn check_expr(&mut self, expr: &Expr) -> Type {
        let t = self.check_expr_multi(expr);
        if matches!(self.u.shallow(&t), Type::Tuple(_)) {
            let binding = self.surface.parallel_binding();
            self.error(
                "E0415",
                format!(
                    "a multi-value call must be returned directly or bound to {binding}; it \
                     can't be used as a single value here"
                ),
                expr.span,
            );
            return Type::Error;
        }
        self.require_value(expr, t)
    }

    /// Reject using a call to a function that returns nothing as a value (`E0435`, `SPEC.md`
    /// §5.5): there is no value, and Grindlang doesn't make one up as Lua's `nil`. `t` is the
    /// type of `expr`, which is used as a value. Where the callee's return type isn't known yet,
    /// the check waits until every body has been ([`Checker::check_valueless_calls`]).
    fn require_value(&mut self, expr: &Expr, t: Type) -> Type {
        match self.u.shallow(&t) {
            Type::Unit => {
                self.error("E0435", VALUELESS_CALL, expr.span);
                Type::Error
            }
            Type::Var(_) if is_call(expr) => {
                self.valueless_calls.push((t.clone(), expr.span));
                t
            }
            _ => t,
        }
    }

    /// Report each call used as a value whose callee turned out to return nothing, once every
    /// body has been checked (see [`Checker::require_value`]).
    fn check_valueless_calls(&mut self) {
        for (t, span) in std::mem::take(&mut self.valueless_calls) {
            if matches!(self.u.shallow(&t), Type::Unit) {
                self.error("E0435", VALUELESS_CALL, span);
            }
        }
    }

    /// The expression-typing engine. Returns a [`Type::Tuple`] for a multi-return call so the
    /// two legal multi-value positions can consume it; every other caller goes through the
    /// [`Checker::check_expr`] wrapper, which forbids a tuple result.
    fn check_expr_multi(&mut self, expr: &Expr) -> Type {
        match &expr.kind {
            ExprKind::Nil => Type::Nil,
            ExprKind::Bool(_) => Type::Bool,
            ExprKind::Number(_) => Type::Number,
            ExprKind::Str(_) => Type::String,
            // Parentheses are transparent in Tier A (no value-count adjustment): `(f())` has
            // the same type as `f()`, so a paren'd pass-through stays legal.
            ExprKind::Paren(inner) => self.check_expr_multi(inner),
            ExprKind::Cond { cond, then, els } => self.check_cond(expr, cond, then, els),
            ExprKind::Name(_) => self.check_name(expr),
            ExprKind::Function(body) => self.check_function(body, None),
            ExprKind::Field { base, name } => self.check_field(base, name, expr.span),
            ExprKind::Index { base, index, .. } => self.check_index(base, index, expr.span),
            ExprKind::Call { callee, args } => self.check_call(callee, args, expr.span),
            ExprKind::MethodCall {
                receiver,
                method,
                args,
            } => self.check_method_call(receiver, method, args, expr.span),
            ExprKind::Table(fields) => self.check_table(fields, expr.span),
            ExprKind::Unary { op, operand } => self.check_unary(*op, operand, expr.span),
            ExprKind::Binary {
                op: BinOp::And | BinOp::Or,
                ..
            } => self.check_logic(expr),
            ExprKind::Binary { op, lhs, rhs } => self.check_binary(*op, lhs, rhs, expr.span),
        }
    }

    fn check_name(&mut self, expr: &Expr) -> Type {
        let Some(binding) = self.res.binding(expr.span) else {
            return Type::Error;
        };
        match binding.clone() {
            Binding::Local(id) | Binding::Upvalue(id) | Binding::Module(id) => self
                .read_narrowed(&Place::Symbol(id), expr.span)
                .unwrap_or_else(|| self.sym_types[id as usize].clone()),
            // A direct call types its callee without coming through here (`check_call`), so
            // this is a named function used as a value.
            Binding::TopFunction(name) | Binding::HostFunction(name) => {
                let closure = self.surface.closure_calling(&name);
                self.error(
                    "E0434",
                    format!(
                        "`{name}` is a named function and can only be called directly; to pass it \
                         as a value, wrap the call in a closure (`{closure}`)"
                    ),
                    expr.span,
                );
                self.named_fn_type(binding).unwrap_or(Type::Error)
            }
            Binding::TopConst(name) => self.const_types[&name].clone(),
            Binding::Memory(name) => self.cfg.memory[&name].clone(),
            Binding::Builtin(ns) => {
                self.error(
                    "E0418",
                    format!("`{ns}` is a builtin namespace and can only be used via a call"),
                    expr.span,
                );
                Type::Error
            }
        }
    }

    fn check_field(&mut self, base: &Expr, name: &Ident, span: Span) -> Type {
        // Builtin namespace value-fields (`math.pi`, `math.huge`) — catalogued in the
        // runtime builtin catalog (single source of truth).
        if let Some(ns) = self.builtin_namespace(base) {
            return match builtins::namespace_field_type(ns, &name.node) {
                Some(ty) => ty,
                None => {
                    self.error(
                        "E0419",
                        format!("`{ns}.{}` must be called", name.node),
                        name.span,
                    );
                    Type::Error
                }
            };
        }
        // A narrowed field place (`if p.name ~= nil then … p.name`) reads at its refined,
        // non-optional type inside the guarded branch (`SPEC.md` §5.3).
        if let Some(base_place) = self.place_of(base) {
            let place = Place::Field(Box::new(base_place), name.node.clone());
            if let Some(t) = self.read_narrowed(&place, span) {
                return t;
            }
        }
        let bt = self.check_expr(base);
        match self.u.shallow(&bt) {
            Type::Record(fields) => match fields.get(&name.node) {
                Some(t) => t.clone(),
                None => {
                    self.error(
                        "E0420",
                        format!("no field `{}` on {}", name.node, self.u.deep(&bt)),
                        name.span,
                    );
                    Type::Error
                }
            },
            Type::Map(v) => Type::optional(*v),
            Type::Var(_) => self.defer_field(bt, name, false, base.span),
            Type::Error => Type::Error,
            other => {
                self.no_fields(&other, &name.node, name.span);
                Type::Error
            }
        }
    }

    /// Reject using field `name` of a value of type `ty`, which has no fields (`E0421`). An
    /// optional says what to do instead, which its type may not: a `local x = nil` that nothing
    /// has been assigned to yet shows as `??`.
    fn no_fields(&mut self, ty: &Type, name: &str, span: Span) {
        let msg = match ty {
            Type::Optional(_) => format!(
                "this value may be nil; check it with `{}` before using its field `{name}`",
                self.surface.nil_test()
            ),
            other => format!("type {} has no fields", self.u.deep(other)),
        };
        self.error("E0421", msg, span);
    }

    /// Type a field access on a value whose type isn't known yet as a fresh variable, and check
    /// it once the value has a type ([`DeferredUse`]). For a write the variable is the assigned
    /// value's type; for a read, the field's.
    fn defer_field(&mut self, base: Type, name: &Ident, write: bool, span: Span) -> Type {
        let ty = self.u.fresh();
        self.deferred.push(DeferredUse {
            base,
            kind: DeferredKind::Field {
                name: name.clone(),
                ty: ty.clone(),
                write,
            },
            span,
        });
        ty
    }

    fn check_index(&mut self, base: &Expr, index: &Expr, span: Span) -> Type {
        // `t["k"]` is the place `t.k` (`place_of`), so it reads a narrowing as `t.k` does.
        if let ExprKind::Str(key) = &index.kind
            && let Some(base_place) = self.place_of(base)
        {
            let place = Place::Field(Box::new(base_place), key.clone());
            if let Some(t) = self.read_narrowed(&place, span) {
                return t;
            }
        }
        let bt = self.check_expr(base);
        match self.u.shallow(&bt) {
            Type::Array(e) => {
                self.require_number(index);
                Type::optional(*e)
            }
            Type::Map(v) => {
                let it = self.check_expr(index);
                self.require(
                    index.span,
                    &it,
                    &Type::String,
                    "E0422",
                    "map key must be a string",
                );
                Type::optional(*v)
            }
            Type::Record(fields) => self.record_key(&bt, &fields, index, false),
            Type::Error => Type::Error,
            // The base type is still unknown — drive inference from the index. A numeric
            // key implies an array (the only numerically-indexed type); a string key
            // implies a map. Either way the result is the (optional) element type.
            Type::Var(_) => {
                let it = self.check_expr(index);
                let elem = self.u.fresh();
                if matches!(self.u.shallow(&it), Type::String) {
                    let _ = self.u.unify(&bt, &Type::map(elem.clone()));
                } else {
                    self.require(index.span, &it, &Type::Number, "E0401", "expected a number");
                    let _ = self.u.unify(&bt, &Type::array(elem.clone()));
                }
                Type::optional(elem)
            }
            other => {
                self.error(
                    "E0424",
                    format!("type {} cannot be indexed", self.u.deep(&other)),
                    base.span,
                );
                Type::Error
            }
        }
    }

    /// The type of the field of record `rec` (with `fields`) that `index` names, read or (if
    /// `write`) assigned: `t["k"]` takes a string literal key, checked as `t.k` is (`SPEC.md`
    /// §5.4). A key from the Ruby surface counts from 0 only for an array, so it is used as
    /// written.
    fn record_key(
        &mut self,
        rec: &Type,
        fields: &BTreeMap<String, Type>,
        index: &Expr,
        write: bool,
    ) -> Type {
        let ExprKind::Str(key) = &index.kind else {
            self.error(
                "E0423",
                "a record can only be indexed by a string literal key",
                index.span,
            );
            return Type::Error;
        };
        match fields.get(key) {
            Some(t) => t.clone(),
            None => {
                let added = if write {
                    " (fields cannot be added)"
                } else {
                    ""
                };
                self.error(
                    "E0420",
                    format!("no field `{key}` on {}{added}", self.u.deep(rec)),
                    index.span,
                );
                Type::Error
            }
        }
    }

    fn check_call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> Type {
        // Plain builtins (`tostring`, `tonumber`). A binding belongs to a name: a callee of
        // another kind may share a name's span, as a call the Ruby translator makes up does.
        if let ExprKind::Name(_) = &callee.kind
            && let Some(Binding::Builtin(ns)) = self.res.binding(callee.span)
        {
            return self.check_builtin_value_call(ns, args, span);
        }
        // Namespace member calls (`math.floor(...)`, `string.sub(...)`).
        if let ExprKind::Field { base, name } = &callee.kind
            && let Some(ns) = self.builtin_namespace(base)
        {
            return self.check_builtin_member_call(ns, name, args, span);
        }

        // A named top-level or host function is called directly; it is not a value.
        let binding = match &callee.kind {
            ExprKind::Name(_) => self.res.binding(callee.span),
            _ => None,
        };
        let direct = binding.and_then(|b| self.named_fn_type(b));
        // The parameters whose types are declared, which a function literal passed takes: all of
        // a host function's (`None`).
        let declared: Option<Vec<bool>> = match binding {
            Some(Binding::HostFunction(_)) => None,
            Some(Binding::TopFunction(f)) => Some(self.declared_params[f].clone()),
            _ => Some(Vec::new()),
        };
        let declared = |i: usize| {
            declared
                .as_ref()
                .is_none_or(|d| d.get(i).copied().unwrap_or(false))
        };
        let ct = match direct {
            Some(t) => t,
            None => self.check_expr(callee),
        };
        match self.u.shallow(&ct) {
            Type::Function(ft) => {
                if args.len() != ft.params.len() {
                    self.error(
                        "E0430",
                        format!(
                            "this function takes {} argument(s) but {} were supplied",
                            ft.params.len(),
                            args.len()
                        ),
                        span,
                    );
                }
                let declared: Vec<bool> = (0..ft.params.len()).map(declared).collect();
                for ((arg, pt), declared) in args.iter().zip(&ft.params).zip(declared) {
                    let at = if declared {
                        self.check_arg(arg, pt)
                    } else {
                        self.check_expr(arg)
                    };
                    self.require_assignable(arg.span, &at, pt, "argument type mismatch");
                }
                // Check any surplus args so their own errors still surface.
                for arg in args.iter().skip(ft.params.len()) {
                    self.check_expr(arg);
                }
                (*ft.ret).clone()
            }
            // Not known yet: checked against the function's type once it is ([`DeferredUse`]).
            Type::Var(_) => {
                let args = args.iter().map(|a| (self.check_expr(a), a.span)).collect();
                let result = self.u.fresh();
                self.deferred.push(DeferredUse {
                    base: ct,
                    kind: DeferredKind::Call {
                        args,
                        result: result.clone(),
                        span,
                    },
                    span: callee.span,
                });
                result
            }
            Type::Error => {
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Error
            }
            other => {
                self.error(
                    "E0431",
                    format!("type {} is not callable", self.u.deep(&other)),
                    callee.span,
                );
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Error
            }
        }
    }

    /// Type a host-memory method call `mem:method(args)` (`SPEC.md` §7). v1 rules: the receiver
    /// must be a host-memory binding referenced directly by name; the method must be declared in
    /// the memory's method schema; and the call arguments (the receiver is implicit) must match
    /// the declared signature. Returns the method's result type.
    fn check_method_call(
        &mut self,
        receiver: &Expr,
        method: &Ident,
        args: &[Expr],
        span: Span,
    ) -> Type {
        // The receiver must be a bare name bound to host memory. Anything else (a local, a
        // field, a call result) can't carry methods in v1.
        let mem_name = if let ExprKind::Name(_) = &receiver.kind {
            match self.res.binding(receiver.span) {
                Some(Binding::Memory(name)) => Some(name.clone()),
                _ => None,
            }
        } else {
            None
        };
        let Some(mem_name) = mem_name else {
            // Type the receiver first so any error inside it still surfaces, then reject `:`.
            let _ = self.check_expr(receiver);
            self.error(
                "E0417",
                "a method call `:` requires a host-memory receiver",
                receiver.span,
            );
            for arg in args {
                self.check_expr(arg);
            }
            return Type::Error;
        };

        let sig = self
            .cfg
            .methods
            .get(&mem_name)
            .and_then(|m| m.get(&method.node))
            .cloned();
        let Some(sig) = sig else {
            self.error(
                "E0432",
                format!("no method `{}` on memory `{mem_name}`", method.node),
                method.span,
            );
            for arg in args {
                self.check_expr(arg);
            }
            return Type::Error;
        };

        if args.len() != sig.params.len() {
            self.error(
                "E0433",
                format!(
                    "method `{}` takes {} argument(s) but {} were supplied",
                    method.node,
                    sig.params.len(),
                    args.len()
                ),
                span,
            );
        }
        for (arg, pt) in args.iter().zip(&sig.params) {
            let at = self.check_arg(arg, pt);
            self.require_assignable(arg.span, &at, pt, "argument type mismatch");
        }
        // Check any surplus args so their own errors still surface.
        for arg in args.iter().skip(sig.params.len()) {
            self.check_expr(arg);
        }
        (*sig.ret).clone()
    }

    /// An argument for a parameter whose type, `expected`, is declared: by the host, a memory
    /// method, or a `---@param`, so known before any body is checked. A function literal there
    /// takes its parameter and return types from `expected` before its body is checked, so a
    /// misuse inside is reported where it is (`SPEC.md` §5.5). Only a declared type counts:
    /// one inferred by the time the call is checked would make the outcome depend on the order
    /// of the code, since a literal returning `T` is accepted for a `T?`.
    fn check_arg(&mut self, arg: &Expr, expected: &Type) -> Type {
        let ExprKind::Function(body) = &arg.kind else {
            return self.check_expr(arg);
        };
        let expected = match self.u.shallow(expected) {
            Type::Optional(inner) => self.u.shallow(&inner),
            t => t,
        };
        match expected {
            Type::Function(ft) if ft.params.len() == body.params.len() => {
                self.check_function(body, Some(&ft))
            }
            _ => self.check_expr(arg),
        }
    }

    /// A function literal, typed as `expected` when it is passed where that is known
    /// ([`Self::check_arg`]).
    fn check_function(&mut self, body: &FuncBody, expected: Option<&FnType>) -> Type {
        let sig = self.make_sig(&body.params);
        let mut ret_known = false;
        if let Some(ft) = expected {
            for (p, want) in sig.params.iter().zip(&ft.params) {
                let _ = self.u.unify(p, want);
            }
            // A known return type is the caller's to give, as a `---@return` is the author's.
            if !matches!(self.u.shallow(&ft.ret), Type::Var(_)) {
                ret_known = self.u.unify(&sig.ret, &ft.ret).is_ok();
            }
        }
        if !ret_known {
            self.infer_return_up_front(body, &sig);
        }
        self.func_literals.insert(body.span, sig.clone());
        self.check_func_body(body, &sig);
        Type::Function(sig)
    }

    fn check_table(&mut self, fields: &[Field], span: Span) -> Type {
        if fields.is_empty() {
            // Ambiguous shape; assume an array of a fresh element type.
            return Type::array(self.u.fresh());
        }
        let all_positional = fields.iter().all(|f| matches!(f, Field::Positional(_)));
        let all_named = fields.iter().all(|f| matches!(f, Field::Named { .. }));
        let all_keyed = fields.iter().all(|f| matches!(f, Field::Keyed { .. }));

        if all_positional {
            let elem = self.u.fresh();
            for f in fields {
                if let Field::Positional(e) = f {
                    let t = self.check_expr(e);
                    self.require(
                        e.span,
                        &t,
                        &elem,
                        "E0440",
                        "array elements must share one type",
                    );
                }
            }
            Type::array(elem)
        } else if all_named {
            let mut rec = BTreeMap::new();
            for f in fields {
                if let Field::Named { name, value } = f {
                    let t = self.check_expr(value);
                    rec.insert(name.node.clone(), self.u.deep(&t));
                }
            }
            Type::Record(rec)
        } else if all_keyed {
            // String-keyed map. (Record-from-literal-keys is folded into `name =` form.)
            let val = self.u.fresh();
            for f in fields {
                if let Field::Keyed { key, value } = f {
                    let kt = self.check_expr(key);
                    self.require(
                        key.span,
                        &kt,
                        &Type::String,
                        "E0441",
                        "map keys must be strings",
                    );
                    let vt = self.check_expr(value);
                    self.require(
                        value.span,
                        &vt,
                        &val,
                        "E0442",
                        "map values must share one type",
                    );
                }
            }
            Type::map(val)
        } else {
            self.error(
                "E0443",
                "a table must be all positional (array), all `name =` (record), or all \
                 `[key] =` (map) — these forms cannot be mixed",
                span,
            );
            Type::Error
        }
    }

    fn check_unary(&mut self, op: UnOp, operand: &Expr, span: Span) -> Type {
        match op {
            UnOp::Neg => {
                self.require_number(operand);
                Type::Number
            }
            UnOp::Not => {
                let t = self.check_expr(operand);
                let ctx = format!("`{}` requires a bool", self.surface.unop(op));
                self.require(operand.span, &t, &Type::Bool, "E0450", &ctx);
                Type::Bool
            }
            UnOp::Len => {
                let t = self.check_expr(operand);
                match self.u.shallow(&t) {
                    Type::String | Type::Array(_) | Type::Error | Type::Var(_) => Type::Number,
                    other => {
                        self.error(
                            "E0451",
                            format!(
                                "`{}` requires a string or array, found {}",
                                self.surface.unop(op),
                                self.u.deep(&other)
                            ),
                            span,
                        );
                        Type::Number
                    }
                }
            }
        }
    }

    fn check_binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, span: Span) -> Type {
        use BinOp::*;
        match op {
            Add | Sub | Mul | Div | FloorDiv | Mod | Pow => {
                for operand in [lhs, rhs] {
                    let t = self.check_expr(operand);
                    let hint = match self.u.shallow(&t) {
                        Type::String if op == Add => self.surface.string_plus_hint(),
                        _ => None,
                    };
                    match hint {
                        Some(hint) => {
                            if self.u.unify(&t, &Type::Number).is_err() {
                                let msg = self.mismatch("expected a number", &t, &Type::Number);
                                self.error("E0401", format!("{msg}; {hint}"), operand.span);
                            }
                        }
                        None => self.require(
                            operand.span,
                            &t,
                            &Type::Number,
                            "E0401",
                            "expected a number",
                        ),
                    }
                }
                Type::Number
            }
            Concat => {
                let lt = self.check_expr(lhs);
                let rt = self.check_expr(rhs);
                let ctx = format!("`{}` requires strings", self.surface.binop(op));
                self.require(lhs.span, &lt, &Type::String, "E0452", &ctx);
                self.require(rhs.span, &rt, &Type::String, "E0452", &ctx);
                Type::String
            }
            Lt | Le | Gt | Ge => {
                let lt = self.check_expr(lhs);
                let rt = self.check_expr(rhs);
                // Numeric by default; allow string-vs-string comparisons.
                let want = if matches!(self.u.shallow(&lt), Type::String)
                    || matches!(self.u.shallow(&rt), Type::String)
                {
                    Type::String
                } else {
                    Type::Number
                };
                self.require(
                    lhs.span,
                    &lt,
                    &want,
                    "E0453",
                    "comparison operand type mismatch",
                );
                self.require(
                    rhs.span,
                    &rt,
                    &want,
                    "E0453",
                    "comparison operand type mismatch",
                );
                Type::Bool
            }
            Eq | Ne => {
                let lt = self.check_expr(lhs);
                let rt = self.check_expr(rhs);
                // `x == nil` / `x ~= nil` is the narrowing idiom: a `nil` literal may be
                // compared against any optional (or any value, treated as present). Only
                // require matching types when neither side is `nil`.
                let l_nil = matches!(self.u.shallow(&lt), Type::Nil);
                let r_nil = matches!(self.u.shallow(&rt), Type::Nil);
                if !l_nil && !r_nil && self.u.unify(&lt, &rt).is_err() {
                    self.error(
                        "E0454",
                        format!(
                            "cannot compare {} with {} — equality requires matching types",
                            self.u.deep(&lt),
                            self.u.deep(&rt)
                        ),
                        span,
                    );
                }
                Type::Bool
            }
            And | Or => unreachable!("`and`/`or` are typed by `check_logic`"),
        }
    }

    /// Type an `and`/`or` expression (`SPEC.md` §3.5). It yields one of its operands, so its
    /// type is the one type that covers each value it can yield: the truthy values of the
    /// operand that ends it, and the `nil` or `false` of any operand it stops at on the way.
    /// That is worked out across a chain (`c and a or b`) before settling on one type, since
    /// an inner operator can yield a value (the `false` of `c and a`) the outer one never lets
    /// through. The type is recorded in [`TypeInfo::logic`] for lowering.
    fn check_logic(&mut self, expr: &Expr) -> Type {
        let out = self.logic_outcomes(expr);
        let ty = match (out.truthy, out.falsy) {
            (Some(t), Some(f)) => match self.join(&t, &f) {
                Some(ty) => ty,
                None => {
                    let ExprKind::Binary { op, .. } = &expr.kind else {
                        unreachable!("`check_logic` types a binary expression")
                    };
                    // What stops a chain early is `nil` or `false`, and `nil` fits any type.
                    let falsy = match self.u.deep(&f) {
                        Type::Bool | Type::Optional(_) => "`false`".to_string(),
                        other => format!("a {other}"),
                    };
                    self.error(
                        "E0455",
                        format!(
                            "this `{}` can yield {falsy} or a {}, which are not one type; to pick \
                             one of two values, write `{}`",
                            self.surface.binop(*op),
                            self.u.deep(&t),
                            self.surface.choice()
                        ),
                        expr.span,
                    );
                    Type::Error
                }
            },
            (Some(ty), None) | (None, Some(ty)) => ty,
            (None, None) => Type::Error,
        };
        self.logic_types.insert(expr.span, ty.clone());
        ty
    }

    /// A conditional expression (`SPEC.md` §3.5): a `bool` condition, and two branches whose
    /// types join as `or`'s operands do. The condition narrows each branch as it would an `if`'s,
    /// and branches assign nothing, so what is narrowed after it is what was before.
    fn check_cond(&mut self, expr: &Expr, cond: &Expr, then: &Expr, els: &Expr) -> Type {
        let entry = self.narrowed.clone();
        self.check_condition(cond);
        let narrowing = self.narrowing_from(cond);
        self.narrowed.extend(narrowing.then);
        let a = self.check_expr(then);
        self.narrowed = entry.clone();
        self.narrowed.extend(narrowing.els);
        let b = self.check_expr(els);
        self.narrowed = entry;
        let ty = match self.join(&a, &b) {
            Some(ty) => ty,
            None => {
                self.error(
                    "E0455",
                    format!(
                        "this conditional expression yields a {} or a {}, which are not one type; \
                         its branches must have the same type, apart from being optional",
                        self.u.deep(&a),
                        self.u.deep(&b)
                    ),
                    expr.span,
                );
                Type::Error
            }
        };
        self.logic_types.insert(expr.span, ty.clone());
        ty
    }

    /// What an operand of `and`/`or` can yield, looking through a chain of them.
    fn logic_outcomes(&mut self, expr: &Expr) -> Outcomes {
        let (op, lhs, rhs) = match &expr.kind {
            ExprKind::Paren(inner) => return self.logic_outcomes(inner),
            ExprKind::Binary {
                op: op @ (BinOp::And | BinOp::Or),
                lhs,
                rhs,
            } => (*op, lhs, rhs),
            _ => {
                let t = self.check_expr(expr);
                return self.outcomes_of(&t, expr.span);
            }
        };
        let l = self.logic_outcomes(lhs);
        if l.falsy.is_none()
            && let Some(t) = &l.truthy
        {
            let consequence = if op == BinOp::Or {
                "the right operand is never used".to_string()
            } else {
                format!(
                    "`{}` always yields its right operand",
                    self.surface.binop(op)
                )
            };
            self.error(
                "E0456",
                format!(
                    "the left operand of `{0}` has type {1}, which is never `nil` or `false`, so \
                     {consequence}; `{0}` needs a bool or an optional on its left",
                    self.surface.binop(op),
                    self.u.deep(t)
                ),
                lhs.span,
            );
        }
        let r = self.logic_outcomes(rhs);
        // `a and b` stops at a falsy `a`, and otherwise yields `b`; `a or b` stops at a truthy
        // `a`, and otherwise yields `b`.
        if op == BinOp::And {
            Outcomes {
                truthy: r.truthy,
                falsy: self.join_outcome(l.falsy, r.falsy, op, expr.span),
            }
        } else {
            Outcomes {
                truthy: self.join_outcome(l.truthy, r.truthy, op, expr.span),
                falsy: r.falsy,
            }
        }
    }

    /// What a value of type `t`, the operand at `span`, yields as an operand of `and`/`or`.
    fn outcomes_of(&mut self, t: &Type, span: Span) -> Outcomes {
        match self.u.shallow(t) {
            Type::Nil => Outcomes {
                truthy: None,
                falsy: Some(Type::Nil),
            },
            Type::Bool => Outcomes {
                truthy: Some(Type::Bool),
                falsy: Some(Type::Bool),
            },
            Type::Optional(inner) => {
                let falsy = match self.u.shallow(&inner) {
                    Type::Bool => Type::optional(Type::Bool),
                    // Taken to be `nil` alone, which is checked once the type is known.
                    var @ Type::Var(_) => {
                        self.logic_optionals.push((var, span));
                        Type::Nil
                    }
                    _ => Type::Nil,
                };
                Outcomes {
                    truthy: Some(*inner),
                    falsy: Some(falsy),
                }
            }
            // Not known yet: of the one type either way, as long as it isn't.
            t @ (Type::Var(_) | Type::Error) => Outcomes {
                truthy: Some(t.clone()),
                falsy: Some(t),
            },
            t => Outcomes {
                truthy: Some(t),
                falsy: None,
            },
        }
    }

    /// Join two operands' truthy (for `or`) or falsy (for `and`) values into one type, reporting
    /// a clash at `span`, the operator's expression.
    fn join_outcome(
        &mut self,
        a: Option<Type>,
        b: Option<Type>,
        op: BinOp,
        span: Span,
    ) -> Option<Type> {
        match (a, b) {
            (Some(a), Some(b)) => Some(self.join(&a, &b).unwrap_or_else(|| {
                // Only truthy values can clash: falsy ones are `nil` or `false`.
                let msg = format!(
                    "`{}` can yield a {} or a {}, which are not one type; its operands must have \
                     the same type, apart from being optional",
                    self.surface.binop(op),
                    self.u.deep(&a),
                    self.u.deep(&b)
                );
                self.error("E0455", msg, span);
                Type::Error
            })),
            (a, b) => a.or(b),
        }
    }

    /// The one type of a value that is either an `a` or a `b`: their common type, optional if
    /// either is. `None` if they have none.
    fn join(&mut self, a: &Type, b: &Type) -> Option<Type> {
        let optional = |t: Type| match t {
            Type::Optional(_) => t,
            t => Type::optional(t),
        };
        match (self.u.shallow(a), self.u.shallow(b)) {
            (Type::Error, _) | (_, Type::Error) => Some(Type::Error),
            (Type::Nil, Type::Nil) => Some(Type::Nil),
            (Type::Nil, t) | (t, Type::Nil) => Some(optional(t)),
            (Type::Optional(x), Type::Optional(y)) => {
                self.u.unify(&x, &y).is_ok().then_some(Type::Optional(x))
            }
            (Type::Optional(x), t) | (t, Type::Optional(x)) => {
                self.u.unify(&x, &t).is_ok().then_some(Type::Optional(x))
            }
            (x, y) => self.u.unify(&x, &y).is_ok().then_some(x),
        }
    }

    /// Report each optional operand of `and`/`or` whose `nil`-or-`false` values were taken to be
    /// `nil` alone, before its type was known ([`Checker::outcomes_of`]), that turned out to be a
    /// `bool?` — which can also be `false`.
    fn check_logic_optionals(&mut self) {
        for (inner, span) in std::mem::take(&mut self.logic_optionals) {
            if self.u.deep(&inner) == Type::Bool {
                let (and, or) = (
                    self.surface.binop(BinOp::And),
                    self.surface.binop(BinOp::Or),
                );
                self.error(
                    "E0455",
                    format!(
                        "`{and}`/`{or}` here took this value to be `nil` whenever it isn't \
                         truthy, but code further on makes it a `bool?`, which can also be \
                         `false`; give it its type before this point (for a local, initialize it \
                         to `false`, not `nil`)"
                    ),
                    span,
                );
            }
        }
    }

    // ---- builtins ------------------------------------------------------------

    /// Type a plain builtin call (`tostring(…)`, `tonumber(…)`) against its catalog signature.
    fn check_builtin_value_call(&mut self, ns: &str, args: &[Expr], span: Span) -> Type {
        match builtins::value_sig(ns) {
            Some(sig) => self.check_builtin_call(ns, sig, args, span),
            // `math(…)` / `string(…)`: a namespace, not a function.
            None => {
                self.error(
                    "E0431",
                    format!(
                        "`{ns}` is a builtin namespace, not a function; call one of its members"
                    ),
                    span,
                );
                for a in args {
                    self.check_expr(a);
                }
                Type::Error
            }
        }
    }

    /// Type a namespace member call (`math.floor(…)`, `string.sub(…)`) against its catalog
    /// signature.
    fn check_builtin_member_call(
        &mut self,
        ns: &str,
        name: &Ident,
        args: &[Expr],
        span: Span,
    ) -> Type {
        let Some(sig) = builtins::member_sig(ns, &name.node) else {
            self.error(
                "E0471",
                format!("`{ns}` has no builtin member `{}`", name.node),
                name.span,
            );
            for a in args {
                self.check_expr(a);
            }
            return Type::Error;
        };
        self.check_builtin_call(&format!("{ns}.{}", name.node), sig, args, span)
    }

    /// Check a builtin call's arguments against its catalog [`Sig`](builtins::Sig) — the same
    /// signature IR lowering reads its result type from, so the two cannot disagree.
    ///
    /// Arity follows [`Sig::arity`](builtins::Sig::arity). Arguments with a declared parameter
    /// type are checked like any function argument; the [`ArgRule`](builtins::ArgRule) says what
    /// the rest must be.
    fn check_builtin_call(
        &mut self,
        name: &str,
        sig: builtins::Sig,
        args: &[Expr],
        span: Span,
    ) -> Type {
        use builtins::{ArgRule, Arity};
        let (arity_ok, expected) = match sig.arity {
            Arity::Exact(n) => (args.len() == n, n.to_string()),
            Arity::AtLeast(n) => (args.len() >= n, format!("at least {n}")),
        };
        if !arity_ok {
            self.error(
                "E0430",
                format!(
                    "`{name}` takes {expected} argument(s) but {} were supplied",
                    args.len()
                ),
                span,
            );
        }
        for (arg, pt) in args.iter().zip(&sig.params) {
            let at = self.check_expr(arg);
            self.require_assignable(arg.span, &at, pt, "argument type mismatch");
        }
        for arg in args.iter().skip(sig.params.len()) {
            let t = self.check_expr(arg);
            match sig.rule {
                // Surplus arguments: already reported as an arity error.
                ArgRule::Fixed => {}
                // `string.format`'s values are formatted by their verbs, whatever their type.
                ArgRule::FormatVariadic => {}
                ArgRule::Scalar => match self.u.shallow(&t) {
                    Type::Number | Type::Bool | Type::String | Type::Error | Type::Var(_) => {}
                    other => self.error(
                        "E0470",
                        format!(
                            "`{name}` accepts a number, bool, or string, found {}",
                            self.u.deep(&other)
                        ),
                        arg.span,
                    ),
                },
            }
        }
        sig.ret
    }

    /// The signature of a named top-level or host function, or `None` for any other binding.
    fn named_fn_type(&self, binding: &Binding) -> Option<Type> {
        match binding {
            Binding::TopFunction(name) => Some(Type::Function(self.func_sigs[name].clone())),
            Binding::HostFunction(name) => {
                Some(Type::Function(self.cfg.host_functions[name].clone()))
            }
            _ => None,
        }
    }

    /// If `expr` is a name bound to a builtin namespace, return its name.
    fn builtin_namespace(&self, expr: &Expr) -> Option<&'static str> {
        if let ExprKind::Name(_) = &expr.kind
            && let Some(Binding::Builtin(ns)) = self.res.binding(expr.span)
        {
            return Some(ns);
        }
        None
    }

    // ---- assignment places & narrowing --------------------------------------

    /// The type of an assignable l-value (used as the *write* target type).
    fn place_type(&mut self, target: &Expr) -> Type {
        match &target.kind {
            // A write takes the variable's declared type, not a narrowing of it: `x = nil` is
            // fine where a guard narrowed an optional `x`, and ends that narrowing.
            ExprKind::Name(_) => match self.res.binding(target.span) {
                Some(Binding::Local(id) | Binding::Upvalue(id) | Binding::Module(id)) => {
                    self.sym_types[*id as usize].clone()
                }
                _ => self.check_name(target),
            },
            ExprKind::Field { base, name } => {
                let bt = self.check_expr(base);
                match self.u.shallow(&bt) {
                    Type::Record(fields) => match fields.get(&name.node) {
                        Some(t) => t.clone(),
                        None => {
                            self.error(
                                "E0420",
                                format!(
                                    "no field `{}` on {} (fields cannot be added)",
                                    name.node,
                                    self.u.deep(&bt)
                                ),
                                name.span,
                            );
                            Type::Error
                        }
                    },
                    Type::Map(v) => *v,
                    Type::Var(_) => self.defer_field(bt, name, true, base.span),
                    Type::Error => Type::Error,
                    other => {
                        self.no_fields(&other, &name.node, name.span);
                        Type::Error
                    }
                }
            }
            ExprKind::Index { base, index, .. } => {
                let bt = self.check_expr(base);
                match self.u.shallow(&bt) {
                    Type::Array(e) => {
                        self.require_number(index);
                        *e
                    }
                    Type::Map(v) => {
                        let it = self.check_expr(index);
                        self.require(
                            index.span,
                            &it,
                            &Type::String,
                            "E0422",
                            "map key must be a string",
                        );
                        *v
                    }
                    Type::Record(fields) => self.record_key(&bt, &fields, index, true),
                    // Not known yet: the index decides, as it does for a read (`check_index`).
                    Type::Var(_) => {
                        let it = self.check_expr(index);
                        let elem = self.u.fresh();
                        if matches!(self.u.shallow(&it), Type::String) {
                            let _ = self.u.unify(&bt, &Type::map(elem.clone()));
                        } else {
                            self.require(
                                index.span,
                                &it,
                                &Type::Number,
                                "E0401",
                                "expected a number",
                            );
                            let _ = self.u.unify(&bt, &Type::array(elem.clone()));
                        }
                        elem
                    }
                    Type::Error => Type::Error,
                    other => {
                        self.error(
                            "E0424",
                            format!("type {} cannot be indexed", self.u.deep(&other)),
                            base.span,
                        );
                        Type::Error
                    }
                }
            }
            _ => self.check_expr(target),
        }
    }

    /// Map an lvalue-ish expression to the [`Place`] it denotes, if it is a narrowable
    /// location (a bare local/upvalue, a memory root, or a field path over one). A string
    /// literal key names a field as `.` does (`t["k"]` is `t.k`), so writing either ends a
    /// guard on the other. Anything else (calls, other indexing, literals) is not a place and
    /// yields `None`.
    fn place_of(&self, expr: &Expr) -> Option<Place> {
        match &expr.kind {
            ExprKind::Paren(inner) => self.place_of(inner),
            // A module variable narrows like a local: only top-level code assigns one, and a
            // function it calls can't, so a guard in either still holds where it is checked.
            ExprKind::Name(_) => match self.res.binding(expr.span)? {
                Binding::Local(id) | Binding::Upvalue(id) | Binding::Module(id) => {
                    Some(Place::Symbol(*id))
                }
                Binding::Memory(name) => Some(Place::Memory(name.clone())),
                _ => None,
            },
            ExprKind::Field { base, name } => Some(Place::Field(
                Box::new(self.place_of(base)?),
                name.node.clone(),
            )),
            ExprKind::Index { base, index, .. } => match &index.kind {
                ExprKind::Str(key) => {
                    Some(Place::Field(Box::new(self.place_of(base)?), key.clone()))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// The current static type of a [`Place`] — the narrowing overlay if present, else the
    /// declared type: a symbol's inferred type, a memory root's schema type, or a record
    /// field of the base's type.
    fn place_current_type(&self, place: &Place) -> Option<Type> {
        if let Some(t) = self.narrowed.get(place) {
            return Some(t.clone());
        }
        match place {
            Place::Symbol(id) => Some(self.sym_types[*id as usize].clone()),
            Place::Memory(name) => self.cfg.memory.get(name).cloned(),
            Place::Field(base, field) => match self.u.shallow(&self.place_current_type(base)?) {
                Type::Record(fields) => fields.get(field).cloned(),
                _ => None,
            },
        }
    }

    /// The refined type of `place` if a `~= nil` guard currently narrows it, recorded against
    /// the read at `span` so lowering unwraps that read to the same type the checker used.
    fn read_narrowed(&mut self, place: &Place, span: Span) -> Option<Type> {
        let t = self.narrowed.get(place)?.clone();
        self.narrowed_reads.insert(span, t.clone());
        Some(t)
    }

    /// What an `if` condition says about a place: `<place> ~= nil` refines it in the
    /// then-branch, and `<place> == nil` in the else-branch and the `elseif` arms after it,
    /// where `<place>` is a bare name **or** a field path (`p.name`). `nil` may be on either
    /// side, and `not` swaps the two branches. Each refinement is the place with its narrowed
    /// (non-optional) type.
    fn narrowing_from(&self, cond: &Expr) -> Narrowing {
        let none = Narrowing {
            then: None,
            els: None,
        };
        let (eq, lhs, rhs) = match &cond.kind {
            ExprKind::Paren(inner) => return self.narrowing_from(inner),
            ExprKind::Unary {
                op: UnOp::Not,
                operand,
            } => {
                let Narrowing { then, els } = self.narrowing_from(operand);
                return Narrowing {
                    then: els,
                    els: then,
                };
            }
            ExprKind::Binary {
                op: op @ (BinOp::Eq | BinOp::Ne),
                lhs,
                rhs,
            } => (*op == BinOp::Eq, lhs, rhs),
            _ => return none,
        };
        let place_expr = match (&lhs.kind, &rhs.kind) {
            (_, ExprKind::Nil) => lhs,
            (ExprKind::Nil, _) => rhs,
            _ => return none,
        };
        let Some(place) = self.place_of(place_expr) else {
            return none;
        };
        let refined = match self.place_current_type(&place).map(|t| self.u.shallow(&t)) {
            Some(Type::Optional(inner)) => Some((place, *inner)),
            _ => None,
        };
        if eq {
            Narrowing {
                then: None,
                els: refined,
            }
        } else {
            Narrowing {
                then: refined,
                els: None,
            }
        }
    }

    // ---- requirement helpers -------------------------------------------------

    fn check_condition(&mut self, cond: &Expr) {
        let t = self.check_expr(cond);
        // An optional isn't a condition; where the surface has a spelling for testing whether
        // it's nil, say it.
        if let (Type::Optional(inner), Some(hint)) =
            (self.u.shallow(&t), self.surface.optional_condition_hint())
            && self.u.shallow(&inner) != Type::Bool
        {
            let msg = self.mismatch("condition must be a bool", &t, &Type::Bool);
            self.error("E0400", format!("{msg}; {hint}"), cond.span);
            return;
        }
        self.require(
            cond.span,
            &t,
            &Type::Bool,
            "E0400",
            "condition must be a bool",
        );
    }

    fn require_number(&mut self, expr: &Expr) {
        let t = self.check_expr(expr);
        self.require(expr.span, &t, &Type::Number, "E0401", "expected a number");
    }

    /// Unify `actual` with `expected`, emitting a diagnostic on mismatch.
    fn require(
        &mut self,
        span: Span,
        actual: &Type,
        expected: &Type,
        code: &'static str,
        ctx: &str,
    ) {
        if self.u.unify(actual, expected).is_err() {
            let msg = self.mismatch(ctx, actual, expected);
            self.error(code, msg, span);
        }
    }

    /// `"{ctx}: expected {expected}, found {actual}"`, except that an optional which narrowing
    /// would fix is described as one. Its type may not say what it is: a `local x = nil` that
    /// nothing has been assigned to yet shows as `??`.
    fn mismatch(&self, ctx: &str, actual: &Type, expected: &Type) -> String {
        let expected = self.u.deep(expected);
        let found = match self.u.deep(actual) {
            Type::Optional(inner) if matches!(*inner, Type::Var(_)) || *inner == expected => {
                format!(
                    "a value that may be nil; check it with `{}` first",
                    self.surface.nil_test()
                )
            }
            other => other.to_string(),
        };
        format!("{ctx}: expected {expected}, found {found}")
    }

    /// Like [`Self::require`] but with optional widening (`T`/`nil` assignable to `T?`).
    fn require_assignable(&mut self, span: Span, from: &Type, to: &Type, ctx: &str) {
        if self.is_assignable(from, to) {
            return;
        }
        let msg = self.mismatch(ctx, from, to);
        self.error("E0402", msg, span);
    }

    fn is_assignable(&mut self, from: &Type, to: &Type) -> bool {
        let f = self.u.shallow(from);
        let t = self.u.shallow(to);
        if matches!(f, Type::Error) || matches!(t, Type::Error) {
            return true;
        }
        // Optional widening.
        if let Type::Optional(inner) = &t {
            if matches!(f, Type::Nil) {
                return true;
            }
            if let Type::Optional(fi) = &f {
                return self.is_assignable(fi, inner);
            }
            return self.is_assignable(&f, inner);
        }
        // A multi-value `return` into a tuple return type, element by element (§5.5).
        if let (Type::Tuple(from_elems), Type::Tuple(to_elems)) = (&f, &t) {
            return from_elems.len() == to_elems.len()
                && from_elems
                    .iter()
                    .zip(to_elems)
                    .all(|(fe, te)| self.is_assignable(fe, te));
        }
        self.u.unify(&f, &t).is_ok()
    }
}

// ---- reachability -------------------------------------------------------------

/// Whether control can never reach the end of `block`: every path through it ends in a
/// `return` or loops forever.
///
/// Only called on a function body and the blocks nested in it outside any loop, so a `break`
/// never has to be considered here — one can only occur inside a loop, and a loop is only ever
/// treated as not finishing when it has no `break` ([`stat_always_returns`]).
pub(crate) fn always_returns(block: &Block) -> bool {
    block.ret.is_some() || block.stats.iter().any(stat_always_returns)
}

fn stat_always_returns(stat: &Stat) -> bool {
    match &stat.kind {
        StatKind::Do(block) => always_returns(block),
        StatKind::If {
            arms,
            else_block: Some(else_block),
        } => arms.iter().all(|(_, b)| always_returns(b)) && always_returns(else_block),
        // `while true` without a `break` can only be left by returning.
        StatKind::While { cond, body } => is_literal_true(cond) && !breaks_out(body),
        _ => false,
    }
}

/// Whether `block` (a loop body) contains a `break` that leaves that loop — not one belonging to
/// a loop nested inside it.
fn breaks_out(block: &Block) -> bool {
    block.stats.iter().any(|stat| match &stat.kind {
        StatKind::Break => true,
        StatKind::Do(b) => breaks_out(b),
        StatKind::If { arms, else_block } => {
            arms.iter().any(|(_, b)| breaks_out(b)) || else_block.as_ref().is_some_and(breaks_out)
        }
        _ => false,
    })
}

fn is_literal_true(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Bool(b) => *b,
        ExprKind::Paren(inner) => is_literal_true(inner),
        _ => false,
    }
}

fn is_literal_nil(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Nil => true,
        ExprKind::Paren(inner) => is_literal_nil(inner),
        _ => false,
    }
}

/// Whether `e` is a call, the only expression that can have no value.
fn is_call(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Call { .. } | ExprKind::MethodCall { .. } => true,
        ExprKind::Paren(inner) => is_call(inner),
        _ => false,
    }
}

/// The `E0435` message: a call to a function that returns nothing, used as a value.
const VALUELESS_CALL: &str = "this call returns no value, so there is none to use here; make \
                              the call a statement of its own";

/// Call `f` on every `return` in `block` and the blocks nested in it, but not on those in the
/// bodies of functions defined inside it: those return from a different function.
fn visit_returns(block: &Block, f: &mut impl FnMut(&RetStat)) {
    for stat in &block.stats {
        match &stat.kind {
            StatKind::Do(body)
            | StatKind::While { body, .. }
            | StatKind::NumericFor { body, .. }
            | StatKind::GenericFor { body, .. } => visit_returns(body, f),
            StatKind::If { arms, else_block } => {
                for (_, body) in arms {
                    visit_returns(body, f);
                }
                if let Some(body) = else_block {
                    visit_returns(body, f);
                }
            }
            _ => {}
        }
    }
    if let Some(ret) = &block.ret {
        f(ret);
    }
}

/// The span of the `end` keyword closing `body`, which is where a function that can finish
/// without returning is reported. A function body's span always ends with that keyword.
fn end_keyword(body: &FuncBody) -> Span {
    Span {
        start: body.span.end - "end".len() as u32,
        end: body.span.end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze(src: &str, cfg: &TypeConfig) -> Result<TypeInfo, Diagnostics> {
        let module = crate::parse(src).expect("parse should succeed");
        let res = crate::resolve::resolve(&module, &cfg.to_resolve_config())
            .expect("resolve should succeed");
        typecheck(&module, &res, cfg)
    }

    fn ok(src: &str, cfg: &TypeConfig) -> TypeInfo {
        analyze(src, cfg).unwrap_or_else(|d| panic!("typecheck failed: {d}"))
    }

    fn err_code(src: &str, cfg: &TypeConfig) -> String {
        analyze(src, cfg).unwrap_err().0[0].code.to_string()
    }

    fn empty() -> TypeConfig {
        TypeConfig::default()
    }

    #[test]
    fn infers_numeric_params_from_usage() {
        let info = ok("function add(a, b) return a + b end", &empty());
        let f = &info.exports["add"];
        assert_eq!(f.to_string(), "fn(number, number) -> number");
    }

    #[test]
    fn infers_const_types() {
        let info = ok("K = 1 + 2\nNAME = \"hi\"\nFLAG = true", &empty());
        assert_eq!(info.exports["K"], Type::Number);
        assert_eq!(info.exports["NAME"], Type::String);
        assert_eq!(info.exports["FLAG"], Type::Bool);
    }

    #[test]
    fn mutual_recursion_infers_bool_return() {
        let info = ok(
            "function even(n)\n\
               if n == 0 then return true end\n\
               return odd(n - 1)\n\
             end\n\
             function odd(n)\n\
               if n == 0 then return false end\n\
               return even(n - 1)\n\
             end",
            &empty(),
        );
        assert_eq!(info.exports["even"].to_string(), "fn(number) -> bool");
    }

    #[test]
    fn array_literal_and_length() {
        let info = ok(
            "function f()\n\
               local xs = { 1, 2, 3 }\n\
               return #xs\n\
             end",
            &empty(),
        );
        assert_eq!(info.exports["f"].to_string(), "fn() -> number");
    }

    #[test]
    fn record_literal_is_inferred() {
        let info = ok("P = { x = 1, y = 2 }", &empty());
        assert_eq!(
            info.exports["P"].to_string(),
            "record { x: number, y: number }"
        );
    }

    #[test]
    fn builtin_math_call_types() {
        let info = ok("function f(x) return math.floor(x) end", &empty());
        assert_eq!(info.exports["f"].to_string(), "fn(number) -> number");
    }

    #[test]
    fn builtin_calls_are_checked_against_the_catalog() {
        let cases = [
            // `tonumber(string)`: an argument of the wrong type, like any function argument.
            ("function f() return tonumber(1) end", "E0402"),
            ("function f() return tostring(1, 2) end", "E0430"),
            ("function f() return tostring({ 1 }) end", "E0470"),
            ("function f() return math.floor(\"x\") end", "E0402"),
            ("function f() return math.max(1) end", "E0430"),
            ("function f() return math.nope(1) end", "E0471"),
            ("function f() return string.format() end", "E0430"),
            ("function f() return string.format(1) end", "E0402"),
            ("function f() return math(1) end", "E0431"),
        ];
        for (src, code) in cases {
            assert_eq!(err_code(src, &empty()), code, "{src}");
        }
        // `string.format` takes any values after the format string.
        ok(
            "function f() return string.format(\"%s %d\", \"a\", 1) end",
            &empty(),
        );
    }

    #[test]
    fn iterating_a_non_collection_is_its_own_error() {
        assert_eq!(
            err_code(
                "function f() for _, v in ipairs(\"abc\") do end end",
                &empty()
            ),
            "E0412"
        );
        assert_eq!(
            err_code(
                "function f() local p = { a = 1 } for k in pairs(p) do end end",
                &empty()
            ),
            "E0412"
        );
    }

    #[test]
    fn condition_must_be_bool() {
        // A known non-bool condition is rejected. (A bare param `if x then` instead
        // *infers* `x: bool`, which is correct and exercised elsewhere.)
        assert_eq!(
            err_code("function f() if 1 then return 1 end return 0 end", &empty()),
            "E0400"
        );
    }

    #[test]
    fn bare_param_condition_infers_bool() {
        let info = ok(
            "function f(x) if x then return 1 end return 0 end",
            &empty(),
        );
        assert_eq!(info.exports["f"].to_string(), "fn(bool) -> number");
    }

    #[test]
    fn arithmetic_on_string_is_error() {
        assert_eq!(
            err_code("function f() return \"a\" + 1 end", &empty()),
            "E0401"
        );
    }

    #[test]
    fn equality_requires_matching_types() {
        assert_eq!(
            err_code("function f() return 1 == \"a\" end", &empty()),
            "E0454"
        );
    }

    #[test]
    fn call_arity_is_checked() {
        assert_eq!(
            err_code(
                "function g(a) return a end\nfunction f() return g(1, 2) end",
                &empty()
            ),
            "E0430"
        );
    }

    #[test]
    fn argument_type_is_checked() {
        assert_eq!(
            err_code(
                "function g(a) return a + 1 end\nfunction f() return g(\"x\") end",
                &empty()
            ),
            "E0402"
        );
    }

    #[test]
    fn ambiguous_param_is_rejected() {
        // `x` is passed through without any type-determining use.
        assert_eq!(err_code("function f(x) return x end", &empty()), "E0410");
    }

    #[test]
    fn annotation_types_an_unused_param() {
        // The keystone: `---@param` pins an otherwise-untypeable parameter (no E0410).
        let info = ok("---@param x number\nfunction f(x) return 1 end", &empty());
        assert_eq!(info.exports["f"].to_string(), "fn(number) -> number");
    }

    #[test]
    fn annotation_expresses_a_record_param() {
        // Record-typed parameters — impossible by inference alone — become expressible, and
        // the annotated shape is visible to the body's field access.
        let info = ok(
            "---@param p { hp: number, tag: string }\nfunction f(p) return p.hp end",
            &empty(),
        );
        assert_eq!(
            info.exports["f"].to_string(),
            "fn(record { hp: number, tag: string }) -> number"
        );
    }

    #[test]
    fn annotation_optional_and_array_and_map() {
        let info = ok(
            "---@param a number[]\n\
             ---@param m { [string]: bool }\n\
             ---@param s string?\n\
             function f(a, m, s) return #a end",
            &empty(),
        );
        assert_eq!(
            info.exports["f"].to_string(),
            "fn(array<number>, map<string, bool>, string?) -> number"
        );
    }

    #[test]
    fn annotation_return_type() {
        let info = ok("---@return number\nfunction f() return 5 end", &empty());
        assert_eq!(info.exports["f"].to_string(), "fn() -> number");
    }

    #[test]
    fn annotation_contradicting_usage_is_error() {
        // `x` annotated `string` but used in arithmetic — the annotation is respected, so the
        // numeric use fails.
        assert_eq!(
            err_code(
                "---@param x string\nfunction f(x) return x + 1 end",
                &empty()
            ),
            "E0401",
        );
    }

    #[test]
    fn annotation_unknown_param_is_error() {
        assert_eq!(
            err_code(
                "---@param y number\nfunction f(x) return x + 0 end",
                &empty()
            ),
            "E0461",
        );
    }

    #[test]
    fn annotation_unknown_type_is_error() {
        assert_eq!(
            err_code("---@param x widget\nfunction f(x) return 1 end", &empty()),
            "E0462",
        );
    }

    #[test]
    fn conflicting_duplicate_annotations_error() {
        // Two `@param` lines for the same parameter with incompatible types: the second pin
        // fails against the first.
        assert_eq!(
            err_code(
                "---@param x number\n---@param x string\nfunction f(x) return 1 end",
                &empty(),
            ),
            "E0460",
        );
    }

    #[test]
    fn field_narrowing_refines_optional_field() {
        // `SPEC.md` §5.3: `p.b` is `number?`; the `~= nil` guard refines it to `number` in
        // the branch, so both returns unify to `number` and the module type-checks.
        let info = ok(
            "function f()\n\
               local p = { b = tonumber(\"5\") }\n\
               if p.b ~= nil then return p.b end\n\
               return 0\n\
             end",
            &empty(),
        );
        assert_eq!(info.exports["f"].to_string(), "fn() -> number");
    }

    #[test]
    fn field_without_narrowing_stays_optional() {
        // Without the guard, `p.b : number?` cannot unify with the `number` return — the
        // refinement is genuinely what makes the narrowed program check.
        assert_eq!(
            err_code(
                "function f()\n\
                   local p = { b = tonumber(\"5\") }\n\
                   return p.b + 0\n\
                 end",
                &empty(),
            ),
            "E0401",
        );
    }

    #[test]
    fn write_invalidates_field_narrowing() {
        // Reassigning the narrowed field inside the branch drops the refinement, so the
        // subsequent use is `number?` again and fails to unify with the `number` return.
        assert_eq!(
            err_code(
                "function f()\n\
                   local p = { b = tonumber(\"5\") }\n\
                   if p.b ~= nil then\n\
                     p.b = tonumber(\"9\")\n\
                     return p.b\n\
                   end\n\
                   return 0\n\
                 end",
                &empty(),
            ),
            "E0413",
        );
    }

    #[test]
    fn a_record_field_is_written_by_its_literal_key() {
        // `SPEC.md` §5.4: `t["k"] = v` assigns the field `t.k`, checked as `t.k = v` is.
        let info = ok(
            "function f()\n\
               local t = { a = 0, inner = { v = 1 } }\n\
               t[\"a\"] = 1\n\
               t[\"inner\"][\"v\"] = t[\"a\"]\n\
               return t[\"inner\"].v\n\
             end",
            &empty(),
        );
        assert_eq!(info.exports["f"].to_string(), "fn() -> number");
        for (write, code) in [
            ("t[\"zz\"] = 1", "E0420"),
            ("t[k] = 1", "E0423"),
            ("t[\"a\"] = \"x\"", "E0402"),
            ("t[\"a\"] = nil", "E0402"),
        ] {
            let src = format!(
                "---@param k string\n\
                 function f(k)\n\
                   local t = {{ a = 0 }}\n\
                   {write}\n\
                 end"
            );
            assert_eq!(err_code(&src, &empty()), code, "{write}");
        }
    }

    #[test]
    fn a_literal_key_is_the_same_place_as_the_field() {
        // `t["k"]` is `t.k`: a guard on either narrows a read of either…
        for (guard, read) in [
            ("t[\"b\"]", "t[\"b\"]"),
            ("t[\"b\"]", "t.b"),
            ("t.b", "t[\"b\"]"),
        ] {
            let info = ok(
                &format!(
                    "function f()\n\
                       local t = {{ b = tonumber(\"5\") }}\n\
                       if {guard} ~= nil then return {read} + 1 end\n\
                       return 0\n\
                     end"
                ),
                &empty(),
            );
            assert_eq!(info.exports["f"].to_string(), "fn() -> number", "{guard}");
        }
        // …and writing either ends it, including a write through the key to a field under it.
        for (guard, write, read) in [
            ("t.b", "t[\"b\"] = nil", "t.b"),
            ("t[\"b\"]", "t.b = nil", "t[\"b\"]"),
            ("t.c.d", "t[\"c\"].d = nil", "t.c.d"),
            ("t.c.d", "t[\"c\"][\"d\"] = nil", "t.c.d"),
        ] {
            let src = format!(
                "function f()\n\
                   local t = {{ b = tonumber(\"5\"), c = {{ d = tonumber(\"6\") }} }}\n\
                   if {guard} ~= nil then\n\
                     {write}\n\
                     return {read} + 1\n\
                   end\n\
                   return 0\n\
                 end"
            );
            assert_eq!(err_code(&src, &empty()), "E0401", "{write}");
        }
    }

    #[test]
    fn eq_nil_narrows_the_else_branch_and_later_arms() {
        // `SPEC.md` §5.3: where `x == nil` is false, `x` reads as `number`, in the `else`
        // branch and in the conditions and bodies of later `elseif` arms.
        let info = ok(
            "function f(s, c)\n\
               local x = tonumber(s)\n\
               if x == nil then\n\
                 return 0\n\
               elseif x > 10 then\n\
                 return x - 10\n\
               elseif c then\n\
                 return x * 2\n\
               else\n\
                 return x\n\
               end\n\
             end",
            &empty(),
        );
        assert_eq!(info.exports["f"].to_string(), "fn(string, bool) -> number");
        // `nil` on the left, `not`, and parentheses read the same way.
        for cond in [
            "nil == x",
            "(x == nil)",
            "not (x ~= nil)",
            "not not (x == nil)",
        ] {
            ok(
                &format!(
                    "function f(s)\n\
                       local x = tonumber(s)\n\
                       if {cond} then return 0 else return x end\n\
                     end"
                ),
                &empty(),
            );
        }
    }

    #[test]
    fn eq_nil_does_not_narrow_the_then_branch() {
        assert_eq!(
            err_code(
                "function f(s)\n\
                   local x = tonumber(s)\n\
                   if x == nil then return x + 1 end\n\
                   return 0\n\
                 end",
                &empty(),
            ),
            "E0401",
        );
    }

    #[test]
    fn a_write_in_one_branch_leaves_the_next_narrowed() {
        // Branches are separate paths: `x = nil` in the first `elseif` body doesn't reach the
        // second, which only runs when `x == nil` was false.
        ok(
            "function f(s, c)\n\
               local x = tonumber(s)\n\
               if x == nil then\n\
                 return 0\n\
               elseif c then\n\
                 x = nil\n\
                 return 1\n\
               else\n\
                 return x\n\
               end\n\
             end",
            &empty(),
        );
    }

    #[test]
    fn a_write_in_any_branch_ends_an_outer_narrowing() {
        // After an inner `if`, `x` is narrowed only if no branch of it wrote `x`, whichever
        // branch wrote it.
        for body in [
            "if c then x = nil end",
            "if c then else x = nil end",
            "if c then elseif not c then x = nil end",
        ] {
            assert_eq!(
                err_code(
                    &format!(
                        "function f(s, c)\n\
                           local x = tonumber(s)\n\
                           if x ~= nil then\n\
                             {body}\n\
                             return x + 1\n\
                           end\n\
                           return 0\n\
                         end"
                    ),
                    &empty(),
                ),
                "E0401",
                "{body}",
            );
        }
        // And one no branch writes stays narrowed.
        ok(
            "function f(s, c)\n\
               local x = tonumber(s)\n\
               if x ~= nil then\n\
                 if c then local y = 1 else local z = 2 end\n\
                 return x + 1\n\
               end\n\
               return 0\n\
             end",
            &empty(),
        );
    }

    #[test]
    fn a_write_later_in_a_loop_ends_a_narrowing_from_outside_it() {
        // The write runs before the next iteration's read, so no read in the loop, nor its
        // condition, sees `x` narrowed.
        for (lp, code) in [
            ("while i < 2 do", "E0401"),
            ("for j = 1, 2 do", "E0401"),
            ("for _, v in ipairs({ 1, 2 }) do", "E0401"),
            // The condition too: an ordering comparison of an optional.
            ("while i < x do", "E0453"),
        ] {
            assert_eq!(
                err_code(
                    &format!(
                        "function f(s)\n\
                           local x = tonumber(s)\n\
                           local n, i = 0, 0\n\
                           if x ~= nil then\n\
                             {lp}\n\
                               n = n + x\n\
                               if n > 5 then x = nil end\n\
                               i = i + 1\n\
                             end\n\
                           end\n\
                           return n\n\
                         end"
                    ),
                    &empty(),
                ),
                code,
                "{lp}",
            );
        }
        // A loop that doesn't write `x` keeps it narrowed.
        ok(
            "function f(s)\n\
               local x = tonumber(s)\n\
               local n = 0\n\
               if x ~= nil then\n\
                 for j = 1, 2 do n = n + x end\n\
               end\n\
               return n\n\
             end",
            &empty(),
        );
    }

    #[test]
    fn a_narrowed_local_can_be_set_to_nil() {
        // A write is checked against the declared `number?`, not the narrowed `number`.
        ok(
            "function f(s)\n\
               local x = tonumber(s)\n\
               if x ~= nil then\n\
                 local y = x + 1\n\
                 x = nil\n\
                 return y\n\
               end\n\
               return 0\n\
             end",
            &empty(),
        );
    }

    #[test]
    fn eq_nil_narrowing_ends_with_the_if() {
        assert_eq!(
            err_code(
                "function f(s)\n\
                   local x = tonumber(s)\n\
                   if x == nil then local y = 1 else local z = 2 end\n\
                   return x + 1\n\
                 end",
                &empty(),
            ),
            "E0401",
        );
    }

    #[test]
    fn mixed_table_shape_is_rejected() {
        assert_eq!(err_code("P = { 1, x = 2 }", &empty()), "E0443");
    }

    #[test]
    fn record_field_access_unknown_field() {
        assert_eq!(
            err_code(
                "function f()\n\
                   local p = { a = 1 }\n\
                   return p.b\n\
                 end",
                &empty()
            ),
            "E0420"
        );
    }

    #[test]
    fn multi_return_infers_a_tuple_type() {
        let info = ok("function f() return 1, 2 end", &empty());
        assert_eq!(info.exports["f"].to_string(), "fn() -> (number, number)");
    }

    #[test]
    fn parallel_binding_unpacks_a_multi_return_call() {
        let info = ok(
            "function divmod(a, b) return a // b, a % b end\n\
             function use(x, y) local q, r = divmod(x, y) return q + r end",
            &empty(),
        );
        assert_eq!(
            info.exports["use"].to_string(),
            "fn(number, number) -> number"
        );
    }

    #[test]
    fn parallel_assignment_unpacks_a_multi_return_call() {
        let info = ok(
            "function pair() return 1, 2 end\n\
             function use() local a = 0\n local b = 0\n a, b = pair() return a + b end",
            &empty(),
        );
        assert_eq!(info.exports["use"].to_string(), "fn() -> number");
    }

    #[test]
    fn return_can_pass_through_a_multi_return_call() {
        let info = ok(
            "function pair() return 1, 2 end\n\
             function forward() return pair() end",
            &empty(),
        );
        assert_eq!(
            info.exports["forward"].to_string(),
            "fn() -> (number, number)"
        );
    }

    #[test]
    fn multi_return_annotation_builds_a_tuple() {
        let info = ok(
            "---@param x bool\n\
             ---@return number\n\
             ---@return string\n\
             function f(x) if x then return 1, \"a\" end return 2, \"b\" end",
            &empty(),
        );
        assert_eq!(
            info.exports["f"].to_string(),
            "fn(bool) -> (number, string)"
        );
    }

    #[test]
    fn binding_arity_mismatch_is_error() {
        // A 2-value call bound to a single name.
        assert_eq!(
            err_code(
                "function pair() return 1, 2 end\nfunction f() local x = pair() return x end",
                &empty()
            ),
            "E0414"
        );
    }

    #[test]
    fn multi_return_in_value_position_is_error() {
        assert_eq!(
            err_code(
                "function pair() return 1, 2 end\nfunction f() return pair() + 1 end",
                &empty()
            ),
            "E0415"
        );
    }

    #[test]
    fn multi_return_as_call_argument_is_error() {
        // Tier A: a multi-value call does not spread into an argument list. Single-param callee
        // so the arity is fine and the tuple guard is unambiguously what fires.
        assert_eq!(
            err_code(
                "function pair() return 1, 2 end\n\
                 function id(a) return a end\n\
                 function f() return id(pair()) end",
                &empty()
            ),
            "E0415"
        );
    }

    #[test]
    fn mixing_multi_return_into_a_list_is_error() {
        assert_eq!(
            err_code(
                "function pair() return 1, 2 end\n\
                 function f() local a, b = pair(), 5 return a + b end",
                &empty()
            ),
            "E0415"
        );
    }

    #[test]
    fn nesting_multi_return_in_a_return_list_is_error() {
        assert_eq!(
            err_code(
                "function pair() return 1, 2 end\nfunction f() return pair(), 3 end",
                &empty()
            ),
            "E0415"
        );
    }

    #[test]
    fn inconsistent_return_arity_is_error() {
        assert_eq!(
            err_code(
                "function f(b) if b then return 1, 2 end return 3 end",
                &empty()
            ),
            "E0413"
        );
    }

    #[test]
    fn return_type_must_be_consistent() {
        assert_eq!(
            err_code(
                "function f(b)\n\
                   if b == 0 then return 1 end\n\
                   return \"x\"\n\
                 end",
                &empty()
            ),
            "E0413"
        );
    }

    // ---- a call that returns nothing (E0435) ----

    #[test]
    fn a_valueless_call_used_as_a_value_is_error() {
        for body in [
            "local x = f()",
            "local x = 0 x = f()",
            "local a, b = f()",
            "return g(f())",
            "return f() == nil",
            "return (f()) + 1",
            "return f() or 1",
            "return #{ f() }",
            "return f(), 1",
            "if f() then return end",
            "local h = function() end local x = h()",
        ] {
            let src = format!(
                "function f() end\nfunction g(x) return x + 1 end\nfunction main() {body} end"
            );
            assert_eq!(err_code(&src, &empty()), "E0435", "{body}");
        }
    }

    #[test]
    fn a_valueless_call_is_error_wherever_its_function_is() {
        for src in [
            // The callee comes after the call.
            "function main() local x = f() end\nfunction f() end",
            // ...and the result is used in a way that would otherwise give the callee a return
            // type first: assigning `nil` makes it optional, which a function may return by
            // ending without a value.
            "function main() local x = f() x = nil end\nfunction f() end",
            // Or a number, which makes this a missing `return` in `f` instead (`E0480`).
            "function main() local x = f() return x + 1 end\nfunction f() end",
            // In top-level code, which is checked before every function body.
            "local x = f()\nfunction f() end",
            // The callee returns nothing by returning a call that returns nothing, and so is
            // only known to once its body has been checked.
            "function main() local x = g() end\nfunction g() return f() end\nfunction f() end",
            // A callback, typed by the closure the caller passes.
            "function run(cb) local x = cb() end\nfunction main() run(function() end) end",
        ] {
            assert_eq!(err_code(src, &empty()), "E0435", "{src}");
        }
        // A host function and a memory method declared to return nothing.
        let valueless = || FnType {
            params: vec![],
            ret: Box::new(Type::Unit),
        };
        let mut cfg = mem_method_cfg();
        cfg.host_functions.insert("log".into(), valueless());
        cfg.methods
            .get_mut("mem")
            .unwrap()
            .insert("touch".into(), valueless());
        for src in [
            "function main() local x = log() end",
            "function main() local x = mem:touch() end",
        ] {
            assert_eq!(err_code(src, &cfg), "E0435", "{src}");
        }
    }

    #[test]
    fn a_valueless_call_may_be_a_statement_or_returned() {
        let info = ok(
            "function f() end\n\
             function pair() return 1, 2 end\n\
             f()\n\
             function main(c)\n\
               f()\n\
               pair()\n\
               if c then return (f()) end\n\
               return f()\n\
             end",
            &empty(),
        );
        assert_eq!(info.exports["main"].to_string(), "fn(bool) -> ()");
    }

    // ---- missing return value (E0480) ----

    #[test]
    fn falling_off_a_value_returning_function_is_error() {
        assert_eq!(
            err_code("function f(x) if x > 0 then return 1 end end", &empty()),
            "E0480"
        );
    }

    #[test]
    fn bare_return_in_a_value_returning_function_is_error() {
        assert_eq!(
            err_code(
                "function f(x) if x > 0 then return end return 1 end",
                &empty()
            ),
            "E0480"
        );
    }

    #[test]
    fn annotated_return_type_with_no_return_is_error() {
        // No `return` at all used to make the function value-less, silently discarding the
        // annotation it contradicts.
        assert_eq!(
            err_code("---@return number\nfunction f() end", &empty()),
            "E0480"
        );
    }

    #[test]
    fn a_closure_that_can_fall_off_is_error() {
        assert_eq!(
            err_code(
                "function f()\n\
                   local g = function(x) if x > 0 then return 1 end end\n\
                   return g(1)\n\
                 end",
                &empty()
            ),
            "E0480"
        );
    }

    #[test]
    fn a_return_type_fixed_by_a_later_function_is_still_checked() {
        // `f`'s return type is only known once `g`'s body has been checked.
        assert_eq!(
            err_code(
                "function f(x) if x > 0 then return g() end end\n\
                 function g() return 1 end",
                &empty()
            ),
            "E0480"
        );
    }

    #[test]
    fn a_function_that_returns_on_every_path_is_ok() {
        for src in [
            "function f(x) if x > 0 then return 1 else return 2 end end",
            "function f(x) if x > 0 then return 1 elseif x < 0 then return 2 else return 3 end end",
            "function f(x) do return x + 1 end end",
            "function f(x) while true do if x > 0 then return x end x = x + 1 end end",
            "function f(x) while (true) do return x + 1 end end",
        ] {
            let info = ok(src, &empty());
            assert!(
                info.exports["f"].to_string().ends_with("-> number"),
                "{src}: {}",
                info.exports["f"]
            );
        }
    }

    #[test]
    fn a_loop_that_can_break_can_fall_off() {
        for src in [
            "function f(x) while true do if x > 0 then break end return 1 end end",
            "function f(x) while x > 0 do return 1 end end",
            "function f(x) for i = 1, x do return i end end",
            "function f(x) if x > 0 then return 1 elseif x < 0 then return 2 end end",
        ] {
            assert_eq!(err_code(src, &empty()), "E0480", "{src}");
        }
    }

    #[test]
    fn a_break_in_a_nested_loop_does_not_leave_the_outer_one() {
        let src = "function f(x)\n\
                     while true do\n\
                       for i = 1, 3 do break end\n\
                       return x + 1\n\
                     end\n\
                   end";
        ok(src, &empty());
    }

    #[test]
    fn an_optional_returning_function_may_end_without_a_value() {
        // The implicit result is `nil`, which the optional return type holds.
        let info = ok(
            "function first_big(xs)\n\
               for _, x in ipairs(xs) do\n\
                 if x > 10 then return tonumber(\"1\") end\n\
               end\n\
             end",
            &empty(),
        );
        assert_eq!(
            info.exports["first_big"].to_string(),
            "fn(array<number>) -> number?"
        );
    }

    #[test]
    fn a_value_less_function_may_return_early() {
        let info = ok("function f(x) if x > 0 then return end x = 1 end", &empty());
        assert_eq!(info.exports["f"].to_string(), "fn(number) -> ()");
    }

    // ---- optional returns ----

    #[test]
    fn a_declared_optional_return_accepts_a_value_and_nil() {
        let info = ok(
            "---@param x number\n\
             ---@return number?\n\
             function f(x) if x > 0 then return 1 end return nil end",
            &empty(),
        );
        assert_eq!(info.exports["f"].to_string(), "fn(number) -> number?");
    }

    #[test]
    fn nil_and_a_value_infer_an_optional_in_either_order() {
        for src in [
            "function f(x) if x > 0 then return 1 end return nil end",
            "function f(x) if x > 0 then return nil end return 1 end",
            "function f(x) if x > 0 then return (nil) end return 1 end",
        ] {
            let info = ok(src, &empty());
            assert_eq!(
                info.exports["f"].to_string(),
                "fn(number) -> number?",
                "{src}"
            );
        }
    }

    #[test]
    fn nil_and_an_optional_value_infer_a_single_optional() {
        let info = ok(
            "function f(s) if s == \"\" then return nil end return tonumber(s) end",
            &empty(),
        );
        assert_eq!(info.exports["f"].to_string(), "fn(string) -> number?");
    }

    #[test]
    fn only_nil_returns_still_infer_nil() {
        let info = ok("function f() return nil end", &empty());
        assert_eq!(info.exports["f"].to_string(), "fn() -> nil");
    }

    #[test]
    fn an_inferred_optional_function_may_end_without_a_value() {
        let info = ok(
            "function f(x) if x > 0 then return 1 elseif x < 0 then return nil end end",
            &empty(),
        );
        assert_eq!(info.exports["f"].to_string(), "fn(number) -> number?");
    }

    #[test]
    fn closures_and_bools_infer_optionals_too() {
        let info = ok(
            "function f(x)\n\
               local g = function(y) if y > 0 then return y end return nil end\n\
               return g(x)\n\
             end\n\
             function h(b) if b then return true end return nil end",
            &empty(),
        );
        assert_eq!(info.exports["f"].to_string(), "fn(number) -> number?");
        assert_eq!(info.exports["h"].to_string(), "fn(bool) -> bool?");
    }

    #[test]
    fn a_declared_optional_tuple_element_accepts_a_value_and_nil() {
        let info = ok(
            "---@param x number\n\
             ---@return number?\n\
             ---@return string\n\
             function f(x) if x > 0 then return 1, \"a\" end return nil, \"b\" end",
            &empty(),
        );
        assert_eq!(
            info.exports["f"].to_string(),
            "fn(number) -> (number?, string)"
        );
    }

    #[test]
    fn a_declared_non_optional_return_still_rejects_nil() {
        assert_eq!(
            err_code(
                "---@return number\n\
                 function f(x) if x > 0 then return 1 end return nil end",
                &empty()
            ),
            "E0413"
        );
    }

    #[test]
    fn a_caller_must_narrow_an_optional_result() {
        assert_eq!(
            err_code(
                "function f(x) if x > 0 then return 1 end return nil end\n\
                 function g(x) return f(x) + 1 end",
                &empty()
            ),
            "E0401"
        );
    }

    #[test]
    fn narrowing_optional_then_branch() {
        // Array indexing yields T?; narrowing makes it usable as T.
        let info = ok(
            "function first(xs)\n\
               local v = xs[1]\n\
               if v ~= nil then\n\
                 return v + 0\n\
               end\n\
               return 0\n\
             end",
            &empty(),
        );
        assert_eq!(
            info.exports["first"].to_string(),
            "fn(array<number>) -> number"
        );
    }

    #[test]
    fn memory_record_field_read_and_write() {
        let mut memory = BTreeMap::new();
        let mut rec = BTreeMap::new();
        rec.insert("gold".to_string(), Type::Number);
        memory.insert("mem".to_string(), Type::Record(rec));
        let cfg = TypeConfig {
            host_functions: BTreeMap::new(),
            memory,
            methods: BTreeMap::new(),
        };
        ok(
            "function spend(n)\n\
               if mem.gold >= n then\n\
                 mem.gold = mem.gold - n\n\
                 return true\n\
               end\n\
               return false\n\
             end",
            &cfg,
        );
    }

    fn mem_method_cfg() -> TypeConfig {
        let mut rec = BTreeMap::new();
        rec.insert("gold".to_string(), Type::Number);
        let mut memory = BTreeMap::new();
        memory.insert("mem".to_string(), Type::Record(rec));
        let mut mm = BTreeMap::new();
        mm.insert(
            "add_item".to_string(),
            FnType {
                params: vec![Type::Number],
                ret: Box::new(Type::Bool),
            },
        );
        let mut methods = BTreeMap::new();
        methods.insert("mem".to_string(), mm);
        TypeConfig {
            host_functions: BTreeMap::new(),
            memory,
            methods,
        }
    }

    #[test]
    fn method_call_types_from_declared_signature() {
        // The receiver is implicit; the argument is typed by the method's declared param, and
        // the call result is the method's return type — here inferring `id: number`.
        let info = ok(
            "function f(id) return mem:add_item(id) end",
            &mem_method_cfg(),
        );
        assert_eq!(info.exports["f"].to_string(), "fn(number) -> bool");
    }

    #[test]
    fn unknown_method_on_memory_is_error() {
        assert_eq!(
            err_code("function f() return mem:nope(1) end", &mem_method_cfg()),
            "E0432"
        );
    }

    #[test]
    fn method_call_arity_mismatch_is_error() {
        assert_eq!(
            err_code(
                "function f() return mem:add_item(1, 2) end",
                &mem_method_cfg()
            ),
            "E0433"
        );
    }

    #[test]
    fn method_call_arg_type_mismatch_is_error() {
        assert_eq!(
            err_code(
                "function f() return mem:add_item(\"x\") end",
                &mem_method_cfg()
            ),
            "E0402"
        );
    }

    #[test]
    fn method_call_on_non_memory_receiver_is_error() {
        assert_eq!(
            err_code(
                "function f()\n  local n = 1\n  return n:add_item(1)\nend",
                &mem_method_cfg()
            ),
            "E0417"
        );
    }

    #[test]
    fn host_function_signature_is_used() {
        let mut host = BTreeMap::new();
        host.insert(
            "roll".to_string(),
            FnType {
                params: vec![Type::Number],
                ret: Box::new(Type::Number),
            },
        );
        let cfg = TypeConfig {
            host_functions: host,
            memory: BTreeMap::new(),
            methods: BTreeMap::new(),
        };
        let info = ok("function f() return roll(6) end", &cfg);
        assert_eq!(info.exports["f"].to_string(), "fn() -> number");
    }

    #[test]
    fn string_concat_types() {
        let info = ok("function f(a) return a .. \"!\" end", &empty());
        assert_eq!(info.exports["f"].to_string(), "fn(string) -> string");
    }

    #[test]
    fn named_functions_are_called_not_used_as_values() {
        let cases = [
            "function g(x) return x + 1 end\nfunction f() local h = g return h(1) end",
            "function g(x) return x + 1 end\nfunction f() return (g)(1) end",
            "function g(x) return x + 1 end\nfunction f() return { g } end",
        ];
        for src in cases {
            assert_eq!(err_code(src, &empty()), "E0434", "{src}");
        }
        // A closure over the call is the first-class form.
        ok(
            "function g(x) return x + 1 end\n\
             function f() local h = function(x) return g(x) end return h(1) end",
            &empty(),
        );
    }

    #[test]
    fn export_entries_name_a_top_level_declaration() {
        assert_eq!(
            err_code(
                "function f() return 1 end\nreturn { run = f, two = 1 + 1 }",
                &empty()
            ),
            "E0411"
        );
        assert_eq!(
            err_code("function f() return 1 end\nreturn { run = (f) }", &empty()),
            "E0411"
        );
    }

    /// A local that starts out `nil` is optional and holds whatever is assigned to it later
    /// (`SPEC.md` §5.2), where it used to be typed `nil` and reject every other value.
    #[test]
    fn a_local_initialized_to_nil_takes_what_is_assigned_later() {
        let cases = [
            (
                "function f(c) local x = nil if c > 0 then x = c end return x end",
                "fn(number) -> number?",
            ),
            (
                "function f(c) local x = nil if c > 0 then x = {a = c} end \
                 if x ~= nil then return x.a end return 0 end",
                "fn(number) -> number",
            ),
            (
                "function f(c) local x, y = nil, nil y = c + 0 return y end",
                "fn(number) -> number?",
            ),
            // Assigned from a closure, which captures it.
            (
                "function f(c) local x = nil local set = function() x = c + 0 end set() \
                 return x end",
                "fn(number) -> number?",
            ),
            // Nothing else is ever assigned: it is only ever `nil`.
            ("function f() local x = nil return x end", "fn() -> nil"),
            (
                "function f() local x = nil x = nil return x end",
                "fn() -> nil",
            ),
        ];
        for (src, want) in cases {
            let info = ok(src, &empty());
            assert_eq!(info.exports["f"].to_string(), want, "{src}");
        }
    }

    /// A local with no initializer holds `nil` until assigned, so it is typed like one initialized
    /// to `nil`. It used to take the type of what was assigned to it, so a path that read it
    /// first returned `nil` from a function typed to return a number.
    #[test]
    fn an_uninitialized_local_is_typed_like_one_initialized_to_nil() {
        let cases = [
            (
                "function f(c) local x if c > 0 then x = c end return x end",
                "fn(number) -> number?",
            ),
            (
                "function f(c) local x, y if c > 0 then x = c y = {a = c} end \
                 if y ~= nil then return y.a end return 0 end",
                "fn(number) -> number",
            ),
            ("function f() local x return x end", "fn() -> nil"),
        ];
        for (src, want) in cases {
            let info = ok(src, &empty());
            assert_eq!(info.exports["f"].to_string(), want, "{src}");
        }
        // Even when every path assigns it before the read.
        let src = "function f(c) local x if c > 0 then x = 1 else x = 2 end return x + 1 end";
        assert_eq!(err_code(src, &empty()), "E0401", "{src}");
    }

    /// A value can be used above the assignment that gives it its type, as in a loop that
    /// remembers the previous iteration's value. The use is checked against that type.
    #[test]
    fn a_value_can_be_used_before_the_code_that_types_it() {
        let cases = [
            "function f(n) local prev = nil local s = 0 for i = 1, n do \
             if prev ~= nil then s = s + prev.v end prev = {v = i} end return s end",
            "function f(n) local prev = nil local s = 0 for i = 1, n do \
             if prev ~= nil then local d = prev.v s = s + d end prev = {v = i} end return s end",
            "function f(n) local step = nil local s = 0 for i = 1, n do \
             if step ~= nil then s = s + step(i) end step = function(k) return k * 2 end end \
             return s end",
            "function f(n) local prev = nil for i = 1, n do \
             if prev ~= nil then prev.v = i end prev = {v = 0} end return 0 end",
            // A field of a field: the inner read is typed once the outer one is.
            "function f(n) local prev = nil local s = 0 for i = 1, n do \
             if prev ~= nil then s = s + prev.inner.v end prev = {inner = {v = i}} end \
             return s end",
            // A parameter given its type by a caller checked after it.
            "function b(p) local y = p.x return y + 1 end\n\
             function f(c) return b({x = c}) end",
        ];
        for src in cases {
            let info = ok(src, &empty());
            assert_eq!(
                info.exports["f"].to_string(),
                "fn(number) -> number",
                "{src}"
            );
        }
    }

    /// Such a use is checked as strictly as one whose type was known at the time: it used to be
    /// typed `<error>` and let through unchecked.
    #[test]
    fn a_use_before_the_code_that_types_it_is_still_checked() {
        let loop_with = |use_: &str, assign: &str| {
            format!(
                "function f(n) local prev = nil local s = 0 for i = 1, n do \
                 if prev ~= nil then {use_} end prev = {assign} end return s end"
            )
        };
        let cases = [
            (loop_with("prev.v = \"s\"", "{v = i}"), "E0402"),
            (loop_with("s = s + prev.w", "{v = i}"), "E0420"),
            (loop_with("prev.w = 1", "{v = i}"), "E0420"),
            (loop_with("s = s + prev.v", "i"), "E0421"),
            (loop_with("local d = prev.v .. \"s\"", "{v = i}"), "E0402"),
            (
                loop_with("s = s + prev(i, i)", "function(k) return k end"),
                "E0430",
            ),
            (
                loop_with("s = s + prev(\"x\")", "function(k) return k + 1 end"),
                "E0402",
            ),
            (
                loop_with("local d = prev(i) .. \"s\"", "function(k) return k end"),
                "E0402",
            ),
            (loop_with("s = s + prev(i)", "i"), "E0431"),
        ];
        for (src, code) in cases {
            assert_eq!(err_code(&src, &empty()), code, "{src}");
        }
    }

    /// A value nothing ever gives a type can't have its fields or call checked (`E0410`), where
    /// the use used to be typed `<error>` and compiled.
    #[test]
    fn a_use_of_a_value_that_never_gets_a_type_is_an_error() {
        let cases = [
            "function f() local x = nil if x ~= nil then return x.a end return 0 end",
            "function f() local x = nil if x ~= nil then x.a = 1 end return 0 end",
            "function f() local x = nil if x ~= nil then return x(1) end return 0 end",
            "function f() local x if x ~= nil then return x.a + 1 end return 0 end",
        ];
        for src in cases {
            assert_eq!(err_code(src, &empty()), "E0410", "{src}");
        }
    }

    /// A local that may still be `nil` has to be checked before it is used as its value, and the
    /// error says so even before anything has been assigned to it to name its type.
    #[test]
    fn a_nil_initialized_local_must_be_narrowed_before_use() {
        let cases = [
            (
                "function f(c) local x = nil x = {a = c} return x.a + 0 end",
                "E0421",
            ),
            (
                "function f(c) local x = nil x = c + 0 return x + 1 end",
                "E0401",
            ),
            (
                "function f(c) local x = nil local y = x + 1 x = c + 0 return y end",
                "E0401",
            ),
        ];
        for (src, code) in cases {
            let diags = analyze(src, &empty()).unwrap_err();
            assert_eq!(diags.0[0].code, code, "{src}");
            assert!(
                diags.0[0].message.contains("may be nil"),
                "{src}: {}",
                diags.0[0].message
            );
        }
    }

    /// `and`/`or` yield one of their operands, typed as the one type that covers every operand
    /// it can yield (`SPEC.md` §3.5).
    #[test]
    fn and_or_is_typed_by_the_operands_it_can_yield() {
        let cases = [
            // A default for an optional.
            (
                "---@param x number?\nfunction f(x) return x or 0 end",
                "fn(number?) -> number",
            ),
            (
                "---@param x bool?\nfunction f(x) return x or false end",
                "fn(bool?) -> bool",
            ),
            (
                "---@param x number?\n---@param y number?\nfunction f(x, y) return x or y end",
                "fn(number?, number?) -> number?",
            ),
            (
                "---@param x number?\nfunction f(x) return x or nil end",
                "fn(number?) -> number?",
            ),
            (
                "---@param p { name: string? }\nfunction f(p) return p.name or \"unknown\" end",
                "fn(record { name: string? }) -> string",
            ),
            // `and` yields its left operand's `nil`, or its right operand.
            (
                "---@param x number?\nfunction f(x) return x and \"s\" end",
                "fn(number?) -> string?",
            ),
            (
                "---@param x bool?\nfunction f(x, y) return x and y > 0 end",
                "fn(bool?, number) -> bool?",
            ),
            // One of two values: the `false` of `c and a` never gets past the `or`.
            (
                "function f(n) return n > 0 and \"pos\" or \"neg\" end",
                "fn(number) -> string",
            ),
            (
                "---@param x number?\nfunction f(n, x) return (n > 0 and x) or 0 end",
                "fn(number, number?) -> number",
            ),
            // Two bools, as before.
            (
                "function f(a, b) return a > 0 and b > 0 or a < b end",
                "fn(number, number) -> bool",
            ),
            // A parameter still takes the other operand's type.
            ("function f(p) return p or 0 end", "fn(number) -> number"),
            // `p or nil` is `T?` for a `p : T` found later; a `T?` one gives `T?` too, not `T??`.
            (
                "function f(p) return p or nil end\n\
                 function g(c) local y = nil if c > 0 then y = c end return f(y) end",
                "fn(number?) -> number?",
            ),
            // A local initialized to `nil` is typed by its default.
            (
                "function f() local x = nil return x or 0 end",
                "fn() -> number",
            ),
        ];
        for (src, want) in cases {
            let info = ok(src, &empty());
            assert_eq!(info.exports["f"].to_string(), want, "{src}");
        }
    }

    #[test]
    fn and_or_rejects_what_has_no_one_type() {
        let cases = [
            // The operands the expression can yield differ in type.
            (
                "---@param x number?\nfunction f(x) return x or \"none\" end",
                "E0455",
            ),
            ("function f(n) return n > 0 or 1 end", "E0455"),
            // `false` or a number.
            ("function f(n) return n > 0 and n end", "E0455"),
            ("function f(n) return (n > 0 and n or 1) and n end", "E0456"),
            // A left operand that is never `nil` or `false`.
            (
                "---@param n number\nfunction f(n) return n or 1 end",
                "E0456",
            ),
            ("function f(n) return n + 0 and n > 0 end", "E0456"),
            (
                "---@param x number?\nfunction f(x) return x or 0 or 1 end",
                "E0456",
            ),
            // In a condition the result must be a bool, not a `bool?`.
            (
                "---@param x number?\nfunction f(x, n) if x and n > 0 then return 1 end return 0 end",
                "E0400",
            ),
        ];
        for (src, code) in cases {
            assert_eq!(err_code(src, &empty()), code, "{src}");
        }
    }

    /// An optional operand whose type isn't known yet is taken to be `nil` when it is falsy. If
    /// it turns out to be a `bool?`, it could be `false`, and that is reported.
    #[test]
    fn and_or_on_a_value_later_found_to_be_a_bool_is_an_error() {
        let src = "function f(c)\n\
                     local x = nil\n\
                     local r = x and 1\n\
                     x = c > 0\n\
                     if r ~= nil then return r end\n\
                     return 0\n\
                   end";
        assert_eq!(err_code(src, &empty()), "E0455");
        // The same code over a number is fine.
        let src = src.replace("c > 0", "c + 0");
        assert_eq!(
            ok(&src, &empty()).exports["f"].to_string(),
            "fn(number) -> number"
        );
    }

    #[test]
    fn explicit_export_table_drives_signature() {
        let info = ok(
            "function helper() return 1 end\n\
             function pub() return helper() end\n\
             return { run = pub }",
            &empty(),
        );
        assert!(info.exports.contains_key("run"));
        assert!(!info.exports.contains_key("pub"));
        assert!(!info.exports.contains_key("helper"));
    }
}
