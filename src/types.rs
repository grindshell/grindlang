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
//!
//! * **Multi-value returns** produce a [`Type::Tuple`], consumable only by a matching-arity
//!   parallel binding or a pass-through `return` (§5.5); anywhere else is `E0415`.
//! * **Method calls** (`mem:m(...)`) are typed against the host memory's declared method
//!   signatures (§7); any other receiver is `E0417`.
//! * **EmmyLua `---@` annotations** (§5.6) are supported for top-level functions: `---@param
//!   <name> <type>` and `---@return <type>` pin a parameter/return type (parsed by
//!   [`crate::annotations`], applied in Pass 2 before bodies are checked). This is what lets a
//!   record/array-typed *parameter* be expressed.
//! * **Every path returns a value** in a function whose return type isn't optional: a bare
//!   `return`, or control reaching the closing `end`, is `E0480` (§5.5). Checked after every
//!   body, once return types are settled.
//! * **Flow narrowing**: inside `if <place> ~= nil then`, a `T?` place reads as `T`, where the
//!   place is a local, an upvalue, a memory root, or a field path over one (§5.3). A write to
//!   the place or a prefix of it ends the narrowing. Each narrowed read is recorded in
//!   [`TypeInfo::narrowed`] so lowering unwraps it.
//!
//! ## Deliberately deferred (documented gaps)
//!
//! * `---@type` annotations on locals.
//! * Narrowing from `== nil`, an `else` branch, or an early return.
//! * Iterating a record with `pairs` (its fields don't share a type).

use std::collections::{BTreeMap, HashMap};

use crate::ast::*;
use crate::diagnostics::{Diagnostic, Diagnostics, Span};
use crate::resolve::{Binding, Resolution, SymbolId};
use crate::runtime::builtins;

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

    Ok(TypeInfo {
        exports,
        symbol_types,
        functions,
        constants,
        function_literals,
        narrowed,
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
            Type::Optional(i) => Type::optional(self.deep(&i)),
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
    saw_value: bool,
    /// Each bare `return` in the function, which returns no value.
    bare_returns: Vec<Span>,
}

/// The ways a value-returning function can return no value, checked once every body has been
/// checked and its return type is fully known (see [`Checker::check_valueless_exits`]).
struct ValuelessExits {
    ret: Type,
    bare_returns: Vec<Span>,
    /// The function's closing `end`, if control can reach it.
    falls_off: Option<Span>,
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

struct Checker<'a> {
    res: &'a Resolution,
    cfg: &'a TypeConfig,
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
    /// Top-level constant types, by name.
    const_types: BTreeMap<String, Type>,
    /// Nested function-literal signatures, by [`FuncBody`] span (deep-resolved at the end).
    func_literals: HashMap<Span, FnType>,
    return_stack: Vec<ReturnFrame>,
    valueless_exits: Vec<ValuelessExits>,
    diags: Diagnostics,
}

impl<'a> Checker<'a> {
    fn new(module: &Module, res: &'a Resolution, cfg: &'a TypeConfig) -> Self {
        let _ = module;
        Checker {
            res,
            cfg,
            u: Unifier::default(),
            sym_types: vec![Type::Error; res.symbols.len()],
            narrowed: BTreeMap::new(),
            narrowed_reads: HashMap::new(),
            func_sigs: BTreeMap::new(),
            const_types: BTreeMap::new(),
            func_literals: HashMap::new(),
            return_stack: Vec::new(),
            valueless_exits: Vec::new(),
            diags: Diagnostics::new(),
        }
    }

    fn error(&mut self, code: &'static str, msg: impl Into<String>, span: Span) {
        self.diags.push(Diagnostic::error(code, msg, span));
    }

    fn run(&mut self, module: &Module) -> BTreeMap<String, Type> {
        // Pass 1: constant types (no name references allowed in const RHS).
        for decl in &module.decls {
            if let TopDecl::Const(c) = decl {
                let t = self.check_expr(&c.value);
                let t = self.u.deep(&t);
                self.const_types.insert(c.name.node.clone(), t);
            }
        }

        // Pass 2: pre-create a signature (fresh vars) for every top-level function so
        // calls — including mutual recursion — resolve before bodies are checked. `---@`
        // annotations pin parameter/return vars here, *before* Pass 3, so the body sees a
        // record/array parameter's shape and an otherwise-untypeable parameter is no longer
        // ambiguous (`SPEC.md` §5.6).
        for decl in &module.decls {
            if let TopDecl::Function(f) = decl {
                let sig = self.make_sig(&f.body.params);
                if !f.annotations.is_empty() {
                    self.apply_fn_annotations(f, &sig);
                }
                self.func_sigs.insert(f.name.node.clone(), sig);
            }
        }

        // Pass 3: check function bodies against their pre-created signatures.
        for decl in &module.decls {
            if let TopDecl::Function(f) = decl {
                let sig = self.func_sigs[&f.name.node].clone();
                self.check_func_body(&f.body, &sig);
            }
        }
        self.check_valueless_exits();

        // Pass 4: flag parameters whose type inference couldn't pin.
        for sym in self.res.symbols.iter() {
            if sym.kind == crate::resolve::SymbolKind::Param
                && let Some(id) = self.res.def(sym.def_span)
            {
                let t = self.u.deep(&self.sym_types[id as usize]);
                if matches!(t, Type::Var(_)) {
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
                            _ => {
                                self.error(
                                    "E0411",
                                    "a module export entry must name a top-level function or \
                                     constant",
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
            for decl in &module.decls {
                match decl {
                    TopDecl::Function(f) => {
                        let sig = self.func_sigs[&f.name.node].clone();
                        exports.insert(f.name.node.clone(), self.u.deep(&Type::Function(sig)));
                    }
                    TopDecl::Const(c) => {
                        let t = self.const_types[&c.name.node].clone();
                        exports.insert(c.name.node.clone(), t);
                    }
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
        }
    }

    fn check_func_body(&mut self, body: &FuncBody, sig: &FnType) {
        self.return_stack.push(ReturnFrame {
            ret: (*sig.ret).clone(),
            saw_value: false,
            bare_returns: Vec::new(),
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

    fn check_block(&mut self, block: &Block) {
        for stat in &block.stats {
            self.check_stat(stat);
        }
        if let Some(ret) = &block.ret {
            self.check_return(ret);
        }
    }

    fn check_return(&mut self, ret: &RetStat) {
        let frame_ret = self.return_stack.last().map(|f| f.ret.clone());
        // 0 exprs → value-less; 1 → a single value (which may itself be a tuple-returning call,
        // i.e. a pass-through); ≥ 2 → a tuple built from single-valued elements (no flattening).
        let (value_ty, span) = if ret.exprs.is_empty() {
            if let Some(frame) = self.return_stack.last_mut() {
                frame.bare_returns.push(ret.span);
            }
            return;
        } else if ret.exprs.len() == 1 {
            let e = &ret.exprs[0];
            (self.check_expr_multi(e), e.span)
        } else {
            // Each element is typed through `check_expr` (the guard), so a nested multi-value
            // call inside the list is rejected as E0415 rather than silently flattened.
            let elems = ret.exprs.iter().map(|e| self.check_expr(e)).collect();
            (Type::Tuple(elems), ret.span)
        };
        if let Some(r) = frame_ret {
            self.require(span, &value_ty, &r, "E0413", "return type mismatch");
        }
        if let Some(frame) = self.return_stack.last_mut() {
            frame.saw_value = true;
        }
    }

    // ---- statements ----------------------------------------------------------

    fn check_stat(&mut self, stat: &Stat) {
        match &stat.kind {
            StatKind::Empty | StatKind::Break => {}
            StatKind::Local { names, exprs } => self.check_local(names, exprs, stat.span),
            StatKind::LocalFunction { name, body } => {
                let sig = self.make_sig(&body.params);
                if let Some(id) = self.res.def(name.span) {
                    self.sym_types[id as usize] = Type::Function(sig.clone());
                }
                self.func_literals.insert(body.span, sig.clone());
                self.check_func_body(body, &sig);
            }
            StatKind::Assign { targets, exprs } => self.check_assign(targets, exprs, stat.span),
            StatKind::Call(e) => {
                self.check_expr(e);
            }
            StatKind::Do(block) => self.check_block(block),
            StatKind::While { cond, body } => {
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
            // A single ordinary value.
            if n != 1 {
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
        if exprs.is_empty() {
            // Uninitialized `local a, b` — each name gets a fresh var pinned by later use.
            for name in names {
                if let Some(id) = self.res.def(name.span) {
                    self.sym_types[id as usize] = self.u.fresh();
                }
            }
            return;
        }
        let value_types = self.check_multi_rhs(exprs, names.len(), span);
        for (name, vt) in names.iter().zip(value_types) {
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
                self.narrowed
                    .retain(|k, _| *k != place && !k.has_ancestor(&place));
            }
        }
    }

    fn check_if(&mut self, arms: &[(Expr, Block)], else_block: &Option<Block>) {
        for (cond, block) in arms {
            self.check_condition(cond);
            // Limited narrowing: `if <name> ~= nil` refines `<name>` inside the branch.
            let narrowing = self.narrowing_from(cond);
            let saved = self.apply_narrowing(narrowing);
            self.check_block(block);
            self.restore_narrowing(saved);
        }
        if let Some(block) = else_block {
            self.check_block(block);
        }
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
                            format!("`ipairs` expects an array, found {}", self.u.deep(&other)),
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
                                "`pairs` expects a map, found {} (iterating records is not \
                                 yet supported)",
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
        self.check_block(body);
    }

    // ---- expressions ---------------------------------------------------------

    /// Type an expression that **must** yield a single value. A multi-value (tuple) call in
    /// this position is an error (`E0415`): tuples are confined to the two legal spots — the
    /// sole expression of a `return` and the sole right-hand side of a parallel binding — which
    /// call [`Checker::check_expr_multi`] directly. Every recursive/operand position routes
    /// through here, so a tuple can never silently unify into an operand, argument, container
    /// element, or parameter var.
    fn check_expr(&mut self, expr: &Expr) -> Type {
        let t = self.check_expr_multi(expr);
        if matches!(self.u.shallow(&t), Type::Tuple(_)) {
            self.error(
                "E0415",
                "a multi-value call must be returned directly or bound to a parallel \
                 `local`/assignment; it can't be used as a single value here",
                expr.span,
            );
            return Type::Error;
        }
        t
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
            ExprKind::Name(_) => self.check_name(expr),
            ExprKind::Function(body) => {
                let sig = self.make_sig(&body.params);
                self.func_literals.insert(body.span, sig.clone());
                self.check_func_body(body, &sig);
                Type::Function(sig)
            }
            ExprKind::Field { base, name } => self.check_field(base, name, expr.span),
            ExprKind::Index { base, index } => self.check_index(base, index),
            ExprKind::Call { callee, args } => self.check_call(callee, args, expr.span),
            ExprKind::MethodCall {
                receiver,
                method,
                args,
            } => self.check_method_call(receiver, method, args, expr.span),
            ExprKind::Table(fields) => self.check_table(fields, expr.span),
            ExprKind::Unary { op, operand } => self.check_unary(*op, operand, expr.span),
            ExprKind::Binary { op, lhs, rhs } => self.check_binary(*op, lhs, rhs, expr.span),
        }
    }

    fn check_name(&mut self, expr: &Expr) -> Type {
        let Some(binding) = self.res.binding(expr.span) else {
            return Type::Error;
        };
        match binding.clone() {
            Binding::Local(id) | Binding::Upvalue(id) => self
                .read_narrowed(&Place::Symbol(id), expr.span)
                .unwrap_or_else(|| self.sym_types[id as usize].clone()),
            // A direct call types its callee without coming through here (`check_call`), so
            // this is a named function used as a value.
            Binding::TopFunction(name) | Binding::HostFunction(name) => {
                self.error(
                    "E0434",
                    format!(
                        "`{name}` is a named function and can only be called directly; to pass it \
                         as a value, wrap the call in a closure (`function(x) return {name}(x) end`)"
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
            Type::Error | Type::Var(_) => Type::Error,
            other => {
                self.error(
                    "E0421",
                    format!("type {} has no fields", self.u.deep(&other)),
                    name.span,
                );
                Type::Error
            }
        }
    }

    fn check_index(&mut self, base: &Expr, index: &Expr) -> Type {
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
            Type::Record(fields) => {
                if let ExprKind::Str(key) = &index.kind {
                    match fields.get(key) {
                        Some(t) => t.clone(),
                        None => {
                            self.error(
                                "E0420",
                                format!("no field `{key}` on {}", self.u.deep(&bt)),
                                index.span,
                            );
                            Type::Error
                        }
                    }
                } else {
                    self.error(
                        "E0423",
                        "a record can only be indexed by a string literal key",
                        index.span,
                    );
                    Type::Error
                }
            }
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

    fn check_call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> Type {
        // Plain builtins (`tostring`, `tonumber`).
        if let Some(Binding::Builtin(ns)) = self.res.binding(callee.span) {
            return self.check_builtin_value_call(ns, args, span);
        }
        // Namespace member calls (`math.floor(...)`, `string.sub(...)`).
        if let ExprKind::Field { base, name } = &callee.kind
            && let Some(ns) = self.builtin_namespace(base)
        {
            return self.check_builtin_member_call(ns, name, args, span);
        }

        // A named top-level or host function is called directly; it is not a value.
        let direct = match (&callee.kind, self.res.binding(callee.span)) {
            (ExprKind::Name(_), Some(b)) => self.named_fn_type(b),
            _ => None,
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
                for (arg, pt) in args.iter().zip(&ft.params) {
                    let at = self.check_expr(arg);
                    self.require_assignable(arg.span, &at, pt, "argument type mismatch");
                }
                // Check any surplus args so their own errors still surface.
                for arg in args.iter().skip(ft.params.len()) {
                    self.check_expr(arg);
                }
                (*ft.ret).clone()
            }
            Type::Error | Type::Var(_) => {
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
            let at = self.check_expr(arg);
            self.require_assignable(arg.span, &at, pt, "argument type mismatch");
        }
        // Check any surplus args so their own errors still surface.
        for arg in args.iter().skip(sig.params.len()) {
            self.check_expr(arg);
        }
        (*sig.ret).clone()
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
                self.require(
                    operand.span,
                    &t,
                    &Type::Bool,
                    "E0450",
                    "`not` requires a bool",
                );
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
                                "`#` requires a string or array, found {}",
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
                self.require_number(lhs);
                self.require_number(rhs);
                Type::Number
            }
            Concat => {
                let lt = self.check_expr(lhs);
                let rt = self.check_expr(rhs);
                self.require(
                    lhs.span,
                    &lt,
                    &Type::String,
                    "E0452",
                    "`..` requires strings",
                );
                self.require(
                    rhs.span,
                    &rt,
                    &Type::String,
                    "E0452",
                    "`..` requires strings",
                );
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
            And | Or => {
                let lt = self.check_expr(lhs);
                let rt = self.check_expr(rhs);
                if self.u.unify(&lt, &rt).is_err() {
                    self.error(
                        "E0455",
                        format!(
                            "`{}` requires both operands to have the same type, found {} and {}",
                            op.symbol(),
                            self.u.deep(&lt),
                            self.u.deep(&rt)
                        ),
                        span,
                    );
                }
                self.u.deep(&lt)
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
            ExprKind::Name(_) => self.check_name(target),
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
                    Type::Error | Type::Var(_) => Type::Error,
                    other => {
                        self.error(
                            "E0421",
                            format!("type {} has no fields", self.u.deep(&other)),
                            name.span,
                        );
                        Type::Error
                    }
                }
            }
            ExprKind::Index { base, index } => {
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
                    Type::Error | Type::Var(_) => Type::Error,
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
    /// location (a bare local/upvalue, a memory root, or a field path over one). Anything
    /// else (calls, indexing, literals) is not a place and yields `None`.
    fn place_of(&self, expr: &Expr) -> Option<Place> {
        match &expr.kind {
            ExprKind::Paren(inner) => self.place_of(inner),
            ExprKind::Name(_) => match self.res.binding(expr.span)? {
                Binding::Local(id) | Binding::Upvalue(id) => Some(Place::Symbol(*id)),
                Binding::Memory(name) => Some(Place::Memory(name.clone())),
                _ => None,
            },
            ExprKind::Field { base, name } => Some(Place::Field(
                Box::new(self.place_of(base)?),
                name.node.clone(),
            )),
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

    /// Extract a narrowing fact from an `if` condition. Supports `<place> ~= nil` (refine in
    /// the then-branch), where `<place>` is a bare name **or** a field path (`p.name`).
    /// Returns the place and its narrowed (non-optional) type.
    fn narrowing_from(&self, cond: &Expr) -> Option<(Place, Type)> {
        let ExprKind::Binary {
            op: BinOp::Ne,
            lhs,
            rhs,
        } = &cond.kind
        else {
            return None;
        };
        // Match `<place> ~= nil` or `nil ~= <place>`.
        let place_expr = match (&lhs.kind, &rhs.kind) {
            (_, ExprKind::Nil) => lhs,
            (ExprKind::Nil, _) => rhs,
            _ => return None,
        };
        let place = self.place_of(place_expr)?;
        match self.u.shallow(&self.place_current_type(&place)?) {
            Type::Optional(inner) => Some((place, *inner)),
            _ => None,
        }
    }

    fn apply_narrowing(
        &mut self,
        narrowing: Option<(Place, Type)>,
    ) -> Option<(Place, Option<Type>)> {
        let (place, t) = narrowing?;
        let prev = self.narrowed.insert(place.clone(), t);
        Some((place, prev))
    }

    fn restore_narrowing(&mut self, saved: Option<(Place, Option<Type>)>) {
        if let Some((place, prev)) = saved {
            match prev {
                Some(t) => {
                    self.narrowed.insert(place, t);
                }
                None => {
                    self.narrowed.remove(&place);
                }
            }
        }
    }

    // ---- requirement helpers -------------------------------------------------

    fn check_condition(&mut self, cond: &Expr) {
        let t = self.check_expr(cond);
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
            self.error(
                code,
                format!(
                    "{ctx}: expected {}, found {}",
                    self.u.deep(expected),
                    self.u.deep(actual)
                ),
                span,
            );
        }
    }

    /// Like [`Self::require`] but with optional widening (`T`/`nil` assignable to `T?`).
    fn require_assignable(&mut self, span: Span, from: &Type, to: &Type, ctx: &str) {
        if self.is_assignable(from, to) {
            return;
        }
        self.error(
            "E0402",
            format!(
                "{ctx}: expected {}, found {}",
                self.u.deep(to),
                self.u.deep(from)
            ),
            span,
        );
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
fn always_returns(block: &Block) -> bool {
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
