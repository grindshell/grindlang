//! # Cranelift JIT backend (`PLAN.md` Phase 7, `jit` feature)
//!
//! Compiles a lowered [`Program`] to native code with cranelift and runs it. Uses the
//! **hybrid value model** (see [`rt`]): numbers and bools flow through compiled code as
//! unboxed `f64`/`i8`; reference values flow as `i64` handles into a per-call [`rt::RtCtx`],
//! and reference ops call back into the shared runtime ([`crate::runtime`] +
//! [`crate::value::Value`]). So the calc/decision core is genuinely native while heap
//! correctness is delegated to the already-proven runtime.
//!
//! [`JitModule`] mirrors the [`crate::ir::Vm`] / [`crate::interp::Interpreter`] surface
//! (`set_host_function`, `set_memory`, `memory`, `call`) so it slots straight into the
//! differential test harness — the Phase 7 exit criterion is "JIT result == interpreter
//! result over a large corpus".
//!
//! Per-function translation lives in [`translate`]; the runtime context and `extern "C"`
//! shims in [`rt`].

mod rt;
mod translate;

pub use rt::{ClosureOrigin, Handle, RtCtx};

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use cranelift_codegen::ir::{AbiParam, Signature, types};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};

use crate::ir::{ExportTarget, Program};
use crate::runtime::repr::Repr;
use crate::value::{NativeFn, RunError, Value};

use rt::Pools;
use translate::{
    Context, Translator, build_env_trampoline, build_trampoline, env_trampoline_signature,
    trampoline_signature, typed_signature,
};

/// An error from JIT compilation.
#[derive(Debug, thiserror::Error)]
pub enum JitError {
    /// A cranelift module / codegen failure.
    #[error("cranelift error: {0}")]
    Cranelift(String),
    /// A construct the backend doesn't compile yet.
    #[error("unsupported by the JIT backend: {0}")]
    Unsupported(String),
}

/// The uniform trampoline ABI the driver invokes: `(ctx, argv, argc) -> handle`.
type TrampFn = unsafe extern "C" fn(*mut RtCtx, *const Handle, u32) -> Handle;

/// The env-trampoline ABI for host-invoking a returned closure: `(ctx, env, argv, argc) ->
/// handle`, where `env` is the closure value handle.
type EnvTrampFn = unsafe extern "C" fn(*mut RtCtx, Handle, *const Handle, u32) -> Handle;

/// Arity-specialized fn-pointer types for the **direct-call fast path**. An exported function
/// whose signature is all-`number` is invoked straight at its native typed address as
/// `(ctx, f64 × N) -> f64`, skipping the trampoline hop and the argument buffer entirely.
/// Signatures involving `bool`/reference/`unit` keep using the uniform trampoline. Cranelift
/// lowers the typed function with the platform C ABI (`isa.default_call_conv()`), so these
/// `extern "C"` pointers match it exactly — the same contract the trampolines already rely on.
type Direct0 = unsafe extern "C" fn(*mut RtCtx) -> f64;
type Direct1 = unsafe extern "C" fn(*mut RtCtx, f64) -> f64;
type Direct2 = unsafe extern "C" fn(*mut RtCtx, f64, f64) -> f64;
type Direct3 = unsafe extern "C" fn(*mut RtCtx, f64, f64, f64) -> f64;
type Direct4 = unsafe extern "C" fn(*mut RtCtx, f64, f64, f64, f64) -> f64;
type Direct5 = unsafe extern "C" fn(*mut RtCtx, f64, f64, f64, f64, f64) -> f64;
type Direct6 = unsafe extern "C" fn(*mut RtCtx, f64, f64, f64, f64, f64, f64) -> f64;
type Direct7 = unsafe extern "C" fn(*mut RtCtx, f64, f64, f64, f64, f64, f64, f64) -> f64;
type Direct8 = unsafe extern "C" fn(*mut RtCtx, f64, f64, f64, f64, f64, f64, f64, f64) -> f64;

/// Maximum arity handled by the direct-call fast path (one arm per arity below). Higher-arity
/// all-`number` functions fall back to the trampoline.
const MAX_DIRECT_ARGS: usize = 8;

/// Source of the process-unique ids that bind a closure to the module that built it. Ids are
/// never reused, so a closure can't be mistaken for one belonging to a later module that
/// happens to occupy a dropped module's address.
static NEXT_MODULE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A compiled module: native code plus the machinery to call it.
pub struct JitModule {
    /// This module's identity, stamped into every closure it builds so a closure can only be
    /// invoked by the module that created it. Also owns the [`Rc<JITModule>`] keeping the
    /// executable memory mapped — shared into any closure that escapes to the host, so holding
    /// a returned closure after this `JitModule` is dropped stays sound. Cloned into each
    /// [`RtCtx`] so the runtime makes the same check compiled code does.
    origin: Rc<rt::ClosureOrigin>,
    pools: Arc<Pools>,
    /// export name → its target (function or constant).
    exports: std::collections::BTreeMap<String, ExportTarget>,
    /// function name → trampoline.
    fn_tramps: HashMap<String, TrampFn>,
    /// constant name → trampoline.
    const_tramps: HashMap<String, TrampFn>,
    /// lifted-closure name → env-trampoline (for host [`JitModule::call_value`]).
    closure_tramps: HashMap<String, EnvTrampFn>,
    /// function name → (param reprs, return repr) — the bits-ABI contract the driver uses to
    /// encode args / decode the result without boxing scalars.
    fn_sigs: HashMap<String, (Vec<Repr>, Repr)>,
    /// constant name → return repr (constants take no parameters).
    const_rets: HashMap<String, Repr>,
    /// lifted-closure name → (source-param reprs excluding the env, return repr).
    closure_sigs: HashMap<String, (Vec<Repr>, Repr)>,
    /// function name → finalized native address (resolves indirect closure calls).
    code_addrs: Arc<HashMap<String, u64>>,
    /// Registered host functions and memory, by name.
    host: HashMap<String, NativeFn>,
    /// Registered host-memory methods, keyed by their `"mem:method"` symbol (`SPEC.md` §7).
    methods: HashMap<String, NativeFn>,
    memory: HashMap<String, Value>,
    /// A pooled runtime context, reused across calls to avoid re-allocating the value table
    /// and re-cloning the shared bindings on every invocation. Taken out for the duration of a
    /// call (so reentrant calls allocate fresh) and `reset` before reuse.
    cached_ctx: Option<RtCtx>,
    /// Set when `host`/`memory` change, so the pooled context's resolved bindings are rebuilt
    /// on the next call rather than every call.
    bindings_dirty: bool,
}

impl JitModule {
    /// Compile a lowered, verified [`Program`] to native code.
    pub fn compile(program: &Program) -> Result<JitModule, JitError> {
        let mut flags = settings::builder();
        // Position-independent, speed-favoring defaults are fine for an embedded JIT.
        flags
            .set("opt_level", "speed")
            .map_err(|e| JitError::Cranelift(e.to_string()))?;
        let isa_builder =
            cranelift_native::builder().map_err(|e| JitError::Cranelift(e.to_string()))?;
        let isa = isa_builder
            .finish(settings::Flags::new(flags))
            .map_err(|e| JitError::Cranelift(e.to_string()))?;
        let call_conv = isa.default_call_conv();

        let mut builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        for (name, ptr) in rt::shim_symbols() {
            builder.symbol(name, ptr);
        }
        let mut module = JITModule::new(builder);

        // Declare the runtime shims as imports.
        let shims = declare_shims(&mut module, call_conv)?;

        // Declare every function and constant (typed bodies + uniform trampolines).
        let mut typed_ids: HashMap<String, FuncId> = HashMap::new();
        let mut const_ids: HashMap<String, FuncId> = HashMap::new();
        let mut fn_tramp_ids: HashMap<String, FuncId> = HashMap::new();
        let mut const_tramp_ids: HashMap<String, FuncId> = HashMap::new();
        // Lifted closures get an env-trampoline (not a plain one): they are never exported and
        // are reached only indirectly (in-script) or via the host `call_value` path.
        let mut closure_tramp_ids: HashMap<String, FuncId> = HashMap::new();
        // ABI signatures the driver needs to encode args / decode results at call time.
        let mut fn_sigs: HashMap<String, (Vec<Repr>, Repr)> = HashMap::new();
        let mut const_rets: HashMap<String, Repr> = HashMap::new();
        let mut closure_sigs: HashMap<String, (Vec<Repr>, Repr)> = HashMap::new();

        for (name, f) in &program.functions {
            let params: Vec<Repr> = f.params.iter().map(|p| Repr::of(&f.locals[p])).collect();
            let ret = Repr::of(&f.ret);
            let sig = typed_signature(call_conv, &params, ret);
            let id = module
                .declare_function(&format!("fn_{name}"), Linkage::Local, &sig)
                .map_err(|e| JitError::Cranelift(e.to_string()))?;
            typed_ids.insert(name.clone(), id);

            if f.is_closure {
                let tsig = env_trampoline_signature(call_conv);
                let tid = module
                    .declare_function(&format!("et_{name}"), Linkage::Export, &tsig)
                    .map_err(|e| JitError::Cranelift(e.to_string()))?;
                closure_tramp_ids.insert(name.clone(), tid);
                // Source-level params exclude the hidden env (params[0]).
                closure_sigs.insert(name.clone(), (params[1..].to_vec(), ret));
            } else {
                let tsig = trampoline_signature(call_conv);
                let tid = module
                    .declare_function(&format!("tr_{name}"), Linkage::Export, &tsig)
                    .map_err(|e| JitError::Cranelift(e.to_string()))?;
                fn_tramp_ids.insert(name.clone(), tid);
                fn_sigs.insert(name.clone(), (params, ret));
            }
        }
        for (name, c) in &program.constants {
            let ret = Repr::of(&c.ret);
            let sig = typed_signature(call_conv, &[], ret);
            let id = module
                .declare_function(&format!("cn_{name}"), Linkage::Local, &sig)
                .map_err(|e| JitError::Cranelift(e.to_string()))?;
            const_ids.insert(name.clone(), id);

            let tsig = trampoline_signature(call_conv);
            let tid = module
                .declare_function(&format!("ct_{name}"), Linkage::Export, &tsig)
                .map_err(|e| JitError::Cranelift(e.to_string()))?;
            const_tramp_ids.insert(name.clone(), tid);
            const_rets.insert(name.clone(), ret);
        }

        // Translate every body, interning constants/names into the shared pools.
        let mut pools = Pools::default();
        let cx = Context {
            call_conv,
            shims: &shims,
            typed_ids: &typed_ids,
            const_ids: &const_ids,
            const_rets: &const_rets,
        };

        let mut fbctx = FunctionBuilderContext::new();
        for (name, f) in &program.functions {
            define_body(&mut module, &cx, &mut pools, &mut fbctx, typed_ids[name], f)?;
        }
        for (name, c) in &program.constants {
            define_body(&mut module, &cx, &mut pools, &mut fbctx, const_ids[name], c)?;
        }

        // Trampolines (declared functions whose only job is arg/result marshaling).
        for (name, f) in &program.functions {
            let ret = Repr::of(&f.ret);
            if f.is_closure {
                // Source-level params exclude the hidden env (params[0]); the env-trampoline
                // threads the closure value through separately.
                let real: Vec<Repr> = f.params[1..]
                    .iter()
                    .map(|p| Repr::of(&f.locals[p]))
                    .collect();
                define_env_trampoline(
                    &mut module,
                    &mut fbctx,
                    closure_tramp_ids[name],
                    typed_ids[name],
                    &real,
                    ret,
                )?;
            } else {
                let params: Vec<Repr> = f.params.iter().map(|p| Repr::of(&f.locals[p])).collect();
                define_trampoline(
                    &mut module,
                    &mut fbctx,
                    fn_tramp_ids[name],
                    typed_ids[name],
                    &params,
                    ret,
                )?;
            }
        }
        for (name, c) in &program.constants {
            let ret = Repr::of(&c.ret);
            define_trampoline(
                &mut module,
                &mut fbctx,
                const_tramp_ids[name],
                const_ids[name],
                &[],
                ret,
            )?;
        }

        module
            .finalize_definitions()
            .map_err(|e| JitError::Cranelift(e.to_string()))?;

        // Resolve trampoline addresses.
        let mut fn_tramps = HashMap::new();
        for (name, &tid) in &fn_tramp_ids {
            let ptr = module.get_finalized_function(tid);
            // SAFETY: `tid` was defined with `trampoline_signature`, matching `TrampFn`.
            let f: TrampFn = unsafe { std::mem::transmute::<*const u8, TrampFn>(ptr) };
            fn_tramps.insert(name.clone(), f);
        }
        let mut const_tramps = HashMap::new();
        for (name, &tid) in &const_tramp_ids {
            let ptr = module.get_finalized_function(tid);
            // SAFETY: as above.
            let f: TrampFn = unsafe { std::mem::transmute::<*const u8, TrampFn>(ptr) };
            const_tramps.insert(name.clone(), f);
        }
        let mut closure_tramps = HashMap::new();
        for (name, &tid) in &closure_tramp_ids {
            let ptr = module.get_finalized_function(tid);
            // SAFETY: `tid` was defined with `env_trampoline_signature`, matching `EnvTrampFn`.
            let f: EnvTrampFn = unsafe { std::mem::transmute::<*const u8, EnvTrampFn>(ptr) };
            closure_tramps.insert(name.clone(), f);
        }

        // Native address of every typed function, by name — the target table for indirect
        // closure calls (`rt_closure_code_addr`).
        let mut code_addrs = HashMap::new();
        for (name, &id) in &typed_ids {
            let ptr = module.get_finalized_function(id);
            code_addrs.insert(name.clone(), ptr as u64);
        }

        let module = Rc::new(module);
        let origin = Rc::new(rt::ClosureOrigin {
            module_id: NEXT_MODULE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            keepalive: module.clone(),
        });

        Ok(JitModule {
            origin,
            pools: Arc::new(pools),
            exports: program.exports.clone(),
            fn_tramps,
            const_tramps,
            closure_tramps,
            fn_sigs,
            const_rets,
            closure_sigs,
            code_addrs: Arc::new(code_addrs),
            host: HashMap::new(),
            methods: HashMap::new(),
            memory: HashMap::new(),
            cached_ctx: None,
            bindings_dirty: false,
        })
    }

    /// Register a host function callable from scripts under `name`.
    pub fn set_host_function<F>(&mut self, name: impl Into<String>, f: F)
    where
        F: Fn(&[Value]) -> Result<Value, RunError> + 'static,
    {
        self.host.insert(name.into(), std::rc::Rc::new(f));
        self.bindings_dirty = true;
    }

    /// Install a pre-built native host function. Used by the embedding API ([`crate::api`]),
    /// which type-erases typed host closures into a [`NativeFn`] before compilation.
    pub fn set_host_fn(&mut self, name: impl Into<String>, f: NativeFn) {
        self.host.insert(name.into(), f);
        self.bindings_dirty = true;
    }

    /// Register a host-memory method impl by its `"mem:method"` symbol (`SPEC.md` §7). The impl
    /// receives the receiver as its first argument, followed by the call arguments.
    pub fn set_method<F>(&mut self, symbol: impl Into<String>, f: F)
    where
        F: Fn(&[Value]) -> Result<Value, RunError> + 'static,
    {
        self.methods.insert(symbol.into(), std::rc::Rc::new(f));
        self.bindings_dirty = true;
    }

    /// Install a pre-built native memory method (the embedding API's type-erased path).
    pub fn set_method_fn(&mut self, symbol: impl Into<String>, f: NativeFn) {
        self.methods.insert(symbol.into(), f);
        self.bindings_dirty = true;
    }

    /// Bind a host memory handle (typically a [`Value::table`]).
    pub fn set_memory(&mut self, name: impl Into<String>, value: Value) {
        self.memory.insert(name.into(), value);
        self.bindings_dirty = true;
    }

    /// Read back a memory handle, e.g. to observe mutations after a call.
    pub fn memory(&self, name: &str) -> Option<Value> {
        self.memory.get(name).cloned()
    }

    /// Call an exported function by name with `args`.
    pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RunError> {
        let mut ctx = self.take_ctx();
        let result = self.dispatch(&mut ctx, name, args);
        self.store_ctx(ctx);
        result
    }

    /// Resolve `name`'s trampoline + ABI signature and invoke it against `ctx`. Split out so
    /// the pooled-context `&mut self` borrow in [`call`](Self::call) doesn't collide with the
    /// immutable signature lookups here.
    fn dispatch(&self, ctx: &mut RtCtx, name: &str, args: Vec<Value>) -> Result<Value, RunError> {
        let target = self
            .exports
            .get(name)
            .ok_or_else(|| RunError::UnknownExport(name.to_string()))?;
        let (tramp, params, ret): (TrampFn, &[Repr], Repr) =
            match target {
                ExportTarget::Function(n) => {
                    let (params, ret) = self
                        .fn_sigs
                        .get(n)
                        .ok_or_else(|| RunError::Internal(format!("missing signature `{n}`")))?;
                    if args.len() != params.len() {
                        return Err(RunError::ArityMismatch {
                            name: name.to_string(),
                            expected: params.len(),
                            got: args.len(),
                        });
                    }
                    // Fast path: an all-`number` export is called directly at its native typed
                    // address, skipping the trampoline hop and the argument buffer.
                    if *ret == Repr::Number
                        && params.len() <= MAX_DIRECT_ARGS
                        && params.iter().all(|&r| r == Repr::Number)
                        && let Some(&addr) = self.code_addrs.get(n)
                    {
                        return self.call_direct_number(ctx, addr, params.len(), &args);
                    }
                    let tramp = *self
                        .fn_tramps
                        .get(n)
                        .ok_or_else(|| RunError::Internal(format!("missing trampoline `{n}`")))?;
                    (tramp, params.as_slice(), *ret)
                }
                ExportTarget::Const(n) => {
                    if !args.is_empty() {
                        return Err(RunError::ArityMismatch {
                            name: name.to_string(),
                            expected: 0,
                            got: args.len(),
                        });
                    }
                    let tramp = *self.const_tramps.get(n).ok_or_else(|| {
                        RunError::Internal(format!("missing const trampoline `{n}`"))
                    })?;
                    let ret = *self.const_rets.get(n).ok_or_else(|| {
                        RunError::Internal(format!("missing const signature `{n}`"))
                    })?;
                    (tramp, &[], ret)
                }
            };

        let argv = encode_args(ctx, params, args)?;
        let argv = argv.as_slice();
        // SAFETY: `tramp` has the `TrampFn` ABI; `ctx` outlives the call; `argv` holds one
        // bits-ABI word per declared parameter.
        let result = unsafe { tramp(ctx as *mut RtCtx, argv.as_ptr(), argv.len() as u32) };

        if let Some(e) = ctx.take_error() {
            return Err(e);
        }
        Ok(ctx.decode_ret(result, ret))
    }

    /// Direct-call fast path for an all-`number` export: invoke the native typed function at
    /// `addr` as `(ctx, f64 × arity) -> f64`, skipping the trampoline and the argument buffer.
    /// `arity` is `<= MAX_DIRECT_ARGS` and matches `args.len()` (both guaranteed by the caller).
    /// Rejects a non-number argument exactly as the trampoline path's [`RtCtx::encode_arg`]
    /// does, so taking the fast path can't change whether a call is accepted.
    fn call_direct_number(
        &self,
        ctx: &mut RtCtx,
        addr: u64,
        arity: usize,
        args: &[Value],
    ) -> Result<Value, RunError> {
        let mut a = [0f64; MAX_DIRECT_ARGS];
        for (slot, v) in a.iter_mut().zip(args) {
            *slot = v.as_f64().ok_or_else(|| {
                RunError::Runtime(format!(
                    "expected a number argument, found {}",
                    v.type_name()
                ))
            })?;
        }
        let p = addr as *const u8;
        let c = ctx as *mut RtCtx;
        // SAFETY: `addr` is the finalized native address of the typed function `fn_<name>`,
        // whose signature the caller verified is `(ctx, f64 × arity) -> f64`. It was compiled
        // with the platform C ABI, so the matching arity-specialized `extern "C"` pointer is
        // the same ABI contract the trampolines rely on. `ctx` is a live, exclusive `*mut
        // RtCtx` that outlives the call.
        let r = unsafe {
            match arity {
                0 => std::mem::transmute::<*const u8, Direct0>(p)(c),
                1 => std::mem::transmute::<*const u8, Direct1>(p)(c, a[0]),
                2 => std::mem::transmute::<*const u8, Direct2>(p)(c, a[0], a[1]),
                3 => std::mem::transmute::<*const u8, Direct3>(p)(c, a[0], a[1], a[2]),
                4 => std::mem::transmute::<*const u8, Direct4>(p)(c, a[0], a[1], a[2], a[3]),
                5 => std::mem::transmute::<*const u8, Direct5>(p)(c, a[0], a[1], a[2], a[3], a[4]),
                6 => std::mem::transmute::<*const u8, Direct6>(p)(
                    c, a[0], a[1], a[2], a[3], a[4], a[5],
                ),
                7 => std::mem::transmute::<*const u8, Direct7>(p)(
                    c, a[0], a[1], a[2], a[3], a[4], a[5], a[6],
                ),
                8 => std::mem::transmute::<*const u8, Direct8>(p)(
                    c, a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7],
                ),
                _ => unreachable!("arity > MAX_DIRECT_ARGS is guarded by the caller"),
            }
        };
        if let Some(e) = ctx.take_error() {
            return Err(e);
        }
        Ok(Value::Number(r))
    }

    /// Host-invoke a closure value previously returned by [`call`](Self::call) (or by another
    /// `call_value`). The closure carries its captured cells, so upvalue mutations persist
    /// across host calls.
    ///
    /// The closure must be one **this** module built. A closure is only meaningful inside its
    /// origin: its lifted name (`enclosing$c0`) collides freely across modules, and its
    /// compiled body reads that module's constant pools by baked-in id — so invoking it
    /// elsewhere is rejected rather than silently running an unrelated function. Holding a
    /// closure after its module is dropped stays sound (it keeps the code mapped); it simply
    /// can no longer be invoked.
    pub fn call_value(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, RunError> {
        let code = match &callee {
            Value::Closure(c) => {
                if rt::closure_module_id(c) != Some(self.origin.module_id) {
                    return Err(RunError::Runtime(rt::FOREIGN_CLOSURE.into()));
                }
                c.code.clone()
            }
            other => {
                return Err(RunError::Internal(format!(
                    "call_value on a {} value",
                    other.type_name()
                )));
            }
        };
        let mut ctx = self.take_ctx();
        let result = self.dispatch_value(&mut ctx, &code, callee, args);
        self.store_ctx(ctx);
        result
    }

    /// Resolve a lifted closure's env-trampoline + ABI signature and invoke it against `ctx`,
    /// threading the closure value as the env argument. Split out for the same borrow reason as
    /// [`dispatch`](Self::dispatch).
    fn dispatch_value(
        &self,
        ctx: &mut RtCtx,
        code: &str,
        callee: Value,
        args: Vec<Value>,
    ) -> Result<Value, RunError> {
        let tramp = *self
            .closure_tramps
            .get(code)
            .ok_or_else(|| RunError::Internal(format!("missing closure trampoline `{code}`")))?;
        let (params, ret) = self
            .closure_sigs
            .get(code)
            .ok_or_else(|| RunError::Internal(format!("missing closure signature `{code}`")))?;
        if args.len() != params.len() {
            return Err(RunError::ArityMismatch {
                name: code.to_string(),
                expected: params.len(),
                got: args.len(),
            });
        }

        // The closure itself is the env argument (it carries the captured cells).
        let env = ctx.intern(callee);
        let argv = encode_args(ctx, params, args)?;
        let argv = argv.as_slice();
        // SAFETY: `tramp` has the `EnvTrampFn` ABI; `ctx` outlives the call; `argv` holds one
        // bits-ABI word per declared (source-level) parameter.
        let result = unsafe { tramp(ctx as *mut RtCtx, env, argv.as_ptr(), argv.len() as u32) };

        if let Some(e) = ctx.take_error() {
            return Err(e);
        }
        Ok(ctx.decode_ret(result, *ret))
    }

    /// Resolve registered host functions into pool-id order (parallel to `pools.host_fns`).
    fn resolve_host(&self) -> Vec<Option<NativeFn>> {
        self.pools
            .host_fns
            .iter()
            .map(|n| self.host.get(n).cloned())
            .collect()
    }

    /// Resolve registered memory methods into pool-id order (parallel to `pools.methods`).
    fn resolve_methods(&self) -> Vec<Option<NativeFn>> {
        self.pools
            .methods
            .iter()
            .map(|s| self.methods.get(s).cloned())
            .collect()
    }

    /// Resolve bound memory handles into pool-id order (parallel to `pools.memories`), leaving
    /// a declared-but-unprovided binding as `None` so reading it fails at the point of use the
    /// way it does in both interpreters. Substituting `Value::Nil` here would make
    /// `rt_memory_ref`'s not-provided error unreachable.
    fn resolve_memory(&self) -> Vec<Option<Value>> {
        self.pools
            .memories
            .iter()
            .map(|n| self.memory.get(n).cloned())
            .collect()
    }

    /// Build a fresh per-invocation [`RtCtx`], resolving host/memory bindings into pool-id
    /// order and threading the indirect-call address table plus this module's closure origin.
    fn make_ctx(&self) -> RtCtx {
        RtCtx::new(
            self.pools.clone(),
            self.resolve_host(),
            self.resolve_methods(),
            self.resolve_memory(),
            self.code_addrs.clone(),
            self.origin.clone(),
        )
    }

    /// Obtain a ready-to-use context for one invocation: reuse the pooled one (rebuilding its
    /// host/memory bindings only if they changed since last call), or build a fresh one. The
    /// caller must return it via [`Self::store_ctx`] after the call so the next call can reuse
    /// it. Taking it out means a reentrant call gets its own fresh context.
    fn take_ctx(&mut self) -> RtCtx {
        let ctx = match self.cached_ctx.take() {
            Some(mut ctx) => {
                if self.bindings_dirty {
                    let host = self.resolve_host();
                    let methods = self.resolve_methods();
                    let memory = self.resolve_memory();
                    ctx.rebind(host, methods, memory);
                }
                ctx.reset();
                ctx
            }
            None => self.make_ctx(),
        };
        self.bindings_dirty = false;
        ctx
    }

    /// Return a context to the pool for reuse by the next call.
    fn store_ctx(&mut self, ctx: RtCtx) {
        self.cached_ctx = Some(ctx);
    }
}

/// Inline capacity for the argument buffer. Calls with this many parameters or fewer (the
/// overwhelming majority — calc/decision functions take a handful of args) encode their args
/// on the stack, avoiding a per-call heap allocation in the hot path.
const INLINE_ARGS: usize = 8;

/// The bits-ABI argument buffer for one call: stack-resident for small arities, heap-backed
/// only when a function has more than [`INLINE_ARGS`] parameters. Either way it owns the
/// storage so its [`as_slice`](Self::as_slice) pointer stays valid for the trampoline call.
enum ArgBuf {
    Inline([u64; INLINE_ARGS], usize),
    Heap(Vec<u64>),
}

impl ArgBuf {
    fn as_slice(&self) -> &[u64] {
        match self {
            ArgBuf::Inline(a, n) => &a[..*n],
            ArgBuf::Heap(v) => v,
        }
    }
}

/// Encode call arguments into the bits-ABI buffer the trampolines read: exactly one 64-bit
/// word per declared parameter (scalars as raw bits via [`RtCtx::encode_arg`], reference
/// values interned to handles). Callers check arity first, so the buffer length always matches
/// the parameter count the trampoline expects.
///
/// Fails if an argument's runtime shape contradicts its declared parameter type, rather than
/// coercing it to `0` / `false` — the host boundary's third no-silent-coercion rule alongside
/// exact arity and integer marshaling (`SPEC.md` §7.1).
fn encode_args(ctx: &mut RtCtx, params: &[Repr], args: Vec<Value>) -> Result<ArgBuf, RunError> {
    let mut args = args.into_iter();
    if params.len() <= INLINE_ARGS {
        let mut buf = [0u64; INLINE_ARGS];
        for (slot, &r) in buf.iter_mut().zip(params) {
            *slot = ctx.encode_arg(args.next().unwrap_or(Value::Nil), r)?;
        }
        Ok(ArgBuf::Inline(buf, params.len()))
    } else {
        Ok(ArgBuf::Heap(
            params
                .iter()
                .map(|&r| ctx.encode_arg(args.next().unwrap_or(Value::Nil), r))
                .collect::<Result<Vec<_>, _>>()?,
        ))
    }
}

/// Build a clif [`Signature`] from raw clif types.
fn sig(call_conv: CallConv, params: &[types::Type], rets: &[types::Type]) -> Signature {
    let mut s = Signature::new(call_conv);
    for p in params {
        s.params.push(AbiParam::new(*p));
    }
    for r in rets {
        s.returns.push(AbiParam::new(*r));
    }
    s
}

/// Declare every runtime shim as an imported function and return their ids.
fn declare_shims(
    module: &mut JITModule,
    cc: CallConv,
) -> Result<HashMap<&'static str, FuncId>, JitError> {
    use types::{F64, I8, I32, I64};
    let table: &[(&str, &[types::Type], &[types::Type])] = &[
        ("rt_box_number", &[I64, F64], &[I64]),
        ("rt_box_bool", &[I64, I8], &[I64]),
        ("rt_unbox_number", &[I64, I64], &[F64]),
        ("rt_unbox_bool", &[I64, I64], &[I8]),
        ("rt_const_string", &[I64, I32], &[I64]),
        ("rt_const_cached", &[I64, I32], &[I64]),
        ("rt_const_store", &[I64, I32, I64], &[]),
        ("rt_memory_ref", &[I64, I32], &[I64]),
        ("rt_namespace_field", &[I64, I32], &[I64]),
        ("rt_array_new", &[I64, I32], &[I64]),
        ("rt_array_push", &[I64, I64, I64], &[]),
        ("rt_tuple_new", &[I64, I32], &[I64]),
        ("rt_tuple_push", &[I64, I64, I64], &[]),
        ("rt_tuple_get", &[I64, I64, I32], &[I64]),
        ("rt_table_new", &[I64], &[I64]),
        ("rt_table_set", &[I64, I64, I64, I64], &[]),
        ("rt_array_get", &[I64, I64, F64], &[I64]),
        ("rt_array_set", &[I64, I64, F64, I64], &[I64]),
        ("rt_map_get", &[I64, I64, I64], &[I64]),
        ("rt_map_set", &[I64, I64, I64, I64], &[]),
        ("rt_field_get", &[I64, I64, I32], &[I64]),
        ("rt_field_set", &[I64, I64, I32, I64], &[]),
        ("rt_map_keys", &[I64, I64], &[I64]),
        ("rt_len", &[I64, I64], &[F64]),
        ("rt_concat", &[I64, I64, I64], &[I64]),
        ("rt_str_cmp", &[I64, I64, I64], &[I32]),
        ("rt_value_eq", &[I64, I64, I64], &[I8]),
        ("rt_truthy", &[I64, I64], &[I8]),
        ("rt_pow", &[F64, F64], &[F64]),
        ("rt_raise_nan_compare", &[I64], &[]),
        ("rt_call_host", &[I64, I32, I64, I32], &[I64]),
        ("rt_call_method", &[I64, I32, I64, I32], &[I64]),
        ("rt_call_builtin_value", &[I64, I32, I64, I32], &[I64]),
        ("rt_call_builtin_member", &[I64, I32, I64, I32], &[I64]),
        ("rt_cell_new", &[I64, I64], &[I64]),
        ("rt_cell_get", &[I64, I64], &[I64]),
        ("rt_cell_set", &[I64, I64, I64], &[]),
        ("rt_closure_new", &[I64, I32, I64, I32], &[I64]),
        ("rt_closure_env_get", &[I64, I64, I32], &[I64]),
        ("rt_closure_code_addr", &[I64, I64], &[I64]),
    ];
    let mut ids = HashMap::new();
    for (name, params, rets) in table {
        let s = sig(cc, params, rets);
        let id = module
            .declare_function(name, Linkage::Import, &s)
            .map_err(|e| JitError::Cranelift(e.to_string()))?;
        ids.insert(*name, id);
    }
    Ok(ids)
}

/// Translate one IR function body into `func_id`.
fn define_body(
    module: &mut JITModule,
    cx: &Context,
    pools: &mut Pools,
    fbctx: &mut FunctionBuilderContext,
    func_id: FuncId,
    f: &crate::ir::Function,
) -> Result<(), JitError> {
    let params: Vec<Repr> = f.params.iter().map(|p| Repr::of(&f.locals[p])).collect();
    let ret = Repr::of(&f.ret);
    let mut ctx = module.make_context();
    ctx.func.signature = typed_signature(cx.call_conv, &params, ret);
    {
        let builder = FunctionBuilder::new(&mut ctx.func, fbctx);
        let t = Translator::new(builder, module as &mut dyn Module, cx, pools, f);
        t.run();
    }
    module
        .define_function(func_id, &mut ctx)
        .map_err(|e| JitError::Cranelift(format!("{e:?}")))?;
    module.clear_context(&mut ctx);
    Ok(())
}

/// Build one trampoline into `tramp_id`.
fn define_trampoline(
    module: &mut JITModule,
    fbctx: &mut FunctionBuilderContext,
    tramp_id: FuncId,
    typed_id: FuncId,
    params: &[Repr],
    ret: Repr,
) -> Result<(), JitError> {
    let mut ctx = module.make_context();
    ctx.func.signature = trampoline_signature(
        // call conv comes from the declared signature; rebuild it the same way.
        module.isa().default_call_conv(),
    );
    {
        let builder = FunctionBuilder::new(&mut ctx.func, fbctx);
        build_trampoline(builder, module as &mut dyn Module, typed_id, params, ret);
    }
    module
        .define_function(tramp_id, &mut ctx)
        .map_err(|e| JitError::Cranelift(e.to_string()))?;
    module.clear_context(&mut ctx);
    Ok(())
}

/// Build one env-trampoline (for a lifted closure) into `tramp_id`. `params` are the
/// source-level parameters, excluding the hidden env.
fn define_env_trampoline(
    module: &mut JITModule,
    fbctx: &mut FunctionBuilderContext,
    tramp_id: FuncId,
    typed_id: FuncId,
    params: &[Repr],
    ret: Repr,
) -> Result<(), JitError> {
    let mut ctx = module.make_context();
    ctx.func.signature = env_trampoline_signature(module.isa().default_call_conv());
    {
        let builder = FunctionBuilder::new(&mut ctx.func, fbctx);
        build_env_trampoline(builder, module as &mut dyn Module, typed_id, params, ret);
    }
    module
        .define_function(tramp_id, &mut ctx)
        .map_err(|e| JitError::Cranelift(e.to_string()))?;
    module.clear_context(&mut ctx);
    Ok(())
}
