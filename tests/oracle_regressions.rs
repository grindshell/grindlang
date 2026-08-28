//! Regression tests for oracle divergences found by code review.
//!
//! These complement [`jit_differential`](../jit_differential.rs), which compares **return
//! values** over a curated corpus. The divergences here escape that net because they are about
//! *side effects*, *cross-module identity*, *missing bindings*, and *ill-typed host arguments* —
//! none of which a return-value comparison observes.
//!
//! Tests for findings that are not yet fixed are `#[ignore]`d with the finding they pin, so the
//! default bar stays green while the expected behavior stays written down. Run them with
//! `cargo test --features interp --test oracle_regressions -- --ignored`.

#![cfg(all(feature = "interp", feature = "jit"))]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use grindlang::codegen::JitModule;
use grindlang::interp::Interpreter;
use grindlang::ir::Vm;
use grindlang::types::FnType;
use grindlang::{RunError, Type, TypeConfig, Value};

/// Build all three executors for a program. Artifacts are leaked for `'static` — fine for a
/// test harness, and identical to what `jit_differential` does.
fn triple(src: &str, cfg: &TypeConfig) -> (Interpreter<'static>, Vm<'static>, JitModule) {
    let (module, res, info) = grindlang::analyze(src, cfg).expect("analyze");
    let module = Box::leak(Box::new(module));
    let res = Box::leak(Box::new(res));
    let info = Box::leak(Box::new(info));
    let program = Box::leak(Box::new(
        grindlang::ir::lower(module, res, info, cfg).expect("lower"),
    ));
    grindlang::ir::verify(program).expect("verify");
    (
        Interpreter::new(module, res).expect("interp"),
        Vm::new(program),
        JitModule::compile(program).expect("jit compile"),
    )
}

/// Compile just the JIT backend (for the cross-module tests, which need two of them).
fn jit_only(src: &str, cfg: &TypeConfig) -> JitModule {
    triple(src, cfg).2
}

/// The comparable shape of one call's outcome. Error *messages* differ by backend by design, so
/// only success/failure and the success value (by `Display`, as `jit_differential` does) are
/// compared.
fn outcome(r: &Result<Value, RunError>) -> Result<String, ()> {
    r.as_ref().map(|v| v.to_string()).map_err(|_| ())
}

/// Assert all three oracles agree on a call's outcome.
#[track_caller]
fn assert_agree(
    label: &str,
    ast: &Result<Value, RunError>,
    vm: &Result<Value, RunError>,
    jit: &Result<Value, RunError>,
) {
    let (a, b, c) = (outcome(ast), outcome(vm), outcome(jit));
    assert_eq!(a, b, "{label}: AST vs VM disagree ({ast:?} / {vm:?})");
    assert_eq!(b, c, "{label}: VM vs JIT disagree ({vm:?} / {jit:?})");
}

/// A host function config declaring `tick() -> number`.
fn tick_config() -> TypeConfig {
    let mut cfg = TypeConfig::default();
    cfg.host_functions.insert(
        "tick".to_string(),
        FnType {
            params: vec![],
            ret: Box::new(Type::Number),
        },
    );
    cfg
}

/// A memory config declaring `mem` as a record with one number field.
fn mem_config(field: &str) -> TypeConfig {
    let mut cfg = TypeConfig::default();
    let mut rec = BTreeMap::new();
    rec.insert(field.to_string(), Type::Number);
    cfg.memory.insert("mem".to_string(), Type::Record(rec));
    cfg
}

/// A counting host function paired with the handle to read its count back.
type Counted = (
    Rc<RefCell<u32>>,
    Box<dyn Fn(&[Value]) -> Result<Value, RunError>>,
);

/// A host function that records how many times it ran.
fn counter() -> Counted {
    let cell = Rc::new(RefCell::new(0u32));
    let c = cell.clone();
    (
        cell,
        Box::new(move |_: &[Value]| {
            *c.borrow_mut() += 1;
            Ok(Value::Number(0.0))
        }),
    )
}

// ---- Finding 1: a latched runtime error must stop subsequent effects --------

/// A failing op must abort the rest of the function. Before the fix the JIT latched the error
/// but kept running, so the host function fired on a call the interpreters aborted.
#[test]
fn error_aborts_later_host_calls() {
    let src = "\
function f(i)
  local xs = {1, 2, 3}
  xs[i] = 9
  tick()
  return 0
end
";
    let cfg = tick_config();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let (ast_n, ast_fn) = counter();
    let (vm_n, vm_fn) = counter();
    let (jit_n, jit_fn) = counter();
    interp.set_host_function("tick", ast_fn);
    vm.set_host_function("tick", vm_fn);
    jit.set_host_function("tick", jit_fn);

    // Index 99 is out of range: every backend must fail here and never reach `tick()`.
    let args = vec![Value::Number(99.0)];
    let a = interp.call("f", args.clone());
    let b = vm.call("f", args.clone());
    let c = jit.call("f", args);
    assert_agree("out-of-range array set", &a, &b, &c);
    assert!(c.is_err(), "expected the erroring call to fail");
    assert_eq!(
        (*ast_n.borrow(), *vm_n.borrow(), *jit_n.borrow()),
        (0, 0, 0),
        "host function ran after a latched error (AST/VM/JIT)"
    );
}

/// The same abort must hold for writes to host memory, which — unlike a host call — persist
/// across invocations, so an escaped write leaves durable divergence.
#[test]
fn error_aborts_later_memory_writes() {
    let src = "\
function f(i)
  local xs = {1, 2, 3}
  xs[i] = 9
  mem.hits = mem.hits + 1
  return 0
end
";
    let cfg = mem_config("hits");
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let fresh = || Value::table([("hits".to_string(), Value::Number(0.0))].into());
    interp.set_memory("mem", fresh());
    vm.set_memory("mem", fresh());
    jit.set_memory("mem", fresh());

    let args = vec![Value::Number(99.0)];
    let a = interp.call("f", args.clone());
    let b = vm.call("f", args.clone());
    let c = jit.call("f", args);
    assert_agree("out-of-range array set", &a, &b, &c);

    let (ma, mb, mc) = (
        interp.memory("mem").map(|v| v.to_string()),
        vm.memory("mem").map(|v| v.to_string()),
        jit.memory("mem").map(|v| v.to_string()),
    );
    assert_eq!(ma, mb, "AST vs VM memory diverged after an errored call");
    assert_eq!(mb, mc, "VM vs JIT memory diverged after an errored call");
    assert_eq!(
        mc,
        Some("{hits = 0}".to_string()),
        "memory was written after a latched error"
    );
}

/// An error raised inside a loop must exit the loop rather than run further iterations. This
/// pins the behavior the loop back-edge check used to provide on its own.
#[test]
fn error_inside_a_loop_stops_iterating() {
    let src = "\
function f()
  local xs = {1, 2, 3}
  local total = 0
  for i = 1, 5 do
    xs[i + 10] = 1
    tick()
    total = total + 1
  end
  return total
end
";
    let cfg = tick_config();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let (ast_n, ast_fn) = counter();
    let (vm_n, vm_fn) = counter();
    let (jit_n, jit_fn) = counter();
    interp.set_host_function("tick", ast_fn);
    vm.set_host_function("tick", vm_fn);
    jit.set_host_function("tick", jit_fn);

    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("erroring loop", &a, &b, &c);
    assert!(c.is_err(), "expected the erroring loop to fail");
    assert_eq!(
        (*ast_n.borrow(), *vm_n.borrow(), *jit_n.borrow()),
        (0, 0, 0),
        "loop kept running after a latched error (AST/VM/JIT)"
    );
}

/// An indirect call whose closure name does not resolve in this module latches an `Internal`
/// error and yields address 0. Without a guard the generated code jumps to null and faults;
/// with one it returns `Err`. (Reaching an unresolvable closure at all is finding 2 — this test
/// only pins that the failure is an error rather than a crash.)
#[test]
fn unresolvable_indirect_call_errors_instead_of_faulting() {
    // `producer` mints the lifted name `producer$c0`, which `consumer` below does not define.
    let producer_src = "\
function producer(n)
  local function add(x) return x + n end
  return add
end
";
    let consumer_src = "\
function apply(x)
  local f = borrow()
  return f(x)
end
";
    let mut consumer_cfg = TypeConfig::default();
    consumer_cfg.host_functions.insert(
        "borrow".to_string(),
        FnType {
            params: vec![],
            ret: Box::new(Type::Function(FnType {
                params: vec![Type::Number],
                ret: Box::new(Type::Number),
            })),
        },
    );

    let mut producer = jit_only(producer_src, &TypeConfig::default());
    let mut consumer = jit_only(consumer_src, &consumer_cfg);

    let foreign = producer
        .call("producer", vec![Value::Number(3.0)])
        .expect("producer");
    consumer.set_host_function("borrow", move |_: &[Value]| Ok(foreign.clone()));

    let r = consumer.call("apply", vec![Value::Number(10.0)]);
    assert!(
        r.is_err(),
        "an unresolvable closure must error, not jump to address 0; got {r:?}"
    );
}

// ---- Finding 2: a closure is bound to the module that built it -------------
//
// A closure can only run inside its origin: its lifted name (`make$c0`) collides freely across
// modules, and its compiled body reads that module's constant pools by baked-in id. Both
// invocation paths used to resolve the name through the *receiving* module, silently running an
// unrelated function. Both now reject a foreign closure.

/// The host `call_value` path. Module B defines a same-named closure with different behavior,
/// so a name-based resolution returns a plausible wrong answer rather than failing loudly.
#[test]
fn foreign_closure_rejected_by_call_value() {
    let a_src = "\
function make(n)
  local function op(x) return x + n end
  return op
end
";
    let b_src = "\
function make(n)
  local function op(x) return x - n end
  return op
end
";
    let cfg = TypeConfig::default();
    let mut a = jit_only(a_src, &cfg);
    let mut b = jit_only(b_src, &cfg);

    let clo = a.call("make", vec![Value::Number(3.0)]).expect("make");
    let via_a = a
        .call_value(clo.clone(), vec![Value::Number(10.0)])
        .expect("origin module invokes its own closure");
    assert_eq!(via_a.to_string(), "13");

    let via_b = b
        .call_value(clo, vec![Value::Number(10.0)])
        .expect_err("module B must reject A's closure");
    assert!(
        format!("{via_b}").contains("different compiled module"),
        "got {via_b:?}"
    );
}

/// The in-script indirect-call path, reached when a host function hands a foreign closure to a
/// script that calls it. This one bypassed even the arity check, going straight to
/// `call_indirect` on whatever address the name resolved to.
#[test]
fn foreign_closure_rejected_by_script_call() {
    let a_src = "\
function make(n)
  local function op(x) return x + n end
  return op
end
";
    let b_src = "\
function make(n)
  local function op(x) return x - n end
  return op
end

function apply(x)
  local f = borrow()
  return f(x)
end
";
    let mut b_cfg = TypeConfig::default();
    b_cfg.host_functions.insert(
        "borrow".to_string(),
        FnType {
            params: vec![],
            ret: Box::new(Type::Function(FnType {
                params: vec![Type::Number],
                ret: Box::new(Type::Number),
            })),
        },
    );

    let mut a = jit_only(a_src, &TypeConfig::default());
    let mut b = jit_only(b_src, &b_cfg);

    let clo = a.call("make", vec![Value::Number(3.0)]).expect("make");
    b.set_host_function("borrow", move |_: &[Value]| Ok(clo.clone()));

    let via_b = b
        .call("apply", vec![Value::Number(10.0)])
        .expect_err("B's script must reject A's closure");
    assert!(
        format!("{via_b}").contains("different compiled module"),
        "got {via_b:?}"
    );
}

/// A closure produced by the IR VM carries no JIT origin stamp at all, so the JIT must refuse
/// it rather than resolving its name against whatever it happens to have compiled.
#[test]
fn vm_closure_rejected_by_jit() {
    let src = "\
function make(n)
  local function op(x) return x + n end
  return op
end
";
    let cfg = TypeConfig::default();
    let (_, mut vm, mut jit) = triple(src, &cfg);
    let from_vm = vm.call("make", vec![Value::Number(3.0)]).expect("vm make");
    let r = jit
        .call_value(from_vm, vec![Value::Number(10.0)])
        .expect_err("the JIT must reject a VM-built closure");
    assert!(
        format!("{r}").contains("different compiled module"),
        "got {r:?}"
    );
}

// ---- Finding 3: a missing memory binding must be an error, not nil ---------

/// Reading a declared-but-unbound memory must fail on every backend. The JIT used to resolve a
/// missing binding to `Value::Nil`, which made `rt_memory_ref`'s not-provided error unreachable
/// and turned `mem.x + 1` into a silently valid `1`.
#[test]
fn missing_memory_binding_is_an_error() {
    let src = "function f() return mem.x + 1 end";
    let cfg = mem_config("x");
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    // Deliberately bind nothing.
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("unbound memory read", &a, &b, &c);
    assert!(c.is_err(), "reading an unbound memory must fail");
}

/// Binding a memory *to* `nil` is not the same as leaving it unbound: the host did provide a
/// binding, so whatever happens next, it must not be reported as missing. Pins that the
/// `Option` distinguishing the two cases isn't collapsed back to a `Value::Nil` sentinel.
///
/// (Binding `nil` where the schema declares a record is itself ill-typed, and the backends
/// disagree on what that *does* — see `ill_typed_memory_binding_is_rejected_or_consistent`.
/// This test deliberately asserts only the provided-vs-missing distinction.)
#[test]
fn memory_bound_to_nil_is_not_reported_as_missing() {
    let src = "function f() return mem.x + 1 end";
    let cfg = mem_config("x");
    let (_, _, mut jit) = triple(src, &cfg);

    let unbound = jit.call("f", vec![]).expect_err("unbound read must fail");
    assert!(
        format!("{unbound}").contains("was not provided"),
        "got {unbound:?}"
    );

    jit.set_memory("mem", Value::Nil);
    if let Err(e) = jit.call("f", vec![]) {
        assert!(
            !format!("{e}").contains("was not provided"),
            "an explicitly bound memory must not read as missing; got {e:?}"
        );
    }
}

/// The same declared-but-not-provided rule for a host function (SPEC §7): calling one that was
/// never registered must fail on every backend rather than returning a default.
#[test]
fn unregistered_host_function_is_an_error() {
    let src = "function f() return tick() end";
    let cfg = tick_config();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    // Deliberately register nothing.
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("unregistered host function", &a, &b, &c);
    assert!(
        c.is_err(),
        "calling an unregistered host function must fail"
    );
}

/// And for a declared memory method (SPEC §7.2).
#[test]
fn unregistered_memory_method_is_an_error() {
    let src = "function f() return mem:bump(1) end";
    let mut cfg = mem_config("x");
    let mut methods = BTreeMap::new();
    methods.insert(
        "bump".to_string(),
        FnType {
            params: vec![Type::Number],
            ret: Box::new(Type::Number),
        },
    );
    cfg.methods.insert("mem".to_string(), methods);

    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let bind = || Value::table([("x".to_string(), Value::Number(0.0))].into());
    interp.set_memory("mem", bind());
    vm.set_memory("mem", bind());
    jit.set_memory("mem", bind());
    // The memory is bound; the method is not registered.
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("unregistered memory method", &a, &b, &c);
    assert!(c.is_err(), "calling an unregistered method must fail");
}

/// Once a binding is provided, reads work — and a later `set_memory` is picked up, so the
/// not-provided path can't be papered over by a stale resolved binding.
#[test]
fn memory_binding_is_observed_after_being_set() {
    let src = "function f() return mem.x + 1 end";
    let cfg = mem_config("x");
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    assert!(jit.call("f", vec![]).is_err(), "unbound read must fail");

    let bind = || Value::table([("x".to_string(), Value::Number(41.0))].into());
    interp.set_memory("mem", bind());
    vm.set_memory("mem", bind());
    jit.set_memory("mem", bind());

    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("bound memory read", &a, &b, &c);
    assert_eq!(c.expect("bound read").to_string(), "42");
}

// ---- Finding 4: constants must have consistent identity across oracles -----

/// A write through a constant-bound table currently means three different things: the AST and
/// VM cache the constant (so the write persists across calls, which SPEC §1 forbids — the only
/// cross-call state is host memory), while the JIT rebuilds it per read (so the write is lost).
///
/// The test accepts either resolution: rejecting the write at check time (the preferred fix,
/// since it closes the SPEC gap rather than standardizing on a SPEC violation), or all three
/// backends agreeing at runtime.
#[test]
#[ignore = "finding 4: SPEC is silent on composite-constant mutability; oracles disagree"]
fn const_table_mutation_is_rejected_or_consistent() {
    let src = "\
C = {x = 1}
function f()
  C.x = C.x + 1
  return C.x
end
";
    let cfg = TypeConfig::default();
    if grindlang::analyze(src, &cfg).is_err() {
        return; // rejected at check time — the SPEC gap is closed
    }
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    for i in 0..3 {
        let a = interp.call("f", vec![]);
        let b = vm.call("f", vec![]);
        let c = jit.call("f", vec![]);
        assert_agree(&format!("const mutation, call {i}"), &a, &b, &c);
    }
}

/// Reading an exported constant back is a three-way disagreement of its own: the AST
/// interpreter cannot dispatch a non-function export at all.
#[test]
#[ignore = "finding 4: Interpreter::call rejects a non-function export"]
fn exported_constant_reads_back_consistently() {
    let src = "C = {x = 1}\nfunction f() return C.x end\n";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let a = interp.call("C", vec![]);
    let b = vm.call("C", vec![]);
    let c = jit.call("C", vec![]);
    assert_agree("read exported constant", &a, &b, &c);
}

// ---- Finding 5: the host boundary must not coerce ill-typed values ---------
//
// The JIT trusts the declared types and unboxes without a runtime shape check, while both
// interpreters check dynamically. That shows up wherever the host supplies a value the
// signature or schema didn't promise — call arguments *and* memory bindings.

/// A memory bound with the wrong shape (the schema declares a record, the host supplies `nil`)
/// reads as a silent `0` in the JIT but errors in both interpreters. Found while fixing
/// finding 3; the same root cause as the argument case below, but a different entry point, so
/// checking `encode_arg` alone would not fix it.
///
/// Accepts either resolution: `set_memory` rejecting the ill-typed binding up front, or all
/// three backends agreeing at runtime.
#[test]
#[ignore = "finding 5: set_memory accepts a binding that violates the declared schema"]
fn ill_typed_memory_binding_is_rejected_or_consistent() {
    let src = "function f() return mem.x + 1 end";
    let cfg = mem_config("x");
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    interp.set_memory("mem", Value::Nil);
    vm.set_memory("mem", Value::Nil);
    jit.set_memory("mem", Value::Nil);
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("memory bound to nil", &a, &b, &c);
}

/// The raw `call` path checks arity but not runtime shapes, so a string passed to a
/// `number` parameter is silently encoded as `0.0` instead of failing.
#[test]
#[ignore = "finding 5: encode_arg coerces a non-number to 0.0 instead of erroring"]
fn raw_call_rejects_mistyped_arguments() {
    let src = "function f(n) return n + 1 end";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let arg = Value::string("hello");
    let a = interp.call("f", vec![arg.clone()]);
    let b = vm.call("f", vec![arg.clone()]);
    let c = jit.call("f", vec![arg]);
    assert_agree("string passed to a number parameter", &a, &b, &c);
    assert!(c.is_err(), "a mistyped argument must be rejected");
}
