//! The JIT **runtime context** and the `extern "C"` **shims** the generated code calls into
//! (`PLAN.md` Phase 7).
//!
//! ## The hybrid value model
//!
//! Compiled code passes **numbers and bools unboxed** (native `f64` / `i8`); every
//! *reference* value (`string`, `array`, `map`, `record`, optionals, `nil`) flows as an
//! `i64` **handle** — an index into the per-call [`RtCtx`] value table, which stores the
//! actual [`Value`]s. This delegates heap correctness to the already-proven runtime
//! ([`crate::value::Value`] + [`crate::runtime::builtins`]) while the calc/decision core
//! (arithmetic, comparisons, control flow) is genuinely native.
//!
//! Every shim takes the context pointer as its first argument. A reference op (build an
//! array, read a field, call a builtin/host fn) becomes a call to one of these shims. Scalars
//! are **boxed** into handles only at these boundaries (e.g. storing an `f64` into an array)
//! and **unboxed** straight back out, so a number never lives in the value table longer than
//! one op.
//!
//! ## Errors
//!
//! Fallible shims return the sentinel [`ERR`] (and stash the [`RunError`] in
//! [`RtCtx::error`]); the generated code compares against it and branches to the function's
//! error-exit. Script-to-script calls can't carry the sentinel in a scalar return, so the
//! callee sets [`RtCtx::error`] and the caller checks it via [`rt_errored`].

use std::any::Any;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use crate::runtime::builtins;
use crate::runtime::repr::{Repr, Slot};
use crate::value::{ClosureObj, NativeFn, RunError, Value};

/// A reference-value handle: an index into [`RtCtx::values`]. Handle `0` is always `nil`.
pub type Handle = u64;

/// The null handle — `nil`.
pub const NIL: Handle = 0;

/// Sentinel returned by a fallible shim to signal that it stored a [`RunError`] in
/// [`RtCtx::error`]. Never a valid handle (handles are small table indices).
pub const ERR: Handle = u64::MAX;

/// Compile-time constant pools, interned during codegen and shared (by [`Arc`]) with every
/// [`RtCtx`]. Shims index into these with the immediates baked into the generated code.
#[derive(Debug, Default)]
pub struct Pools {
    /// Constant string literals (`ConstString`).
    pub strings: Vec<String>,
    /// Field / map-key name literals (`FieldGet`/`FieldSet`).
    pub names: Vec<String>,
    /// Host function names, by host id.
    pub host_fns: Vec<String>,
    /// Plain-value builtin namespaces (`tostring`/`tonumber`), by id.
    pub builtin_values: Vec<String>,
    /// Namespace-member builtins (`math.floor`, …) as `(ns, member)`, by id.
    pub builtin_members: Vec<(String, String)>,
    /// Namespace value fields (`math.pi`, …) as `(ns, field)`, by id.
    pub namespace_fields: Vec<(String, String)>,
    /// Host memory binding names, by memory id.
    pub memories: Vec<String>,
    /// Lifted-closure function names (`MakeClosure`), by closure id.
    pub closure_names: Vec<String>,
}

impl Pools {
    fn intern(pool: &mut Vec<String>, s: &str) -> u32 {
        if let Some(i) = pool.iter().position(|x| x == s) {
            return i as u32;
        }
        pool.push(s.to_string());
        (pool.len() - 1) as u32
    }
    fn intern_pair(pool: &mut Vec<(String, String)>, a: &str, b: &str) -> u32 {
        if let Some(i) = pool.iter().position(|(x, y)| x == a && y == b) {
            return i as u32;
        }
        pool.push((a.to_string(), b.to_string()));
        (pool.len() - 1) as u32
    }

    pub fn intern_string(&mut self, s: &str) -> u32 {
        Self::intern(&mut self.strings, s)
    }
    pub fn intern_name(&mut self, s: &str) -> u32 {
        Self::intern(&mut self.names, s)
    }
    pub fn intern_host(&mut self, s: &str) -> u32 {
        Self::intern(&mut self.host_fns, s)
    }
    pub fn intern_builtin_value(&mut self, ns: &str) -> u32 {
        Self::intern(&mut self.builtin_values, ns)
    }
    pub fn intern_builtin_member(&mut self, ns: &str, member: &str) -> u32 {
        Self::intern_pair(&mut self.builtin_members, ns, member)
    }
    pub fn intern_namespace_field(&mut self, ns: &str, field: &str) -> u32 {
        Self::intern_pair(&mut self.namespace_fields, ns, field)
    }
    pub fn intern_memory(&mut self, name: &str) -> u32 {
        Self::intern(&mut self.memories, name)
    }
    pub fn intern_closure_name(&mut self, name: &str) -> u32 {
        Self::intern(&mut self.closure_names, name)
    }
}

/// One invocation's runtime state. Created fresh per top-level call (this *is* the per-call
/// arena: the value table is dropped when the call returns). Passed to the generated entry as
/// an opaque `*mut RtCtx`; shims reconstitute `&mut RtCtx` from it.
///
/// Has no lifetime parameter so it is sound to pass as a raw pointer: it owns its
/// [`Value`]s and shares the [`Pools`] by [`Arc`].
pub struct RtCtx {
    /// The value table; index `0` is permanently `nil`.
    values: Vec<Value>,
    pools: Arc<Pools>,
    /// Host functions resolved by id (parallel to [`Pools::host_fns`]).
    host: Vec<Option<NativeFn>>,
    /// Memory bindings resolved by id (parallel to [`Pools::memories`]).
    memory: Vec<Value>,
    /// Finalized native address of every compiled function, by name. Used to resolve the
    /// callee of an indirect closure call ([`rt_closure_code_addr`]).
    code_addrs: Arc<HashMap<String, u64>>,
    /// Keeps the compiled module's native code mapped for any closure created during this
    /// invocation that escapes to the host. Stamped into each [`Value::Closure`] by
    /// [`rt_closure_new`] so a returned closure outlives the call without dangling.
    keepalive: Option<Rc<dyn Any>>,
    /// The first error raised during this invocation, if any.
    pub error: Option<RunError>,
}

impl RtCtx {
    /// Build a context for one invocation. `host`/`memory` are resolved into id order by the
    /// caller (the compiled module) from the user-registered bindings.
    pub fn new(
        pools: Arc<Pools>,
        host: Vec<Option<NativeFn>>,
        memory: Vec<Value>,
        code_addrs: Arc<HashMap<String, u64>>,
        keepalive: Option<Rc<dyn Any>>,
    ) -> Self {
        RtCtx {
            values: vec![Value::Nil],
            pools,
            host,
            memory,
            code_addrs,
            keepalive,
            error: None,
        }
    }

    /// Box a value into the table, returning its handle. `nil` always maps to [`NIL`].
    pub fn intern(&mut self, v: Value) -> Handle {
        if matches!(v, Value::Nil) {
            return NIL;
        }
        self.values.push(v);
        (self.values.len() - 1) as Handle
    }

    /// Read a handle's value (cloning; reference values clone their `Rc`).
    pub fn value(&self, h: Handle) -> Value {
        self.values.get(h as usize).cloned().unwrap_or(Value::Nil)
    }

    /// Encode a call argument into a raw ABI word per its declared [`Repr`] (the **bits ABI**
    /// the trampolines decode). Scalars become raw bits with no value-table entry; only
    /// reference values are interned to a handle. Mirrors the trampoline's `decode_param`.
    pub fn encode_arg(&mut self, v: Value, repr: Repr) -> u64 {
        match repr {
            Repr::Number => Slot::from_number(v.as_f64().unwrap_or(0.0)).bits(),
            Repr::Bool => Slot::from_bool(v.as_bool().unwrap_or(false)).bits(),
            Repr::Ptr => self.intern(v),
            Repr::Unit => 0,
        }
    }

    /// Decode a trampoline's raw ABI result word back into a [`Value`] per the return
    /// [`Repr`]. Mirrors the trampoline's `encode_ret`.
    pub fn decode_ret(&self, bits: u64, repr: Repr) -> Value {
        match repr {
            Repr::Number => Value::Number(Slot::from_bits(bits).as_number()),
            Repr::Bool => Value::Bool(Slot::from_bits(bits).as_bool()),
            Repr::Ptr => self.value(bits),
            Repr::Unit => Value::Nil,
        }
    }

    /// Reset for reuse on the next invocation: drop all interned values (retaining the table's
    /// capacity, so steady-state calls don't reallocate) and clear any error. The permanent
    /// `nil` at index 0 is reinstated. Bindings (pools/host/memory/keepalive) are preserved;
    /// use [`rebind`](Self::rebind) when host/memory change. This keeps the per-call "arena"
    /// semantics — every invocation still starts with an empty value table — while letting the
    /// driver pool one context across calls instead of allocating a fresh one each time.
    pub fn reset(&mut self) {
        self.values.clear();
        self.values.push(Value::Nil);
        self.error = None;
    }

    /// Replace the host-function and memory bindings, for when the user re-registers them
    /// between pooled invocations.
    pub fn rebind(&mut self, host: Vec<Option<NativeFn>>, memory: Vec<Value>) {
        self.host = host;
        self.memory = memory;
    }

    fn get(&self, h: Handle) -> &Value {
        self.values.get(h as usize).unwrap_or(&Value::Nil)
    }

    fn fail(&mut self, e: RunError) -> Handle {
        if self.error.is_none() {
            self.error = Some(e);
        }
        ERR
    }
}

// SAFETY: every shim is called only by generated code that passes the same `*mut RtCtx` it
// was handed (the invocation's context), which outlives the call. The pointer is never null
// and never aliased mutably across shims (calls are sequential).
macro_rules! ctx {
    ($p:ident) => {
        // SAFETY: see module/abi notes — `$p` is a live, exclusive `RtCtx` pointer.
        unsafe { &mut *$p }
    };
}

// ---- boxing / unboxing ------------------------------------------------------

pub unsafe extern "C" fn rt_box_number(ctx: *mut RtCtx, n: f64) -> Handle {
    ctx!(ctx).intern(Value::Number(n))
}

pub unsafe extern "C" fn rt_box_bool(ctx: *mut RtCtx, b: i8) -> Handle {
    ctx!(ctx).intern(Value::Bool(b != 0))
}

pub unsafe extern "C" fn rt_unbox_number(ctx: *mut RtCtx, h: Handle) -> f64 {
    ctx!(ctx).get(h).as_f64().unwrap_or(0.0)
}

pub unsafe extern "C" fn rt_unbox_bool(ctx: *mut RtCtx, h: Handle) -> i8 {
    ctx!(ctx).get(h).as_bool().unwrap_or(false) as i8
}

// ---- constants / refs -------------------------------------------------------

pub unsafe extern "C" fn rt_const_string(ctx: *mut RtCtx, idx: u32) -> Handle {
    let ctx = ctx!(ctx);
    let s = ctx.pools.strings[idx as usize].clone();
    ctx.intern(Value::string(s))
}

pub unsafe extern "C" fn rt_memory_ref(ctx: *mut RtCtx, id: u32) -> Handle {
    let ctx = ctx!(ctx);
    match ctx.memory.get(id as usize).cloned() {
        Some(v) => ctx.intern(v),
        None => {
            let name = ctx.pools.memories[id as usize].clone();
            ctx.fail(RunError::Runtime(format!(
                "memory `{name}` was not provided"
            )))
        }
    }
}

pub unsafe extern "C" fn rt_namespace_field(ctx: *mut RtCtx, id: u32) -> Handle {
    let ctx = ctx!(ctx);
    let (ns, field) = ctx.pools.namespace_fields[id as usize].clone();
    match builtins::field_value(&ns, &field) {
        Ok(v) => ctx.intern(v),
        Err(e) => ctx.fail(e),
    }
}

// ---- construction -----------------------------------------------------------

pub unsafe extern "C" fn rt_array_new(ctx: *mut RtCtx, cap: u32) -> Handle {
    let ctx = ctx!(ctx);
    ctx.intern(Value::array(Vec::with_capacity(cap as usize)))
}

pub unsafe extern "C" fn rt_array_push(ctx: *mut RtCtx, arr: Handle, elem: Handle) {
    let ctx = ctx!(ctx);
    let v = ctx.value(elem);
    if let Value::Array(a) = ctx.get(arr) {
        a.borrow_mut().push(v);
    }
}

pub unsafe extern "C" fn rt_table_new(ctx: *mut RtCtx) -> Handle {
    ctx!(ctx).intern(Value::empty_table())
}

pub unsafe extern "C" fn rt_table_set(ctx: *mut RtCtx, tbl: Handle, key: Handle, val: Handle) {
    let ctx = ctx!(ctx);
    let k = ctx.get(key).as_string();
    let v = ctx.value(val);
    if let (Some(k), Value::Table(t)) = (k, ctx.get(tbl)) {
        t.borrow_mut().insert(k, v);
    }
}

// ---- indexed access ---------------------------------------------------------

pub unsafe extern "C" fn rt_array_get(ctx: *mut RtCtx, arr: Handle, idx: f64) -> Handle {
    let ctx = ctx!(ctx);
    let elem = match ctx.get(arr) {
        Value::Array(a) => {
            if idx < 1.0 || idx.fract() != 0.0 {
                None
            } else {
                a.borrow().get(idx as usize - 1).cloned()
            }
        }
        _ => None,
    };
    match elem {
        Some(v) => ctx.intern(v),
        None => NIL,
    }
}

pub unsafe extern "C" fn rt_array_set(
    ctx: *mut RtCtx,
    arr: Handle,
    idx: f64,
    val: Handle,
) -> Handle {
    let ctx = ctx!(ctx);
    let v = ctx.value(val);
    let a = match ctx.get(arr) {
        Value::Array(a) => a.clone(),
        _ => return ctx.fail(RunError::Internal("array set on a non-array".into())),
    };
    if idx < 1.0 || idx.fract() != 0.0 {
        return ctx.fail(RunError::Runtime(format!(
            "array index {} is not a positive integer",
            builtins::num_to_string(idx)
        )));
    }
    let i = idx as usize;
    let len = a.borrow().len();
    if i <= len {
        a.borrow_mut()[i - 1] = v;
        NIL
    } else if i == len + 1 {
        a.borrow_mut().push(v);
        NIL
    } else {
        ctx.fail(RunError::Runtime(format!(
            "array index {i} is out of range (length {len}); arrays may only grow by one"
        )))
    }
}

pub unsafe extern "C" fn rt_map_get(ctx: *mut RtCtx, map: Handle, key: Handle) -> Handle {
    let ctx = ctx!(ctx);
    let k = ctx.get(key).as_string();
    let v = match (k, ctx.get(map)) {
        (Some(k), Value::Table(t)) => t.borrow().get(&k).cloned(),
        _ => None,
    };
    match v {
        Some(v) => ctx.intern(v),
        None => NIL,
    }
}

pub unsafe extern "C" fn rt_map_set(ctx: *mut RtCtx, map: Handle, key: Handle, val: Handle) {
    // Same backing representation as a record/table write.
    unsafe { rt_table_set(ctx, map, key, val) }
}

pub unsafe extern "C" fn rt_field_get(ctx: *mut RtCtx, base: Handle, name_idx: u32) -> Handle {
    let ctx = ctx!(ctx);
    let name = ctx.pools.names[name_idx as usize].clone();
    let v = ctx.get(base).field(&name);
    match v {
        Some(v) => ctx.intern(v),
        None => NIL,
    }
}

pub unsafe extern "C" fn rt_field_set(ctx: *mut RtCtx, base: Handle, name_idx: u32, val: Handle) {
    let ctx = ctx!(ctx);
    let name = ctx.pools.names[name_idx as usize].clone();
    let v = ctx.value(val);
    if let Value::Table(t) = ctx.get(base) {
        t.borrow_mut().insert(name, v);
    }
}

pub unsafe extern "C" fn rt_map_keys(ctx: *mut RtCtx, base: Handle) -> Handle {
    let ctx = ctx!(ctx);
    let keys: Vec<Value> = match ctx.get(base) {
        Value::Table(t) => t
            .borrow()
            .keys()
            .map(|k| Value::string(k.clone()))
            .collect(),
        _ => Vec::new(),
    };
    ctx.intern(Value::array(keys))
}

// ---- scalar / value ops -----------------------------------------------------

pub unsafe extern "C" fn rt_len(ctx: *mut RtCtx, h: Handle) -> f64 {
    match ctx!(ctx).get(h) {
        Value::Str(s) => s.len() as f64,
        Value::Array(a) => a.borrow().len() as f64,
        _ => 0.0,
    }
}

pub unsafe extern "C" fn rt_concat(ctx: *mut RtCtx, l: Handle, r: Handle) -> Handle {
    let ctx = ctx!(ctx);
    let a = ctx.get(l).as_string().unwrap_or_default();
    let b = ctx.get(r).as_string().unwrap_or_default();
    ctx.intern(Value::string(format!("{a}{b}")))
}

/// String ordering: `-1` / `0` / `1`. (Number comparison is native; only strings call here.)
pub unsafe extern "C" fn rt_str_cmp(ctx: *mut RtCtx, l: Handle, r: Handle) -> i32 {
    let ctx = ctx!(ctx);
    let a = ctx.get(l).as_string().unwrap_or_default();
    let b = ctx.get(r).as_string().unwrap_or_default();
    match a.cmp(&b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// Value equality for reference / string operands (numbers and bools compare natively).
pub unsafe extern "C" fn rt_value_eq(ctx: *mut RtCtx, l: Handle, r: Handle) -> i8 {
    let ctx = ctx!(ctx);
    value_eq(ctx.get(l), ctx.get(r)) as i8
}

pub unsafe extern "C" fn rt_truthy(ctx: *mut RtCtx, h: Handle) -> i8 {
    !matches!(ctx!(ctx).get(h), Value::Nil | Value::Bool(false)) as i8
}

/// `f64` exponentiation (no native cranelift op).
pub extern "C" fn rt_pow(a: f64, b: f64) -> f64 {
    a.powf(b)
}

/// Whether an error has been recorded — checked after a script-to-script call (whose scalar
/// return can't carry the [`ERR`] sentinel).
pub unsafe extern "C" fn rt_errored(ctx: *mut RtCtx) -> i8 {
    ctx!(ctx).error.is_some() as i8
}

// ---- calls ------------------------------------------------------------------

/// Collect `argc` argument values from a handle array.
///
/// # Safety
/// `argv` must point to `argc` readable `u64` handles.
unsafe fn collect_args(ctx: &RtCtx, argv: *const Handle, argc: u32) -> Vec<Value> {
    let mut out = Vec::with_capacity(argc as usize);
    for i in 0..argc as usize {
        // SAFETY: caller guarantees `argv` has `argc` handles.
        let h = unsafe { *argv.add(i) };
        out.push(ctx.value(h));
    }
    out
}

pub unsafe extern "C" fn rt_call_host(
    ctx: *mut RtCtx,
    id: u32,
    argv: *const Handle,
    argc: u32,
) -> Handle {
    let ctx = ctx!(ctx);
    let args = unsafe { collect_args(ctx, argv, argc) };
    let f = match ctx.host.get(id as usize).and_then(|f| f.clone()) {
        Some(f) => f,
        None => {
            let name = ctx.pools.host_fns[id as usize].clone();
            return ctx.fail(RunError::Runtime(format!(
                "host function `{name}` was not registered"
            )));
        }
    };
    match f(&args) {
        Ok(v) => ctx.intern(v),
        Err(e) => ctx.fail(e),
    }
}

pub unsafe extern "C" fn rt_call_builtin_value(
    ctx: *mut RtCtx,
    id: u32,
    argv: *const Handle,
    argc: u32,
) -> Handle {
    let ctx = ctx!(ctx);
    let args = unsafe { collect_args(ctx, argv, argc) };
    let ns = ctx.pools.builtin_values[id as usize].clone();
    match builtins::value_call(&ns, &args) {
        Ok(v) => ctx.intern(v),
        Err(e) => ctx.fail(e),
    }
}

pub unsafe extern "C" fn rt_call_builtin_member(
    ctx: *mut RtCtx,
    id: u32,
    argv: *const Handle,
    argc: u32,
) -> Handle {
    let ctx = ctx!(ctx);
    let args = unsafe { collect_args(ctx, argv, argc) };
    let (ns, member) = ctx.pools.builtin_members[id as usize].clone();
    match builtins::member_call(&ns, &member, &args) {
        Ok(v) => ctx.intern(v),
        Err(e) => ctx.fail(e),
    }
}

// ---- closures (upvalues) ----------------------------------------------------

/// Box a value into a fresh shared upvalue cell; returns its handle.
pub unsafe extern "C" fn rt_cell_new(ctx: *mut RtCtx, val: Handle) -> Handle {
    let ctx = ctx!(ctx);
    let v = ctx.value(val);
    ctx.intern(Value::Cell(Rc::new(std::cell::RefCell::new(v))))
}

/// Read the value currently held in a cell.
pub unsafe extern "C" fn rt_cell_get(ctx: *mut RtCtx, cell: Handle) -> Handle {
    let ctx = ctx!(ctx);
    let v = match ctx.get(cell) {
        Value::Cell(c) => c.borrow().clone(),
        _ => Value::Nil,
    };
    ctx.intern(v)
}

/// Write a value into a cell.
pub unsafe extern "C" fn rt_cell_set(ctx: *mut RtCtx, cell: Handle, val: Handle) {
    let ctx = ctx!(ctx);
    let v = ctx.value(val);
    if let Value::Cell(c) = ctx.get(cell) {
        *c.borrow_mut() = v;
    }
}

/// Build a closure value: the lifted function named by `name_id` plus its captured cells
/// (`argc` handles at `argv`, each a cell). The closure carries the module keepalive so it
/// stays callable if it escapes to the host.
pub unsafe extern "C" fn rt_closure_new(
    ctx: *mut RtCtx,
    name_id: u32,
    argv: *const Handle,
    argc: u32,
) -> Handle {
    let ctx = ctx!(ctx);
    let env = unsafe { collect_args(ctx, argv, argc) };
    let code = ctx.pools.closure_names[name_id as usize].clone();
    let keepalive = ctx.keepalive.clone();
    ctx.intern(Value::Closure(Rc::new(ClosureObj {
        code,
        env,
        keepalive,
    })))
}

/// Read the i-th captured cell from a closure's environment.
pub unsafe extern "C" fn rt_closure_env_get(ctx: *mut RtCtx, clo: Handle, i: u32) -> Handle {
    let ctx = ctx!(ctx);
    let cell = match ctx.get(clo) {
        Value::Closure(c) => c.env.get(i as usize).cloned(),
        _ => None,
    };
    match cell {
        Some(v) => ctx.intern(v),
        None => NIL,
    }
}

/// Resolve the native code address of a closure's lifted function, for an indirect call.
pub unsafe extern "C" fn rt_closure_code_addr(ctx: *mut RtCtx, clo: Handle) -> i64 {
    let ctx = ctx!(ctx);
    let addr = match ctx.get(clo) {
        Value::Closure(c) => ctx.code_addrs.get(&c.code).copied(),
        _ => None,
    };
    match addr {
        Some(a) => a as i64,
        None => {
            ctx.fail(RunError::Internal(
                "indirect call to an unknown closure".into(),
            ));
            0
        }
    }
}

/// Local re-implementation of [`Value`] equality (the interpreter's is private): scalars by
/// value, reference types by `Rc` identity (Lua semantics).
fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Nil, Value::Nil) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => Rc::ptr_eq(x, y),
        (Value::Table(x), Value::Table(y)) => Rc::ptr_eq(x, y),
        // The JIT never produces these interpreter-only callables; the arms exist only so a
        // build with `interp` also enabled still matches them exhaustively by identity.
        #[cfg(feature = "interp")]
        (Value::Function(x), Value::Function(y)) => Rc::ptr_eq(x, y),
        #[cfg(feature = "interp")]
        (Value::Native(x), Value::Native(y)) => Rc::ptr_eq(x, y),
        (Value::Cell(x), Value::Cell(y)) => Rc::ptr_eq(x, y),
        (Value::Closure(x), Value::Closure(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

/// A shim's name and function pointer, for registration with the JIT linker.
pub fn shim_symbols() -> Vec<(&'static str, *const u8)> {
    vec![
        ("rt_box_number", rt_box_number as *const u8),
        ("rt_box_bool", rt_box_bool as *const u8),
        ("rt_unbox_number", rt_unbox_number as *const u8),
        ("rt_unbox_bool", rt_unbox_bool as *const u8),
        ("rt_const_string", rt_const_string as *const u8),
        ("rt_memory_ref", rt_memory_ref as *const u8),
        ("rt_namespace_field", rt_namespace_field as *const u8),
        ("rt_array_new", rt_array_new as *const u8),
        ("rt_array_push", rt_array_push as *const u8),
        ("rt_table_new", rt_table_new as *const u8),
        ("rt_table_set", rt_table_set as *const u8),
        ("rt_array_get", rt_array_get as *const u8),
        ("rt_array_set", rt_array_set as *const u8),
        ("rt_map_get", rt_map_get as *const u8),
        ("rt_map_set", rt_map_set as *const u8),
        ("rt_field_get", rt_field_get as *const u8),
        ("rt_field_set", rt_field_set as *const u8),
        ("rt_map_keys", rt_map_keys as *const u8),
        ("rt_len", rt_len as *const u8),
        ("rt_concat", rt_concat as *const u8),
        ("rt_str_cmp", rt_str_cmp as *const u8),
        ("rt_value_eq", rt_value_eq as *const u8),
        ("rt_truthy", rt_truthy as *const u8),
        ("rt_pow", rt_pow as *const u8),
        ("rt_errored", rt_errored as *const u8),
        ("rt_call_host", rt_call_host as *const u8),
        ("rt_call_builtin_value", rt_call_builtin_value as *const u8),
        (
            "rt_call_builtin_member",
            rt_call_builtin_member as *const u8,
        ),
        ("rt_cell_new", rt_cell_new as *const u8),
        ("rt_cell_get", rt_cell_get as *const u8),
        ("rt_cell_set", rt_cell_set as *const u8),
        ("rt_closure_new", rt_closure_new as *const u8),
        ("rt_closure_env_get", rt_closure_env_get as *const u8),
        ("rt_closure_code_addr", rt_closure_code_addr as *const u8),
    ]
}
