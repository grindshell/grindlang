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
//! - a shim returning a scalar word, or nothing → the `errored` byte, since a NaN can carry any
//!   bit pattern, `ERR`'s included;
//! - a NaN comparison → detected natively, raised on the failing edge only;
//! - `rt_closure_code_addr` → address `0` (jumping there would fault);
//! - a script / indirect call → nothing, so the caller loads the context's `errored` byte
//!   directly (see [`ERRORED_OFFSET`](super::rt::ERRORED_OFFSET)).
//!
//! Every shim call is guarded, even one whose op can't fail, because any shim can panic: the
//! panic stops at the shim and comes back as a failure (see [`super::rt`]'s module docs). The
//! only unguarded calls are to `rt_pow`, which has no context to report through and nothing that
//! can panic, and to the `rt_raise_*` shims, which lead straight to the error-exit.
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
//!
//! ## Loops and the value table
//!
//! A loop that can intern values compacts the value table at its back edge (see [`super::rt`]'s
//! module docs). On entering the loop it notes the table's length, its *mark*: everything
//! interned from there on is the loop's to drop. At the back edge it keeps only the handles held
//! by reference-typed locals that are live at the loop header and assigned inside the loop. The
//! IR carries nothing else across a back edge — it has no block parameters, so a value computed
//! in one iteration reaches the next only through a local — and a local the loop never assigns
//! still holds a handle from before the mark. Compaction moves the survivors, so the back edge
//! reloads those locals from where the runtime rewrote them.
//!
//! The back edge only calls into the runtime once the table has outgrown a limit, which the
//! runtime sets on each compaction; the hot path is a load, a compare, and a branch. A loop made
//! only of ops that never intern (see [`never_interns`]) skips even that.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    AbiParam, Block as ClifBlock, InstBuilder, MemFlags, Signature, StackSlot, StackSlotData,
    StackSlotKind, Value as ClifValue, types,
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
}

/// Translates one IR [`Function`] into the builder's clif function.
pub struct Translator<'a, 'b> {
    builder: FunctionBuilder<'b>,
    module: &'a mut dyn Module,
    cx: &'a Context<'a>,
    pools: &'a mut Pools,
    func: &'a Function,

    /// Whether this body is a script call, counted against the call-depth limit (`SPEC.md`
    /// §7.3). The module's top-level code is not: it isn't a call in the interpreters, so
    /// counting it here would make the JIT fail one call sooner than they do.
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
    /// The loops that compact the value table, by header block index (see the module docs).
    loops: HashMap<usize, LoopPlan>,
}

/// How one loop keeps the value table from growing with its iteration count (see the module docs).
struct LoopPlan {
    /// The blocks whose jump to the header is a back edge. Any other jump to it enters the loop.
    back_edges: HashSet<usize>,
    /// Two words: the table's length when the loop was last entered (its mark), then the length
    /// past which the next back edge compacts. In the frame rather than in variables, so the
    /// loop's hot path keeps no extra values alive across its iterations — or across its calls.
    bounds: StackSlot,
    /// The locals whose handles survive a compaction: reference-typed, live at the header, and
    /// assigned inside the loop.
    live: Vec<Variable>,
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
            loops: HashMap::new(),
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

        for (header, back_edges, live) in plan_loops(self.func) {
            let plan = LoopPlan {
                back_edges,
                bounds: self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    16,
                    3, // align 2^3 = 8
                )),
                live: live.iter().map(|id| self.vars[id]).collect(),
            };
            self.loops.insert(header, plan);
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
                let target = *t as usize;
                match self.loops.get(&target) {
                    Some(plan) if plan.back_edges.contains(&idx) => {
                        self.compact_at_back_edge(target);
                    }
                    entering => {
                        if entering.is_some() {
                            self.enter_loop(target);
                        }
                        self.builder.ins().jump(self.blocks[target], &[]);
                    }
                }
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

    /// Call a shim without guarding it. Every caller guards the call itself, by whichever means
    /// the shim's result allows (see the module docs).
    fn call_shim(&mut self, name: &'static str, args: &[ClifValue]) -> Option<ClifValue> {
        let r = self.shim_ref(name);
        let call = self.builder.ins().call(r, args);
        self.builder.inst_results(call).first().copied()
    }

    /// Call a shim whose result, if any, can't carry the [`ERR`] sentinel, then guard on the
    /// errored flag.
    fn call_checked_shim(&mut self, name: &'static str, args: &[ClifValue]) -> Option<ClifValue> {
        let result = self.call_shim(name, args);
        self.guard_errored();
        result
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

    /// Load a `u64` field of the context at `offset`.
    fn load_ctx_u64(&mut self, offset: i32) -> ClifValue {
        self.builder
            .ins()
            .load(types::I64, MemFlags::trusted(), self.ctx_val, offset)
    }

    /// On a jump into the loop at `header` from outside it, mark where the loop's part of the value
    /// table starts, and how far it may grow before the first compaction.
    fn enter_loop(&mut self, header: usize) {
        let bounds = self.loops[&header].bounds;
        let len = self.load_ctx_u64(super::rt::TABLE_LEN_OFFSET);
        let slack = self.load_ctx_u64(super::rt::COMPACT_SLACK_OFFSET);
        let limit = self.builder.ins().iadd(len, slack);
        self.builder.ins().stack_store(len, bounds, 0);
        self.builder.ins().stack_store(limit, bounds, 8);
    }

    /// Take the back edge to the loop at `header`, compacting the value table first once the
    /// loop has grown it past its limit (see the module docs). Only that cold path calls into the
    /// runtime. Ends the current block.
    fn compact_at_back_edge(&mut self, header: usize) {
        let plan = &self.loops[&header];
        let (bounds, live) = (plan.bounds, plan.live.clone());
        let header = self.blocks[header];
        let len = self.load_ctx_u64(super::rt::TABLE_LEN_OFFSET);
        let limit = self.builder.ins().stack_load(types::I64, bounds, 8);
        let grown = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedGreaterThan, len, limit);
        let compact = self.builder.create_block();
        self.builder.set_cold_block(compact);
        self.builder.ins().brif(grown, compact, &[], header, &[]);

        self.builder.switch_to_block(compact);
        let mark = self.builder.ins().stack_load(types::I64, bounds, 0);
        let handles: Vec<ClifValue> = live.iter().map(|&v| self.builder.use_var(v)).collect();
        let (base, count) = self.arg_array(&handles);
        let limit = self
            .call_shim("rt_compact", &[self.ctx_val, mark, base, count])
            .unwrap();
        self.guard_errored();
        // Each survivor's handle now reads back from where the runtime rewrote it.
        for (i, &var) in live.iter().enumerate() {
            let h = self
                .builder
                .ins()
                .load(types::I64, MemFlags::trusted(), base, (i * 8) as i32);
            self.builder.def_var(var, h);
        }
        self.builder.ins().stack_store(limit, bounds, 8);
        self.builder.ins().jump(header, &[]);
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
    /// result is guarded on [`ERR`].
    fn call_read_shim(&mut self, name: &'static str, args: &[ClifValue], repr: Repr) -> ClifValue {
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
                let failed = self.builder.ins().icmp_imm(IntCC::Equal, word, ERR as i64);
                self.guard(failed);
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
            Repr::Number => self
                .call_checked_shim("rt_unbox_number", &[self.ctx_val, handle])
                .unwrap(),
            Repr::Bool => self
                .call_checked_shim("rt_unbox_bool", &[self.ctx_val, handle])
                .unwrap(),
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
            (Repr::Number, Repr::Ptr) => {
                self.call_fallible_shim("rt_box_number", &[self.ctx_val, val])
            }
            (Repr::Bool, Repr::Ptr) => self.call_fallible_shim("rt_box_bool", &[self.ctx_val, val]),
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
                Some(self.call_fallible_shim("rt_const_string", &[self.ctx_val, i]))
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
                Some(self.call_read_shim("rt_tuple_get", &[self.ctx_val, t, i], repr))
            }
            Op::ArrayGet(base, idx) => {
                let b = self.val(*base);
                let i = self.val(*idx);
                let repr = self.dest_repr(dest);
                Some(self.call_read_shim("rt_array_get", &[self.ctx_val, b, i], repr))
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
                Some(self.call_read_shim("rt_map_get", &[self.ctx_val, b, k], repr))
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
                Some(self.call_read_shim("rt_field_get", &[self.ctx_val, b, i], repr))
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
                Some(self.call_fallible_shim("rt_map_keys", &[self.ctx_val, b]))
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
            Op::GlobalGet(slot) => Some(self.translate_global_get(*slot, self.dest_repr(dest))),
            Op::GlobalSet(slot, v) => {
                let s = self.iconst32(*slot);
                let [word, tag] = self.operand_args(*v);
                self.call_checked_shim("rt_global_set", &[self.ctx_val, s, word, tag]);
                None
            }
            Op::MemoryRef(name) => {
                let idx = self.pools.intern_memory(name);
                let i = self.iconst32(idx);
                let repr = self.dest_repr(dest);
                Some(self.call_read_shim("rt_memory_ref", &[self.ctx_val, i], repr))
            }
            Op::NamespaceField(ns, field) => {
                let idx = self.pools.intern_namespace_field(ns, field);
                let i = self.iconst32(idx);
                let repr = self.dest_repr(dest);
                Some(self.call_read_shim("rt_namespace_field", &[self.ctx_val, i], repr))
            }
            Op::MakeCell(v) => {
                let [word, tag] = self.operand_args(*v);
                Some(self.call_fallible_shim("rt_cell_new", &[self.ctx_val, word, tag]))
            }
            Op::CellGet(cell) => {
                let c = self.val(*cell);
                let repr = self.dest_repr(dest);
                Some(self.call_read_shim("rt_cell_get", &[self.ctx_val, c], repr))
            }
            Op::CellSet(cell, v) => {
                let c = self.val(*cell);
                let [word, tag] = self.operand_args(*v);
                self.call_checked_shim("rt_cell_set", &[self.ctx_val, c, word, tag]);
                None
            }
            Op::ClosureEnvGet(clo, i) => {
                let c = self.val(*clo);
                let idx = self.iconst32(*i);
                // The result is a cell handle (a `Ptr`); never unbox it.
                Some(self.call_fallible_shim("rt_closure_env_get", &[self.ctx_val, c, idx]))
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
        self.call_fallible_shim("rt_closure_new", &[self.ctx_val, idc, base, argc])
    }

    /// Call a closure value indirectly: resolve its lifted function's native address, then
    /// `call_indirect` with the closure as the hidden env argument followed by the (typed)
    /// real arguments. Mirrors [`Self::translate_call_script`]'s typed ABI.
    fn translate_call_value(&mut self, callee: ValueId, args: &[ValueId]) -> Option<ClifValue> {
        let clo = self.val(callee);
        // The lifted function's typed signature, from the callee's static type: the closure (env)
        // first, then the real params.
        let callee_ty = self.func.value_type(callee).clone();
        let (param_reprs, ret_repr) = match &callee_ty {
            Type::Function(ft) => (
                std::iter::once(Repr::Ptr)
                    .chain(ft.params.iter().map(repr_of))
                    .collect::<Vec<_>>(),
                repr_of(&ft.ret),
            ),
            _ => (vec![Repr::Ptr], Repr::Ptr),
        };
        // The runtime refuses a closure whose body was compiled for another signature, which the
        // `call_indirect` below would otherwise call through this one.
        let want =
            self.pools
                .intern_fn_type(callee_ty.to_string(), param_reprs[1..].to_vec(), ret_repr);
        let want = self.iconst32(want);
        let addr = self
            .call_shim("rt_closure_code_addr", &[self.ctx_val, clo, want])
            .unwrap();
        // An unresolvable or mismatched closure latches an error and yields address 0; calling
        // through it would jump to null, so take the error-exit before the `call_indirect`.
        let unresolved = self.builder.ins().icmp_imm(IntCC::Equal, addr, 0);
        self.guard(unresolved);
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
            UnOp::Len => self
                .call_checked_shim("rt_len", &[self.ctx_val, operand])
                .unwrap(),
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
                self.call_fallible_shim("rt_concat", &[self.ctx_val, a, b])
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
            let ord = self
                .call_checked_shim("rt_str_cmp", &[self.ctx_val, a, b])
                .unwrap();
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
                .call_checked_shim("rt_value_eq", &[self.ctx_val, a, b])
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
                self.call_checked_shim("rt_truthy", &[self.ctx_val, h])
                    .unwrap()
            }
        }
    }

    fn translate_make_array(&mut self, elems: &[ValueId]) -> ClifValue {
        let cap = self.iconst32(elems.len() as u32);
        let arr = self.call_fallible_shim("rt_array_new", &[self.ctx_val, cap]);
        for &e in elems {
            let [word, tag] = self.operand_args(e);
            self.call_checked_shim("rt_array_push", &[self.ctx_val, arr, word, tag]);
        }
        arr
    }

    fn translate_make_tuple(&mut self, elems: &[ValueId]) -> ClifValue {
        let cap = self.iconst32(elems.len() as u32);
        let tup = self.call_fallible_shim("rt_tuple_new", &[self.ctx_val, cap]);
        for &e in elems {
            let [word, tag] = self.operand_args(e);
            self.call_checked_shim("rt_tuple_push", &[self.ctx_val, tup, word, tag]);
        }
        tup
    }

    fn translate_make_table(&mut self, pairs: &[(String, ValueId)]) -> ClifValue {
        let tbl = self.call_fallible_shim("rt_table_new", &[self.ctx_val]);
        for (k, v) in pairs {
            let idx = self.pools.intern_name(k);
            let i = self.iconst32(idx);
            let [word, tag] = self.operand_args(*v);
            // Building the literal, not assigning through it — no immutability check needed.
            self.call_checked_shim("rt_field_init", &[self.ctx_val, tbl, i, word, tag]);
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

    /// Read module slot `slot` as `repr`.
    ///
    /// A `number` or `bool` slot never changes once top-level code has run, so outside top-level
    /// code its bits are loaded straight from the module's scalar table — two loads and a
    /// branch, where a constant used to be a native call. Top-level code itself takes the shim,
    /// which knows whether the slot has been set yet.
    fn translate_global_get(&mut self, slot: u32, repr: Repr) -> ClifValue {
        let Some(ty) = clif_ty(repr).filter(|_| matches!(repr, Repr::Number | Repr::Bool)) else {
            let s = self.iconst32(slot);
            return self.call_read_shim("rt_global_get", &[self.ctx_val, s], repr);
        };
        let initializing = self.builder.ins().load(
            types::I8,
            MemFlags::trusted(),
            self.ctx_val,
            super::rt::INITIALIZING_OFFSET,
        );
        let fast = self.builder.create_block();
        let slow = self.builder.create_block();
        self.builder.set_cold_block(slow);
        let done = self.builder.create_block();
        self.builder.append_block_param(done, ty);
        self.builder.ins().brif(initializing, slow, &[], fast, &[]);

        self.builder.switch_to_block(fast);
        let base = self.load_ctx_u64(super::rt::GLOBAL_BITS_OFFSET);
        let bits =
            self.builder
                .ins()
                .load(types::I64, MemFlags::trusted(), base, (slot * 8) as i32);
        let v = decode_word(&mut self.builder, repr, bits).expect("a scalar repr has a value");
        self.builder.ins().jump(done, &[v.into()]);

        self.builder.switch_to_block(slow);
        let s = self.iconst32(slot);
        let v = self.call_read_shim("rt_global_get", &[self.ctx_val, s], repr);
        self.builder.ins().jump(done, &[v.into()]);

        self.builder.switch_to_block(done);
        self.builder.block_params(done)[0]
    }

    fn translate_call_host(&mut self, name: &str, args: &[ValueId], ret: Repr) -> ClifValue {
        let id = self.pools.intern_host(name);
        let (base, argc) = self.operand_array(args);
        let idc = self.iconst32(id);
        self.call_read_shim("rt_call_host", &[self.ctx_val, idc, base, argc], ret)
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
        self.call_read_shim("rt_call_method", &[self.ctx_val, idc, base, argc], ret)
    }

    fn translate_call_builtin_value(&mut self, ns: &str, args: &[ValueId], ret: Repr) -> ClifValue {
        let id = self.pools.intern_builtin_value(ns);
        let (base, argc) = self.operand_array(args);
        let idc = self.iconst32(id);
        self.call_read_shim(
            "rt_call_builtin_value",
            &[self.ctx_val, idc, base, argc],
            ret,
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

/// A block's successors, by index.
fn successors(term: &Terminator) -> Vec<usize> {
    match term {
        Terminator::Jump(t) => vec![*t as usize],
        Terminator::Branch {
            then_blk, else_blk, ..
        } => vec![*then_blk as usize, *else_blk as usize],
        Terminator::Return(_) | Terminator::Unreachable => Vec::new(),
    }
}

/// The loops in `func` that compact the value table (see the module docs): each loop's header,
/// the blocks whose jump to it is a back edge, and the locals whose handles must survive.
///
/// A loop is left alone if every op in it never interns, if its header is the function's entry
/// (which the prologue enters with no mark), or if anything enters or repeats it through a
/// branch rather than a jump, since only a jump carries the code that marks or compacts. The last
/// two never happen with the IR's structured lowering; they are checked because a loop entered
/// without its mark would compact values from before the loop.
fn plan_loops(func: &Function) -> Vec<(usize, HashSet<usize>, Vec<LocalId>)> {
    let n = func.blocks.len();
    let succs: Vec<Vec<usize>> = func.blocks.iter().map(|b| successors(&b.term)).collect();
    let mut preds = vec![Vec::new(); n];
    for (b, ss) in succs.iter().enumerate() {
        for &s in ss {
            preds[s].push(b);
        }
    }

    // Back edges: edges to a block still on the depth-first stack. The IR's CFG comes from
    // structured lowering, so it is reducible, and these are exactly the edges that close loops.
    let entry = func.entry as usize;
    let mut back: BTreeMap<usize, HashSet<usize>> = BTreeMap::new();
    let mut state = vec![0u8; n]; // 0 unvisited, 1 on the stack, 2 done
    let mut stack = vec![(entry, 0usize)];
    state[entry] = 1;
    while let Some(&(b, i)) = stack.last() {
        match succs[b].get(i) {
            Some(&s) => {
                let top = stack.len() - 1;
                stack[top].1 += 1;
                match state[s] {
                    0 => {
                        state[s] = 1;
                        stack.push((s, 0));
                    }
                    1 => {
                        back.entry(s).or_default().insert(b);
                    }
                    _ => {}
                }
            }
            None => {
                state[b] = 2;
                stack.pop();
            }
        }
    }

    let mut live_in: Option<Vec<BTreeSet<LocalId>>> = None;
    let mut plans = Vec::new();
    for (header, sources) in back {
        let jumped = |b: usize| matches!(func.blocks[b].term, Terminator::Jump(_));
        if header == entry || !preds[header].iter().all(|&p| jumped(p)) {
            continue;
        }
        // The loop's blocks: the header, plus every block that reaches a back edge without
        // passing through the header.
        let mut body = BTreeSet::from([header]);
        let mut work: Vec<usize> = sources.iter().copied().collect();
        while let Some(b) = work.pop() {
            if body.insert(b) {
                work.extend(preds[b].iter().copied());
            }
        }
        let instrs = || body.iter().flat_map(|&b| &func.blocks[b].instrs);
        if instrs().all(|i| never_interns(func, &i.op, i.dest)) {
            continue;
        }
        let assigned: BTreeSet<LocalId> = instrs()
            .filter_map(|i| match i.op {
                Op::LocalSet(id, _) => Some(id),
                _ => None,
            })
            .collect();
        let live_in = live_in.get_or_insert_with(|| live_handle_locals(func, &succs));
        let live = live_in[header].intersection(&assigned).copied().collect();
        plans.push((header, sources, live));
    }
    plans
}

/// Whether the JIT's lowering of `op` never adds to the value table, so that a loop of nothing
/// else never needs compacting.
fn never_interns(func: &Function, op: &Op, dest: Option<ValueId>) -> bool {
    let repr = |v: ValueId| repr_of(func.value_type(v));
    let scalar = |r: Repr| matches!(r, Repr::Number | Repr::Bool);
    let dest_scalar = dest.is_some_and(|d| scalar(repr(d)));
    match op {
        Op::ConstNil | Op::ConstBool(_) | Op::ConstNumber(_) | Op::LocalGet(_) => true,
        Op::Unary(..) | Op::Truthy(_) => true,
        Op::Binary(bop, ..) => *bop != BinOp::Concat,
        // Writes pass their operand as bits; nothing is interned.
        Op::ArraySet(..)
        | Op::MapSet(..)
        | Op::FieldSet(..)
        | Op::CellSet(..)
        | Op::GlobalSet(..) => true,
        // A scalar stored into a reference-typed local, or unwrapped into a reference, is boxed.
        Op::LocalSet(id, v) => {
            let into = func.locals.get(id).map_or(Repr::Ptr, repr_of);
            !(scalar(repr(*v)) && into == Repr::Ptr)
        }
        Op::Unwrap(v) => !scalar(repr(*v)) || dest_scalar,
        // A read or call whose result is a scalar returns it as bits.
        Op::TupleGet(..)
        | Op::ArrayGet(..)
        | Op::MapGet(..)
        | Op::FieldGet(..)
        | Op::CellGet(_)
        | Op::GlobalGet(_)
        | Op::MemoryRef(_)
        | Op::NamespaceField(..)
        | Op::CallHost(..)
        | Op::CallMethod(..)
        | Op::CallBuiltinValue(..)
        | Op::CallBuiltinMember(..) => dest_scalar,
        // Script calls, strings, and everything that builds a value.
        _ => false,
    }
}

/// The reference-typed locals live on entry to each block: read on some path from there before
/// being assigned.
fn live_handle_locals(func: &Function, succs: &[Vec<usize>]) -> Vec<BTreeSet<LocalId>> {
    let is_handle = |id: &LocalId| func.locals.get(id).map(repr_of) == Some(Repr::Ptr);
    let n = func.blocks.len();
    // Per block: the locals it reads before assigning, and the ones it assigns.
    let mut uses = vec![BTreeSet::new(); n];
    let mut defs = vec![BTreeSet::new(); n];
    for (b, block) in func.blocks.iter().enumerate() {
        for instr in &block.instrs {
            match instr.op {
                Op::LocalGet(id) if is_handle(&id) && !defs[b].contains(&id) => {
                    uses[b].insert(id);
                }
                Op::LocalSet(id, _) if is_handle(&id) => {
                    defs[b].insert(id);
                }
                _ => {}
            }
        }
    }
    let mut live_in = uses.clone();
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            let mut live: BTreeSet<LocalId> = succs[b]
                .iter()
                .flat_map(|&s| live_in[s].iter().copied())
                .filter(|id| !defs[b].contains(id))
                .collect();
            live.extend(uses[b].iter().copied());
            if live != live_in[b] {
                live_in[b] = live;
                changed = true;
            }
        }
    }
    live_in
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
