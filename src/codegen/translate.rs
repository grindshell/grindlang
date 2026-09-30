//! IR → cranelift translation for one function (`PLAN.md` Phase 7).
//!
//! Implements the hybrid value model (see [`super::rt`]): numbers/bools are native unboxed
//! `f64`/`i8`; reference values are `i64` handles threaded through the per-call [`RtCtx`],
//! which is passed as the hidden first parameter of every compiled function. Reference ops
//! lower to calls into the `rt_*` shims; scalar ops lower to native cranelift instructions.
//!
//! ## Error propagation
//!
//! Every operation that can fail branches to the function's error-exit **at its own call
//! site**, so a raised error aborts the rest of the function exactly as it does in the two
//! interpreter oracles. [`Translator::call_fallible_shim`] splits the current block after each
//! such op: the failure edge goes to the error-exit, and the following instructions land in a
//! fresh continuation block.
//!
//! It is not enough to latch the error and keep going. The *value* a failed op produces is
//! harmless — the null handle, which unboxes to a default — but the *statements after it* are
//! not: they run host functions and write host memory the interpreters never reach, so an
//! errored call would leave side effects (and durable memory state) the oracles disagree on.
//!
//! How a failure is signalled depends on what the callee can carry:
//!
//! - a shim returning a handle → the [`ERR`](super::rt::ERR) sentinel;
//! - a shim returning a scalar word → the `errored` byte, since a NaN can carry any bit
//!   pattern, `ERR`'s included;
//! - a NaN comparison → detected natively, raised on the failing edge only;
//! - `rt_closure_code_addr` → address `0` (jumping there would fault);
//! - a script / const / indirect call → nothing, so the caller loads the context's `errored`
//!   byte directly (see [`ERRORED_OFFSET`](super::rt::ERRORED_OFFSET)).
//!
//! ## Call depth
//!
//! Every compiled function takes a hidden second parameter after the ctx: how many more script
//! calls may start, the call it begins included (`SPEC.md` §7.3). A script function raises the
//! call-depth error on entry when that is zero and passes one less to everything it calls; a
//! trampoline starts it at the context's limit. Without the limit, runaway recursion would
//! overflow the native stack and abort the host process.
//!
//! The budget travels in a register rather than in the context because it is on every call
//! path: a counter in memory costs a load and a store on entry and again on every return, and
//! each call waits on the store its caller just made. As a parameter it costs a compare and a
//! subtract, and a return has nothing to undo. See [`Translator::enter_frame`].

use std::collections::HashMap;

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    AbiParam, Block as ClifBlock, InstBuilder, MemFlags, Signature, StackSlotData, StackSlotKind,
    Value as ClifValue, types,
};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::{FunctionBuilder, Variable};
use cranelift_module::{FuncId, Module};

use crate::ast::{BinOp, UnOp};
use crate::ir::{Function, LocalId, Op, Terminator, ValueId};
use crate::runtime::repr::Repr;
use crate::types::Type;

use super::rt::{ERR, Pools, repr_tag};

/// The cranelift type a [`Repr`] lowers to. `None` for [`Repr::Unit`] (no value).
pub fn clif_ty(r: Repr) -> Option<types::Type> {
    match r {
        Repr::Number => Some(types::F64),
        Repr::Bool => Some(types::I8),
        Repr::Ptr => Some(types::I64),
        Repr::Unit => None,
    }
}

fn repr_of(ty: &Type) -> Repr {
    Repr::of(ty)
}

/// A constant's return repr.
///
/// Indexed, not defaulted: `const_rets` is filled for every constant in the program before any
/// body is translated, and [`Translator::call_const_body`] indexes `const_ids` the same way one
/// step later. A fallback here would not be conservative — guessing `Ptr` for a *scalar*
/// constant makes [`Translator::translate_const_ref`] pass an `f64` where the memo path wants an
/// `i64` handle, which fails cranelift's verifier rather than merely costing a lookup.
fn repr_of_const(cx: &Context<'_>, name: &str) -> Repr {
    cx.const_rets[name]
}

/// Build the typed signature of a compiled function: `(ctx, depth, params...) -> ret`, where
/// `depth` is the call budget (see the module docs). A lifted closure's hidden env parameter is
/// simply its first `Ptr` param.
pub fn typed_signature(call_conv: CallConv, params: &[Repr], ret: Repr) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(types::I64)); // ctx
    sig.params.push(AbiParam::new(types::I32)); // depth
    for &p in params {
        if let Some(t) = clif_ty(p) {
            sig.params.push(AbiParam::new(t));
        }
    }
    if let Some(t) = clif_ty(ret) {
        sig.returns.push(AbiParam::new(t));
    }
    sig
}

/// The uniform trampoline signature: `(ctx, argv, argc) -> word`, or with `env`,
/// `(ctx, env, argv, argc) -> word` — the form the host invokes a returned closure through
/// ([`crate::codegen::JitModule::call_value`]), passing the closure value as `env`.
pub fn trampoline_signature(call_conv: CallConv, env: bool) -> Signature {
    let mut sig = Signature::new(call_conv);
    sig.params.push(AbiParam::new(types::I64)); // ctx
    if env {
        sig.params.push(AbiParam::new(types::I64)); // env (closure value handle)
    }
    sig.params.push(AbiParam::new(types::I64)); // argv: *const u64
    sig.params.push(AbiParam::new(types::I32)); // argc
    sig.returns.push(AbiParam::new(types::I64)); // result word
    sig
}

/// Shared lookups the translator needs: shim and script-function ids.
pub struct Context<'a> {
    pub call_conv: CallConv,
    pub shims: &'a HashMap<&'static str, FuncId>,
    pub typed_ids: &'a HashMap<String, FuncId>,
    pub const_ids: &'a HashMap<String, FuncId>,
    /// Each constant's return repr, so [`Translator::translate_const_ref`] can tell a
    /// reference-valued constant (memoized per call, to keep its identity stable) from a scalar
    /// one (re-evaluated, since a scalar has no identity to keep).
    pub const_rets: &'a HashMap<String, Repr>,
}

/// Translates one IR [`Function`] into the builder's clif function.
pub struct Translator<'a, 'b> {
    builder: FunctionBuilder<'b>,
    module: &'a mut dyn Module,
    cx: &'a Context<'a>,
    pools: &'a mut Pools,
    func: &'a Function,

    /// Whether this body is a script call, counted against the call-depth limit (`SPEC.md`
    /// §7.3). A constant initializer is not: reading a constant isn't a call in the
    /// interpreters, so counting it here would make the JIT fail one call sooner than they do.
    counts_calls: bool,

    ctx_val: ClifValue,
    /// The call budget this function passes to everything it calls (see the module docs).
    depth_val: ClifValue,
    blocks: Vec<ClifBlock>,
    error_block: ClifBlock,
    vars: HashMap<LocalId, Variable>,
    ssa: Vec<Option<ClifValue>>,
    /// Per-function cache of imported shim func refs.
    shim_refs: HashMap<&'static str, cranelift_codegen::ir::FuncRef>,
}

impl<'a, 'b> Translator<'a, 'b> {
    pub fn new(
        builder: FunctionBuilder<'b>,
        module: &'a mut dyn Module,
        cx: &'a Context<'a>,
        pools: &'a mut Pools,
        func: &'a Function,
        counts_calls: bool,
    ) -> Self {
        Translator {
            builder,
            module,
            cx,
            pools,
            func,
            counts_calls,
            ctx_val: ClifValue::from_u32(0),
            depth_val: ClifValue::from_u32(0),
            blocks: Vec::new(),
            error_block: ClifBlock::from_u32(0),
            vars: HashMap::new(),
            ssa: vec![None; func.values.len()],
            shim_refs: HashMap::new(),
        }
    }

    /// Translate the whole function body.
    pub fn run(mut self) {
        // One clif block per IR block, plus a shared error-exit block.
        for _ in 0..self.func.blocks.len() {
            let b = self.builder.create_block();
            self.blocks.push(b);
        }
        self.error_block = self.builder.create_block();

        // Declare a Variable for every local, typed by its repr (block-independent).
        let locals: Vec<(LocalId, Type)> = self
            .func
            .locals
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        for (id, ty) in &locals {
            let r = repr_of(ty);
            if let Some(t) = clif_ty(r) {
                let var = self.builder.declare_var(t);
                self.vars.insert(*id, var);
            }
        }

        // A prologue block receives the parameters — the hidden ctx and call budget first, then
        // the script's params in order — checks the budget, and continues into the IR's entry
        // block. It must receive its first instruction *before* any other block so it becomes
        // cranelift's layout entry — hence it is filled before the IR blocks and the error exit.
        let prologue = self.builder.create_block();
        self.builder
            .append_block_params_for_function_params(prologue);
        self.builder.switch_to_block(prologue);
        let params = self.builder.block_params(prologue).to_vec();
        self.ctx_val = params[0];
        let depth = params[1];
        let mut pi = 2usize;
        let param_ids: Vec<LocalId> = self.func.params.clone();
        for id in param_ids {
            if let Some(&var) = self.vars.get(&id) {
                let v = params[pi];
                self.builder.def_var(var, v);
                pi += 1;
            }
        }
        self.depth_val = if self.counts_calls {
            self.enter_frame(depth)
        } else {
            depth
        };
        let entry = self.blocks[self.func.entry as usize];
        self.builder.ins().jump(entry, &[]);

        for i in 0..self.func.blocks.len() {
            self.translate_block(i);
        }

        // Fill the error-exit block last: return the default for the function's ret repr.
        let ret_repr = repr_of(&self.func.ret);
        self.builder.switch_to_block(self.error_block);
        match clif_ty(ret_repr) {
            Some(types::F64) => {
                let z = self.builder.ins().f64const(0.0);
                self.builder.ins().return_(&[z]);
            }
            Some(t) => {
                let z = self.builder.ins().iconst(t, 0);
                self.builder.ins().return_(&[z]);
            }
            None => {
                self.builder.ins().return_(&[]);
            }
        }

        self.builder.seal_all_blocks();
        self.builder.finalize();
    }

    fn translate_block(&mut self, idx: usize) {
        let clif_block = self.blocks[idx];
        self.builder.switch_to_block(clif_block);

        let block = &self.func.blocks[idx];
        for instr in &block.instrs {
            let result = self.translate_op(&instr.op, instr.dest);
            if let Some(dest) = instr.dest {
                // Even Unit-typed dests get a placeholder so later lookups never panic.
                let v = result.unwrap_or_else(|| self.builder.ins().iconst(types::I64, 0));
                self.ssa[dest as usize] = Some(v);
            }
        }

        match &block.term {
            Terminator::Return(Some(v)) => {
                // Coerce to the function's declared return repr: box a scalar returned where an
                // optional is declared (`return 1` from a `number?` function), and unbox the
                // reverse. Narrowed reads no longer need this — `Op::Unwrap` already unboxed
                // them where they were read.
                let from = self.value_repr(*v);
                let to = repr_of(&self.func.ret);
                let val = self.val(*v);
                let val = self.coerce(val, from, to);
                self.builder.ins().return_(&[val]);
            }
            Terminator::Return(None) => {
                match clif_ty(repr_of(&self.func.ret)) {
                    Some(types::F64) => {
                        let z = self.builder.ins().f64const(0.0);
                        self.builder.ins().return_(&[z]);
                    }
                    Some(t) => {
                        let z = self.builder.ins().iconst(t, 0);
                        self.builder.ins().return_(&[z]);
                    }
                    None => {
                        self.builder.ins().return_(&[]);
                    }
                };
            }
            Terminator::Jump(t) => {
                // No back-edge error check: every fallible op now diverts to the error-exit at
                // its own call site, so control can't reach a back-edge with an error pending.
                self.builder.ins().jump(self.blocks[*t as usize], &[]);
            }
            Terminator::Branch {
                cond,
                then_blk,
                else_blk,
            } => {
                let c = self.val(*cond);
                let then_b = self.blocks[*then_blk as usize];
                let else_b = self.blocks[*else_blk as usize];
                self.builder.ins().brif(c, then_b, &[], else_b, &[]);
            }
            Terminator::Unreachable => {
                self.builder
                    .ins()
                    .trap(cranelift_codegen::ir::TrapCode::user(1).unwrap());
            }
        }
    }

    fn val(&self, v: ValueId) -> ClifValue {
        self.ssa[v as usize].expect("use of undefined IR value")
    }

    fn value_repr(&self, v: ValueId) -> Repr {
        repr_of(self.func.value_type(v))
    }

    // ---- shim plumbing ------------------------------------------------------

    fn shim_ref(&mut self, name: &'static str) -> cranelift_codegen::ir::FuncRef {
        if let Some(r) = self.shim_refs.get(name) {
            return *r;
        }
        let id = self.cx.shims[name];
        let r = self.module.declare_func_in_func(id, self.builder.func);
        self.shim_refs.insert(name, r);
        r
    }

    fn call_shim(&mut self, name: &'static str, args: &[ClifValue]) -> Option<ClifValue> {
        let r = self.shim_ref(name);
        let call = self.builder.ins().call(r, args);
        self.builder.inst_results(call).first().copied()
    }

    /// Divert to the function's error-exit when `failed` (an `i8` flag) is nonzero, and
    /// continue translating into a fresh block. Splitting here is what makes an error *abort*
    /// the rest of the function rather than merely being recorded: everything emitted after
    /// this point is reachable only on the success edge.
    fn guard(&mut self, failed: ClifValue) {
        let cont = self.builder.create_block();
        self.builder
            .ins()
            .brif(failed, self.error_block, &[], cont, &[]);
        self.builder.switch_to_block(cont);
    }

    /// Guard on a natively computed failure condition. Unlike [`guard`](Self::guard), the
    /// error still has to be *recorded*, so the failing edge lands in a slow-path block that
    /// calls `raise_shim` before taking the error-exit. The fast path stays call-free.
    fn guard_raising(&mut self, failed: ClifValue, raise_shim: &'static str) {
        let raise = self.builder.create_block();
        let cont = self.builder.create_block();
        self.builder.ins().brif(failed, raise, &[], cont, &[]);

        self.builder.switch_to_block(raise);
        self.call_shim(raise_shim, &[self.ctx_val]);
        self.builder.ins().jump(self.error_block, &[]);

        self.builder.switch_to_block(cont);
    }

    /// Call a shim that reports failure with the [`ERR`] handle sentinel, then guard on it.
    /// The returned handle is only ever observed on the success edge, so callers may unbox it
    /// without first checking for a poisoned value.
    fn call_fallible_shim(&mut self, name: &'static str, args: &[ClifValue]) -> ClifValue {
        let result = self
            .call_shim(name, args)
            .expect("a fallible shim returns a handle");
        let failed = self
            .builder
            .ins()
            .icmp_imm(IntCC::Equal, result, ERR as i64);
        self.guard(failed);
        result
    }

    /// Guard on the context's latched-error flag. Used after calls whose return value can't
    /// carry a sentinel — script-to-script, constant, and indirect closure calls — where the
    /// callee has already returned its error-exit default.
    ///
    /// Reads the flag straight out of the context rather than calling a shim: this sits on
    /// every script call, so a call here would cost more than the branch it guards.
    fn guard_errored(&mut self) {
        let failed = self.builder.ins().load(
            types::I8,
            MemFlags::trusted(),
            self.ctx_val,
            super::rt::ERRORED_OFFSET,
        );
        self.guard(failed);
    }

    /// Start this call against the `depth` budget it was given (`SPEC.md` §7.3): raise the
    /// call-depth error if the budget is spent, and otherwise return the budget left for the
    /// calls this function makes. Only the failing edge calls into the runtime.
    fn enter_frame(&mut self, depth: ClifValue) -> ClifValue {
        let spent = self.builder.ins().icmp_imm(IntCC::Equal, depth, 0);
        self.guard_raising(spent, "rt_raise_call_depth");
        self.builder.ins().iadd_imm(depth, -1)
    }

    fn script_ref(&mut self, id: FuncId) -> cranelift_codegen::ir::FuncRef {
        self.module.declare_func_in_func(id, self.builder.func)
    }

    fn iconst32(&mut self, n: u32) -> ClifValue {
        self.builder.ins().iconst(types::I32, n as i64)
    }

    /// Encode an SSA value as a shim operand: its word plus its repr tag (see [`super::rt`]).
    /// A scalar crosses as raw bits, so handing one to the runtime never creates a value-table
    /// entry.
    fn operand(&mut self, v: ValueId) -> (ClifValue, u32) {
        let repr = self.value_repr(v);
        let val = self.val(v);
        (
            encode_word(&mut self.builder, repr, Some(val)),
            repr_tag(repr),
        )
    }

    /// [`operand`](Self::operand) as the two trailing arguments of a direct shim call.
    fn operand_args(&mut self, v: ValueId) -> [ClifValue; 2] {
        let (word, tag) = self.operand(v);
        [word, self.iconst32(tag)]
    }

    /// Call a shim whose trailing argument is the destination's repr tag and whose result is a
    /// word in that representation, then guard on failure.
    ///
    /// A scalar result has no spare bit pattern for a sentinel, so it is guarded on the errored
    /// flag — which also catches a value whose runtime shape contradicts its static scalar type
    /// (an off-schema memory binding, a host function returning the wrong shape). A handle
    /// result from a `fallible` shim is guarded on [`ERR`].
    fn call_read_shim(
        &mut self,
        name: &'static str,
        args: &[ClifValue],
        repr: Repr,
        fallible: bool,
    ) -> ClifValue {
        let mut call_args = args.to_vec();
        call_args.push(self.iconst32(repr_tag(repr)));
        let word = self
            .call_shim(name, &call_args)
            .expect("a read shim returns a word");
        match repr {
            Repr::Number | Repr::Bool => {
                self.guard_errored();
                decode_word(&mut self.builder, repr, word).expect("a scalar repr has a value")
            }
            Repr::Ptr | Repr::Unit => {
                if fallible {
                    let failed = self.builder.ins().icmp_imm(IntCC::Equal, word, ERR as i64);
                    self.guard(failed);
                }
                word
            }
        }
    }

    /// Unbox a handle into the requested representation.
    ///
    /// Forcing a reference value into a scalar slot is fallible: its static type says `number`
    /// or `bool`, but a host-supplied value (an off-schema memory binding, a host function
    /// returning the wrong shape) can contradict that at runtime. The shim latches the error
    /// and returns a default, so guard before the default can be used.
    fn unbox_handle(&mut self, handle: ClifValue, repr: Repr) -> ClifValue {
        match repr {
            Repr::Number => {
                let v = self
                    .call_shim("rt_unbox_number", &[self.ctx_val, handle])
                    .unwrap();
                self.guard_errored();
                v
            }
            Repr::Bool => {
                let v = self
                    .call_shim("rt_unbox_bool", &[self.ctx_val, handle])
                    .unwrap();
                self.guard_errored();
                v
            }
            Repr::Ptr => handle,
            Repr::Unit => self.builder.ins().iconst(types::I64, 0),
        }
    }

    /// Coerce a clif value from one representation to another, boxing/unboxing through the
    /// runtime as needed. Identity when the reprs already match. Needed where the IR's static
    /// type for a value and the slot it flows into disagree — e.g. a discard loop variable
    /// (`_`) the checker leaves untyped (`Repr::Ptr`) but that receives an unboxed number.
    fn coerce(&mut self, val: ClifValue, from: Repr, to: Repr) -> ClifValue {
        if from == to {
            return val;
        }
        match (from, to) {
            (Repr::Number, Repr::Ptr) => self
                .call_shim("rt_box_number", &[self.ctx_val, val])
                .unwrap(),
            (Repr::Bool, Repr::Ptr) => self.call_shim("rt_box_bool", &[self.ctx_val, val]).unwrap(),
            (Repr::Ptr, Repr::Number) => self.unbox_handle(val, Repr::Number),
            (Repr::Ptr, Repr::Bool) => self.unbox_handle(val, Repr::Bool),
            // Unit or scalar↔scalar (shouldn't arise post-typecheck): pass through.
            _ => val,
        }
    }

    /// The declared representation of a local variable.
    fn local_repr(&self, id: LocalId) -> Repr {
        self.func.locals.get(&id).map(repr_of).unwrap_or(Repr::Ptr)
    }

    /// Store call arguments into a fresh stack slot as `(word, tag)` pairs — the layout
    /// `rt::collect_args` decodes — and return `(base_ptr, count)`.
    fn operand_array(&mut self, args: &[ValueId]) -> (ClifValue, ClifValue) {
        let argc = self.iconst32(args.len() as u32);
        if args.is_empty() {
            let null = self.builder.ins().iconst(types::I64, 0);
            return (null, argc);
        }
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            (args.len() * 16) as u32,
            3, // align 2^3 = 8
        ));
        for (i, &a) in args.iter().enumerate() {
            let (word, tag) = self.operand(a);
            // The tag is stored as a full word: the runtime reads 8 bytes per slot.
            let tag = self.builder.ins().iconst(types::I64, tag as i64);
            self.builder.ins().stack_store(word, slot, (i * 16) as i32);
            self.builder
                .ins()
                .stack_store(tag, slot, (i * 16 + 8) as i32);
        }
        let base = self.builder.ins().stack_addr(types::I64, slot, 0);
        (base, argc)
    }

    /// Store `handles` into a fresh stack slot and return `(base_ptr, count)`.
    fn arg_array(&mut self, handles: &[ClifValue]) -> (ClifValue, ClifValue) {
        let argc = self.iconst32(handles.len() as u32);
        if handles.is_empty() {
            let null = self.builder.ins().iconst(types::I64, 0);
            return (null, argc);
        }
        let bytes = (handles.len() * 8) as u32;
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            bytes,
            3, // align 2^3 = 8
        ));
        for (i, &h) in handles.iter().enumerate() {
            self.builder.ins().stack_store(h, slot, (i * 8) as i32);
        }
        let base = self.builder.ins().stack_addr(types::I64, slot, 0);
        (base, argc)
    }

    // ---- ops ----------------------------------------------------------------

    fn translate_op(&mut self, op: &Op, dest: Option<ValueId>) -> Option<ClifValue> {
        match op {
            Op::ConstNil => Some(self.builder.ins().iconst(types::I64, 0)),
            Op::ConstBool(b) => Some(self.builder.ins().iconst(types::I8, *b as i64)),
            Op::ConstNumber(n) => Some(self.builder.ins().f64const(*n)),
            Op::ConstString(s) => {
                let idx = self.pools.intern_string(s);
                let i = self.iconst32(idx);
                self.call_shim("rt_const_string", &[self.ctx_val, i])
            }
            Op::LocalGet(id) => {
                let var = *self.vars.get(id).expect("get of undeclared local");
                Some(self.builder.use_var(var))
            }
            Op::LocalSet(id, v) => {
                if let Some(&var) = self.vars.get(id) {
                    let from = self.value_repr(*v);
                    let to = self.local_repr(*id);
                    let val = self.val(*v);
                    let val = self.coerce(val, from, to);
                    self.builder.def_var(var, val);
                }
                None
            }
            Op::Unary(uop, v) => Some(self.translate_unary(*uop, *v)),
            Op::Binary(bop, l, r) => Some(self.translate_binary(*bop, *l, *r)),
            Op::Truthy(v) => Some(self.translate_truthy(*v)),
            Op::Unwrap(v) => {
                // A narrowed `number?`/`bool?` handle becomes a native scalar here, so every
                // later use (arithmetic, branches, call arguments) sees the unboxed repr its
                // type promises. Reference optionals are already handles: identity.
                let from = self.value_repr(*v);
                let to = self.dest_repr(dest);
                let val = self.val(*v);
                Some(self.coerce(val, from, to))
            }
            Op::MakeArray(elems) => Some(self.translate_make_array(elems)),
            Op::MakeTable(pairs) => Some(self.translate_make_table(pairs)),
            Op::MakeTuple(elems) => Some(self.translate_make_tuple(elems)),
            Op::TupleGet(tup, idx) => {
                let t = self.val(*tup);
                let i = self.iconst32(*idx);
                let repr = self.dest_repr(dest);
                Some(self.call_read_shim("rt_tuple_get", &[self.ctx_val, t, i], repr, false))
            }
            Op::ArrayGet(base, idx) => {
                let b = self.val(*base);
                let i = self.val(*idx);
                let repr = self.dest_repr(dest);
                Some(self.call_read_shim("rt_array_get", &[self.ctx_val, b, i], repr, false))
            }
            Op::ArraySet(base, idx, v) => {
                let b = self.val(*base);
                let i = self.val(*idx);
                let [word, tag] = self.operand_args(*v);
                self.call_fallible_shim("rt_array_set", &[self.ctx_val, b, i, word, tag]);
                None
            }
            Op::MapGet(base, key) => {
                let b = self.val(*base);
                let k = self.val(*key);
                let repr = self.dest_repr(dest);
                Some(self.call_read_shim("rt_map_get", &[self.ctx_val, b, k], repr, false))
            }
            Op::MapSet(base, key, v) => {
                let b = self.val(*base);
                let k = self.val(*key);
                let [word, tag] = self.operand_args(*v);
                self.call_fallible_shim("rt_map_set", &[self.ctx_val, b, k, word, tag]);
                None
            }
            Op::FieldGet(base, name) => {
                let b = self.val(*base);
                let idx = self.pools.intern_name(name);
                let i = self.iconst32(idx);
                let repr = self.dest_repr(dest);
                Some(self.call_read_shim("rt_field_get", &[self.ctx_val, b, i], repr, false))
            }
            Op::FieldSet(base, name, v) => {
                let b = self.val(*base);
                let idx = self.pools.intern_name(name);
                let i = self.iconst32(idx);
                let [word, tag] = self.operand_args(*v);
                self.call_fallible_shim("rt_field_set", &[self.ctx_val, b, i, word, tag]);
                None
            }
            Op::MapKeys(base) => {
                let b = self.val(*base);
                self.call_shim("rt_map_keys", &[self.ctx_val, b])
            }
            Op::CallScript(name, args) => self.translate_call_script(name, args),
            Op::CallHost(name, args) => {
                let ret = self.dest_repr(dest);
                Some(self.translate_call_host(name, args, ret))
            }
            Op::CallMethod(symbol, receiver, args) => {
                let ret = self.dest_repr(dest);
                Some(self.translate_call_method(symbol, *receiver, args, ret))
            }
            Op::CallBuiltinValue(ns, args) => {
                let ret = self.dest_repr(dest);
                Some(self.translate_call_builtin_value(ns, args, ret))
            }
            Op::CallBuiltinMember(ns, member, args) => {
                let ret = self.dest_repr(dest);
                Some(self.translate_call_builtin_member(ns, member, args, ret))
            }
            Op::ConstRef(name) => self.translate_const_ref(name),
            Op::MemoryRef(name) => {
                let idx = self.pools.intern_memory(name);
                let i = self.iconst32(idx);
                let repr = self.dest_repr(dest);
                Some(self.call_read_shim("rt_memory_ref", &[self.ctx_val, i], repr, true))
            }
            Op::NamespaceField(ns, field) => {
                let idx = self.pools.intern_namespace_field(ns, field);
                let i = self.iconst32(idx);
                let repr = self.dest_repr(dest);
                Some(self.call_read_shim("rt_namespace_field", &[self.ctx_val, i], repr, true))
            }
            Op::MakeCell(v) => {
                let [word, tag] = self.operand_args(*v);
                self.call_shim("rt_cell_new", &[self.ctx_val, word, tag])
            }
            Op::CellGet(cell) => {
                let c = self.val(*cell);
                let repr = self.dest_repr(dest);
                Some(self.call_read_shim("rt_cell_get", &[self.ctx_val, c], repr, false))
            }
            Op::CellSet(cell, v) => {
                let c = self.val(*cell);
                let [word, tag] = self.operand_args(*v);
                self.call_shim("rt_cell_set", &[self.ctx_val, c, word, tag]);
                None
            }
            Op::ClosureEnvGet(clo, i) => {
                let c = self.val(*clo);
                let idx = self.iconst32(*i);
                // The result is a cell handle (a `Ptr`); never unbox it.
                self.call_shim("rt_closure_env_get", &[self.ctx_val, c, idx])
            }
            Op::MakeClosure(name, cells) => Some(self.translate_make_closure(name, cells)),
            Op::CallValue(callee, args) => self.translate_call_value(*callee, args),
        }
    }

    fn translate_make_closure(&mut self, name: &str, cells: &[ValueId]) -> ClifValue {
        let id = self.pools.intern_closure_name(name);
        let idc = self.iconst32(id);
        let handles: Vec<ClifValue> = cells.iter().map(|&c| self.val(c)).collect();
        let (base, argc) = self.arg_array(&handles);
        self.call_shim("rt_closure_new", &[self.ctx_val, idc, base, argc])
            .unwrap()
    }

    /// Call a closure value indirectly: resolve its lifted function's native address, then
    /// `call_indirect` with the closure as the hidden env argument followed by the (typed)
    /// real arguments. Mirrors [`Self::translate_call_script`]'s typed ABI.
    fn translate_call_value(&mut self, callee: ValueId, args: &[ValueId]) -> Option<ClifValue> {
        let clo = self.val(callee);
        let addr = self
            .call_shim("rt_closure_code_addr", &[self.ctx_val, clo])
            .unwrap();
        // An unresolvable closure latches an error and yields address 0; calling through it
        // would jump to null, so take the error-exit before the `call_indirect` below.
        let unresolved = self.builder.ins().icmp_imm(IntCC::Equal, addr, 0);
        self.guard(unresolved);
        // The lifted function's typed signature: the closure (env) first, then the real params.
        let (param_reprs, ret_repr) = match self.func.value_type(callee).clone() {
            Type::Function(ft) => (
                std::iter::once(Repr::Ptr)
                    .chain(ft.params.iter().map(repr_of))
                    .collect::<Vec<_>>(),
                repr_of(&ft.ret),
            ),
            _ => (vec![Repr::Ptr], Repr::Ptr),
        };
        let sig = typed_signature(self.cx.call_conv, &param_reprs, ret_repr);
        let sigref = self.builder.import_signature(sig);
        let mut call_args = vec![self.ctx_val, self.depth_val, clo];
        for &a in args {
            call_args.push(self.val(a));
        }
        let call = self.builder.ins().call_indirect(sigref, addr, &call_args);
        let result = self.builder.inst_results(call).first().copied();
        self.guard_errored();
        result
    }

    fn translate_unary(&mut self, op: UnOp, v: ValueId) -> ClifValue {
        let operand = self.val(v);
        match op {
            UnOp::Neg => self.builder.ins().fneg(operand),
            UnOp::Not => self.builder.ins().icmp_imm(IntCC::Equal, operand, 0),
            UnOp::Len => self.call_shim("rt_len", &[self.ctx_val, operand]).unwrap(),
        }
    }

    fn translate_binary(&mut self, op: BinOp, l: ValueId, r: ValueId) -> ClifValue {
        use BinOp::*;
        match op {
            Add | Sub | Mul | Div | FloorDiv | Mod | Pow => {
                // The native form of `runtime::ops::arith`, which the interpreters evaluate; the
                // two must compute the same formulas (`%` floored, `//` floor of the quotient).
                let a = self.val(l);
                let b = self.val(r);
                match op {
                    Add => self.builder.ins().fadd(a, b),
                    Sub => self.builder.ins().fsub(a, b),
                    Mul => self.builder.ins().fmul(a, b),
                    Div => self.builder.ins().fdiv(a, b),
                    FloorDiv => {
                        let q = self.builder.ins().fdiv(a, b);
                        self.builder.ins().floor(q)
                    }
                    Mod => {
                        let q = self.builder.ins().fdiv(a, b);
                        let fl = self.builder.ins().floor(q);
                        let prod = self.builder.ins().fmul(fl, b);
                        self.builder.ins().fsub(a, prod)
                    }
                    Pow => self.call_shim("rt_pow", &[a, b]).unwrap(),
                    _ => unreachable!(),
                }
            }
            Concat => {
                let a = self.val(l);
                let b = self.val(r);
                self.call_shim("rt_concat", &[self.ctx_val, a, b]).unwrap()
            }
            Lt | Le | Gt | Ge => self.translate_compare(op, l, r),
            Eq | Ne => self.translate_equality(op, l, r),
            And | Or => unreachable!("short-circuited in lowering"),
        }
    }

    fn translate_compare(&mut self, op: BinOp, l: ValueId, r: ValueId) -> ClifValue {
        let a = self.val(l);
        let b = self.val(r);
        if self.value_repr(l) == Repr::Number {
            let cc = match op {
                BinOp::Lt => FloatCC::LessThan,
                BinOp::Le => FloatCC::LessThanOrEqual,
                BinOp::Gt => FloatCC::GreaterThan,
                BinOp::Ge => FloatCC::GreaterThanOrEqual,
                _ => unreachable!(),
            };
            let result = self.builder.ins().fcmp(cc, a, b);
            // The native `fcmp` yields `false` for a NaN operand (IEEE unordered), but both
            // interpreter oracles error on ordering a NaN. Detect that natively — a second
            // `fcmp`, no call — and raise only on the failing edge, so ordinary comparisons
            // cost one extra instruction rather than a trip into the runtime. (`==`/`!=` need
            // no guard: IEEE equality matches the oracles' `partial_cmp`-free `scalar_eq`.)
            let unordered = self.builder.ins().fcmp(FloatCC::Unordered, a, b);
            self.guard_raising(unordered, "rt_raise_nan_compare");
            result
        } else {
            // String ordering via the runtime: rt_str_cmp -> {-1,0,1}.
            let ord = self.call_shim("rt_str_cmp", &[self.ctx_val, a, b]).unwrap();
            let cc = match op {
                BinOp::Lt => IntCC::SignedLessThan,
                BinOp::Le => IntCC::SignedLessThanOrEqual,
                BinOp::Gt => IntCC::SignedGreaterThan,
                BinOp::Ge => IntCC::SignedGreaterThanOrEqual,
                _ => unreachable!(),
            };
            self.builder.ins().icmp_imm(cc, ord, 0)
        }
    }

    fn translate_equality(&mut self, op: BinOp, l: ValueId, r: ValueId) -> ClifValue {
        let a = self.val(l);
        let b = self.val(r);
        let eq = match self.value_repr(l) {
            Repr::Number => self.builder.ins().fcmp(FloatCC::Equal, a, b),
            Repr::Bool => self.builder.ins().icmp(IntCC::Equal, a, b),
            _ => self
                .call_shim("rt_value_eq", &[self.ctx_val, a, b])
                .unwrap(),
        };
        match op {
            BinOp::Eq => eq,
            BinOp::Ne => self.builder.ins().icmp_imm(IntCC::Equal, eq, 0),
            _ => unreachable!(),
        }
    }

    fn translate_truthy(&mut self, v: ValueId) -> ClifValue {
        match self.value_repr(v) {
            Repr::Bool => self.val(v),
            Repr::Number => self.builder.ins().iconst(types::I8, 1),
            _ => {
                let h = self.val(v);
                self.call_shim("rt_truthy", &[self.ctx_val, h]).unwrap()
            }
        }
    }

    fn translate_make_array(&mut self, elems: &[ValueId]) -> ClifValue {
        let cap = self.iconst32(elems.len() as u32);
        let arr = self
            .call_shim("rt_array_new", &[self.ctx_val, cap])
            .unwrap();
        for &e in elems {
            let [word, tag] = self.operand_args(e);
            self.call_shim("rt_array_push", &[self.ctx_val, arr, word, tag]);
        }
        arr
    }

    fn translate_make_tuple(&mut self, elems: &[ValueId]) -> ClifValue {
        let cap = self.iconst32(elems.len() as u32);
        let tup = self
            .call_shim("rt_tuple_new", &[self.ctx_val, cap])
            .unwrap();
        for &e in elems {
            let [word, tag] = self.operand_args(e);
            self.call_shim("rt_tuple_push", &[self.ctx_val, tup, word, tag]);
        }
        tup
    }

    fn translate_make_table(&mut self, pairs: &[(String, ValueId)]) -> ClifValue {
        let tbl = self.call_shim("rt_table_new", &[self.ctx_val]).unwrap();
        for (k, v) in pairs {
            let idx = self.pools.intern_name(k);
            let i = self.iconst32(idx);
            let [word, tag] = self.operand_args(*v);
            // Building the literal, not assigning through it — no immutability check needed.
            self.call_shim("rt_field_init", &[self.ctx_val, tbl, i, word, tag]);
        }
        tbl
    }

    fn translate_call_script(&mut self, name: &str, args: &[ValueId]) -> Option<ClifValue> {
        let id = self.cx.typed_ids[name];
        let fref = self.script_ref(id);
        let mut call_args = vec![self.ctx_val, self.depth_val];
        for &a in args {
            call_args.push(self.val(a));
        }
        let call = self.builder.ins().call(fref, &call_args);
        let result = self.builder.inst_results(call).first().copied();
        // A callee that failed took its own error-exit and returned that block's default, which
        // is indistinguishable from a real result — so propagate through the context flag.
        self.guard_errored();
        result
    }

    /// Read a top-level constant.
    ///
    /// A **reference-valued** constant is memoized for the duration of the call: every read in
    /// one invocation yields the same value, so a constant is equal to itself (reference values
    /// compare by identity), while the cache dies with the call so nothing survives between
    /// them (`SPEC.md` §1, §3). A **scalar** constant skips all of this — it has no identity to
    /// keep stable, and caching one would mean boxing it into the value table per call.
    ///
    /// The lookup is lazy: a constant this call never reads is never evaluated.
    fn translate_const_ref(&mut self, name: &str) -> Option<ClifValue> {
        if repr_of_const(self.cx, name) != Repr::Ptr {
            return self.call_const_body(name);
        }

        let idc = {
            let id = self.pools.intern_const(name);
            self.iconst32(id)
        };
        let cached = self
            .call_shim("rt_const_cached", &[self.ctx_val, idc])
            .unwrap();
        // `ERR` marks a miss: `nil` is a legal constant value, so the null handle can't.
        let miss = self
            .builder
            .ins()
            .icmp_imm(IntCC::Equal, cached, ERR as i64);

        let eval = self.builder.create_block();
        let done = self.builder.create_block();
        self.builder.append_block_param(done, types::I64);
        self.builder
            .ins()
            .brif(miss, eval, &[], done, &[cached.into()]);

        self.builder.switch_to_block(eval);
        let fresh = self
            .call_const_body(name)
            .expect("a reference-valued constant returns a handle");
        self.call_shim("rt_const_store", &[self.ctx_val, idc, fresh]);
        self.builder.ins().jump(done, &[fresh.into()]);

        self.builder.switch_to_block(done);
        Some(self.builder.block_params(done)[0])
    }

    /// Invoke a constant's compiled body, propagating a failure the way any script call does —
    /// a `constexpr` can contain a comparison, so its body is not unconditionally infallible.
    fn call_const_body(&mut self, name: &str) -> Option<ClifValue> {
        let id = self.cx.const_ids[name];
        let fref = self.script_ref(id);
        let call = self
            .builder
            .ins()
            .call(fref, &[self.ctx_val, self.depth_val]);
        let result = self.builder.inst_results(call).first().copied();
        self.guard_errored();
        result
    }

    fn translate_call_host(&mut self, name: &str, args: &[ValueId], ret: Repr) -> ClifValue {
        let id = self.pools.intern_host(name);
        let (base, argc) = self.operand_array(args);
        let idc = self.iconst32(id);
        self.call_read_shim("rt_call_host", &[self.ctx_val, idc, base, argc], ret, true)
    }

    /// Lower a host-memory method call (`mem:method(args)`, `SPEC.md` §7). Mirrors
    /// [`translate_call_host`](Self::translate_call_host) but passes the receiver as the first
    /// argument and dispatches through the method pool.
    fn translate_call_method(
        &mut self,
        symbol: &str,
        receiver: ValueId,
        args: &[ValueId],
        ret: Repr,
    ) -> ClifValue {
        let id = self.pools.intern_method(symbol);
        let operands: Vec<ValueId> = std::iter::once(receiver)
            .chain(args.iter().copied())
            .collect();
        let (base, argc) = self.operand_array(&operands);
        let idc = self.iconst32(id);
        self.call_read_shim(
            "rt_call_method",
            &[self.ctx_val, idc, base, argc],
            ret,
            true,
        )
    }

    fn translate_call_builtin_value(&mut self, ns: &str, args: &[ValueId], ret: Repr) -> ClifValue {
        let id = self.pools.intern_builtin_value(ns);
        let (base, argc) = self.operand_array(args);
        let idc = self.iconst32(id);
        self.call_read_shim(
            "rt_call_builtin_value",
            &[self.ctx_val, idc, base, argc],
            ret,
            true,
        )
    }

    fn translate_call_builtin_member(
        &mut self,
        ns: &str,
        member: &str,
        args: &[ValueId],
        ret: Repr,
    ) -> ClifValue {
        // Fast path: the common single-argument numeric `math.*` builtins lower to a single
        // native cranelift float instruction, skipping the box → `extern "C"` shim → unbox
        // round-trip (which otherwise dominates the cost of an otherwise-trivial call). These
        // map exactly onto IEEE operations, so the result is bit-identical to the runtime's
        // reference impl (`builtins::member_call`) that the interpreter and IR VM use.
        //
        // `math.min`/`math.max` are deliberately NOT inlined: cranelift `fmin`/`fmax`
        // propagate NaN and prefer negative zero, whereas the reference impl uses Rust's
        // `f64::min`/`max` (NaN-ignoring). Keeping them on the shim guarantees the three
        // oracles stay bit-identical. `pow` has no native cranelift instruction.
        if ns == "math"
            && ret == Repr::Number
            && args.len() == 1
            && self.value_repr(args[0]) == Repr::Number
            && let Some(native) = self.try_native_math1(member, args[0])
        {
            return native;
        }

        let id = self.pools.intern_builtin_member(ns, member);
        let (base, argc) = self.operand_array(args);
        let idc = self.iconst32(id);
        self.call_read_shim(
            "rt_call_builtin_member",
            &[self.ctx_val, idc, base, argc],
            ret,
            true,
        )
    }

    /// Lower a single-argument `math.*` builtin to a native cranelift float instruction, if one
    /// exists with matching IEEE semantics. The argument is already an unboxed `f64`
    /// (`Repr::Number`); the result is an unboxed `f64`. Returns `None` for members without a
    /// safe native equivalent, so the caller falls back to the runtime shim.
    fn try_native_math1(&mut self, member: &str, arg: ValueId) -> Option<ClifValue> {
        let v = self.val(arg);
        Some(match member {
            "floor" => self.builder.ins().floor(v),
            "ceil" => self.builder.ins().ceil(v),
            "abs" => self.builder.ins().fabs(v),
            "sqrt" => self.builder.ins().sqrt(v),
            _ => return None,
        })
    }

    /// The repr of an op's destination value (defaults to `Ptr` for the absent/Unit case).
    fn dest_repr(&self, dest: Option<ValueId>) -> Repr {
        match dest {
            Some(v) => self.value_repr(v),
            None => Repr::Ptr,
        }
    }
}

/// Decode a raw 64-bit ABI word into the typed value of representation `r`. Scalars are
/// reinterpreted natively — no value-table round-trip: a `Number` word holds the `f64` bit
/// pattern, a `Bool` word holds `0`/`1`, a `Ptr` word is already the `i64` handle. Returns
/// `None` for `Unit` (no value). Used for trampoline arguments and shim results alike.
fn decode_word(builder: &mut FunctionBuilder<'_>, r: Repr, slot: ClifValue) -> Option<ClifValue> {
    Some(match r {
        Repr::Number => builder.ins().bitcast(types::F64, MemFlags::new(), slot),
        Repr::Bool => builder.ins().ireduce(types::I8, slot),
        Repr::Ptr => slot,
        Repr::Unit => return None,
    })
}

/// Encode a typed value as a raw 64-bit ABI word — the inverse of [`decode_word`]: an `f64` as
/// its bit pattern, a `bool` zero-extended, a `Ptr` handle as-is, `Unit` as `0`. Used for
/// trampoline results and shim operands alike.
fn encode_word(builder: &mut FunctionBuilder<'_>, ret: Repr, res: Option<ClifValue>) -> ClifValue {
    match ret {
        Repr::Number => builder
            .ins()
            .bitcast(types::I64, MemFlags::new(), res.unwrap()),
        Repr::Bool => builder.ins().uextend(types::I64, res.unwrap()),
        Repr::Ptr => res.unwrap(),
        Repr::Unit => builder.ins().iconst(types::I64, 0),
    }
}

/// Build a uniform trampoline (see [`trampoline_signature`]) that decodes `argv` per the callee's
/// parameter reprs, calls the typed function, and re-encodes the result. Used by the driver,
/// which can't form an arbitrary-arity native call.
///
/// With `env` (a lifted closure), the closure value is threaded through as the typed function's
/// hidden first argument and `params` are the *source-level* parameters, excluding the env.
///
/// The buffer uses the **bits ABI** (see [`decode_word`]/[`encode_word`]): scalar args and
/// results cross the boundary as raw native words, so a number/bool call never touches the
/// value table — only genuine reference values are interned by the driver. Shim calls inside
/// the body use the same encoding for their operands and results.
pub fn build_trampoline(
    mut builder: FunctionBuilder<'_>,
    module: &mut dyn Module,
    typed_id: FuncId,
    params: &[Repr],
    ret: Repr,
    env: bool,
) {
    let entry = builder.create_block();
    builder.append_block_params_for_function_params(entry);
    builder.switch_to_block(entry);
    let bp = builder.block_params(entry).to_vec();
    // `(ctx, [env,] argv, argc)`: everything before argv is forwarded as-is, with the call
    // budget — the context's whole limit, since this is where a host call enters — after ctx.
    let leading = if env { 2 } else { 1 };
    let argv = bp[leading];
    let ctx = bp[0];
    let depth = builder.ins().load(
        types::I32,
        MemFlags::trusted(),
        ctx,
        super::rt::MAX_CALL_DEPTH_OFFSET,
    );

    let mut call_args = vec![ctx, depth];
    call_args.extend_from_slice(&bp[1..leading]);
    for (i, &r) in params.iter().enumerate() {
        let slot = builder
            .ins()
            .load(types::I64, MemFlags::trusted(), argv, (i * 8) as i32);
        if let Some(a) = decode_word(&mut builder, r, slot) {
            call_args.push(a);
        }
    }

    let fref = module.declare_func_in_func(typed_id, builder.func);
    let call = builder.ins().call(fref, &call_args);
    let res = builder.inst_results(call).first().copied();

    let out = encode_word(&mut builder, ret, res);
    builder.ins().return_(&[out]);
    builder.seal_all_blocks();
    builder.finalize();
}
