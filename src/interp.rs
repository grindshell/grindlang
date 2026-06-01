//! Reference interpreter: a tree-walking evaluator over the resolved AST (`PLAN.md`
//! Phase 4, gated behind the `interp` feature).
//!
//! This is the **semantics oracle**. It implements the canonical meaning of every
//! construct so the future cranelift JIT (Phase 7) can be validated by differential
//! testing — interpret a program, JIT the same program, assert equal results. It also
//! serves as a debug / fallback execution mode.
//!
//! It is intentionally simple, not fast: runtime values are reference-counted rather than
//! arena-allocated (the arena model described in `PLAN.md` is a JIT-runtime concern; the
//! oracle just needs to be *correct*). Closures own a cloned [`crate::ast::FuncBody`] via
//! `Rc`, so [`Value`] carries no lifetime and host code can hold values freely.
//!
//! The runtime [`Value`] and [`RunError`] themselves live in [`crate::value`] (shared with
//! the JIT and the embedding API); this module owns only the tree-walking [`Interpreter`]
//! plus the interpreter-only callable variants ([`Value::Function`] / [`Value::Native`]).
//!
//! ## Execution model
//!
//! * A [`Value`] is `nil`, a bool, an `f64` number, an immutable string, a mutable array,
//!   a mutable string-keyed table (used for both records and maps *and* host memory), or a
//!   callable (a script function/closure or a host native function).
//! * Each function activation gets a fresh [`Frame`] keyed by [`SymbolId`]. Block scoping
//!   is already resolved into unique symbol ids, so one flat frame per call is sufficient.
//!   A closure captures the individual [`Slot`]s of its free variables (its upvalues),
//!   not the enclosing frames — capturing whole frames would create `Rc` reference cycles
//!   whenever a closure is stored back into a frame it reads an upvalue from.
//! * `return`/`break` propagate via [`Flow`].
//! * Host state is injected after construction: [`Interpreter::set_host_function`] and
//!   [`Interpreter::set_memory`]. Memory tables are shared by `Rc`, so script mutations are
//!   observable by the host through [`Interpreter::memory`] after a call returns.
//!
//! Assumes its input already passed resolution and type checking — runtime "type" errors
//! are treated as internal invariant violations ([`RunError::Internal`]) rather than user
//! errors, since the checker should have rejected them.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use crate::ast::*;
use crate::diagnostics::Diagnostics;
use crate::resolve::{Binding, Resolution, ResolveConfig, SymbolId};
use crate::runtime::builtins::{field_value, member_call, num_to_string, value_call};
use crate::value::{RunError, Value};

/// A script function: its parameter symbols, body, and (for closures) the captured
/// upvalue slots. Top-level functions capture nothing.
pub struct Func {
    params: Vec<SymbolId>,
    body: Rc<FuncBody>,
    /// The closure's free variables, each captured as the *individual* slot it refers to
    /// (see [`Slot`]) rather than the whole enclosing frame.
    captured: Frame,
}

/// A single variable's storage: a shared, mutable cell.
///
/// Closures capture the individual slots of their free variables, *not* the frames those
/// variables live in. This is what keeps the interpreter leak-free: a closure routinely
/// gets stored back into a frame it also reads an upvalue from (`local f = function() ...
/// base ... end`). Capturing the whole frame would make `frame -> f -> frame.clone()` a
/// reference cycle that `Rc` can never reclaim, so every such closure would leak its
/// frame (and everything that frame transitively owns). Capturing just the `base` slot
/// keeps the upvalue shared-by-reference — writes through it are still observed by the
/// enclosing scope and by sibling closures — while letting the frame and the closure drop
/// normally once nothing outside still holds them.
type Slot = Rc<RefCell<Value>>;

/// One activation's variables, keyed by resolved [`SymbolId`]. A call owns one of these
/// for its params/locals; a closure additionally carries a `captured` frame of the
/// upvalue slots it uses. Frames are owned outright (not shared) — only the individual
/// [`Slot`]s inside them are shared, and only with the closures that capture them.
type Frame = HashMap<SymbolId, Slot>;

fn new_frame() -> Frame {
    HashMap::new()
}

/// Control-flow signal threaded out of statement/block execution.
enum Flow {
    Normal,
    Break,
    Return(Value),
}

/// Structural-by-value for scalars, identity-by-`Rc` for reference types (Lua semantics).
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Nil, Value::Nil) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => Rc::ptr_eq(x, y),
        (Value::Table(x), Value::Table(y)) => Rc::ptr_eq(x, y),
        (Value::Function(x), Value::Function(y)) => Rc::ptr_eq(x, y),
        (Value::Native(x), Value::Native(y)) => Rc::ptr_eq(x, y),
        (Value::Cell(x), Value::Cell(y)) => Rc::ptr_eq(x, y),
        (Value::Closure(x), Value::Closure(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

/// The reference interpreter for one resolved module.
pub struct Interpreter<'a> {
    res: &'a Resolution,
    /// Top-level function declarations, by declared name.
    funcs: HashMap<String, Value>,
    /// Top-level constants, by declared name.
    consts: HashMap<String, Value>,
    /// The public surface callable via [`Interpreter::call`] — the curated export table if
    /// present, otherwise every top-level declaration.
    exports: HashMap<String, Value>,
    /// Host-registered native functions, by name.
    host: HashMap<String, Value>,
    /// Host memory handles, by name.
    memory: HashMap<String, Value>,
    /// The current activation frame chain (innermost last).
    env: Vec<Frame>,
}

impl<'a> Interpreter<'a> {
    /// Build an interpreter for a resolved module. Evaluates top-level constants and the
    /// export table eagerly.
    pub fn new(module: &Module, res: &'a Resolution) -> Result<Self, RunError> {
        let mut me = Interpreter {
            res,
            funcs: HashMap::new(),
            consts: HashMap::new(),
            exports: HashMap::new(),
            host: HashMap::new(),
            memory: HashMap::new(),
            env: Vec::new(),
        };
        me.init(module)?;
        Ok(me)
    }

    fn init(&mut self, module: &Module) -> Result<(), RunError> {
        // Top-level functions: closures over nothing (all free names are global).
        for decl in &module.decls {
            if let TopDecl::Function(f) = decl {
                let func = Func {
                    params: self.param_ids(&f.body),
                    body: Rc::new(f.body.clone()),
                    captured: Frame::new(),
                };
                self.funcs
                    .insert(f.name.node.clone(), Value::Function(Rc::new(func)));
            }
        }

        // Top-level constants: evaluated in an empty environment (const RHS references no
        // names, per the resolver's E0303 rule).
        self.env = vec![new_frame()];
        for decl in &module.decls {
            if let TopDecl::Const(c) = decl {
                let v = self.eval_expr(&c.value)?;
                self.consts.insert(c.name.node.clone(), v);
            }
        }

        // Exports.
        if let Some(export) = &module.export {
            for field in &export.node {
                if let Field::Named { name, value } = field {
                    let v = self.eval_expr(value)?;
                    self.exports.insert(name.node.clone(), v);
                }
            }
        } else {
            for (k, v) in self.funcs.iter().chain(self.consts.iter()) {
                self.exports.insert(k.clone(), v.clone());
            }
        }

        self.env.clear();
        Ok(())
    }

    /// Register a host function callable from scripts under `name`.
    pub fn set_host_function<F>(&mut self, name: impl Into<String>, f: F)
    where
        F: Fn(&[Value]) -> Result<Value, RunError> + 'static,
    {
        self.host.insert(name.into(), Value::Native(Rc::new(f)));
    }

    /// Bind a host memory handle. `value` is typically a [`Value::table`]; the interpreter
    /// shares it by `Rc`, so mutations made by scripts are visible through [`Self::memory`].
    pub fn set_memory(&mut self, name: impl Into<String>, value: Value) {
        self.memory.insert(name.into(), value);
    }

    /// Read back a memory handle (a clone of the shared `Rc`), e.g. to observe mutations.
    pub fn memory(&self, name: &str) -> Option<Value> {
        self.memory.get(name).cloned()
    }

    /// Call an exported function by name with `args`.
    pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RunError> {
        let func = self
            .exports
            .get(name)
            .cloned()
            .ok_or_else(|| RunError::UnknownExport(name.to_string()))?;
        self.call_value(&func, args)
    }

    /// Host-invoke a function value previously returned by [`call`](Self::call) (e.g. a
    /// closure). Mirrors the JIT's `call_value`, so the differential harness can validate
    /// host-invoke across all three backends.
    pub fn call_value_public(
        &mut self,
        callee: Value,
        args: Vec<Value>,
    ) -> Result<Value, RunError> {
        self.call_value(&callee, args)
    }

    fn param_ids(&self, body: &FuncBody) -> Vec<SymbolId> {
        body.params
            .iter()
            .filter_map(|p| self.res.def(p.span))
            .collect()
    }

    // ---- calling -------------------------------------------------------------

    fn call_value(&mut self, callee: &Value, args: Vec<Value>) -> Result<Value, RunError> {
        match callee {
            Value::Native(f) => f(&args),
            Value::Function(func) => {
                let func = Rc::clone(func);
                let mut activation: Frame = HashMap::new();
                for (i, &pid) in func.params.iter().enumerate() {
                    let arg = args.get(i).cloned().unwrap_or(Value::Nil);
                    activation.insert(pid, Rc::new(RefCell::new(arg)));
                }
                // The closure's environment is its captured upvalue slots (outer) plus this
                // call's own activation frame (inner). Cloning `captured` copies the slot
                // handles, not the cells, so upvalue writes still reach the shared cells.
                let new_env = vec![func.captured.clone(), activation];

                let saved = std::mem::replace(&mut self.env, new_env);
                let result = self.exec_block(&func.body.block);
                self.env = saved;

                match result? {
                    Flow::Return(v) => Ok(v),
                    Flow::Normal => Ok(Value::Nil),
                    Flow::Break => {
                        Err(RunError::Internal("`break` escaped a function body".into()))
                    }
                }
            }
            other => Err(RunError::Internal(format!(
                "attempted to call a {} value",
                other.type_name()
            ))),
        }
    }

    // ---- statements ----------------------------------------------------------

    fn exec_block(&mut self, block: &Block) -> Result<Flow, RunError> {
        for stat in &block.stats {
            match self.exec_stat(stat)? {
                Flow::Normal => {}
                other => return Ok(other),
            }
        }
        if let Some(ret) = &block.ret {
            let v = match ret.exprs.first() {
                Some(e) => self.eval_expr(e)?,
                None => Value::Nil,
            };
            return Ok(Flow::Return(v));
        }
        Ok(Flow::Normal)
    }

    fn exec_stat(&mut self, stat: &Stat) -> Result<Flow, RunError> {
        match &stat.kind {
            StatKind::Empty => Ok(Flow::Normal),
            StatKind::Break => Ok(Flow::Break),
            StatKind::Local { names, exprs } => {
                let values = self.eval_list(exprs)?;
                for (i, name) in names.iter().enumerate() {
                    let v = values.get(i).cloned().unwrap_or(Value::Nil);
                    self.declare(name, v)?;
                }
                Ok(Flow::Normal)
            }
            StatKind::LocalFunction { name, body } => {
                // Declare first (so the body may recurse), then build the closure.
                self.declare(name, Value::Nil)?;
                let func = Func {
                    params: self.param_ids(body),
                    body: Rc::new(body.clone()),
                    captured: self.capture_env(body),
                };
                let v = Value::Function(Rc::new(func));
                self.assign_symbol(name.span, v)?;
                Ok(Flow::Normal)
            }
            StatKind::Assign { targets, exprs } => {
                let values = self.eval_list(exprs)?;
                for (i, target) in targets.iter().enumerate() {
                    let v = values.get(i).cloned().unwrap_or(Value::Nil);
                    self.assign_to(target, v)?;
                }
                Ok(Flow::Normal)
            }
            StatKind::Call(e) => {
                self.eval_expr(e)?;
                Ok(Flow::Normal)
            }
            StatKind::Do(block) => self.exec_block(block),
            StatKind::While { cond, body } => {
                while self.eval_expr(cond)?.is_truthy() {
                    match self.exec_block(body)? {
                        Flow::Normal => {}
                        Flow::Break => break,
                        ret @ Flow::Return(_) => return Ok(ret),
                    }
                }
                Ok(Flow::Normal)
            }
            StatKind::If { arms, else_block } => {
                for (cond, block) in arms {
                    if self.eval_expr(cond)?.is_truthy() {
                        return self.exec_block(block);
                    }
                }
                if let Some(block) = else_block {
                    return self.exec_block(block);
                }
                Ok(Flow::Normal)
            }
            StatKind::NumericFor {
                var,
                start,
                end,
                step,
                body,
            } => self.exec_numeric_for(var, start, end, step.as_ref(), body),
            StatKind::GenericFor { names, iter, body } => self.exec_generic_for(names, iter, body),
        }
    }

    fn exec_numeric_for(
        &mut self,
        var: &Ident,
        start: &Expr,
        end: &Expr,
        step: Option<&Expr>,
        body: &Block,
    ) -> Result<Flow, RunError> {
        let start = self.eval_number(start)?;
        let end = self.eval_number(end)?;
        let step = match step {
            Some(e) => self.eval_number(e)?,
            None => 1.0,
        };
        let mut i = start;
        loop {
            let cont = if step >= 0.0 { i <= end } else { i >= end };
            if !cont {
                break;
            }
            self.declare(var, Value::Number(i))?;
            match self.exec_block(body)? {
                Flow::Normal => {}
                Flow::Break => break,
                ret @ Flow::Return(_) => return Ok(ret),
            }
            i += step;
        }
        Ok(Flow::Normal)
    }

    fn exec_generic_for(
        &mut self,
        names: &[Ident],
        iter: &IterExpr,
        body: &Block,
    ) -> Result<Flow, RunError> {
        match iter {
            IterExpr::IPairs { arg, .. } => {
                let arr = match self.eval_expr(arg)? {
                    Value::Array(a) => a,
                    other => {
                        return Err(RunError::Internal(format!(
                            "ipairs expected an array, found {}",
                            other.type_name()
                        )));
                    }
                };
                let len = arr.borrow().len();
                for idx in 0..len {
                    let item = arr.borrow()[idx].clone();
                    if let Some(k) = names.first() {
                        self.declare(k, Value::Number((idx + 1) as f64))?;
                    }
                    if let Some(v) = names.get(1) {
                        self.declare(v, item)?;
                    }
                    match self.exec_block(body)? {
                        Flow::Normal => {}
                        Flow::Break => break,
                        ret @ Flow::Return(_) => return Ok(ret),
                    }
                }
                Ok(Flow::Normal)
            }
            IterExpr::Pairs { arg, .. } => {
                let table = match self.eval_expr(arg)? {
                    Value::Table(t) => t,
                    other => {
                        return Err(RunError::Internal(format!(
                            "pairs expected a table, found {}",
                            other.type_name()
                        )));
                    }
                };
                let entries: Vec<(String, Value)> = table
                    .borrow()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                for (k, v) in entries {
                    if let Some(kn) = names.first() {
                        self.declare(kn, Value::string(k))?;
                    }
                    if let Some(vn) = names.get(1) {
                        self.declare(vn, v)?;
                    }
                    match self.exec_block(body)? {
                        Flow::Normal => {}
                        Flow::Break => break,
                        ret @ Flow::Return(_) => return Ok(ret),
                    }
                }
                Ok(Flow::Normal)
            }
        }
    }

    // ---- environment ---------------------------------------------------------

    /// Introduce or rebind a symbol in the current (innermost) frame.
    ///
    /// Re-declaring an existing symbol (a loop variable on each iteration, say) reuses its
    /// slot rather than allocating a fresh one, so any closure already capturing that slot
    /// keeps observing the live binding — matching the previous whole-frame behavior.
    fn declare(&mut self, name: &Ident, value: Value) -> Result<(), RunError> {
        let id = self
            .res
            .def(name.span)
            .ok_or_else(|| RunError::Internal(format!("no symbol id for `{}`", name.node)))?;
        let frame = self
            .env
            .last_mut()
            .ok_or_else(|| RunError::Internal("declare with no active frame".into()))?;
        match frame.get(&id).cloned() {
            Some(slot) => *slot.borrow_mut() = value,
            None => {
                frame.insert(id, Rc::new(RefCell::new(value)));
            }
        }
        Ok(())
    }

    /// Assign to an existing symbol, found by searching the frame chain outward.
    fn assign_symbol(
        &mut self,
        span: crate::diagnostics::Span,
        value: Value,
    ) -> Result<(), RunError> {
        let id = self
            .res
            .binding(span)
            .and_then(|b| match b {
                Binding::Local(id) | Binding::Upvalue(id) => Some(*id),
                _ => None,
            })
            .or_else(|| self.res.def(span))
            .ok_or_else(|| RunError::Internal("assignment to unresolved symbol".into()))?;

        for frame in self.env.iter().rev() {
            if let Some(slot) = frame.get(&id) {
                *slot.borrow_mut() = value;
                return Ok(());
            }
        }
        // Not yet present (e.g. first assignment to a same-frame local): bind innermost.
        self.env
            .last_mut()
            .ok_or_else(|| RunError::Internal("assign with no active frame".into()))?
            .insert(id, Rc::new(RefCell::new(value)));
        Ok(())
    }

    fn lookup_symbol(&self, id: SymbolId) -> Option<Value> {
        for frame in self.env.iter().rev() {
            if let Some(slot) = frame.get(&id) {
                return Some(slot.borrow().clone());
            }
        }
        None
    }

    /// Find the live [`Slot`] for `id` in the current environment, if any.
    fn find_slot(&self, id: SymbolId) -> Option<Slot> {
        self.env
            .iter()
            .rev()
            .find_map(|frame| frame.get(&id).map(Rc::clone))
    }

    /// Build the captured environment for a closure whose body is `body`: the slots of the
    /// free variables it (or any function nested in it) refers to. We collect every
    /// in-function symbol id the body mentions and keep those that currently resolve to a
    /// live slot — i.e. genuine upvalues from the enclosing scope. The closure's own
    /// params/locals (and those of nested closures) are not in scope yet, so they are
    /// naturally excluded and materialized later when their defining call runs.
    fn capture_env(&self, body: &FuncBody) -> Frame {
        crate::capture::referenced_symbols(self.res, body)
            .into_iter()
            .filter_map(|id| self.find_slot(id).map(|slot| (id, slot)))
            .collect()
    }

    // ---- assignment targets --------------------------------------------------

    fn assign_to(&mut self, target: &Expr, value: Value) -> Result<(), RunError> {
        match &target.kind {
            ExprKind::Name(_) => self.assign_symbol(target.span, value),
            ExprKind::Field { base, name } => {
                let base_v = self.eval_expr(base)?;
                match base_v {
                    Value::Table(t) => {
                        t.borrow_mut().insert(name.node.clone(), value);
                        Ok(())
                    }
                    other => Err(RunError::Internal(format!(
                        "cannot set field `{}` on a {} value",
                        name.node,
                        other.type_name()
                    ))),
                }
            }
            ExprKind::Index { base, index } => {
                let base_v = self.eval_expr(base)?;
                match base_v {
                    Value::Array(a) => {
                        let idx = self.eval_number(index)?;
                        self.array_set(&a, idx, value)
                    }
                    Value::Table(t) => {
                        let key = self.eval_string_key(index)?;
                        t.borrow_mut().insert(key, value);
                        Ok(())
                    }
                    other => Err(RunError::Internal(format!(
                        "cannot index-assign a {} value",
                        other.type_name()
                    ))),
                }
            }
            _ => Err(RunError::Internal("invalid assignment target".into())),
        }
    }

    fn array_set(
        &self,
        arr: &Rc<RefCell<Vec<Value>>>,
        idx: f64,
        value: Value,
    ) -> Result<(), RunError> {
        let len = arr.borrow().len();
        if idx < 1.0 || idx.fract() != 0.0 {
            return Err(RunError::Runtime(format!(
                "array index {} is not a positive integer",
                num_to_string(idx)
            )));
        }
        let i = idx as usize;
        if i <= len {
            arr.borrow_mut()[i - 1] = value;
            Ok(())
        } else if i == len + 1 {
            // Append at exactly len+1 (the canonical array-growth idiom).
            arr.borrow_mut().push(value);
            Ok(())
        } else {
            Err(RunError::Runtime(format!(
                "array index {} is out of range (length {len}); arrays may only grow by one",
                i
            )))
        }
    }

    // ---- expressions ---------------------------------------------------------

    fn eval_list(&mut self, exprs: &[Expr]) -> Result<Vec<Value>, RunError> {
        exprs.iter().map(|e| self.eval_expr(e)).collect()
    }

    fn eval_number(&mut self, expr: &Expr) -> Result<f64, RunError> {
        match self.eval_expr(expr)? {
            Value::Number(n) => Ok(n),
            other => Err(RunError::Internal(format!(
                "expected a number, found {}",
                other.type_name()
            ))),
        }
    }

    fn eval_string_key(&mut self, expr: &Expr) -> Result<String, RunError> {
        match self.eval_expr(expr)? {
            Value::Str(s) => Ok(s.to_string()),
            other => Err(RunError::Internal(format!(
                "expected a string key, found {}",
                other.type_name()
            ))),
        }
    }

    fn eval_expr(&mut self, expr: &Expr) -> Result<Value, RunError> {
        match &expr.kind {
            ExprKind::Nil => Ok(Value::Nil),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::Number(n) => Ok(Value::Number(*n)),
            ExprKind::Str(s) => Ok(Value::string(s.clone())),
            ExprKind::Paren(inner) => self.eval_expr(inner),
            ExprKind::Name(name) => self.eval_name(expr, name),
            ExprKind::Function(body) => {
                let func = Func {
                    params: self.param_ids(body),
                    body: Rc::new(body.clone()),
                    captured: self.capture_env(body),
                };
                Ok(Value::Function(Rc::new(func)))
            }
            ExprKind::Field { base, name } => self.eval_field(base, name),
            ExprKind::Index { base, index } => self.eval_index(base, index),
            ExprKind::Call { callee, args } => self.eval_call(callee, args),
            ExprKind::MethodCall { method, .. } => Err(RunError::Internal(format!(
                "method call `:{}` is not supported by the interpreter",
                method.node
            ))),
            ExprKind::Table(fields) => self.eval_table(fields),
            ExprKind::Unary { op, operand } => self.eval_unary(*op, operand),
            ExprKind::Binary { op, lhs, rhs } => self.eval_binary(*op, lhs, rhs),
        }
    }

    fn eval_name(&mut self, expr: &Expr, name: &str) -> Result<Value, RunError> {
        let binding = self
            .res
            .binding(expr.span)
            .ok_or_else(|| RunError::Internal(format!("unresolved name `{name}`")))?;
        match binding {
            Binding::Local(id) | Binding::Upvalue(id) => self
                .lookup_symbol(*id)
                .ok_or_else(|| RunError::Internal(format!("`{name}` used before assignment"))),
            Binding::TopFunction(n) => self
                .funcs
                .get(n)
                .cloned()
                .ok_or_else(|| RunError::Internal(format!("missing function `{n}`"))),
            Binding::TopConst(n) => self
                .consts
                .get(n)
                .cloned()
                .ok_or_else(|| RunError::Internal(format!("missing const `{n}`"))),
            Binding::HostFunction(n) => self.host.get(n).cloned().ok_or_else(|| {
                RunError::Runtime(format!("host function `{n}` was not registered"))
            }),
            Binding::Memory(n) => self
                .memory
                .get(n)
                .cloned()
                .ok_or_else(|| RunError::Runtime(format!("memory `{n}` was not provided"))),
            Binding::Builtin(ns) => Err(RunError::Internal(format!(
                "builtin namespace `{ns}` used as a value"
            ))),
        }
    }

    fn eval_field(&mut self, base: &Expr, name: &Ident) -> Result<Value, RunError> {
        if let Some(ns) = self.builtin_namespace(base) {
            return field_value(ns, &name.node);
        }
        let base_v = self.eval_expr(base)?;
        match base_v {
            Value::Table(t) => Ok(t.borrow().get(&name.node).cloned().unwrap_or(Value::Nil)),
            other => Err(RunError::Internal(format!(
                "cannot read field `{}` of a {} value",
                name.node,
                other.type_name()
            ))),
        }
    }

    fn eval_index(&mut self, base: &Expr, index: &Expr) -> Result<Value, RunError> {
        let base_v = self.eval_expr(base)?;
        match base_v {
            Value::Array(a) => {
                let idx = self.eval_number(index)?;
                if idx < 1.0 || idx.fract() != 0.0 {
                    return Ok(Value::Nil);
                }
                let i = idx as usize;
                Ok(a.borrow().get(i - 1).cloned().unwrap_or(Value::Nil))
            }
            Value::Table(t) => {
                let key = self.eval_string_key(index)?;
                Ok(t.borrow().get(&key).cloned().unwrap_or(Value::Nil))
            }
            other => Err(RunError::Internal(format!(
                "cannot index a {} value",
                other.type_name()
            ))),
        }
    }

    fn eval_call(&mut self, callee: &Expr, args: &[Expr]) -> Result<Value, RunError> {
        // Plain builtin value call: `tostring(...)`, `tonumber(...)`.
        if let ExprKind::Name(_) = &callee.kind
            && let Some(Binding::Builtin(ns)) = self.res.binding(callee.span)
        {
            let argv = self.eval_list(args)?;
            return value_call(ns, &argv);
        }
        // Builtin namespace member call: `math.floor(...)`, `string.sub(...)`.
        if let ExprKind::Field { base, name } = &callee.kind
            && let Some(ns) = self.builtin_namespace(base)
        {
            let argv = self.eval_list(args)?;
            return member_call(ns, &name.node, &argv);
        }
        let func = self.eval_expr(callee)?;
        let argv = self.eval_list(args)?;
        self.call_value(&func, argv)
    }

    fn eval_table(&mut self, fields: &[Field]) -> Result<Value, RunError> {
        // Mirror the checker's shape inference: all-positional → array; otherwise table.
        let all_positional =
            !fields.is_empty() && fields.iter().all(|f| matches!(f, Field::Positional(_)));
        if all_positional || fields.is_empty() {
            let mut items = Vec::with_capacity(fields.len());
            for f in fields {
                if let Field::Positional(e) = f {
                    items.push(self.eval_expr(e)?);
                }
            }
            return Ok(Value::array(items));
        }
        let mut map = BTreeMap::new();
        for f in fields {
            match f {
                Field::Named { name, value } => {
                    let v = self.eval_expr(value)?;
                    map.insert(name.node.clone(), v);
                }
                Field::Keyed { key, value } => {
                    let k = self.eval_string_key(key)?;
                    let v = self.eval_expr(value)?;
                    map.insert(k, v);
                }
                Field::Positional(e) => {
                    // The checker rejects mixed shapes; treat as internal if reached.
                    let _ = self.eval_expr(e)?;
                    return Err(RunError::Internal(
                        "mixed positional/keyed table reached the interpreter".into(),
                    ));
                }
            }
        }
        Ok(Value::table(map))
    }

    fn eval_unary(&mut self, op: UnOp, operand: &Expr) -> Result<Value, RunError> {
        let v = self.eval_expr(operand)?;
        match op {
            UnOp::Neg => match v {
                Value::Number(n) => Ok(Value::Number(-n)),
                other => Err(RunError::Internal(format!(
                    "unary `-` on a {} value",
                    other.type_name()
                ))),
            },
            UnOp::Not => Ok(Value::Bool(!v.is_truthy())),
            UnOp::Len => match v {
                Value::Str(s) => Ok(Value::Number(s.len() as f64)),
                Value::Array(a) => Ok(Value::Number(a.borrow().len() as f64)),
                other => Err(RunError::Internal(format!(
                    "`#` on a {} value",
                    other.type_name()
                ))),
            },
        }
    }

    fn eval_binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr) -> Result<Value, RunError> {
        use BinOp::*;
        // Short-circuiting logical operators (Lua semantics, used post-typecheck).
        match op {
            And => {
                let l = self.eval_expr(lhs)?;
                return if l.is_truthy() {
                    self.eval_expr(rhs)
                } else {
                    Ok(l)
                };
            }
            Or => {
                let l = self.eval_expr(lhs)?;
                return if l.is_truthy() {
                    Ok(l)
                } else {
                    self.eval_expr(rhs)
                };
            }
            _ => {}
        }

        let l = self.eval_expr(lhs)?;
        let r = self.eval_expr(rhs)?;
        match op {
            Add | Sub | Mul | Div | FloorDiv | Mod | Pow => {
                let (a, b) = (number(&l)?, number(&r)?);
                Ok(Value::Number(match op {
                    Add => a + b,
                    Sub => a - b,
                    Mul => a * b,
                    Div => a / b,
                    FloorDiv => (a / b).floor(),
                    Mod => a - (a / b).floor() * b,
                    Pow => a.powf(b),
                    _ => unreachable!(),
                }))
            }
            Concat => Ok(Value::string(format!(
                "{}{}",
                string_of(&l)?,
                string_of(&r)?
            ))),
            Lt | Le | Gt | Ge => {
                let ord = compare(&l, &r)?;
                Ok(Value::Bool(match op {
                    Lt => ord == std::cmp::Ordering::Less,
                    Le => ord != std::cmp::Ordering::Greater,
                    Gt => ord == std::cmp::Ordering::Greater,
                    Ge => ord != std::cmp::Ordering::Less,
                    _ => unreachable!(),
                }))
            }
            Eq => Ok(Value::Bool(values_equal(&l, &r))),
            Ne => Ok(Value::Bool(!values_equal(&l, &r))),
            And | Or => unreachable!("handled above"),
        }
    }

    /// If `expr` is a bare name bound to a builtin namespace, return it.
    fn builtin_namespace(&self, expr: &Expr) -> Option<&'static str> {
        if let ExprKind::Name(_) = &expr.kind
            && let Some(Binding::Builtin(ns)) = self.res.binding(expr.span)
        {
            return Some(ns);
        }
        None
    }
}

// ---- builtin helpers --------------------------------------------------------

fn number(v: &Value) -> Result<f64, RunError> {
    v.as_f64()
        .ok_or_else(|| RunError::Internal(format!("expected a number, found {}", v.type_name())))
}

fn string_of(v: &Value) -> Result<String, RunError> {
    match v {
        Value::Str(s) => Ok(s.to_string()),
        other => Err(RunError::Internal(format!(
            "expected a string, found {}",
            other.type_name()
        ))),
    }
}

fn compare(l: &Value, r: &Value) -> Result<std::cmp::Ordering, RunError> {
    match (l, r) {
        (Value::Number(a), Value::Number(b)) => a
            .partial_cmp(b)
            .ok_or_else(|| RunError::Runtime("comparison with NaN".into())),
        (Value::Str(a), Value::Str(b)) => Ok(a.cmp(b)),
        _ => Err(RunError::Internal(format!(
            "cannot compare {} with {}",
            l.type_name(),
            r.type_name()
        ))),
    }
}

/// Convenience: parse, resolve, and interpret a call against a fresh interpreter with no
/// host functions or memory. Mainly for tests and quick experiments.
pub fn run_call(
    src: &str,
    cfg: &ResolveConfig,
    func: &str,
    args: Vec<Value>,
) -> Result<Value, RunError> {
    let module = crate::parse(src).map_err(diag_to_run)?;
    let res = crate::resolve::resolve(&module, cfg).map_err(diag_to_run)?;
    let mut interp = Interpreter::new(&module, &res)?;
    interp.call(func, args)
}

fn diag_to_run(d: Diagnostics) -> RunError {
    RunError::Runtime(format!("{d}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interp_module<'a>(module: &'a Module, res: &'a Resolution) -> Interpreter<'a> {
        Interpreter::new(module, res).expect("interpreter init")
    }

    fn build<'a>(
        src: &str,
        cfg: &ResolveConfig,
        store: &'a mut Option<(Module, Resolution)>,
    ) -> Interpreter<'a> {
        let module = crate::parse(src).expect("parse");
        let res = crate::resolve::resolve(&module, cfg).expect("resolve");
        *store = Some((module, res));
        let (m, r) = store.as_ref().unwrap();
        interp_module(m, r)
    }

    fn empty() -> ResolveConfig {
        ResolveConfig::default()
    }

    #[test]
    fn calls_a_simple_function() {
        let v = run_call(
            "function add(a, b) return a + b end",
            &empty(),
            "add",
            vec![Value::Number(2.0), Value::Number(3.0)],
        )
        .unwrap();
        assert_eq!(v.as_f64(), Some(5.0));
    }

    #[test]
    fn recursion_with_loop_and_branch() {
        let src = "function fact(n)\n\
                     local acc = 1\n\
                     local i = 2\n\
                     while i <= n do\n\
                       acc = acc * i\n\
                       i = i + 1\n\
                     end\n\
                     return acc\n\
                   end";
        let v = run_call(src, &empty(), "fact", vec![Value::Number(5.0)]).unwrap();
        assert_eq!(v.as_f64(), Some(120.0));
    }

    #[test]
    fn mutual_recursion() {
        let src = "function even(n)\n\
                     if n == 0 then return true end\n\
                     return odd(n - 1)\n\
                   end\n\
                   function odd(n)\n\
                     if n == 0 then return false end\n\
                     return even(n - 1)\n\
                   end";
        let mut store = None;
        let mut it = build(src, &empty(), &mut store);
        assert_eq!(
            it.call("even", vec![Value::Number(10.0)])
                .unwrap()
                .as_bool(),
            Some(true)
        );
        assert_eq!(
            it.call("even", vec![Value::Number(7.0)]).unwrap().as_bool(),
            Some(false)
        );
    }

    #[test]
    fn closure_captures_upvalue() {
        let src = "function make(base)\n\
                     local add = function(x) return x + base end\n\
                     return add(10)\n\
                   end";
        let v = run_call(src, &empty(), "make", vec![Value::Number(5.0)]).unwrap();
        assert_eq!(v.as_f64(), Some(15.0));
    }

    #[test]
    fn closure_escapes_its_defining_frame() {
        // The closure returned by `make_adder` outlives `make_adder`'s activation, so the
        // captured `n` slot must stay alive past the inner return. Per-slot capture keeps
        // exactly that slot reachable (and nothing more) — no leaked frame, no dangling
        // upvalue.
        let src = "function make_adder(n)\n\
                     return function(x) return x + n end\n\
                   end\n\
                   function use()\n\
                     local add5 = make_adder(5)\n\
                     return add5(10)\n\
                   end";
        let v = run_call(src, &empty(), "use", vec![]).unwrap();
        assert_eq!(v.as_f64(), Some(15.0));
    }

    #[test]
    fn closure_writes_propagate_through_shared_upvalue() {
        // Two calls to the same closure must observe each other's writes to the captured
        // `n` — capture is by shared slot, not by value snapshot.
        let src = "function counter()\n\
                     local n = 0\n\
                     local inc = function() n = n + 1 return n end\n\
                     local a = inc()\n\
                     local b = inc()\n\
                     return a + b\n\
                   end";
        let v = run_call(src, &empty(), "counter", vec![]).unwrap();
        assert_eq!(v.as_f64(), Some(3.0)); // 1 + 2
    }

    #[test]
    fn generic_for_sums_array() {
        let src = "function total(xs)\n\
                     local s = 0\n\
                     for _, v in ipairs(xs) do\n\
                       s = s + v\n\
                     end\n\
                     return s\n\
                   end";
        let arr = Value::array(vec![
            Value::Number(1.0),
            Value::Number(2.0),
            Value::Number(3.0),
        ]);
        let v = run_call(src, &empty(), "total", vec![arr]).unwrap();
        assert_eq!(v.as_f64(), Some(6.0));
    }

    #[test]
    fn array_append_idiom() {
        let src = "function grow()\n\
                     local out = { 1, 2 }\n\
                     out[#out + 1] = 3\n\
                     return out\n\
                   end";
        let v = run_call(src, &empty(), "grow", vec![]).unwrap();
        let items = v.as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[2].as_f64(), Some(3.0));
    }

    #[test]
    fn numeric_for_with_step() {
        let src = "function countdown(n)\n\
                     local s = 0\n\
                     for i = n, 1, -1 do\n\
                       s = s + i\n\
                     end\n\
                     return s\n\
                   end";
        let v = run_call(src, &empty(), "countdown", vec![Value::Number(4.0)]).unwrap();
        assert_eq!(v.as_f64(), Some(10.0));
    }

    #[test]
    fn break_exits_loop() {
        let src = "function firsthit(xs)\n\
                     local found = 0\n\
                     for _, v in ipairs(xs) do\n\
                       if v > 2 then\n\
                         found = v\n\
                         break\n\
                       end\n\
                     end\n\
                     return found\n\
                   end";
        let arr = Value::array(vec![
            Value::Number(1.0),
            Value::Number(3.0),
            Value::Number(5.0),
        ]);
        let v = run_call(src, &empty(), "firsthit", vec![arr]).unwrap();
        assert_eq!(v.as_f64(), Some(3.0));
    }

    #[test]
    fn string_concat_and_builtins() {
        let src = "function greet(name)\n\
                     return \"hi \" .. string.upper(name)\n\
                   end";
        let v = run_call(src, &empty(), "greet", vec![Value::string("bob")]).unwrap();
        assert_eq!(v.as_string().as_deref(), Some("hi BOB"));
    }

    #[test]
    fn math_floor_and_const() {
        let src = "K = 10\n\
                   function f(x) return math.floor(x) + K end";
        let v = run_call(src, &empty(), "f", vec![Value::Number(3.7)]).unwrap();
        assert_eq!(v.as_f64(), Some(13.0));
    }

    #[test]
    fn tostring_formats_integers() {
        let src = "function f(n) return tostring(n) end";
        let v = run_call(src, &empty(), "f", vec![Value::Number(42.0)]).unwrap();
        assert_eq!(v.as_string().as_deref(), Some("42"));
    }

    #[test]
    fn logical_operators_short_circuit() {
        // `or` returns the first truthy operand; with bools that's the disjunction.
        let src = "function f(a, b) return a or b end";
        let v = run_call(
            src,
            &empty(),
            "f",
            vec![Value::Bool(false), Value::Bool(true)],
        )
        .unwrap();
        assert_eq!(v.as_bool(), Some(true));
    }

    #[test]
    fn host_function_is_called() {
        let src = "function f(n) return roll(n) + 1 end";
        let module = crate::parse(src).unwrap();
        let res = crate::resolve::resolve(
            &module,
            &ResolveConfig {
                host_functions: vec!["roll".into()],
                memory: vec![],
            },
        )
        .unwrap();
        let mut it = Interpreter::new(&module, &res).unwrap();
        it.set_host_function("roll", |args| {
            Ok(Value::Number(args[0].as_f64().unwrap_or(0.0) * 2.0))
        });
        let v = it.call("f", vec![Value::Number(3.0)]).unwrap();
        assert_eq!(v.as_f64(), Some(7.0));
    }

    #[test]
    fn host_function_error_propagates() {
        let src = "function f() return boom() end";
        let module = crate::parse(src).unwrap();
        let res = crate::resolve::resolve(
            &module,
            &ResolveConfig {
                host_functions: vec!["boom".into()],
                memory: vec![],
            },
        )
        .unwrap();
        let mut it = Interpreter::new(&module, &res).unwrap();
        it.set_host_function("boom", |_| Err(RunError::Host("kaboom".into())));
        let err = it.call("f", vec![]).unwrap_err();
        assert!(matches!(err, RunError::Host(msg) if msg == "kaboom"));
    }

    #[test]
    fn memory_persists_across_calls() {
        let src = "function spend(n)\n\
                     if mem.gold >= n then\n\
                       mem.gold = mem.gold - n\n\
                       return true\n\
                     end\n\
                     return false\n\
                   end";
        let module = crate::parse(src).unwrap();
        let res = crate::resolve::resolve(&module, &ResolveConfig::with_memory("mem")).unwrap();
        let mut it = Interpreter::new(&module, &res).unwrap();

        let mut initial = BTreeMap::new();
        initial.insert("gold".to_string(), Value::Number(100.0));
        it.set_memory("mem", Value::table(initial));

        assert_eq!(
            it.call("spend", vec![Value::Number(30.0)])
                .unwrap()
                .as_bool(),
            Some(true)
        );
        assert_eq!(
            it.call("spend", vec![Value::Number(50.0)])
                .unwrap()
                .as_bool(),
            Some(true)
        );
        // 100 - 30 - 50 = 20; a 30 spend still succeeds, a later large one fails.
        assert_eq!(
            it.call("spend", vec![Value::Number(30.0)])
                .unwrap()
                .as_bool(),
            Some(false)
        );

        let gold = it.memory("mem").unwrap().field("gold").unwrap();
        assert_eq!(gold.as_f64(), Some(20.0));
    }

    #[test]
    fn unknown_export_errors() {
        let module = crate::parse("function a() return 1 end").unwrap();
        let res = crate::resolve::resolve(&module, &empty()).unwrap();
        let mut it = Interpreter::new(&module, &res).unwrap();
        assert!(matches!(
            it.call("nope", vec![]),
            Err(RunError::UnknownExport(_))
        ));
    }

    #[test]
    fn curated_export_renames() {
        let src = "function impl() return 7 end\n\
                   return { run = impl }";
        let v = run_call(src, &empty(), "run", vec![]).unwrap();
        assert_eq!(v.as_f64(), Some(7.0));
    }
}
