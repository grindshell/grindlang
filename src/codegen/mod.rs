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

pub use rt::{Handle, RtCtx};

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

/// A compiled module: native code plus the machinery to call it.
pub struct JitModule {
    /// Keeps the executable memory mapped. Shared (by [`Rc`]) into any closure that escapes to
    /// the host, so a returned closure stays callable even if this `JitModule` is dropped.
    module: Rc<JITModule>,
    pools: Arc<Pools>,
    /// export name → its target (function or constant).
    exports: std::collections::BTreeMap<String, ExportTarget>,
    /// function name → trampoline.
    fn_tramps: HashMap<String, TrampFn>,
    /// constant name → trampoline.
    const_tramps: HashMap<String, TrampFn>,
    /// lifted-closure name → env-trampoline (for host [`JitModule::call_value`]).
    closure_tramps: HashMap<String, EnvTrampFn>,
    /// function name → finalized native address (resolves indirect closure calls).
    code_addrs: Arc<HashMap<String, u64>>,
    /// Registered host functions and memory, by name.
    host: HashMap<String, NativeFn>,
    memory: HashMap<String, Value>,
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
            } else {
                let tsig = trampoline_signature(call_conv);
                let tid = module
                    .declare_function(&format!("tr_{name}"), Linkage::Export, &tsig)
                    .map_err(|e| JitError::Cranelift(e.to_string()))?;
                fn_tramp_ids.insert(name.clone(), tid);
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
        }

        // Translate every body, interning constants/names into the shared pools.
        let mut pools = Pools::default();
        let cx = Context {
            call_conv,
            shims: &shims,
            typed_ids: &typed_ids,
            const_ids: &const_ids,
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
                    &shims,
                    &mut fbctx,
                    closure_tramp_ids[name],
                    typed_ids[name],
                    &real,
                    ret,
                )?;
            } else {
                let params: Vec<Repr> =
                    f.params.iter().map(|p| Repr::of(&f.locals[p])).collect();
                define_trampoline(
                    &mut module,
                    &shims,
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
                &shims,
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

        Ok(JitModule {
            module: Rc::new(module),
            pools: Arc::new(pools),
            exports: program.exports.clone(),
            fn_tramps,
            const_tramps,
            closure_tramps,
            code_addrs: Arc::new(code_addrs),
            host: HashMap::new(),
            memory: HashMap::new(),
        })
    }

    /// Register a host function callable from scripts under `name`.
    pub fn set_host_function<F>(&mut self, name: impl Into<String>, f: F)
    where
        F: Fn(&[Value]) -> Result<Value, RunError> + 'static,
    {
        self.host.insert(name.into(), std::rc::Rc::new(f));
    }

    /// Install a pre-built native host function. Used by the embedding API ([`crate::api`]),
    /// which type-erases typed host closures into a [`NativeFn`] before compilation.
    pub fn set_host_fn(&mut self, name: impl Into<String>, f: NativeFn) {
        self.host.insert(name.into(), f);
    }

    /// Bind a host memory handle (typically a [`Value::table`]).
    pub fn set_memory(&mut self, name: impl Into<String>, value: Value) {
        self.memory.insert(name.into(), value);
    }

    /// Read back a memory handle, e.g. to observe mutations after a call.
    pub fn memory(&self, name: &str) -> Option<Value> {
        self.memory.get(name).cloned()
    }

    /// Call an exported function by name with `args`.
    pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RunError> {
        let target = self
            .exports
            .get(name)
            .ok_or_else(|| RunError::UnknownExport(name.to_string()))?;
        let tramp = match target {
            ExportTarget::Function(n) => *self
                .fn_tramps
                .get(n)
                .ok_or_else(|| RunError::Internal(format!("missing trampoline `{n}`")))?,
            ExportTarget::Const(n) => *self
                .const_tramps
                .get(n)
                .ok_or_else(|| RunError::Internal(format!("missing const trampoline `{n}`")))?,
        };

        let mut ctx = self.make_ctx();
        let argv: Vec<Handle> = args.into_iter().map(|a| ctx.intern(a)).collect();
        // SAFETY: `tramp` has the `TrampFn` ABI; `ctx` outlives the call; `argv` is valid for
        // `argv.len()` handles.
        let result = unsafe { tramp(&mut ctx as *mut RtCtx, argv.as_ptr(), argv.len() as u32) };

        if let Some(e) = ctx.error.take() {
            return Err(e);
        }
        Ok(ctx.value(result))
    }

    /// Host-invoke a closure value previously returned by [`call`](Self::call) (or by another
    /// `call_value`). The closure carries its captured cells, so upvalue mutations persist
    /// across host calls. The closure also keeps this module's native code mapped, so it stays
    /// callable even after the originating `JitModule` is dropped.
    pub fn call_value(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, RunError> {
        let code = match &callee {
            Value::Closure(c) => c.code.clone(),
            other => {
                return Err(RunError::Internal(format!(
                    "call_value on a {} value",
                    other.type_name()
                )));
            }
        };
        let tramp = *self
            .closure_tramps
            .get(&code)
            .ok_or_else(|| RunError::Internal(format!("missing closure trampoline `{code}`")))?;

        let mut ctx = self.make_ctx();
        // The closure itself is the env argument (it carries the captured cells).
        let env = ctx.intern(callee);
        let argv: Vec<Handle> = args.into_iter().map(|a| ctx.intern(a)).collect();
        // SAFETY: `tramp` has the `EnvTrampFn` ABI; `ctx` outlives the call; `argv` is valid
        // for `argv.len()` handles.
        let result =
            unsafe { tramp(&mut ctx as *mut RtCtx, env, argv.as_ptr(), argv.len() as u32) };

        if let Some(e) = ctx.error.take() {
            return Err(e);
        }
        Ok(ctx.value(result))
    }

    /// Build a fresh per-invocation [`RtCtx`], resolving host/memory bindings into pool-id
    /// order and threading the indirect-call address table plus a code keepalive.
    fn make_ctx(&self) -> RtCtx {
        let host: Vec<Option<NativeFn>> = self
            .pools
            .host_fns
            .iter()
            .map(|n| self.host.get(n).cloned())
            .collect();
        let memory: Vec<Value> = self
            .pools
            .memories
            .iter()
            .map(|n| self.memory.get(n).cloned().unwrap_or(Value::Nil))
            .collect();
        let keepalive: Rc<dyn std::any::Any> = self.module.clone();
        RtCtx::new(
            self.pools.clone(),
            host,
            memory,
            self.code_addrs.clone(),
            Some(keepalive),
        )
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
        ("rt_memory_ref", &[I64, I32], &[I64]),
        ("rt_namespace_field", &[I64, I32], &[I64]),
        ("rt_array_new", &[I64, I32], &[I64]),
        ("rt_array_push", &[I64, I64, I64], &[]),
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
        ("rt_errored", &[I64], &[I8]),
        ("rt_call_host", &[I64, I32, I64, I32], &[I64]),
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
    shims: &HashMap<&'static str, FuncId>,
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
        build_trampoline(
            builder,
            module as &mut dyn Module,
            shims,
            typed_id,
            params,
            ret,
        );
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
    shims: &HashMap<&'static str, FuncId>,
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
        build_env_trampoline(
            builder,
            module as &mut dyn Module,
            shims,
            typed_id,
            params,
            ret,
        );
    }
    module
        .define_function(tramp_id, &mut ctx)
        .map_err(|e| JitError::Cranelift(e.to_string()))?;
    module.clear_context(&mut ctx);
    Ok(())
}
