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
//
// A write through a constant-bound table meant three different things: the AST and VM cache the
// constant (so the write persisted across calls, which SPEC §1 forbids — the only cross-call
// state is host memory), while the JIT rebuilt it per read (so the write was lost).
//
// Resolved by closing the SPEC gap rather than standardizing on a SPEC violation: a `constdecl`
// is immutable all the way down, and writing through one is `E0307` at check time. See
// `const_identity_is_consistent` below for what that does *not* cover.

/// Writing through a constant is rejected — directly, nested, and through an index.
#[test]
fn const_table_write_is_rejected() {
    let cases = [
        "C = {x = 1}\nfunction f() C.x = 2 return C.x end\n",
        "C = {a = {b = 1}}\nfunction f() C.a.b = 2 return C.a.b end\n",
        "C = {arr = {1, 2}}\nfunction f() C.arr[1] = 9 return C.arr[1] end\n",
    ];
    for src in cases {
        let err = grindlang::analyze(src, &TypeConfig::default())
            .expect_err(&format!("must be rejected:\n{src}"));
        assert!(format!("{err:?}").contains("E0307"), "got {err:?}");
    }
}

/// Reading a constant stays legal — the rule is about writes, not about touching constants.
#[test]
fn const_table_read_is_allowed() {
    let src = "C = {x = 1}\nfunction f() return C.x + 1 end\n";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("read through a constant", &a, &b, &c);
    assert_eq!(c.expect("read").to_string(), "2");
}

/// A *local* that shadows a constant is an ordinary mutable table. The rejection resolves the
/// root through normal scoping, so it must not fire on the shadowing local — a false positive
/// here would reject correct code.
#[test]
fn a_local_shadowing_a_constant_is_still_writable() {
    let src = "\
C = {x = 1}
function f()
  local C = {x = 10}
  C.x = C.x + 1
  return C.x
end
";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("write through a shadowing local", &a, &b, &c);
    assert_eq!(c.expect("shadowed write").to_string(), "11");
}

/// Writes through *host memory* must stay legal — that is what memory is for (SPEC §7), and the
/// rejection above keys on the root binding precisely so it doesn't catch this.
#[test]
fn memory_field_write_is_still_allowed() {
    let src = "function f() mem.x = mem.x + 1 return mem.x end";
    let cfg = mem_config("x");
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let bind = || Value::table([("x".to_string(), Value::Number(1.0))].into());
    interp.set_memory("mem", bind());
    vm.set_memory("mem", bind());
    jit.set_memory("mem", bind());
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("memory field write", &a, &b, &c);
    assert_eq!(c.expect("memory write").to_string(), "2");
}

/// Reading an exported constant back was a three-way disagreement of its own: the AST
/// interpreter treated every export as callable and failed with "attempted to call a table
/// value", while the VM and JIT returned the value.
#[test]
fn exported_constant_reads_back_consistently() {
    let src = "C = {x = 1}\nfunction f() return C.x end\n";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let a = interp.call("C", vec![]);
    let b = vm.call("C", vec![]);
    let c = jit.call("C", vec![]);
    assert_agree("read exported constant", &a, &b, &c);
    assert_eq!(c.expect("read export").to_string(), "{x = 1}");
}

/// Reference values compare by identity, so a constant read twice in one call must *be* the
/// same value. The AST evaluated constants once at construction (`true`) while the VM and JIT
/// produced a fresh value per read (`false`) — a value not equal to itself.
///
/// Fixed by memoizing constants **per call** in all three backends, which is the lifetime that
/// satisfies both halves: stable identity within an invocation, nothing surviving between them.
#[test]
fn a_constant_is_equal_to_itself() {
    let src = "C = {x = 1}\nfunction f()\n  local a = C\n  local b = C\n  return a == b\nend\n";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("a constant compared with itself", &a, &b, &c);
    assert_eq!(c.expect("identity").to_string(), "true");
}

/// The same identity, reached through the two ways a constant is read: in-script, and as an
/// export. Both must observe the call's one memoized value.
#[test]
fn a_constant_read_as_an_export_matches_the_in_script_read() {
    let src = "C = {x = 1}\nfunction f() return C end\n";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    for (label, from_fn, from_export) in [
        ("AST", interp.call("f", vec![]), interp.call("C", vec![])),
        ("VM", vm.call("f", vec![]), vm.call("C", vec![])),
        ("JIT", jit.call("f", vec![]), jit.call("C", vec![])),
    ] {
        assert_eq!(
            outcome(&from_fn),
            outcome(&from_export),
            "{label}: reading `C` in-script and as an export disagreed"
        );
    }
}

/// Aliasing a constant into a local gets past the syntactic `E0307` check, which keys on the
/// *root* of the assignment target. The write is refused at runtime instead, so a constant is
/// immutable however it was reached — matching what `E0307`'s message and `SPEC.md` §3 claim.
#[test]
fn a_write_through_an_aliased_constant_is_refused() {
    let src = "\
C = {x = 1}
function f()
  local t = C
  t.x = t.x + 1
  return C.x
end
";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    // Repeated, because a per-call frozen set that leaked would change the verdict on call 2.
    for i in 0..3 {
        let a = interp.call("f", vec![]);
        let b = vm.call("f", vec![]);
        let c = jit.call("f", vec![]);
        assert_agree(&format!("aliased write, call {i}"), &a, &b, &c);
        let e = c.expect_err("writing through an aliased constant must fail");
        assert!(
            format!("{e}").contains("cannot modify a constant"),
            "call {i}: got {e:?}"
        );
    }
}

/// The freeze reaches *into* a constant, not just its outermost table: a nested table and an
/// array element are equally unwritable through an alias.
#[test]
fn a_write_into_a_nested_constant_is_refused() {
    for (label, src) in [
        (
            "nested table",
            "C = {a = {b = 1}}\nfunction f()\n  local t = C.a\n  t.b = 2\n  return C.a.b\nend\n",
        ),
        (
            "array element",
            "C = {arr = {1, 2}}\nfunction f()\n  local t = C.arr\n  t[1] = 9\n  return C.arr[1]\nend\n",
        ),
    ] {
        let cfg = TypeConfig::default();
        let (mut interp, mut vm, mut jit) = triple(src, &cfg);
        let a = interp.call("f", vec![]);
        let b = vm.call("f", vec![]);
        let c = jit.call("f", vec![]);
        assert_agree(label, &a, &b, &c);
        assert!(c.is_err(), "{label}: a nested constant must be unwritable");
    }
}

/// The freeze must not leak onto ordinary tables. A module that *has* a constant still writes
/// freely to everything else, including a table built with the same shape.
#[test]
fn freezing_a_constant_does_not_freeze_other_tables() {
    let src = "\
C = {x = 1}
function f()
  local t = {x = C.x}
  t.x = t.x + 1
  local arr = {1, 2}
  arr[1] = 9
  return t.x
end
";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("write to a normal table alongside a constant", &a, &b, &c);
    // Reaching the return at all proves the array write was allowed too.
    assert_eq!(c.expect("normal write").to_string(), "2");
}

/// Reference equality is by `Rc` identity in every backend. Found while fixing the constant
/// identity above: the IR VM's copy of the rule answered `false` for *every* reference pair, so
/// a table was not equal to itself. No constant is involved — the three backends each had their
/// own copy of the rule and one had drifted, and the differential corpus never compared two
/// reference values, so nothing caught it.
#[test]
fn a_table_is_equal_to_itself() {
    let src = "function f()\n  local t = {x = 1}\n  local u = t\n  return t == u\nend\n";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("a table compared with itself", &a, &b, &c);
    assert_eq!(c.expect("identity").to_string(), "true");
}

/// The other half of identity semantics: two structurally identical tables are still distinct
/// values. Guards against "fixing" the above by switching to structural equality.
#[test]
fn structurally_identical_tables_are_not_equal() {
    let src = "function f()\n  local a = {x = 1}\n  local b = {x = 1}\n  return a == b\nend\n";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("two equal-looking tables", &a, &b, &c);
    assert_eq!(c.expect("identity").to_string(), "false");
}

/// A scalar constant skips the cache entirely (it has no identity to keep stable). It must
/// still read correctly and identically everywhere.
#[test]
fn scalar_constants_still_read_correctly() {
    let src = "MAX = 99\nSCALE = 1.5\nfunction f() return MAX * SCALE end\n";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("scalar constants", &a, &b, &c);
    assert_eq!(c.expect("scalar const").to_string(), "148.5");
}

// ---- Finding 5: the host boundary must not coerce ill-typed values ---------
//
// The JIT trusted the declared types and unboxed without a runtime shape check, turning any
// host-supplied value that contradicted its declared type into `0` / `false`. It bites wherever
// the host hands over a value the signature or schema didn't promise, and the fix has two
// halves because the value survives to different points:
//
//   * a scalar *argument* is converted to raw bits at the boundary, so it must be checked
//     there (`encode_arg`) — nothing downstream can still tell a string from `0.0`;
//   * everything else (a memory binding, a host function's result) reaches compiled code as a
//     handle and is only forced into a scalar at the point of use (`rt_unbox_number`), which
//     is exactly where both interpreters check.

/// A memory bound off-schema (the schema declares a record of numbers, the host supplies `nil`)
/// used to read as a silent `0`. Found while fixing finding 3 — a different entry point from
/// the argument case, so checking `encode_arg` alone would not have covered it.
#[test]
fn ill_typed_memory_binding_is_an_error() {
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
    assert!(c.is_err(), "an off-schema memory binding must fail");
}

/// A host function that returns the wrong shape is caught at the same use site — its result
/// crosses as a handle and only becomes a scalar when the script does arithmetic on it.
#[test]
fn ill_typed_host_function_result_is_an_error() {
    let src = "function f() return tick() + 1 end";
    let cfg = tick_config();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    // Declared `tick() -> number`, but hands back a string.
    interp.set_host_function("tick", |_: &[Value]| Ok(Value::string("nope")));
    vm.set_host_function("tick", |_: &[Value]| Ok(Value::string("nope")));
    jit.set_host_function("tick", |_: &[Value]| Ok(Value::string("nope")));
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("host function returning a string", &a, &b, &c);
    assert!(c.is_err(), "an off-signature host result must fail");
}

/// The raw `call` path checked arity but not runtime shapes, so a string passed to a `number`
/// parameter was silently encoded as `0.0`. This signature is all-`number`, so it also covers
/// the direct-call fast path, which encodes arguments separately from the trampoline.
///
/// The interpreters happen to reach the same verdict here — they fail at the `n + 1` use site —
/// so this one can be compared across all three. That agreement is incidental, not the rule;
/// see `trampoline_call_rejects_mistyped_arguments`.
#[test]
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

/// The trampoline path (a signature that isn't all-`number`, so the direct fast path is not
/// taken) must reject the same way, and `bool` parameters are checked too, not just `number`.
///
/// Deliberately **not** an `assert_agree` comparison. Argument checking is a host-boundary rule
/// (`SPEC.md` §7.1), which the embedding API enforces and the interpreter oracles do not —
/// exactly as they already skip §7.1's exact-arity rule, padding missing arguments with `nil`
/// instead. Here the interpreters accept the number as a *truthy* value and return "yes", which
/// is Lua behavior for a value the type system says cannot occur. Comparing the two would be
/// comparing a boundary against something that isn't one.
#[test]
fn trampoline_call_rejects_mistyped_arguments() {
    let src = "function f(flag, s) if flag then return s end return \"no\" end";
    let cfg = TypeConfig::default();
    let jit = &mut triple(src, &cfg).2;

    // Sanity: the well-typed call works, so the rejection below is about the argument's shape.
    let ok = jit
        .call("f", vec![Value::Bool(true), Value::string("yes")])
        .expect("well-typed call");
    assert_eq!(ok.to_string(), "yes");

    let r = jit.call("f", vec![Value::Number(1.0), Value::string("yes")]);
    let e = r.expect_err("a number passed to a bool parameter must be rejected");
    assert!(
        format!("{e}").contains("expected a bool argument"),
        "got {e:?}"
    );
}

/// A parameter can only carry a scalar type because the body *uses* it as one — the checker
/// rejects a parameter whose type nothing determines (`E0410`). So "the host passed the wrong
/// type but the body never touches it" is not a reachable state, which is what keeps the
/// boundary check above from diverging from the interpreters in the common case.
#[test]
fn a_parameter_type_is_always_justified_by_use() {
    let err = grindlang::analyze("function f(n) return 7 end", &TypeConfig::default())
        .expect_err("an undetermined parameter type must be rejected");
    assert!(format!("{err:?}").contains("E0410"), "got {err:?}");
}

// ---- Found reviewing the fixes above --------------------------------------

/// `SPEC.md` §7.1's *other* boundary rule. Exact arity was enforced by the JIT alone: both
/// interpreters padded a missing argument with `nil` and dropped surplus ones, so a call the
/// JIT rejected returned a value from the oracles. Found while reviewing the finding-5 fix,
/// which added the argument-*type* rule one bullet below this one in the same list.
#[test]
fn surplus_arguments_are_rejected_by_every_backend() {
    let src = "C = {x = 1}\nN = 7\nfunction f() return 1 end\nfunction g(n) return n + 1 end\n";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    // A function taking none, a function taking one, and both flavors of constant export.
    for (export, args) in [
        ("f", vec![Value::Number(1.0)]),
        ("g", vec![Value::Number(1.0), Value::Number(2.0)]),
        ("g", vec![]),
        ("C", vec![Value::Number(1.0)]),
        ("N", vec![Value::Number(1.0)]),
    ] {
        let a = interp.call(export, args.clone());
        let b = vm.call(export, args.clone());
        let c = jit.call(export, args.clone());
        let label = format!("`{export}` with {} argument(s)", args.len());
        assert_agree(&label, &a, &b, &c);
        assert!(c.is_err(), "{label}: wrong arity must be a call error");
    }
}

/// The arity rule must not fire on a *correct* call — the point is to reject a mismatch, not to
/// make the boundary stricter than the export's own signature.
#[test]
fn exact_arity_calls_still_succeed() {
    let src = "C = {x = 1}\nfunction f() return 1 end\nfunction g(n) return n + 1 end\n";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    for (export, args, want) in [
        ("f", vec![], "1"),
        ("g", vec![Value::Number(1.0)], "2"),
        ("C", vec![], "{x = 1}"),
    ] {
        let a = interp.call(export, args.clone());
        let b = vm.call(export, args.clone());
        let c = jit.call(export, args);
        assert_agree(export, &a, &b, &c);
        assert_eq!(c.expect("correct arity").to_string(), want);
    }
}
