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

/// A host function that records how many times it ran, returning `0`.
fn counter() -> Counted {
    counter_returning(Value::Number(0.0))
}

/// A host function that records how many times it ran, returning `ret`.
fn counter_returning(ret: Value) -> Counted {
    let cell = Rc::new(RefCell::new(0u32));
    let c = cell.clone();
    (
        cell,
        Box::new(move |_: &[Value]| {
            *c.borrow_mut() += 1;
            Ok(ret.clone())
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

// ---- A script calls a closure only through the signature it was compiled with -
//
// The JIT calls a closure value through a native signature fixed at the call site, from the
// callee's static type. The checker keeps that type and the closure's own in step, but a closure
// that comes from the host has only the host's word for its type: one taken from another export
// could have any signature. Calling it through the wrong one passed arguments where the function
// didn't read them and read a result where it hadn't written one — undefined behavior, which in
// practice returned garbage. The JIT now checks, and fails the call.
//
// JIT-only, like `trampoline_call_rejects_mistyped_arguments`: the interpreters have no native
// signature to get wrong, and call a closure with whatever they are given.

const CLOSURES: &str = "\
function make_add(n) return function(x) return x + n end end
function make_shout() return function(s) return s .. \"!\" end end
function make_show() return function(x) return tostring(x + 0) end end
function make_flag() return function(b) if b then return 1 end return 0 end end
function via_host(x)
  local f = borrow()
  return f(x) + 1
end
function via_memory(x)
  local f = mem.f
  return f(x) + 1
end
function helper(s, x)
  if s == \"a\" then return 0 end
  return x * 100
end
";

/// [`CLOSURES`] compiled for the JIT. `borrow()` and `mem.f` are both declared
/// `fn(number) -> number`, and both hand out whatever closure is lent to them.
struct Closures {
    jit: JitModule,
    lent: Rc<RefCell<Value>>,
}

impl Closures {
    fn new() -> Self {
        let fn_type = Type::Function(FnType {
            params: vec![Type::Number],
            ret: Box::new(Type::Number),
        });
        let mut cfg = TypeConfig::default();
        cfg.host_functions.insert(
            "borrow".to_string(),
            FnType {
                params: vec![],
                ret: Box::new(fn_type.clone()),
            },
        );
        let mut rec = BTreeMap::new();
        rec.insert("f".to_string(), fn_type);
        cfg.memory.insert("mem".to_string(), Type::Record(rec));

        let mut jit = jit_only(CLOSURES, &cfg);
        let lent = Rc::new(RefCell::new(Value::Nil));
        let from = lent.clone();
        jit.set_host_function("borrow", move |_: &[Value]| Ok(from.borrow().clone()));
        Closures { jit, lent }
    }

    /// Call a script that expects a `fn(number) -> number` with `closure`, by each route the host
    /// can hand it one: a host function's result, and a memory field.
    fn apply(&mut self, closure: Value) -> [Result<Value, RunError>; 2] {
        *self.lent.borrow_mut() = closure.clone();
        self.jit
            .set_memory("mem", Value::table([("f".to_string(), closure)].into()));
        let x = || vec![Value::Number(2.0)];
        [
            self.jit.call("via_host", x()),
            self.jit.call("via_memory", x()),
        ]
    }
}

/// A parameter of another type (`string`, `bool`), and a result of another type (`string`).
#[test]
fn a_closure_of_another_signature_is_not_called() {
    let mut c = Closures::new();
    for make in ["make_shout", "make_flag", "make_show"] {
        let closure = c.jit.call(make, vec![]).expect(make);
        for r in c.apply(closure) {
            let e = r.expect_err(make);
            assert!(
                e.to_string()
                    .contains("cannot be called as `fn(number) -> number`"),
                "{make}: got {e}"
            );
        }
    }
}

/// The check compares signatures, not identities: another closure compiled for the same one is
/// still called.
#[test]
fn a_closure_of_the_expected_signature_is_called() {
    let mut c = Closures::new();
    let closure = c
        .jit
        .call("make_add", vec![Value::Number(3.0)])
        .expect("make_add");
    for r in c.apply(closure) {
        assert_eq!(r.expect("a matching closure runs").to_string(), "6");
    }
}

/// The check must never fire on a closure the script made itself, whatever its signature: the
/// checker gives the closure and every call of it the same type.
#[test]
fn script_closures_of_every_signature_are_called() {
    let src = "\
function run(n)
  local flag = function(b) if b then return 1 end return 0 end
  local shout = function(s) return s .. \"!\" end
  local maybe = function(x) if x > 0 then return x end return nil end
  local pair = function(x) return x, x * 2 end
  local make = function(k) return function(x) return x + k end end
  local t = {above = function(x) return x > 1 end}
  local a, b = pair(n)
  local s = flag(true) + a + b + make(1)(n)
  local m = maybe(n)
  if m ~= nil then s = s + m end
  if t.above(n) then s = s + 1 end
  return shout(tostring(s))
end
";
    let mut all = triple(src, &TypeConfig::default());
    assert_eq!(call_all(&mut all, "run", 2.0).unwrap().to_string(), "13!");
}

/// A closure the host assembles itself, naming a top-level function rather than a closure's
/// lifted body. `helper(s, x)` even has the native signature a `fn(number) -> number` closure
/// lowers to (the env, then a number), so only the name gives it away — and running it would pass
/// the closure itself as `s`.
#[test]
fn a_forged_closure_naming_a_top_level_function_is_not_called() {
    let mut c = Closures::new();
    let real = c
        .jit
        .call("make_add", vec![Value::Number(3.0)])
        .expect("make_add");
    let Value::Closure(real) = real else {
        panic!("make_add returns a closure")
    };
    let forged = Value::Closure(Rc::new(grindlang::value::ClosureObj {
        code: "helper".to_string(),
        env: vec![],
        origin: real.origin.clone(),
    }));
    for r in c.apply(forged) {
        assert!(r.is_err(), "ran a top-level function as a closure: {r:?}");
    }
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
// is immutable all the way down, and writing through one is `E0307` at check time. Constants are
// now built once, by the module's top-level code, and frozen with everything else it builds
// (SPEC §3.1), so they have one identity for the module's lifetime.

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
/// First fixed by memoizing constants per call; now every backend builds a constant once, when
/// the module initializes, and freezes it, so nothing about it survives between calls to change.
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
/// export. Both must observe the module's one value.
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
    // Repeated, because a frozen set that a call could lose would change the verdict on call 2.
    for i in 0..3 {
        let a = interp.call("f", vec![]);
        let b = vm.call("f", vec![]);
        let c = jit.call("f", vec![]);
        assert_agree(&format!("aliased write, call {i}"), &a, &b, &c);
        let e = c.expect_err("writing through an aliased constant must fail");
        assert!(
            format!("{e}").contains("cannot modify a frozen module value"),
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

/// A scalar constant has no identity to keep stable, but must still read correctly and
/// identically everywhere.
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
//   * everything else (a memory binding, a host function's result) is checked where the
//     runtime first decodes it into a scalar for compiled code — the shim that reads or
//     returns it, or `rt_unbox_number` for a narrowed optional.

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

/// A host function that returns the wrong shape is caught too — its declared `number` result is
/// decoded to a native scalar as the call returns, and that decode checks the shape.
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

/// The `bool` half of the rule above. A scalar result is decoded inside the shim that produced
/// it, with one mismatch branch per scalar kind, so `number` passing says nothing about `bool`.
///
/// JIT-only, for the reason `trampoline_call_rejects_mistyped_arguments` gives: the interpreters
/// treat any value as a condition by Lua truthiness, so they return 1 for a result the type
/// system says cannot occur.
#[test]
fn ill_typed_bool_host_result_is_an_error_in_the_jit() {
    let src = "function f() if ready() then return 1 end return 0 end";
    let mut cfg = TypeConfig::default();
    cfg.host_functions.insert(
        "ready".to_string(),
        FnType {
            params: vec![],
            ret: Box::new(Type::Bool),
        },
    );
    let jit = &mut triple(src, &cfg).2;
    jit.set_host_function("ready", |_: &[Value]| Ok(Value::Number(1.0)));
    let e = jit
        .call("f", vec![])
        .expect_err("a number where `bool` is declared must be rejected");
    assert!(format!("{e}").contains("expected a bool"), "got {e:?}");
}

/// Scalar results cross the runtime boundary as raw bits, and a NaN can have *any* bit pattern —
/// including all ones, which is the handle-level error sentinel `ERR`. A scalar result must
/// therefore be checked through the errored flag, never compared against `ERR`, or this legal
/// return value would be reported as a failure.
#[test]
fn nan_with_the_error_sentinels_bits_is_an_ordinary_number() {
    let src = "function f() return tick() + 1 end";
    let cfg = tick_config();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let all_ones = || Ok(Value::Number(f64::from_bits(u64::MAX)));
    interp.set_host_function("tick", move |_: &[Value]| all_ones());
    vm.set_host_function("tick", move |_: &[Value]| all_ones());
    jit.set_host_function("tick", move |_: &[Value]| all_ones());
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("host function returning an all-ones NaN", &a, &b, &c);
    assert!(
        c.expect("a NaN is a valid number")
            .as_f64()
            .unwrap()
            .is_nan()
    );
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

/// A map write goes through a *different* shim from a record-field write (`rt_map_set`, not
/// `rt_field_set`), with its own constant check. Every other alias test writes a field or an
/// array element, so nothing pinned this path.
#[test]
fn a_map_key_write_through_an_aliased_constant_is_refused() {
    let src = "\
C = { [\"a\"] = 1 }
function f()
  local t = C
  t[\"a\"] = 2
  return 0
end
";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("map-key write through an alias", &a, &b, &c);
    let e = c.expect_err("a map constant must be unwritable through an alias");
    assert!(
        format!("{e}").contains("cannot modify a frozen module value"),
        "got {e:?}"
    );
}

/// Reading a map constant stays legal, so the check above isn't rejecting the whole type.
#[test]
fn a_map_constant_is_readable() {
    let src = "C = { [\"a\"] = 1 }\nfunction f()\n  local t = C\n  return t[\"a\"]\nend\n";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("map constant read", &a, &b, &c);
    assert_eq!(c.expect("map read").to_string(), "1");
}

/// The freeze belongs to the *module*, not to the function that read the constant: a callee's
/// read must still forbid the caller's write. Nothing else pins that boundary — every other alias
/// test reads and writes inside one function.
#[test]
fn a_constant_read_in_a_callee_is_frozen_for_the_caller() {
    let src = "\
C = {x = 1}
function g() return C end
function f()
  local t = g()
  t.x = 2
  return t.x
end
";
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let a = interp.call("f", vec![]);
    let b = vm.call("f", vec![]);
    let c = jit.call("f", vec![]);
    assert_agree("write to a constant returned by a callee", &a, &b, &c);
    let e = c.expect_err("a constant returned by a callee must stay unwritable");
    assert!(
        format!("{e}").contains("cannot modify a frozen module value"),
        "got {e:?}"
    );
}

// ---- Runaway recursion must be an error, not a crashed host ---------------
//
// Every backend runs a script call on the native stack of the calling thread, so unbounded
// recursion overflowed it and aborted the whole process. A call-depth limit (`SPEC.md` §7.3)
// turns that into a runtime error, raised at the same call in all three backends. These tests
// lower the limit: the interpreter oracles take several KiB of stack per call in a debug build,
// too much to reach the default on a test thread.

/// All three executors, each limited to `depth` script calls at once.
fn triple_with_depth(src: &str, depth: u32) -> (Interpreter<'static>, Vm<'static>, JitModule) {
    let (mut interp, mut vm, mut jit) = triple(src, &TypeConfig::default());
    interp.set_max_call_depth(depth);
    vm.set_max_call_depth(depth);
    jit.set_max_call_depth(depth);
    (interp, vm, jit)
}

/// Call `name(n)` on all three executors, assert they agree, and return the JIT's outcome.
fn call_all(
    (interp, vm, jit): &mut (Interpreter<'static>, Vm<'static>, JitModule),
    name: &str,
    n: f64,
) -> Result<Value, RunError> {
    let args = || vec![Value::Number(n)];
    let (a, b, c) = (
        interp.call(name, args()),
        vm.call(name, args()),
        jit.call(name, args()),
    );
    assert_agree(&format!("{name}({n})"), &a, &b, &c);
    if let (Err(a), Err(b), Err(c)) = (&a, &b, &c) {
        let (a, b, c) = (a.to_string(), b.to_string(), c.to_string());
        assert!(
            a == b && b == c,
            "{name}({n}): errors differ: {a} / {b} / {c}"
        );
    }
    c
}

#[track_caller]
fn assert_too_deep(r: Result<Value, RunError>, limit: u32) {
    let e = r.expect_err("the call should exceed the call-depth limit");
    assert_eq!(
        e.to_string(),
        grindlang::value::call_depth_exceeded(limit).to_string()
    );
}

/// The limit counts the calls active at once, the host's own call included, identically in
/// every backend: a chain exactly `limit` deep runs and one more fails.
#[test]
fn the_call_depth_limit_is_exact_in_every_backend() {
    // `down(n)` is n + 1 nested calls.
    let src = "function down(n) if n == 0 then return 0 end return 1 + down(n - 1) end";
    let mut all = triple_with_depth(src, 50);
    assert_eq!(call_all(&mut all, "down", 49.0).unwrap().to_string(), "49");
    assert_too_deep(call_all(&mut all, "down", 50.0), 50);
    // The failed call leaves nothing behind: the next one starts with the full limit again.
    assert_eq!(call_all(&mut all, "down", 49.0).unwrap().to_string(), "49");
}

/// Recursion through a local function (a closure, called indirectly) counts the same way.
#[test]
fn recursion_through_a_closure_is_limited_too() {
    let src = "\
function outer(n)
  local function go(k)
    if k == 0 then return 0 end
    return 1 + go(k - 1)
  end
  return go(n)
end
";
    let mut all = triple_with_depth(src, 50);
    // `outer` plus n + 1 calls of `go`.
    assert_eq!(call_all(&mut all, "outer", 48.0).unwrap().to_string(), "48");
    assert_too_deep(call_all(&mut all, "outer", 49.0), 50);
}

/// Only calls active *at once* count: a call that has returned gives its place back, whichever
/// way it returned. Were any return path to skip that, a loop of calls would run out.
#[test]
fn calls_that_have_returned_do_not_count() {
    let src = "\
function one() return 1 end
function nothing(x) if x > 0 then return end end
function maybe(x) if x > 0 then return tonumber(\"1\") end end
function many(n)
  local s = 0
  for i = 1, n do
    nothing(i)
    local m = maybe(i)
    if m ~= nil then s = s + m + one() end
  end
  return s
end
";
    let mut all = triple_with_depth(src, 3);
    assert_eq!(
        call_all(&mut all, "many", 1000.0).unwrap().to_string(),
        "2000"
    );
}

/// The limit is the *most* calls active at once, so a limit of zero refuses even the host's own
/// call. Constants are read, not called, so reading one still works.
#[test]
fn a_zero_limit_refuses_every_call_but_not_a_constant() {
    let src = "C = {x = 1}\nfunction f(n) return n + 1 end\n";
    let mut all = triple_with_depth(src, 0);
    assert_too_deep(call_all(&mut all, "f", 1.0), 0);
    let (interp, vm, jit) = &mut all;
    let (a, b, c) = (
        interp.call("C", vec![]),
        vm.call("C", vec![]),
        jit.call("C", vec![]),
    );
    assert_agree("read C", &a, &b, &c);
    assert!(c.is_ok(), "{c:?}");
}

// ---- A function returning a value must return one on every path -------------
//
// Falling off the end of a function typed `number` (or a bare `return` in one) returned `nil`
// from the interpreters but `0` from the JIT. The checker now rejects it (`E0480`); an
// optional-returning function may still end without a value, and must mean `nil` everywhere.

#[test]
fn falling_off_a_number_function_is_rejected() {
    let err = grindlang::analyze(
        "function f(x) if x > 0 then return 1 end end",
        &TypeConfig::default(),
    )
    .expect_err("falling off a `number` function must not type-check");
    assert!(format!("{err:?}").contains("E0480"), "got {err:?}");
}

#[test]
fn falling_off_an_optional_function_is_nil_everywhere() {
    let src = "\
function first_big(n)
  for i = 1, n do
    if i > 10 then return tonumber(\"7\") end
  end
end
";
    let mut all = triple(src, &TypeConfig::default());
    assert_eq!(
        call_all(&mut all, "first_big", 20.0).unwrap().to_string(),
        "7"
    );
    assert_eq!(
        call_all(&mut all, "first_big", 5.0).unwrap().to_string(),
        "nil"
    );
}

// ---- Returning a call to a function that returns nothing -------------------
//
// `return log()` where `log` returns nothing failed JIT compilation: lowering returned the call's
// (absent) value from a function whose native signature returns none. It now makes the call and
// returns no value — the call must still be made, once, in every backend.

#[test]
fn returning_a_valueless_host_call_calls_it_once_everywhere() {
    let src = "function main(x) if x > 0 then return log() end end";
    let mut cfg = TypeConfig::default();
    cfg.host_functions.insert(
        "log".to_string(),
        FnType {
            params: vec![],
            ret: Box::new(Type::Unit),
        },
    );
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let (ast_n, ast_fn) = counter_returning(Value::Nil);
    let (vm_n, vm_fn) = counter_returning(Value::Nil);
    let (jit_n, jit_fn) = counter_returning(Value::Nil);
    interp.set_host_function("log", ast_fn);
    vm.set_host_function("log", vm_fn);
    jit.set_host_function("log", jit_fn);

    for x in [1.0, -1.0] {
        let args = vec![Value::Number(x)];
        let a = interp.call("main", args.clone());
        let b = vm.call("main", args.clone());
        let c = jit.call("main", args);
        assert_agree("return of a value-less host call", &a, &b, &c);
        assert_eq!(c.unwrap().to_string(), "nil");
    }
    assert_eq!(
        (*ast_n.borrow(), *vm_n.borrow(), *jit_n.borrow()),
        (1, 1, 1),
        "`log` runs on the branch that returns it, and only there"
    );
}

// ---- An optional return accepts a value and nil ---------------------------
//
// `return 1` in a function declared `---@return number?` was an E0413 mismatch, and so was
// `return 1` beside `return nil`, so a function could only produce "a number or nil" by falling
// off its end. Both now type-check; the JIT has to box the scalar into the optional's handle on
// the way out, which is what these pin across the backends.

#[test]
fn optional_returns_agree_everywhere() {
    let src = "\
function maybe(x)
  if x > 0 then return x * 2 elseif x < 0 then return nil end
end
function flag(x) if x > 0 then return true end return nil end
---@param x number
---@return number?
function declared(x) if x > 0 then return x end return nil end
function use(x)
  local m = maybe(x)
  if m ~= nil then return m + 1 end
  return 0
end
";
    let mut all = triple(src, &TypeConfig::default());
    for (name, n, want) in [
        ("maybe", 3.0, "6"),
        ("maybe", -1.0, "nil"),
        ("maybe", 0.0, "nil"),
        ("flag", 1.0, "true"),
        ("flag", -1.0, "nil"),
        ("declared", 4.0, "4"),
        ("declared", -4.0, "nil"),
        ("use", 3.0, "7"),
        ("use", -1.0, "0"),
    ] {
        let got = call_all(&mut all, name, n).unwrap_or_else(|e| panic!("{name}({n}): {e}"));
        assert_eq!(got.to_string(), want, "{name}({n})");
    }
}

#[test]
fn an_optional_tuple_element_agrees_everywhere() {
    let src = "\
---@param x number
---@return number?
---@return string
function pair(x) if x > 0 then return x, \"pos\" end return nil, \"neg\" end
function use(x)
  local n, s = pair(x)
  if n ~= nil then return s .. tostring(n) end
  return s
end
";
    let mut all = triple(src, &TypeConfig::default());
    assert_eq!(call_all(&mut all, "use", 2.0).unwrap().to_string(), "pos2");
    assert_eq!(call_all(&mut all, "use", -1.0).unwrap().to_string(), "neg");
}

// ---- An optional parameter accepts a value ---------------------------------
//
// A `number` or `bool` passed for a `number?`/`bool?` parameter reached the callee as the native
// scalar where its signature takes the optional's handle, so the JIT failed cranelift
// verification (an `f64` where an `i64` was expected). An argument is now coerced to its
// parameter, as a returned value is to the declared result: by a direct call, and by a call of a
// closure value.

#[test]
fn optional_parameters_take_a_value_everywhere() {
    let src = "\
---@param x number?
function num(x) if x ~= nil then return x * 2 end return -1 end
---@param b bool?
function flag(b) if b ~= nil then return b end return false end
function use(n)
  local narrowed = 0
  local parsed = tonumber(tostring(n))
  if parsed ~= nil then narrowed = num(parsed) end
  local bonus = 0
  if flag(n > 0) then bonus = 100 end
  return num(n) + num(nil) + narrowed + bonus
end
";
    let mut all = triple(src, &TypeConfig::default());
    for (n, want) in [(3.0, "111"), (-2.0, "-9")] {
        let got = call_all(&mut all, "use", n).unwrap_or_else(|e| panic!("use({n}): {e}"));
        assert_eq!(got.to_string(), want, "use({n})");
    }
}

#[test]
fn a_closure_with_an_optional_parameter_takes_a_value_everywhere() {
    // `keep` gives back the closure it is passed, typed `fn(number?) -> number`, so the script
    // calls a closure value whose parameter is optional.
    let ty = Type::function(vec![Type::optional(Type::Number)], Type::Number);
    let mut cfg = TypeConfig::default();
    cfg.host_functions.insert(
        "keep".to_string(),
        FnType {
            params: vec![ty.clone()],
            ret: Box::new(ty),
        },
    );
    let src = "\
function use(n)
  local g = keep(function(x) if x ~= nil then return x * 2 end return -1 end)
  return g(n) + g(nil)
end
";
    let mut all = triple(src, &cfg);
    let keep = |a: &[Value]| Ok(a[0].clone());
    let (interp, vm, jit) = &mut all;
    interp.set_host_function("keep", keep);
    vm.set_host_function("keep", keep);
    jit.set_host_function("keep", keep);
    for (n, want) in [(3.0, "5"), (-2.0, "-5")] {
        let got = call_all(&mut all, "use", n).unwrap_or_else(|e| panic!("use({n}): {e}"));
        assert_eq!(got.to_string(), want, "use({n})");
    }
}

// ---- Deep source must not overflow any later phase ------------------------

/// Source nested just inside the parser's limit (`E0209`) must survive every recursive phase —
/// resolver, checker, lowering, each backend, and dropping the tree — on the stack that limit is
/// sized for: 1 MiB in a release build (the Windows main thread's default), 2 MiB in a debug one,
/// whose unoptimized frames are several times larger. Each kind of nesting recurses through
/// different code, so each gets its own program.
#[test]
fn source_nested_to_the_limit_runs_on_the_stack_it_is_sized_for() {
    let n = 185; // levels; function literals and calls take two per nesting
    let programs = [
        format!(
            "function f(x) {}return x + 1 {}return 0 end",
            "if x > 0 then ".repeat(n),
            "end ".repeat(n)
        ),
        format!(
            "function f(x) return {}x + 1{} end",
            "(".repeat(n),
            ")".repeat(n)
        ),
        format!("function f(x) return x{} end", " + 1".repeat(n)),
        format!(
            "function f(x) local t = {}x{} return x + 1 end",
            "{".repeat(n),
            "}".repeat(n)
        ),
        format!(
            "function f(x) {}return x + 1 {}return x + 1 end",
            "local g = function() ".repeat(n / 2),
            "end ".repeat(n / 2)
        ),
        format!(
            "function id(y) return y + 0 end\nfunction f(x) return {}x{} end",
            "id(".repeat(n / 2),
            ")".repeat(n / 2)
        ),
    ];
    let stack = if cfg!(debug_assertions) {
        2 << 20
    } else {
        1 << 20
    };
    std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || {
            for src in programs {
                let mut all = triple(&src, &TypeConfig::default());
                call_all(&mut all, "f", 1.0).unwrap_or_else(|e| panic!("{e}\n{src}"));
            }
        })
        .unwrap()
        .join()
        .expect("no stack overflow");
}

// ---- A panic in host code panics the call, in every backend ---------------
//
// The JIT calls host functions from `extern "C"` runtime shims, and a panic can't unwind out of
// one: a panicking host function took the whole process down, where the interpreters let the
// panic out of `call`. The shim now catches it, the compiled code stops as it would on an error,
// and `call` resumes the panic.

/// The two ways the host enters a script, over any of the three executors.
trait Exec {
    fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RunError>;
    fn call_value(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, RunError>;
}

impl Exec for Interpreter<'static> {
    fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RunError> {
        Interpreter::call(self, name, args)
    }
    fn call_value(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, RunError> {
        self.call_value_public(callee, args)
    }
}

impl Exec for Vm<'static> {
    fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RunError> {
        Vm::call(self, name, args)
    }
    fn call_value(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, RunError> {
        Vm::call_value(self, callee, args)
    }
}

impl Exec for JitModule {
    fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RunError> {
        JitModule::call(self, name, args)
    }
    fn call_value(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, RunError> {
        JitModule::call_value(self, callee, args)
    }
}

/// One executor for the panic tests, with its `tick` count and its `mem`.
struct Panicky {
    name: &'static str,
    exec: Box<dyn Exec>,
    ticks: Rc<RefCell<u32>>,
    mem: Value,
}

impl Panicky {
    fn hits(&self) -> String {
        self.mem
            .field("hits")
            .map(|v| v.to_string())
            .unwrap_or_default()
    }
}

/// A host function that panics for a positive argument and otherwise returns it.
fn boom(args: &[Value]) -> Result<Value, RunError> {
    let x = args[0].as_f64().unwrap_or(0.0);
    assert!(x <= 0.0, "boom({x})");
    Ok(Value::Number(x))
}

/// All three executors for `src`, with `boom(number) -> number`, a counting `tick()`, and a
/// fresh `mem = {hits = 0, items = {1, 2, 3}}`, each limited to 10 script calls at once.
fn panicky(src: &str) -> [Panicky; 3] {
    let mut cfg = tick_config();
    cfg.host_functions.insert(
        "boom".to_string(),
        FnType {
            params: vec![Type::Number],
            ret: Box::new(Type::Number),
        },
    );
    let mut rec = BTreeMap::new();
    rec.insert("hits".to_string(), Type::Number);
    rec.insert("items".to_string(), Type::Array(Box::new(Type::Number)));
    cfg.memory.insert("mem".to_string(), Type::Record(rec));

    let fresh = || {
        let items = Value::array((1..=3).map(|n| Value::Number(f64::from(n))).collect());
        Value::table(
            [
                ("hits".to_string(), Value::Number(0.0)),
                ("items".to_string(), items),
            ]
            .into(),
        )
    };
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let (interp_n, interp_tick) = counter();
    let (vm_n, vm_tick) = counter();
    let (jit_n, jit_tick) = counter();
    let mem = [fresh(), fresh(), fresh()];
    interp.set_host_function("tick", interp_tick);
    vm.set_host_function("tick", vm_tick);
    jit.set_host_function("tick", jit_tick);
    interp.set_host_function("boom", boom);
    vm.set_host_function("boom", boom);
    jit.set_host_function("boom", boom);
    interp.set_memory("mem", mem[0].clone());
    vm.set_memory("mem", mem[1].clone());
    jit.set_memory("mem", mem[2].clone());
    interp.set_max_call_depth(10);
    vm.set_max_call_depth(10);
    jit.set_max_call_depth(10);
    [
        Panicky {
            name: "AST",
            exec: Box::new(interp),
            ticks: interp_n,
            mem: mem[0].clone(),
        },
        Panicky {
            name: "VM",
            exec: Box::new(vm),
            ticks: vm_n,
            mem: mem[1].clone(),
        },
        Panicky {
            name: "JIT",
            exec: Box::new(jit),
            ticks: jit_n,
            mem: mem[2].clone(),
        },
    ]
}

/// A call's outcome, shown: the value, or the error's message.
fn shown(r: Result<Value, RunError>) -> Result<String, String> {
    r.map(|v| v.to_string()).map_err(|e| e.to_string())
}

/// The message of the panic `call` raised, or `None` if it returned.
fn panic_of(call: impl FnOnce() -> Result<Value, RunError>) -> Option<String> {
    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)).err()?;
    Some(
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default(),
    )
}

/// The panic propagates out of `call` with its own payload, the statements after the host call
/// never run, the ones before it keep their effects, and the executor still works afterwards.
#[test]
fn a_panicking_host_function_panics_every_backend() {
    let src = "\
function f(x)
  mem.hits = mem.hits + 1
  local y = boom(x)
  tick()
  mem.hits = mem.hits + 100
  return y
end
";
    for mut b in panicky(src) {
        let name = b.name;
        let panic = panic_of(|| b.exec.call("f", vec![Value::Number(1.0)]));
        assert_eq!(panic.as_deref(), Some("boom(1)"), "{name}");
        assert_eq!(*b.ticks.borrow(), 0, "{name}: ran on after the panic");
        assert_eq!(b.hits(), "1", "{name}: memory after the panic");

        let r = b.exec.call("f", vec![Value::Number(0.0)]);
        assert_eq!(shown(r), Ok("0".to_string()), "{name}");
        assert_eq!(
            (*b.ticks.borrow(), b.hits()),
            (1, "102".to_string()),
            "{name}"
        );
    }
}

/// A panic deep in nested calls, and one inside a closure, unwinds every script call between it
/// and the host. The calls it unwound no longer count against the call-depth limit, and a
/// closure the host invokes panics the same way.
#[test]
fn a_panic_unwinds_nested_calls_and_closures() {
    let src = "\
function down(n, x)
  if n == 0 then return boom(x) end
  return 1 + down(n - 1, x)
end
function via_closure(x, label)
  local go = function(k) return boom(k) + 1 end
  tick()
  local r = go(x)
  tick()
  return label .. tostring(r)
end
function make() return function(x) return boom(x) end end
";
    for mut b in panicky(src) {
        let name = b.name;
        let args = |n, x| vec![Value::Number(n), Value::Number(x)];
        assert!(
            panic_of(|| b.exec.call("down", args(8.0, 1.0))).is_some(),
            "{name}"
        );
        // Ten calls deep: exactly the limit, so any depth the panic left behind fails it.
        let r = b.exec.call("down", args(9.0, 0.0));
        assert_eq!(shown(r), Ok("9".to_string()), "{name}");

        let args = |x| vec![Value::Number(x), Value::string("r=")];
        let panic = panic_of(|| b.exec.call("via_closure", args(2.0)));
        assert_eq!(panic.as_deref(), Some("boom(2)"), "{name}");
        assert_eq!(*b.ticks.borrow(), 1, "{name}: ran on after the panic");
        let r = b.exec.call("via_closure", args(-1.0));
        assert_eq!(shown(r), Ok("r=0".to_string()), "{name}");

        let closure = b.exec.call("make", vec![]).expect("make");
        let panic = panic_of(|| b.exec.call_value(closure.clone(), vec![Value::Number(3.0)]));
        assert_eq!(panic.as_deref(), Some("boom(3)"), "{name}");
        let r = b.exec.call_value(closure, vec![Value::Number(0.0)]);
        assert_eq!(shown(r), Ok("0".to_string()), "{name}");
    }
}

/// The runtime itself can panic: reading an array the host holds mutably borrowed across the
/// call. Under the JIT that happens inside a shim — one whose op can't otherwise fail, so the
/// generated code used not to check it — and must stop the call there, as it does elsewhere.
#[test]
fn a_panic_inside_the_runtime_stops_the_call() {
    let src = "\
function count()
  local n = #mem.items
  tick()
  return n
end
";
    let mut messages = Vec::new();
    for mut b in panicky(src) {
        let name = b.name;
        let items = b.mem.field("items").expect("items");
        let Value::Array(cells) = &items else {
            panic!("items is an array")
        };
        let held = cells.borrow_mut();
        let panic = panic_of(|| b.exec.call("count", vec![]));
        drop(held);
        messages.push(panic.unwrap_or_else(|| panic!("{name}: count() returned")));
        assert_eq!(*b.ticks.borrow(), 0, "{name}: ran on after the panic");

        let r = b.exec.call("count", vec![]);
        assert_eq!(shown(r), Ok("3".to_string()), "{name}");
    }
    assert!(
        messages.iter().all(|m| *m == messages[0]),
        "the backends panicked differently: {messages:?}"
    );
}
