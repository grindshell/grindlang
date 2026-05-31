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
//! * Parameter types are inferred from how they're *used* (operators, calls, indexing).
//!   A parameter whose type can't be pinned that way is an error ("ambiguous, annotate").
//! * Table literals get a concrete shape: all-positional → [`Type::Array`], all-`name =`
//!   → [`Type::Record`], string-keyed → [`Type::Map`].
//! * Conditions must be `bool`; equality needs matching operand types; arithmetic/relational
//!   operators are numeric (relational also allows `string`); `..` is `string`.
//!
//! ## Deliberately deferred (documented gaps)
//!
//! * **Multi-value returns / tuples.** A function returns 0 or 1 values in v1; a `return`
//!   with two or more values is rejected. Call/assignment lists require exact arity with
//!   one value per expression.
//! * **EmmyLua `---@` annotations.** Comments are stripped by the lexer, so annotations
//!   can't yet pin types; inference is the only source. (Record-typed *parameters*
//!   therefore can't be expressed yet — they'd need an annotation.)
//! * **Flow narrowing** is limited to `if <name> ~= nil`/`== nil` on a bare local; richer
//!   narrowing (fields, early-return flow) is future work.
//! * **Method calls** (`recv:m(...)`) and **`pairs` over records** are not yet typed.

use std::collections::BTreeMap;

use crate::ast::*;
use crate::diagnostics::{Diagnostic, Diagnostics, Span};
use crate::resolve::{Binding, Resolution, SymbolId};

/// A Grindlang type (`SPEC.md` §5.1).
#[derive(Clone, Debug, PartialEq, Eq)]
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
    Function(FnType),
    /// Opaque named host type.
    Userdata(String),
    /// An inference variable (resolved by the [`Unifier`]).
    Var(u32),
    /// Poison: suppresses cascading diagnostics after an error.
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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

    Ok(TypeInfo {
        exports,
        symbol_types,
        functions,
        constants,
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
}

struct Checker<'a> {
    res: &'a Resolution,
    cfg: &'a TypeConfig,
    u: Unifier,
    /// Type of each symbol, indexed by [`SymbolId`].
    sym_types: Vec<Type>,
    /// Narrowing overlay: a symbol temporarily refined to a non-optional type.
    narrowed: BTreeMap<SymbolId, Type>,
    /// Top-level function signatures, by name.
    func_sigs: BTreeMap<String, FnType>,
    /// Top-level constant types, by name.
    const_types: BTreeMap<String, Type>,
    return_stack: Vec<ReturnFrame>,
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
            func_sigs: BTreeMap::new(),
            const_types: BTreeMap::new(),
            return_stack: Vec::new(),
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
        // calls — including mutual recursion — resolve before bodies are checked.
        for decl in &module.decls {
            if let TopDecl::Function(f) = decl {
                let sig = self.make_sig(&f.body.params);
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
                    Field::Named { name, value } => {
                        let t = self.check_expr(value);
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

    fn check_func_body(&mut self, body: &FuncBody, sig: &FnType) {
        self.return_stack.push(ReturnFrame {
            ret: (*sig.ret).clone(),
            saw_value: false,
        });
        self.check_block(&body.block);
        let frame = self.return_stack.pop().expect("return frame");
        // A function with no value-returning `return` is value-less.
        if !frame.saw_value {
            let _ = self.u.unify(&frame.ret, &Type::Unit);
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
        if ret.exprs.len() > 1 {
            self.error(
                "E0412",
                "multiple return values are not supported in this version of Grindlang",
                ret.span,
            );
        }
        let frame_ret = self.return_stack.last().map(|f| f.ret.clone());
        if let Some(expr) = ret.exprs.first() {
            let t = self.check_expr(expr);
            if let Some(r) = frame_ret {
                self.require(expr.span, &t, &r, "E0413", "return type mismatch");
            }
            if let Some(frame) = self.return_stack.last_mut() {
                frame.saw_value = true;
            }
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

    fn check_local(&mut self, names: &[Ident], exprs: &[Expr], span: Span) {
        // v1: one value per expression; counts must match (no multi-value adjustment).
        if !exprs.is_empty() && exprs.len() != names.len() {
            self.error(
                "E0414",
                format!(
                    "assignment arity mismatch: {} name(s) but {} value(s)",
                    names.len(),
                    exprs.len()
                ),
                span,
            );
        }
        let mut value_types: Vec<Type> = exprs.iter().map(|e| self.check_expr(e)).collect();
        value_types.resize(names.len(), Type::Error);
        for (name, vt) in names.iter().zip(value_types) {
            if let Some(id) = self.res.def(name.span) {
                // Uninitialized `local x` gets a fresh var to be pinned by later use.
                let t = if exprs.is_empty() { self.u.fresh() } else { vt };
                self.sym_types[id as usize] = t;
            }
        }
    }

    fn check_assign(&mut self, targets: &[Expr], exprs: &[Expr], span: Span) {
        if targets.len() != exprs.len() {
            self.error(
                "E0414",
                format!(
                    "assignment arity mismatch: {} target(s) but {} value(s)",
                    targets.len(),
                    exprs.len()
                ),
                span,
            );
        }
        let value_types: Vec<Type> = exprs.iter().map(|e| self.check_expr(e)).collect();
        for (target, vt) in targets.iter().zip(value_types) {
            let place = self.place_type(target);
            self.require_assignable(target.span, &vt, &place, "assignment type mismatch");
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
                            "E0415",
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
                            "E0415",
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

    fn check_expr(&mut self, expr: &Expr) -> Type {
        match &expr.kind {
            ExprKind::Nil => Type::Nil,
            ExprKind::Bool(_) => Type::Bool,
            ExprKind::Number(_) => Type::Number,
            ExprKind::Str(_) => Type::String,
            ExprKind::Paren(inner) => self.check_expr(inner),
            ExprKind::Name(_) => self.check_name(expr),
            ExprKind::Function(body) => {
                let sig = self.make_sig(&body.params);
                self.check_func_body(body, &sig);
                Type::Function(sig)
            }
            ExprKind::Field { base, name } => self.check_field(base, name),
            ExprKind::Index { base, index } => self.check_index(base, index),
            ExprKind::Call { callee, args } => self.check_call(callee, args, expr.span),
            ExprKind::MethodCall { method, .. } => {
                self.error(
                    "E0417",
                    "method calls are not yet supported by the type checker",
                    method.span,
                );
                Type::Error
            }
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
                .narrowed
                .get(&id)
                .cloned()
                .unwrap_or_else(|| self.sym_types[id as usize].clone()),
            Binding::TopFunction(name) => Type::Function(self.func_sigs[&name].clone()),
            Binding::TopConst(name) => self.const_types[&name].clone(),
            Binding::HostFunction(name) => Type::Function(self.cfg.host_functions[&name].clone()),
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

    fn check_field(&mut self, base: &Expr, name: &Ident) -> Type {
        // Builtin namespace value-fields (`math.pi`, `math.huge`) — catalogued in the
        // runtime builtin catalog (single source of truth).
        if let Some(ns) = self.builtin_namespace(base) {
            return match crate::runtime::builtins::namespace_field_type(ns, &name.node) {
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

        let ct = self.check_expr(callee);
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

    fn check_builtin_value_call(&mut self, ns: &str, args: &[Expr], span: Span) -> Type {
        match ns {
            "tostring" => {
                self.expect_arity(ns, args, 1, span);
                if let Some(a) = args.first() {
                    let t = self.check_expr(a);
                    match self.u.shallow(&t) {
                        Type::Number | Type::Bool | Type::String | Type::Error | Type::Var(_) => {}
                        other => self.error(
                            "E0460",
                            format!(
                                "`tostring` accepts a number, bool, or string, found {}",
                                self.u.deep(&other)
                            ),
                            a.span,
                        ),
                    }
                }
                Type::String
            }
            "tonumber" => {
                self.expect_arity(ns, args, 1, span);
                if let Some(a) = args.first() {
                    let t = self.check_expr(a);
                    self.require(
                        a.span,
                        &t,
                        &Type::String,
                        "E0461",
                        "`tonumber` requires a string",
                    );
                }
                Type::optional(Type::Number)
            }
            _ => Type::Error,
        }
    }

    fn check_builtin_member_call(
        &mut self,
        ns: &str,
        name: &Ident,
        args: &[Expr],
        span: Span,
    ) -> Type {
        // The signature table is owned by the runtime builtin catalog (single source of
        // truth, shared with IR lowering and Phase 7 codegen). `string.format` is variadic
        // and handled specially.
        use crate::runtime::builtins::ArgRule;
        let Some(sig) = crate::runtime::builtins::member_sig(ns, &name.node) else {
            self.error(
                "E0462",
                format!("`{ns}` has no builtin member `{}`", name.node),
                name.span,
            );
            for a in args {
                self.check_expr(a);
            }
            return Type::Error;
        };

        match sig.rule {
            ArgRule::FormatVariadic => {
                // format(fmt: string, ...any) -> string.
                if args.is_empty() {
                    self.error(
                        "E0463",
                        "`string.format` requires at least a format string",
                        span,
                    );
                } else {
                    let ft = self.check_expr(&args[0]);
                    self.require(
                        args[0].span,
                        &ft,
                        &Type::String,
                        "E0463",
                        "format string must be a string",
                    );
                    for a in args.iter().skip(1) {
                        self.check_expr(a);
                    }
                }
                sig.ret
            }
            // No namespace member uses the `Scalar` rule (that's `tostring`, a value
            // builtin); treat anything non-variadic as a fixed positional signature.
            ArgRule::Fixed | ArgRule::Scalar => {
                if args.len() != sig.params.len() {
                    self.error(
                        "E0430",
                        format!(
                            "`{ns}.{}` takes {} argument(s) but {} were supplied",
                            name.node,
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
                for arg in args.iter().skip(sig.params.len()) {
                    self.check_expr(arg);
                }
                sig.ret
            }
        }
    }

    fn expect_arity(&mut self, name: &str, args: &[Expr], n: usize, span: Span) {
        if args.len() != n {
            self.error(
                "E0430",
                format!(
                    "`{name}` takes {n} argument(s) but {} were supplied",
                    args.len()
                ),
                span,
            );
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

    /// Extract a narrowing fact from an `if` condition. Supports `name ~= nil` (refine in
    /// the then-branch). Returns the symbol and its narrowed (non-optional) type.
    fn narrowing_from(&self, cond: &Expr) -> Option<(SymbolId, Type)> {
        let ExprKind::Binary {
            op: BinOp::Ne,
            lhs,
            rhs,
        } = &cond.kind
        else {
            return None;
        };
        // Match `name ~= nil` or `nil ~= name`.
        let name = match (&lhs.kind, &rhs.kind) {
            (ExprKind::Name(_), ExprKind::Nil) => lhs,
            (ExprKind::Nil, ExprKind::Name(_)) => rhs,
            _ => return None,
        };
        let binding = self.res.binding(name.span)?;
        let id = match binding {
            Binding::Local(id) | Binding::Upvalue(id) => *id,
            _ => return None,
        };
        match self.u.shallow(&self.sym_types[id as usize]) {
            Type::Optional(inner) => Some((id, *inner)),
            _ => None,
        }
    }

    fn apply_narrowing(
        &mut self,
        narrowing: Option<(SymbolId, Type)>,
    ) -> Option<(SymbolId, Option<Type>)> {
        let (id, t) = narrowing?;
        let prev = self.narrowed.insert(id, t);
        Some((id, prev))
    }

    fn restore_narrowing(&mut self, saved: Option<(SymbolId, Option<Type>)>) {
        if let Some((id, prev)) = saved {
            match prev {
                Some(t) => {
                    self.narrowed.insert(id, t);
                }
                None => {
                    self.narrowed.remove(&id);
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
    fn multiple_returns_rejected() {
        assert_eq!(err_code("function f() return 1, 2 end", &empty()), "E0412");
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
