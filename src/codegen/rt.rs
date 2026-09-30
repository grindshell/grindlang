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
//! array, read a field, call a builtin/host fn) becomes a call to one of these shims.
//!
//! ## Operands, results, and the value table
//!
//! The value table lives for the whole invocation and is never compacted, so anything a loop
//! body interns per iteration is memory proportional to the iteration count. Two rules keep
//! ordinary loops from growing it:
//!
//! * **Scalars cross as bits.** A value going into a shim (a stored element, a call argument)
//!   or coming out of one (a field read, a call result) is an operand word plus a [`repr_tag`]
//!   immediate — the same bits encoding the trampolines use: a `number` is its `f64` bits, a
//!   `bool` is `0`/`1`, anything else is a handle. A `number` stored into a record or read back
//!   out never creates a table entry. (An *optional* scalar is a handle by type, so reading one
//!   still interns it.)
//! * **Reads share handles.** A reference value read out of somewhere else — a field, a cell,
//!   host memory, the string pool — is interned through [`RtCtx::intern_shared`], which reuses
//!   the handle already given to the same object this call. Freshly *created* values (a table
//!   literal, a concatenation) still take a new entry each: they are new objects.
//!
//! ## Errors
//!
//! Fallible shims return the sentinel [`ERR`] (and latch the [`RunError`] in the context); the
//! generated code compares against it and branches to the function's error-exit **at the call
//! site**, so nothing after a failed op runs. Shims whose return type can't carry the sentinel
//! report failure another way: a NaN comparison is detected natively and only *raises* through
//! [`rt_raise_nan_compare`] on the failing edge, and [`rt_closure_code_addr`] returns address
//! `0`. Script-to-script calls carry nothing at all — the callee latches the error and the
//! caller loads the context's `errored` byte directly (see [`ERRORED_OFFSET`]), keeping the
//! check off the shim-call path it would otherwise sit on.
//!
//! Latching without branching is not enough: the *value* of a failed op is harmless (a null
//! handle unboxing to a default), but the *statements after it* are not — they would run host
//! functions and write host memory that the interpreter oracles never reach.
//!
//! ## Panics
//!
//! A panic must not unwind out of a shim. One that reaches an `extern "C"` boundary ends the
//! process, and the unwinder couldn't cross the compiled frames beyond it anyway: cranelift-jit
//! registers no unwind info for them. Yet a shim can panic — a host function or memory method
//! can, and so can a `RefCell` borrow of a value the host holds borrowed across the call — and
//! each interpreter lets that panic propagate out of its `call`.
//!
//! So every shim runs its body under `catch_unwind`, which is how `define_shims!` defines them.
//! A caught panic is stashed in the context and latched as an error, the shim returns a
//! placeholder, and the generated code takes the error-exit as it would for any failed op, so no
//! later statement runs. Once control is back in Rust the driver resumes the panic, and the host
//! sees it come out of `call` just as the interpreters raise it.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::panic::{self, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::Arc;

use cranelift_codegen::ir::types;

use crate::ast::BinOp;
use crate::runtime::repr::Repr;
use crate::runtime::{builtins, ops};
use crate::value::{
    ClosureObj, DEFAULT_MAX_CALL_DEPTH, NativeFn, RunError, Value, call_depth_exceeded,
};

/// A reference-value handle: an index into [`RtCtx::values`]. Handle `0` is always `nil`.
pub type Handle = u64;

/// The null handle — `nil`.
pub const NIL: Handle = 0;

/// Sentinel returned by a fallible shim to signal that it stored a [`RunError`] in
/// [`RtCtx::error`]. Never a valid handle (handles are small table indices).
pub const ERR: Handle = u64::MAX;

/// Operand/result tag: the word is an `f64`'s bits.
pub const TAG_NUMBER: u32 = 0;
/// Operand/result tag: the word is a `bool` (`0`/`1`).
pub const TAG_BOOL: u32 = 1;
/// Operand/result tag: the word is a value-table [`Handle`].
pub const TAG_HANDLE: u32 = 2;

/// The tag generated code passes alongside an operand or result word of representation `r`
/// (see the module docs). `Unit` has no value; its word is the `nil` handle.
pub fn repr_tag(r: Repr) -> u32 {
    match r {
        Repr::Number => TAG_NUMBER,
        Repr::Bool => TAG_BOOL,
        Repr::Ptr | Repr::Unit => TAG_HANDLE,
    }
}

/// Compile-time constant pools, interned during codegen and shared (by [`Rc`]) with every
/// [`RtCtx`]. Shims index into these with the immediates baked into the generated code.
#[derive(Debug, Default)]
pub struct Pools {
    /// Constant string literals (`ConstString`), already in [`Value::Str`]'s form so evaluating
    /// a literal shares the pooled string rather than allocating a copy.
    pub strings: Vec<Rc<str>>,
    /// Field / map-key name literals (`FieldGet`/`FieldSet`).
    pub names: Vec<String>,
    /// Host function names, by host id.
    pub host_fns: Vec<String>,
    /// Host-memory method symbols (`"mem:method"`), by method id.
    pub methods: Vec<String>,
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
    /// Reference-valued top-level constants (`ConstRef`), by constant id. Only constants whose
    /// value is a reference are interned: a scalar has no identity to keep stable, so it is
    /// re-evaluated rather than cached (see [`RtCtx::const_cache`]).
    pub consts: Vec<String>,
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
        if let Some(i) = self.strings.iter().position(|x| &**x == s) {
            return i as u32;
        }
        self.strings.push(Rc::from(s));
        (self.strings.len() - 1) as u32
    }
    pub fn intern_name(&mut self, s: &str) -> u32 {
        Self::intern(&mut self.names, s)
    }
    pub fn intern_host(&mut self, s: &str) -> u32 {
        Self::intern(&mut self.host_fns, s)
    }
    pub fn intern_method(&mut self, s: &str) -> u32 {
        Self::intern(&mut self.methods, s)
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
    pub fn intern_const(&mut self, name: &str) -> u32 {
        Self::intern(&mut self.consts, name)
    }
}

/// One invocation's runtime state. Created fresh per top-level call (this *is* the per-call
/// arena: the value table is dropped when the call returns). Passed to the generated entry as
/// an opaque `*mut RtCtx`; shims reconstitute `&mut RtCtx` from it.
///
/// Has no lifetime parameter so it is sound to pass as a raw pointer: it owns its
/// [`Value`]s and shares the [`Pools`] by [`Rc`].
///
/// `#[repr(C)]` is **load-bearing**: generated code reads [`errored`](Self::errored) with a
/// plain load at offset 0 (see [`ERRORED_OFFSET`]), so that field must stay first and the
/// layout must stay stable. Reordering the fields silently miscompiles error propagation.
#[repr(C)]
pub struct RtCtx {
    /// Nonzero once an error has been latched — the same answer [`error`](Self::error) gives,
    /// in a form the JIT can read without a call. Kept in sync by [`fail`](Self::fail),
    /// [`reset`](Self::reset), and [`take_error`](Self::take_error); never written directly.
    errored: u8,
    /// The most script calls that may be active at once (`SPEC.md` §7.3). A trampoline loads
    /// it from [`MAX_CALL_DEPTH_OFFSET`] as the call budget it hands the function it enters,
    /// which counts down from there in a register (see `translate`'s module docs).
    max_call_depth: u32,
    /// The value table; index `0` is permanently `nil`.
    values: Vec<Value>,
    /// The handle already given to each object interned by [`intern_shared`](Self::intern_shared)
    /// this call, keyed by allocation address.
    shared: HashMap<usize, Handle, BuildHasherDefault<AddrHasher>>,
    pools: Rc<Pools>,
    /// Host functions resolved by id (parallel to [`Pools::host_fns`]).
    host: Vec<Option<NativeFn>>,
    /// Host-memory methods resolved by id (parallel to [`Pools::methods`]).
    methods: Vec<Option<NativeFn>>,
    /// Memory bindings resolved by id (parallel to [`Pools::memories`]), `None` where the host
    /// declared a binding but never provided one. Optional for the same reason `host` is:
    /// substituting a value for a missing binding would make [`rt_memory_ref`]'s
    /// not-provided error unreachable and turn an unbound memory into a silently valid `nil`.
    /// A binding the host explicitly set *to* `nil` is `Some(Value::Nil)` and stays legal —
    /// the same distinction the interpreters draw by map presence.
    memory: Vec<Option<Value>>,
    /// Finalized native address of every compiled function, by name. Used to resolve the
    /// callee of an indirect closure call ([`rt_closure_code_addr`]).
    code_addrs: Arc<HashMap<String, u64>>,
    /// Reference-valued constants already evaluated during this invocation, by constant id
    /// (parallel to [`Pools::consts`]).
    ///
    /// A constant is memoized **per call** (`SPEC.md` §3): within one invocation every read
    /// yields the same value — so a constant is equal to itself, which reference values decide
    /// by identity — and the cache is dropped with the rest of the per-call arena, so nothing
    /// survives between calls (§1). Populated lazily, so a constant a call never reads costs
    /// nothing.
    const_cache: Vec<Option<Handle>>,
    /// Allocations reachable from the constants read so far this call, which may not be written
    /// to (`SPEC.md` §3). Empty for a call that has read no composite constant, which is what
    /// keeps the check on the write path down to one branch.
    frozen: std::collections::HashSet<usize>,
    /// The compiled module this context belongs to. Stamped into every closure
    /// [`rt_closure_new`] builds, and compared against a closure's own stamp before invoking it
    /// ([`rt_closure_code_addr`]).
    origin: Rc<ClosureOrigin>,
    /// The first error raised during this invocation, if any. Private so it cannot drift out of
    /// sync with [`errored`](Self::errored); read it with [`take_error`](Self::take_error).
    error: Option<RunError>,
    /// A panic a shim caught during this invocation, for the driver to resume once the compiled
    /// code has returned (see the module docs).
    panic: Option<Box<dyn Any + Send>>,
}

/// The stamp a compiled module puts on every closure it builds: which module that was, plus
/// the keepalive holding that module's native code mapped.
///
/// A closure is only meaningful inside the module that created it — its `code` name is
/// synthetic and collides across modules, and its compiled body indexes that module's constant
/// pools by baked-in id. Carrying the origin lets both invocation paths reject a foreign
/// closure outright instead of running an unrelated function (or the right function against
/// the wrong pools).
pub struct ClosureOrigin {
    /// Process-unique id of the [`super::JitModule`] that built the closure. Ids come from a
    /// monotonic counter and are never reused, so a closure can't be mistaken for one from a
    /// later module that happens to reuse a dropped module's address.
    pub module_id: u64,
    /// Keeps the origin module's native code mapped for as long as any closure it built is
    /// reachable, so holding a closure past its module's drop stays sound.
    pub keepalive: Rc<dyn Any>,
}

/// The error both invocation paths raise for a closure built by a different module.
pub const FOREIGN_CLOSURE: &str =
    "closure belongs to a different compiled module and cannot be invoked here";

/// A host-supplied argument whose runtime shape contradicts the declared parameter type.
fn arg_type_error(want: &str, got: &Value) -> RunError {
    RunError::Runtime(format!(
        "expected a {want} argument, found {}",
        got.type_name()
    ))
}

/// A value flowing into a scalar slot whose runtime shape contradicts its static type.
fn unbox_type_error(want: &str, got: &Value) -> RunError {
    RunError::Runtime(format!("expected a {want}, found {}", got.type_name()))
}

/// The allocation a reference value points at, or `None` for a value with no identity (`nil`,
/// a scalar). Two values with the same address are the same object.
fn identity(v: &Value) -> Option<usize> {
    Some(match v {
        Value::Str(s) => Rc::as_ptr(s) as *const u8 as usize,
        Value::Array(a) => Rc::as_ptr(a) as *const () as usize,
        Value::Table(t) => Rc::as_ptr(t) as *const () as usize,
        Value::Tuple(t) => Rc::as_ptr(t) as *const () as usize,
        Value::Cell(c) => Rc::as_ptr(c) as *const () as usize,
        Value::Closure(c) => Rc::as_ptr(c) as *const () as usize,
        _ => return None,
    })
}

/// Hasher for [`RtCtx::shared`]'s address keys. An address is already unique, so SipHash's
/// flood resistance buys nothing, and this lookup sits on every reference read. The multiply
/// spreads the address and the fold brings high bits down, since aligned addresses have their
/// low bits clear and the table indexes buckets by the low bits.
#[derive(Default)]
struct AddrHasher(u64);

impl Hasher for AddrHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    // Only `usize` keys are hashed; this byte fold exists because the trait requires it.
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3);
        }
    }

    fn write_u64(&mut self, n: u64) {
        let h = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 32);
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
}

/// The id of the module that built `c`, or `None` if it carries no JIT origin stamp (an IR VM
/// closure, which the JIT can't run either).
pub fn closure_module_id(c: &ClosureObj) -> Option<u64> {
    c.origin
        .as_ref()?
        .downcast_ref::<ClosureOrigin>()
        .map(|o| o.module_id)
}

/// Byte offset of [`RtCtx::errored`] within the context. Generated code loads the flag from
/// here instead of calling into a shim — the check sits on the hot script-call path, where a
/// call would cost more than the branch it guards. Guaranteed by `#[repr(C)]` plus the field
/// being declared first; asserted in the module's tests.
pub const ERRORED_OFFSET: i32 = 0;

/// Byte offset of [`RtCtx::max_call_depth`], which the trampolines load directly.
pub const MAX_CALL_DEPTH_OFFSET: i32 = std::mem::offset_of!(RtCtx, max_call_depth) as i32;

impl RtCtx {
    /// Build a context for one invocation. `host`/`memory` are resolved into id order by the
    /// caller (the compiled module) from the user-registered bindings.
    pub fn new(
        pools: Rc<Pools>,
        host: Vec<Option<NativeFn>>,
        methods: Vec<Option<NativeFn>>,
        memory: Vec<Option<Value>>,
        code_addrs: Arc<HashMap<String, u64>>,
        origin: Rc<ClosureOrigin>,
    ) -> Self {
        RtCtx {
            errored: 0,
            max_call_depth: DEFAULT_MAX_CALL_DEPTH,
            values: vec![Value::Nil],
            shared: HashMap::default(),
            const_cache: vec![None; pools.consts.len()],
            frozen: std::collections::HashSet::new(),
            pools,
            host,
            methods,
            memory,
            code_addrs,
            origin,
            error: None,
            panic: None,
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

    /// Intern a value that was *read* rather than created, reusing the handle this call already
    /// gave the same object. Reading one table field a thousand times is one object and takes
    /// one entry, not a thousand.
    ///
    /// Keying on the allocation address is sound because the table holds a clone of every value
    /// it interns until [`reset`](Self::reset): no interned object can be freed mid-call, so no
    /// other object can come to occupy its address. Handles are interchangeable references —
    /// nothing ever overwrites a table entry — so two SSA values sharing one is harmless.
    pub fn intern_shared(&mut self, v: Value) -> Handle {
        let Some(addr) = identity(&v) else {
            return self.intern(v);
        };
        if let Some(&h) = self.shared.get(&addr) {
            return h;
        }
        let h = self.intern(v);
        self.shared.insert(addr, h);
        h
    }

    /// Read a handle's value (cloning; reference values clone their `Rc`).
    pub fn value(&self, h: Handle) -> Value {
        self.values.get(h as usize).cloned().unwrap_or(Value::Nil)
    }

    /// Decode an operand word generated code passed with tag `tag` (see [`repr_tag`]).
    fn operand(&self, bits: u64, tag: u32) -> Value {
        match tag {
            TAG_NUMBER => Value::Number(f64::from_bits(bits)),
            TAG_BOOL => Value::Bool(bits != 0),
            _ => self.value(bits),
        }
    }

    /// Encode a value a shim produced as the result word its destination's tag asks for.
    ///
    /// A scalar destination gets raw bits and never touches the value table. Its static type
    /// promises a `number`/`bool`, but a host-supplied value (an off-schema memory binding, a
    /// host function returning the wrong shape) can contradict that at runtime; that latches an
    /// error and returns `0`, and generated code guards on the errored flag before using it.
    fn result(&mut self, v: Value, tag: u32) -> u64 {
        match tag {
            TAG_NUMBER => match v.as_f64() {
                Some(n) => n.to_bits(),
                None => {
                    self.fail(unbox_type_error("number", &v));
                    0
                }
            },
            TAG_BOOL => match v.as_bool() {
                Some(b) => b as u64,
                None => {
                    self.fail(unbox_type_error("bool", &v));
                    0
                }
            },
            _ => self.intern_shared(v),
        }
    }

    /// Encode a call argument into a raw ABI word per its declared [`Repr`] (the **bits ABI**
    /// the trampolines decode). Scalars become raw bits with no value-table entry; only
    /// reference values are interned to a handle. Mirrors the trampoline's `decode_word`.
    ///
    /// A value that doesn't fit the declared scalar repr is **rejected**, not coerced. This is
    /// the one place the host boundary has to check: a scalar becomes raw `f64`/`i8` bits here,
    /// so by the time compiled code sees it there is nothing left to tell a string from the
    /// `0.0` it would have been silently turned into. Reference params keep their value and are
    /// checked where they are first forced into a scalar instead ([`RtCtx::result`],
    /// [`rt_unbox_number`]).
    pub fn encode_arg(&mut self, v: Value, repr: Repr) -> Result<u64, RunError> {
        match repr {
            Repr::Number => {
                let n = v.as_f64().ok_or_else(|| arg_type_error("number", &v))?;
                Ok(n.to_bits())
            }
            Repr::Bool => {
                let b = v.as_bool().ok_or_else(|| arg_type_error("bool", &v))?;
                Ok(b as u64)
            }
            Repr::Ptr => Ok(self.intern(v)),
            Repr::Unit => Ok(0),
        }
    }

    /// Decode a trampoline's raw ABI result word back into a [`Value`] per the return
    /// [`Repr`]. Mirrors the trampoline's `encode_word`.
    pub fn decode_ret(&self, bits: u64, repr: Repr) -> Value {
        match repr {
            Repr::Number => Value::Number(f64::from_bits(bits)),
            Repr::Bool => Value::Bool(bits != 0),
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
        self.shared.clear();
        // Constants are memoized per call: the next invocation re-evaluates what it reads, so
        // no constant (and nothing reachable from one) survives between calls.
        self.const_cache.fill(None);
        self.frozen.clear();
        self.error = None;
        self.errored = 0;
        self.panic = None;
    }

    /// Set how many script calls may be active at once, for calls entered from now on.
    pub fn set_max_call_depth(&mut self, depth: u32) {
        self.max_call_depth = depth;
    }

    /// Take the latched error, clearing the errored state. The only way out of the context, so
    /// [`error`](Self::error) and [`errored`](Self::errored) can't drift apart.
    pub fn take_error(&mut self) -> Option<RunError> {
        self.errored = 0;
        self.error.take()
    }

    /// Take the payload of a panic a shim caught during this call. The driver resumes it once
    /// the compiled code has returned (see the module docs).
    pub fn take_panic(&mut self) -> Option<Box<dyn Any + Send>> {
        self.panic.take()
    }

    /// Replace the host-function, method, and memory bindings, for when the user re-registers
    /// them between pooled invocations.
    pub fn rebind(
        &mut self,
        host: Vec<Option<NativeFn>>,
        methods: Vec<Option<NativeFn>>,
        memory: Vec<Option<Value>>,
    ) {
        self.host = host;
        self.methods = methods;
        self.memory = memory;
    }

    /// Number of entries in the value table (including the permanent `nil`).
    #[cfg(test)]
    pub(crate) fn value_count(&self) -> usize {
        self.values.len()
    }

    fn get(&self, h: Handle) -> &Value {
        self.values.get(h as usize).unwrap_or(&Value::Nil)
    }

    fn fail(&mut self, e: RunError) -> Handle {
        if self.error.is_none() {
            self.error = Some(e);
        }
        self.errored = 1;
        ERR
    }

    /// Keep a panic a shim caught, and latch an error so the generated code leaves the way it
    /// does after any failed op. The error is never reported: the driver resumes the panic.
    #[cold]
    #[inline(never)]
    fn panicked(&mut self, payload: Box<dyn Any + Send>) {
        self.panic.get_or_insert(payload);
        self.fail(RunError::Internal("a runtime shim panicked".into()));
    }
}

// ---- panics -----------------------------------------------------------------

/// What a shim returns when its body panicked. Never used: every shim call in generated code is
/// guarded (see `translate`'s module docs). A handle or word is [`ERR`], so a call site that
/// guards on the sentinel takes the error-exit as surely as one that guards on the flag.
trait Failed {
    const FAILED: Self;
}

impl Failed for u64 {
    const FAILED: Self = ERR;
}

impl Failed for f64 {
    const FAILED: Self = 0.0;
}

impl Failed for i8 {
    const FAILED: Self = 0;
}

impl Failed for i32 {
    const FAILED: Self = 0;
}

/// Address `0`, the failure [`rt_closure_code_addr`] already reports.
impl Failed for i64 {
    const FAILED: Self = 0;
}

impl Failed for () {
    const FAILED: Self = ();
}

/// Run a shim's body against the context, stopping a panic before it can unwind into the
/// generated code that called the shim (see the module docs).
///
/// Unwind safety is asserted rather than proven. A panic can't leave the context unsound — its
/// collections stay valid Rust values, and a `RefCell` borrow is released as the panic unwinds —
/// and afterwards the context is only used to take the error-exit and then reset. Whatever else
/// the body touched, such as host memory a host function wrote before panicking, is left as the
/// interpreters would leave it.
///
/// # Safety
/// `ctx` must be the live context the generated code was handed, with no other reference to it.
unsafe fn catching<T: Failed>(ctx: *mut RtCtx, body: impl FnOnce(&mut RtCtx) -> T) -> T {
    // SAFETY: the caller's contract.
    let ctx = unsafe { &mut *ctx };
    match panic::catch_unwind(AssertUnwindSafe(|| body(&mut *ctx))) {
        Ok(v) => v,
        Err(payload) => {
            ctx.panicked(payload);
            T::FAILED
        }
    }
}

/// Defines runtime shims. Each is written `fn name(ctx, args...) -> ret { body }`, with `ctx`
/// bound to the `&mut RtCtx`, and becomes a `pub unsafe extern "C" fn` that takes the context
/// pointer first and runs `body` under [`catching`]. A shim defined any other way would let a
/// panic reach the `extern "C"` boundary, which ends the process.
macro_rules! define_shims {
    ($(
        $(#[$meta:meta])*
        fn $name:ident($ctx:ident $(, $arg:ident: $ty:ty)* $(,)?) $(-> $ret:ty)? $body:block
    )*) => {$(
        $(#[$meta])*
        pub unsafe extern "C" fn $name(ctx: *mut RtCtx $(, $arg: $ty)*) $(-> $ret)? {
            let body = |$ctx: &mut RtCtx| $body;
            // SAFETY: generated code passes the context it was handed, which outlives the call.
            // Shims run one at a time, so nothing else holds a reference to it.
            unsafe { catching(ctx, body) }
        }
    )*};
}

// ---- boxing / unboxing ------------------------------------------------------

define_shims! {
    fn rt_box_number(ctx, n: f64) -> Handle {
        ctx.intern(Value::Number(n))
    }

    fn rt_box_bool(ctx, b: i8) -> Handle {
        ctx.intern(Value::Bool(b != 0))
    }

    /// Unbox a handle into a native `f64`. A value whose runtime shape contradicts its static type
    /// is an error, not a silent `0.0` — matching `interp`'s `eval_number` and `ir::Vm`'s `num`,
    /// which both fail rather than default. This is where an ill-typed *host-supplied* value gets
    /// caught: a memory bound off-schema, or a host function returning the wrong shape, reaches
    /// compiled code as a handle and is only forced into a scalar here.
    ///
    /// The generated code guards on the errored flag after this call, so the `0.0` returned on the
    /// failing path is never observed.
    fn rt_unbox_number(ctx, h: Handle) -> f64 {
        match ctx.get(h).as_f64() {
            Some(n) => n,
            None => {
                let e = unbox_type_error("number", ctx.get(h));
                ctx.fail(e);
                0.0
            }
        }
    }

    /// The `bool` counterpart of [`rt_unbox_number`]; same contract.
    fn rt_unbox_bool(ctx, h: Handle) -> i8 {
        match ctx.get(h).as_bool() {
            Some(b) => b as i8,
            None => {
                let e = unbox_type_error("bool", ctx.get(h));
                ctx.fail(e);
                0
            }
        }
    }
}

// ---- constants / refs -------------------------------------------------------

/// Latch the immutable-constant error if `h` refers to (part of) a constant, returning [`ERR`]
/// for the caller to propagate. `None` means the write may proceed.
fn reject_frozen(ctx: &mut RtCtx, h: Handle) -> Option<Handle> {
    if ctx.frozen.is_empty() {
        return None; // the common case: no composite constant read this call
    }
    if ctx.get(h).is_frozen(&ctx.frozen) {
        return Some(ctx.fail(RunError::Runtime(
            crate::value::IMMUTABLE_CONSTANT.to_string(),
        )));
    }
    None
}

define_shims! {
    /// Evaluate a string literal. The pooled string is shared, not copied, so a literal inside a
    /// loop takes one table entry for the whole call.
    fn rt_const_string(ctx, idx: u32) -> Handle {
        let s = Rc::clone(&ctx.pools.strings[idx as usize]);
        ctx.intern_shared(Value::Str(s))
    }

    /// The constant already evaluated during this call, or [`ERR`] as a *miss* marker. A miss
    /// can't be signalled with [`NIL`] because `nil` is a legal constant value, whereas `ERR` is
    /// never a valid handle. This is a lookup, not a failure path — it never latches an error.
    fn rt_const_cached(ctx, id: u32) -> Handle {
        match ctx.const_cache.get(id as usize) {
            Some(Some(h)) => *h,
            _ => ERR,
        }
    }

    /// Memoize a freshly evaluated constant for the rest of this call, and mark everything inside
    /// it unwritable — the runtime half of a constant's immutability (`SPEC.md` §3), which catches
    /// a write that reached it through an alias and so slipped past `E0307`.
    fn rt_const_store(ctx, id: u32, h: Handle) {
        let v = ctx.value(h);
        v.collect_reachable(&mut ctx.frozen);
        if let Some(slot) = ctx.const_cache.get_mut(id as usize) {
            *slot = Some(h);
        }
    }

    /// Read a host memory binding, as a result word per `tag`. A binding the host declared but
    /// never provided is an error at the point of use, matching both interpreters — not a silent
    /// `nil`.
    fn rt_memory_ref(ctx, id: u32, tag: u32) -> u64 {
        match ctx.memory.get(id as usize).cloned().flatten() {
            Some(v) => ctx.result(v, tag),
            None => {
                let name = ctx.pools.memories[id as usize].clone();
                ctx.fail(RunError::Runtime(format!(
                    "memory `{name}` was not provided"
                )))
            }
        }
    }

    fn rt_namespace_field(ctx, id: u32, tag: u32) -> u64 {
        let (ns, field) = &ctx.pools.namespace_fields[id as usize];
        match builtins::field_value(ns, field) {
            Ok(v) => ctx.result(v, tag),
            Err(e) => ctx.fail(e),
        }
    }
}

// ---- construction -----------------------------------------------------------

define_shims! {
    fn rt_array_new(ctx, cap: u32) -> Handle {
        ctx.intern(Value::array(Vec::with_capacity(cap as usize)))
    }

    fn rt_array_push(ctx, arr: Handle, bits: u64, tag: u32) {
        let v = ctx.operand(bits, tag);
        if let Value::Array(a) = ctx.get(arr) {
            a.borrow_mut().push(v);
        }
    }
}

// ---- tuples (multi-value returns, `SPEC.md` §5.5) ---------------------------

define_shims! {
    fn rt_tuple_new(ctx, cap: u32) -> Handle {
        ctx.intern(Value::tuple(Vec::with_capacity(cap as usize)))
    }

    fn rt_tuple_push(ctx, tup: Handle, bits: u64, tag: u32) {
        let v = ctx.operand(bits, tag);
        if let Value::Tuple(t) = ctx.get(tup) {
            t.borrow_mut().push(v);
        }
    }

    /// Read the `idx`-th (0-based, always in range) element of a tuple, as a result word per
    /// `tag`.
    fn rt_tuple_get(ctx, tup: Handle, idx: u32, tag: u32) -> u64 {
        let elem = match ctx.get(tup) {
            Value::Tuple(t) => t.borrow().get(idx as usize).cloned(),
            _ => None,
        };
        ctx.result(elem.unwrap_or(Value::Nil), tag)
    }

    fn rt_table_new(ctx) -> Handle {
        ctx.intern(Value::empty_table())
    }
}

// ---- indexed access ---------------------------------------------------------

/// Store an operand into field `name_idx` of the table `base`. Anything but a table is left alone.
fn set_field(ctx: &mut RtCtx, base: Handle, name_idx: u32, bits: u64, tag: u32) {
    let v = ctx.operand(bits, tag);
    if let Value::Table(t) = ctx.get(base) {
        ops::table_set(t, &ctx.pools.names[name_idx as usize], v);
    }
}

define_shims! {
    fn rt_array_get(ctx, arr: Handle, idx: f64, tag: u32) -> u64 {
        let elem = match ctx.get(arr) {
            Value::Array(a) => ops::array_get(a, idx),
            _ => Value::Nil,
        };
        ctx.result(elem, tag)
    }

    fn rt_array_set(ctx, arr: Handle, idx: f64, bits: u64, tag: u32) -> Handle {
        if let Some(e) = reject_frozen(ctx, arr) {
            return e;
        }
        let v = ctx.operand(bits, tag);
        let result = match ctx.get(arr) {
            Value::Array(a) => ops::array_set(a, idx, v),
            _ => Err(RunError::Internal("array set on a non-array".into())),
        };
        match result {
            Ok(()) => NIL,
            Err(e) => ctx.fail(e),
        }
    }

    fn rt_map_get(ctx, map: Handle, key: Handle, tag: u32) -> u64 {
        let v = match (ctx.get(key), ctx.get(map)) {
            (Value::Str(k), Value::Table(t)) => t.borrow().get(&**k).cloned(),
            _ => None,
        };
        ctx.result(v.unwrap_or(Value::Nil), tag)
    }

    /// Assign through a map key (`m[k] = v`), refusing a write to a constant. Also inserts the
    /// entries of a map literal with computed keys (`{ [k] = v }`), which [`rt_field_init`]'s
    /// compile-time names can't express; that table was just allocated, so it is never frozen.
    fn rt_map_set(ctx, map: Handle, key: Handle, bits: u64, tag: u32) -> Handle {
        if let Some(e) = reject_frozen(ctx, map) {
            return e;
        }
        let v = ctx.operand(bits, tag);
        // A map shares the record/table backing representation.
        if let (Value::Str(k), Value::Table(t)) = (ctx.get(key), ctx.get(map)) {
            ops::table_set(t, k, v);
        }
        NIL
    }

    fn rt_field_get(ctx, base: Handle, name_idx: u32, tag: u32) -> u64 {
        let v = ctx.get(base).field(&ctx.pools.names[name_idx as usize]);
        ctx.result(v.unwrap_or(Value::Nil), tag)
    }

    /// Populate a field while *building* a table literal. Never a user write — the table does not
    /// exist outside the constructor yet — so it needs no immutability check.
    fn rt_field_init(ctx, base: Handle, name_idx: u32, bits: u64, tag: u32) {
        set_field(ctx, base, name_idx, bits, tag);
    }

    /// Assign to a field (`t.x = v`). A user write, so it refuses one to a constant.
    fn rt_field_set(ctx, base: Handle, name_idx: u32, bits: u64, tag: u32) -> Handle {
        if let Some(e) = reject_frozen(ctx, base) {
            return e;
        }
        set_field(ctx, base, name_idx, bits, tag);
        NIL
    }

    fn rt_map_keys(ctx, base: Handle) -> Handle {
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
}

// ---- scalar / value ops -----------------------------------------------------

/// A string operand's text, borrowed; a non-string reads as `""`.
fn str_of(v: &Value) -> &str {
    match v {
        Value::Str(s) => s,
        _ => "",
    }
}

define_shims! {
    /// `#v`. The checker admits only strings and arrays, so the fallback `0` is never observed.
    fn rt_len(ctx, h: Handle) -> f64 {
        ops::len(ctx.get(h)).unwrap_or(0.0)
    }

    fn rt_concat(ctx, l: Handle, r: Handle) -> Handle {
        let joined = [str_of(ctx.get(l)), str_of(ctx.get(r))].concat();
        ctx.intern(Value::string(joined))
    }

    /// String ordering: `-1` / `0` / `1`. (Number comparison is native; only strings call here.)
    fn rt_str_cmp(ctx, l: Handle, r: Handle) -> i32 {
        match str_of(ctx.get(l)).cmp(str_of(ctx.get(r))) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }
    }

    /// Value equality for reference / string operands (numbers and bools compare natively), by
    /// the one rule every backend uses: [`Value::ref_eq`].
    fn rt_value_eq(ctx, l: Handle, r: Handle) -> i8 {
        ctx.get(l).ref_eq(ctx.get(r)) as i8
    }

    fn rt_truthy(ctx, h: Handle) -> i8 {
        ctx.get(h).is_truthy() as i8
    }
}

/// `f64` exponentiation (no native cranelift op). The one shim without a context, and with
/// nothing in it that can panic, so it is not defined through `define_shims!`.
pub extern "C" fn rt_pow(a: f64, b: f64) -> f64 {
    ops::arith(BinOp::Pow, a, b)
}

define_shims! {
    /// Latch the "ordered a NaN" error, matching the interpreters (both order operands with
    /// [`ops::compare`], which fails when `partial_cmp` returns `None`). The JIT lowers
    /// `< <= > >=` on numbers to a native unboxed `fcmp`, but an IEEE *unordered* compare
    /// silently yields `false` for a NaN operand — so without this the JIT would diverge from the
    /// two interpreters, breaking the three-oracle invariant.
    ///
    /// **Detecting** the NaN is native (an `fcmp unordered` alongside the relational one), so
    /// ordinary comparisons never call here; only the failing branch does. That matters because
    /// comparisons sit in the hot path of exactly the calc/decision code Grindlang exists to run.
    fn rt_raise_nan_compare(ctx) {
        ctx.fail(RunError::Runtime(ops::NAN_COMPARISON.into()));
    }
}

// ---- calls ------------------------------------------------------------------

/// Decode `argc` call arguments. Each takes two words at `argv`: the operand bits, then its tag
/// (see [`repr_tag`]), so a scalar argument never passes through the value table.
///
/// # Safety
/// `argv` must point to `2 * argc` readable `u64` words.
unsafe fn collect_args(ctx: &RtCtx, argv: *const u64, argc: u32) -> Vec<Value> {
    (0..argc as usize)
        .map(|i| {
            // SAFETY: caller guarantees `argv` holds `argc` (bits, tag) pairs.
            let (bits, tag) = unsafe { (*argv.add(2 * i), *argv.add(2 * i + 1)) };
            ctx.operand(bits, tag as u32)
        })
        .collect()
}

define_shims! {
    /// Raise the call-depth error (`SPEC.md` §7.3). Like [`rt_raise_nan_compare`], only the
    /// failing edge calls here: the depth check itself is inline in every compiled function's
    /// prologue.
    fn rt_raise_call_depth(ctx) {
        let limit = ctx.max_call_depth;
        ctx.fail(call_depth_exceeded(limit));
    }

    /// Call a host function; the result is a word per `tag`.
    fn rt_call_host(ctx, id: u32, argv: *const u64, argc: u32, tag: u32) -> u64 {
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
            Ok(v) => ctx.result(v, tag),
            Err(e) => ctx.fail(e),
        }
    }

    /// Invoke a host-memory method (`mem:method(args)`, `SPEC.md` §7). The first argument is the
    /// receiver, passed through to the impl as its first argument; the result is a word per
    /// `tag`.
    fn rt_call_method(ctx, id: u32, argv: *const u64, argc: u32, tag: u32) -> u64 {
        let args = unsafe { collect_args(ctx, argv, argc) };
        let f = match ctx.methods.get(id as usize).and_then(|f| f.clone()) {
            Some(f) => f,
            None => {
                let name = ctx.pools.methods[id as usize].clone();
                return ctx.fail(RunError::Runtime(format!(
                    "memory method `{name}` was not registered"
                )));
            }
        };
        match f(&args) {
            Ok(v) => ctx.result(v, tag),
            Err(e) => ctx.fail(e),
        }
    }

    fn rt_call_builtin_value(ctx, id: u32, argv: *const u64, argc: u32, tag: u32) -> u64 {
        let args = unsafe { collect_args(ctx, argv, argc) };
        match builtins::value_call(&ctx.pools.builtin_values[id as usize], &args) {
            Ok(v) => ctx.result(v, tag),
            Err(e) => ctx.fail(e),
        }
    }

    fn rt_call_builtin_member(ctx, id: u32, argv: *const u64, argc: u32, tag: u32) -> u64 {
        let args = unsafe { collect_args(ctx, argv, argc) };
        let (ns, member) = &ctx.pools.builtin_members[id as usize];
        match builtins::member_call(ns, member, &args) {
            Ok(v) => ctx.result(v, tag),
            Err(e) => ctx.fail(e),
        }
    }
}

// ---- closures (upvalues) ----------------------------------------------------

define_shims! {
    /// Put a value into a fresh shared upvalue cell; returns the cell's handle.
    fn rt_cell_new(ctx, bits: u64, tag: u32) -> Handle {
        let v = ctx.operand(bits, tag);
        ctx.intern(Value::Cell(Rc::new(RefCell::new(v))))
    }

    /// Read the value currently held in a cell, as a result word per `tag`.
    fn rt_cell_get(ctx, cell: Handle, tag: u32) -> u64 {
        let v = match ctx.get(cell) {
            Value::Cell(c) => c.borrow().clone(),
            _ => Value::Nil,
        };
        ctx.result(v, tag)
    }

    /// Write a value into a cell.
    fn rt_cell_set(ctx, cell: Handle, bits: u64, tag: u32) {
        let v = ctx.operand(bits, tag);
        if let Value::Cell(c) = ctx.get(cell) {
            *c.borrow_mut() = v;
        }
    }

    /// Build a closure value: the lifted function named by `name_id` plus its captured cells
    /// (`argc` handles at `argv`, each a cell). Stamped with this module's [`ClosureOrigin`],
    /// which both keeps the native code mapped if the closure escapes to the host and identifies
    /// the only module that may invoke it.
    fn rt_closure_new(ctx, name_id: u32, argv: *const Handle, argc: u32) -> Handle {
        let env = (0..argc as usize)
            // SAFETY: generated code passes `argc` cell handles at `argv`.
            .map(|i| ctx.value(unsafe { *argv.add(i) }))
            .collect();
        let code = ctx.pools.closure_names[name_id as usize].clone();
        let origin: Rc<dyn Any> = ctx.origin.clone();
        ctx.intern(Value::Closure(Rc::new(ClosureObj {
            code,
            env,
            origin: Some(origin),
        })))
    }

    /// Read the i-th captured cell from a closure's environment. Every upvalue access starts
    /// here, so the cell's handle is shared rather than re-interned on each one.
    fn rt_closure_env_get(ctx, clo: Handle, i: u32) -> Handle {
        let cell = match ctx.get(clo) {
            Value::Closure(c) => c.env.get(i as usize).cloned(),
            _ => None,
        };
        match cell {
            Some(v) => ctx.intern_shared(v),
            None => NIL,
        }
    }

    /// Resolve the native code address of a closure's lifted function, for an indirect call.
    ///
    /// Only a closure this module built may be called: its `code` name means nothing in another
    /// module's address table, and its body would index this module's pools by ids baked in
    /// elsewhere. A foreign closure is rejected rather than silently dispatched (see
    /// [`ClosureOrigin`]).
    fn rt_closure_code_addr(ctx, clo: Handle) -> i64 {
        let (foreign, addr) = match ctx.get(clo) {
            Value::Closure(c) => {
                if closure_module_id(c) == Some(ctx.origin.module_id) {
                    (false, ctx.code_addrs.get(&c.code).copied())
                } else {
                    (true, None)
                }
            }
            _ => (false, None),
        };
        match addr {
            Some(a) => a as i64,
            None if foreign => {
                ctx.fail(RunError::Runtime(FOREIGN_CLOSURE.into()));
                0
            }
            None => {
                ctx.fail(RunError::Internal(
                    "indirect call to an unknown closure".into(),
                ));
                0
            }
        }
    }
}

// ---- the shim table ---------------------------------------------------------

/// A Rust type that crosses the shim ABI, and the cranelift type it is passed as.
trait AbiType {
    const CLIF: types::Type;
}

macro_rules! abi_types {
    ($($rust:ty => $clif:ident),* $(,)?) => {
        $(impl AbiType for $rust {
            const CLIF: types::Type = types::$clif;
        })*
    };
}

abi_types! {
    f64 => F64,
    i8 => I8,
    i32 => I32,
    u32 => I32,
    i64 => I64,
    u64 => I64,
    *mut RtCtx => I64,
    *const u64 => I64,
}

/// One runtime shim, as the JIT links and declares it.
pub struct Shim {
    pub name: &'static str,
    pub addr: *const u8,
    /// Parameter types, the context pointer included.
    pub params: Vec<types::Type>,
    pub returns: Vec<types::Type>,
}

/// Declares [`shims`] from each shim's Rust signature. The `as` cast in each entry only compiles
/// if the signature matches the function's definition, and the cranelift signature is derived
/// from it, so the linker symbol, the declared import, and the Rust function can't disagree.
macro_rules! shim_table {
    ($($name:ident($($param:ty),*) $(-> $ret:ty)?;)*) => {
        /// Every runtime shim generated code may call.
        pub fn shims() -> Vec<Shim> {
            vec![$(
                Shim {
                    name: stringify!($name),
                    addr: ($name as unsafe extern "C" fn($($param),*) $(-> $ret)?) as *const u8,
                    params: vec![$(<$param as AbiType>::CLIF),*],
                    returns: vec![$(<$ret as AbiType>::CLIF)?],
                },
            )*]
        }
    };
}

shim_table! {
    rt_box_number(*mut RtCtx, f64) -> Handle;
    rt_box_bool(*mut RtCtx, i8) -> Handle;
    rt_unbox_number(*mut RtCtx, Handle) -> f64;
    rt_unbox_bool(*mut RtCtx, Handle) -> i8;
    rt_const_string(*mut RtCtx, u32) -> Handle;
    rt_const_cached(*mut RtCtx, u32) -> Handle;
    rt_const_store(*mut RtCtx, u32, Handle);
    rt_memory_ref(*mut RtCtx, u32, u32) -> u64;
    rt_namespace_field(*mut RtCtx, u32, u32) -> u64;
    rt_array_new(*mut RtCtx, u32) -> Handle;
    rt_array_push(*mut RtCtx, Handle, u64, u32);
    rt_tuple_new(*mut RtCtx, u32) -> Handle;
    rt_tuple_push(*mut RtCtx, Handle, u64, u32);
    rt_tuple_get(*mut RtCtx, Handle, u32, u32) -> u64;
    rt_table_new(*mut RtCtx) -> Handle;
    rt_array_get(*mut RtCtx, Handle, f64, u32) -> u64;
    rt_array_set(*mut RtCtx, Handle, f64, u64, u32) -> Handle;
    rt_map_get(*mut RtCtx, Handle, Handle, u32) -> u64;
    rt_map_set(*mut RtCtx, Handle, Handle, u64, u32) -> Handle;
    rt_field_get(*mut RtCtx, Handle, u32, u32) -> u64;
    rt_field_init(*mut RtCtx, Handle, u32, u64, u32);
    rt_field_set(*mut RtCtx, Handle, u32, u64, u32) -> Handle;
    rt_map_keys(*mut RtCtx, Handle) -> Handle;
    rt_len(*mut RtCtx, Handle) -> f64;
    rt_concat(*mut RtCtx, Handle, Handle) -> Handle;
    rt_str_cmp(*mut RtCtx, Handle, Handle) -> i32;
    rt_value_eq(*mut RtCtx, Handle, Handle) -> i8;
    rt_truthy(*mut RtCtx, Handle) -> i8;
    rt_pow(f64, f64) -> f64;
    rt_raise_nan_compare(*mut RtCtx);
    rt_raise_call_depth(*mut RtCtx);
    rt_call_host(*mut RtCtx, u32, *const u64, u32, u32) -> u64;
    rt_call_method(*mut RtCtx, u32, *const u64, u32, u32) -> u64;
    rt_call_builtin_value(*mut RtCtx, u32, *const u64, u32, u32) -> u64;
    rt_call_builtin_member(*mut RtCtx, u32, *const u64, u32, u32) -> u64;
    rt_cell_new(*mut RtCtx, u64, u32) -> Handle;
    rt_cell_get(*mut RtCtx, Handle, u32) -> u64;
    rt_cell_set(*mut RtCtx, Handle, u64, u32);
    rt_closure_new(*mut RtCtx, u32, *const Handle, u32) -> Handle;
    rt_closure_env_get(*mut RtCtx, Handle, u32) -> Handle;
    rt_closure_code_addr(*mut RtCtx, Handle) -> i64;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_ctx() -> RtCtx {
        RtCtx::new(
            Rc::new(Pools::default()),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Arc::new(HashMap::new()),
            Rc::new(ClosureOrigin {
                module_id: 0,
                keepalive: Rc::new(()),
            }),
        )
    }

    /// Generated code loads the errored flag from [`ERRORED_OFFSET`] instead of calling a shim.
    /// If a field reorder moved it, every script-call error check would read an unrelated byte
    /// and errors would stop propagating — silently, and only across function boundaries. Pin
    /// the offset here so that becomes a test failure instead.
    #[test]
    fn errored_flag_sits_at_the_offset_codegen_loads() {
        let ctx = empty_ctx();
        let base = &ctx as *const RtCtx as usize;
        let flag = &ctx.errored as *const u8 as usize;
        assert_eq!(
            (flag - base) as i32,
            ERRORED_OFFSET,
            "RtCtx::errored moved; codegen's load offset is now wrong"
        );
    }

    /// The flag must track the latched error through every transition, since codegen trusts it
    /// alone.
    #[test]
    fn errored_flag_tracks_the_latched_error() {
        let mut ctx = empty_ctx();
        assert_eq!(ctx.errored, 0);

        assert_eq!(ctx.fail(RunError::Runtime("boom".into())), ERR);
        assert_eq!(ctx.errored, 1, "fail must raise the flag");

        // A second failure keeps the *first* error, and leaves the flag raised.
        ctx.fail(RunError::Runtime("later".into()));
        assert_eq!(ctx.errored, 1);

        let taken = ctx.take_error();
        assert!(matches!(taken, Some(RunError::Runtime(m)) if m == "boom"));
        assert_eq!(ctx.errored, 0, "take_error must clear the flag");

        ctx.fail(RunError::Runtime("again".into()));
        ctx.reset();
        assert_eq!(ctx.errored, 0, "reset must clear the flag");
        assert!(ctx.take_error().is_none());
    }

    /// A shim whose body panics returns its failure value, with the panic kept and an error
    /// latched, instead of unwinding into the generated code that called it.
    #[test]
    fn a_panicking_shim_returns_with_the_panic_kept() {
        let pools = Pools {
            host_fns: vec!["boom".into()],
            ..Pools::default()
        };
        let boom: NativeFn = Rc::new(|_: &[Value]| -> Result<Value, RunError> { panic!("boom") });
        let mut ctx = RtCtx::new(
            Rc::new(pools),
            vec![Some(boom)],
            Vec::new(),
            Vec::new(),
            Arc::new(HashMap::new()),
            Rc::new(ClosureOrigin {
                module_id: 0,
                keepalive: Rc::new(()),
            }),
        );

        // SAFETY: a live, exclusive context, and no arguments to read.
        let word = unsafe { rt_call_host(&mut ctx, 0, std::ptr::null(), 0, TAG_NUMBER) };
        assert_eq!(word, ERR);
        assert_eq!(ctx.errored, 1, "the panic must stop the generated code");
        let payload = ctx.take_panic().expect("the panic is kept for the driver");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"boom"));

        ctx.panicked(Box::new("again"));
        ctx.reset();
        assert!(ctx.take_panic().is_none(), "reset must drop a kept panic");
    }
}
