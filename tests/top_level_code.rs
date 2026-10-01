//! Top-level code (`SPEC.md` §3.1): a module's constants and top-level statements run **once**,
//! when the module initializes, and everything they build is then **frozen**.
//!
//! Like `jit_differential`, every case runs through all three oracles — AST `Interpreter`, IR
//! `Vm`, cranelift `JitModule` (plus a JIT that compacts at every loop back edge) — and they must
//! agree. The later sections pin what a return value can't show, as `oracle_regressions` does:
//! that top-level code runs once, how its errors surface, and that nothing it built can carry
//! state from one call to the next.

#![cfg(all(feature = "interp", feature = "jit"))]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

mod common;

use common::Executors;
use grindlang::types::FnType;
use grindlang::{RunError, Type, TypeConfig, Value};

fn num(n: f64) -> Value {
    Value::Number(n)
}

// ---- what top-level code computes --------------------------------------------

/// The motivating case: a lookup table built by a loop, once, and read by every call.
#[test]
fn a_loop_builds_a_lookup_table() {
    let src = "\
local SQUARES = {}
for i = 1, 10 do
  SQUARES[i] = i * i
end
function square(n)
  local v = SQUARES[n]
  if v ~= nil then return v end
  return -1
end
";
    let mut ex = Executors::new(src, &TypeConfig::default());
    ex.assert_returns("square", vec![num(7.0)], "49");
    ex.assert_returns("square", vec![num(11.0)], "-1");
}

/// A constant may be computed from names and calls — including a function declared below it,
/// since a `function` is in scope everywhere.
#[test]
fn a_constant_is_computed_from_other_constants_and_calls() {
    let src = "\
BASE = 10
SCALE = double(BASE) + 1
function double(x) return x * 2 end
function total(n) return n * SCALE end
";
    let mut ex = Executors::new(src, &TypeConfig::default());
    ex.assert_returns("total", vec![num(2.0)], "42");
    ex.assert_returns("SCALE", vec![], "21");
}

/// `local` makes a module variable or function private: readable by the functions after it, but
/// not exported.
#[test]
fn a_top_level_local_is_private() {
    let src = "\
local rate = 0.5
local function apply(x) return x * rate end
function discounted(price) return price - apply(price) end
";
    let mut ex = Executors::new(src, &TypeConfig::default());
    ex.assert_returns("discounted", vec![num(80.0)], "40");
    for private in ["rate", "apply"] {
        for (oracle, got) in ex.call(private, vec![]) {
            assert!(
                matches!(got, Err(RunError::UnknownExport(_))),
                "{oracle}: `{private}` should not be exported, got {got:?}"
            );
        }
    }
}

/// Top-level code can branch and loop, and assign module variables as it goes.
#[test]
fn top_level_code_branches_and_loops() {
    let src = "\
local mode = \"hard\"
local mult = 1
if mode == \"easy\" then
  mult = 0.5
elseif mode == \"hard\" then
  mult = 2
end
local n = 0
while n < 5 do n = n + 1 end
LIMIT = n * mult
function limit() return LIMIT end
";
    let mut ex = Executors::new(src, &TypeConfig::default());
    ex.assert_returns("limit", vec![], "10");
}

/// A table of closures built at the top level, dispatched to by name.
#[test]
fn a_dispatch_table_of_closures() {
    let src = "\
local handlers = {
  [\"add\"] = function(a, b) return a + b end,
}
handlers[\"mul\"] = function(a, b) return a * b end
function run(op, a, b)
  local h = handlers[op]
  if h ~= nil then return h(a, b) end
  return 0
end
";
    let mut ex = Executors::new(src, &TypeConfig::default());
    ex.assert_returns("run", vec![Value::string("add"), num(2.0), num(3.0)], "5");
    ex.assert_returns("run", vec![Value::string("mul"), num(2.0), num(3.0)], "6");
    ex.assert_returns("run", vec![Value::string("pow"), num(2.0), num(3.0)], "0");
}

/// A closure top-level code builds may capture a local of one of its blocks, and read it from
/// any later call.
#[test]
fn a_closure_reads_what_it_captured_at_the_top_level() {
    let src = "\
local get
do
  local secret = 41
  get = function() return secret + 1 end
end
function reveal()
  if get ~= nil then return get() end
  return 0
end
";
    let mut ex = Executors::new(src, &TypeConfig::default());
    ex.assert_returns("reveal", vec![], "42");
    ex.assert_returns("reveal", vec![], "42");
}

/// A curated export table may name a top-level local.
#[test]
fn an_export_table_may_name_a_top_level_local() {
    let src = "\
local version = \"1.2\"
function describe() return \"v\" .. version end
return { version = version, describe = describe }
";
    let mut ex = Executors::new(src, &TypeConfig::default());
    ex.assert_returns("version", vec![], "1.2");
    ex.assert_returns("describe", vec![], "v1.2");
}

/// A module value is one object for the module's lifetime: the table one call returns is the
/// one the next call compares against.
#[test]
fn a_module_value_keeps_its_identity_across_calls() {
    let src = "\
local T = {x = 1}
function get() return T end
function same(t) return t == T end
";
    let mut ex = Executors::new(src, &TypeConfig::default());
    let t = ex.jit.call("get", vec![]).expect("get");
    for (oracle, got) in [
        ("AST", ex.interp.call("get", vec![])),
        ("IR", ex.vm.call("get", vec![])),
        ("JIT", ex.jit.call("get", vec![])),
    ] {
        let got = got.expect("get");
        if oracle == "JIT" {
            assert!(got.ref_eq(&t), "JIT: two calls returned different tables");
        }
        let back = match oracle {
            "AST" => ex.interp.call("same", vec![got]),
            "IR" => ex.vm.call("same", vec![got]),
            _ => ex.jit.call("same", vec![got]),
        };
        assert_eq!(back.expect("same").to_string(), "true", "{oracle}");
    }
}

// ---- top-level code runs once --------------------------------------------------

/// A host function, boxed.
type HostFn = Box<dyn Fn(&[Value]) -> Result<Value, RunError>>;

/// A host function that counts its calls, returning the count so far, with the handle to read
/// the count back.
fn counter() -> (Rc<RefCell<u32>>, HostFn) {
    let count = Rc::new(RefCell::new(0u32));
    let c = count.clone();
    (
        count,
        Box::new(move |_: &[Value]| {
            *c.borrow_mut() += 1;
            Ok(Value::Number(f64::from(*c.borrow())))
        }),
    )
}

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

/// Top-level code may call host functions, and runs exactly once however many calls follow.
#[test]
fn top_level_code_runs_once() {
    let src = "SEED = tick()\nfunction seed() return SEED end\n";
    let mut ex = Executors::new(src, &tick_config());
    let (a, fa) = counter();
    let (b, fb) = counter();
    let (c, fc) = counter();
    let (d, fd) = counter();
    ex.interp.set_host_function("tick", fa);
    ex.vm.set_host_function("tick", fb);
    ex.jit.set_host_function("tick", fc);
    ex.eager.set_host_function("tick", fd);
    for _ in 0..3 {
        ex.assert_returns("seed", vec![], "1");
    }
    for (oracle, n) in [("AST", a), ("IR", b), ("JIT", c), ("eager JIT", d)] {
        assert_eq!(
            *n.borrow(),
            1,
            "{oracle} ran top-level code {} times",
            n.borrow()
        );
    }
}

/// `init` runs top-level code up front, so a host sees its errors before the first call; calling
/// it again, or calling an export after it, doesn't run it again.
#[test]
fn init_runs_top_level_code_up_front() {
    let src = "SEED = tick()\nfunction seed() return SEED end\n";
    let mut ex = Executors::new(src, &tick_config());
    let (n, f) = counter();
    ex.jit.set_host_function("tick", f);
    ex.jit.init().expect("init");
    assert_eq!(*n.borrow(), 1);
    ex.jit.init().expect("init again");
    assert_eq!(ex.jit.call("seed", vec![]).expect("seed").to_string(), "1");
    assert_eq!(*n.borrow(), 1, "top-level code ran again");
}

// ---- how top-level code fails -----------------------------------------------------

/// Reading a constant before the top-level code that sets it has run — here through a function
/// top-level code calls too early — is a runtime error, and every later call reports it.
#[test]
fn reading_a_value_before_it_is_set_fails_every_call() {
    let src = "\
function early() return LATE + 1 end
X = early()
LATE = 1
function f() return X end
function g() return 7 end
";
    let mut ex = Executors::new(src, &TypeConfig::default());
    ex.assert_fails("f", vec![], "`LATE` was read before");
    // The module failed to initialize, so nothing in it runs — not even what doesn't read `X`.
    ex.assert_fails("g", vec![], "`LATE` was read before");
}

fn mem_config() -> TypeConfig {
    let mut cfg = TypeConfig::default();
    let rec = BTreeMap::from([("gold".to_string(), Type::Number)]);
    cfg.memory.insert("mem".to_string(), Type::Record(rec));
    cfg
}

/// Host memory isn't available while the module initializes — not even through a function top-
/// level code calls, which `E0310` can't see — since what top-level code builds would freeze a
/// snapshot of it.
#[test]
fn top_level_code_cannot_reach_memory_through_a_function() {
    let src = "\
function gold() return mem.gold end
G = gold()
function f() return G end
";
    let mut ex = Executors::new(src, &mem_config());
    let bind = || Value::table(BTreeMap::from([("gold".to_string(), num(5.0))]));
    ex.interp.set_memory("mem", bind());
    ex.vm.set_memory("mem", bind());
    ex.jit.set_memory("mem", bind());
    ex.eager.set_memory("mem", bind());
    ex.assert_fails("f", vec![], "not available while the module initializes");
}

/// The same function reads memory fine once the module has initialized.
#[test]
fn a_function_reads_memory_after_initialization() {
    let src = "\
local bonus = 2
function gold() return mem.gold + bonus end
";
    let mut ex = Executors::new(src, &mem_config());
    let bind = || Value::table(BTreeMap::from([("gold".to_string(), num(5.0))]));
    ex.interp.set_memory("mem", bind());
    ex.vm.set_memory("mem", bind());
    ex.jit.set_memory("mem", bind());
    ex.eager.set_memory("mem", bind());
    ex.assert_returns("gold", vec![], "7");
}

/// A host function that panics while top-level code runs panics the call, and leaves top-level
/// code to run again, from the start, on the next one.
#[test]
fn a_panic_in_top_level_code_lets_it_run_again() {
    let src = "SEED = tick()\nfunction seed() return SEED end\n";
    let mut ex = Executors::new(src, &tick_config());
    let panics_once = || {
        let calls = Rc::new(RefCell::new(0u32));
        move |_: &[Value]| -> Result<Value, RunError> {
            *calls.borrow_mut() += 1;
            if *calls.borrow() == 1 {
                panic!("tick failed");
            }
            Ok(Value::Number(f64::from(*calls.borrow())))
        }
    };
    ex.interp.set_host_function("tick", panics_once());
    ex.vm.set_host_function("tick", panics_once());
    ex.jit.set_host_function("tick", panics_once());
    ex.eager.set_host_function("tick", panics_once());

    let caught = |r: std::thread::Result<Result<Value, RunError>>| r.is_err();
    use std::panic::{AssertUnwindSafe, catch_unwind};
    assert!(caught(catch_unwind(AssertUnwindSafe(|| ex
        .interp
        .call("seed", vec![])))));
    assert!(caught(catch_unwind(AssertUnwindSafe(|| ex
        .vm
        .call("seed", vec![])))));
    assert!(caught(catch_unwind(AssertUnwindSafe(|| ex
        .jit
        .call("seed", vec![])))));
    assert!(caught(catch_unwind(AssertUnwindSafe(|| ex
        .eager
        .call("seed", vec![])))));
    ex.assert_returns("seed", vec![], "2");
}

// ---- what top-level code builds is frozen -------------------------------------------

/// A write through an alias of a module value gets past `E0307` and is refused at run time —
/// for a record (by field or by literal key), an array, and a map alike.
#[test]
fn a_write_through_an_aliased_module_value_is_refused() {
    let src = "\
local T = {x = 1}
local A = {1, 2}
local M = {[\"k\"] = 1}
function rec() local t = T t.x = 2 return t.x end
function rec_key() local t = T t[\"x\"] = 2 return t[\"x\"] end
function arr() local a = A a[1] = 9 return a[1] end
function map() local m = M m[\"k\"] = 2 return m[\"k\"] end
";
    let mut ex = Executors::new(src, &TypeConfig::default());
    for f in ["rec", "rec_key", "arr", "map"] {
        // Repeated: a call must not unfreeze anything for the next.
        for _ in 0..2 {
            ex.assert_fails(f, vec![], "frozen module value");
        }
    }
}

/// A closure kept in a module value can't change what it captured: that would be state carried
/// from one call to the next, which only host memory may hold.
#[test]
fn a_closure_in_a_module_value_cannot_write_its_upvalue() {
    let src = "\
local function make()
  local n = 0
  return function()
    n = n + 1
    return n
  end
end
COUNTER = make()
function tick() return COUNTER() end
";
    let mut ex = Executors::new(src, &TypeConfig::default());
    ex.assert_fails("tick", vec![], "frozen module value");
}

/// Top-level code itself mutates what it builds freely; only after it has run is it frozen. And
/// the freeze covers only module values: a function's own tables stay writable.
#[test]
fn top_level_code_mutates_before_the_freeze_and_functions_own_tables_stay_writable() {
    let src = "\
local T = {x = 1}
T.x = T.x + 1
local function bump() return T.x + 1 end
T.x = bump()
T[\"x\"] = T[\"x\"] * 2
function f()
  local mine = {x = T.x}
  mine[\"x\"] = mine.x * 10
  return mine.x
end
";
    let mut ex = Executors::new(src, &TypeConfig::default());
    ex.assert_returns("f", vec![], "60");
}

// ---- the embedding API ---------------------------------------------------------------

/// `Engine::compile` runs top-level code, so an error in it is a build error.
#[test]
fn engine_compile_reports_a_failing_top_level_code() {
    let err = grindlang::Engine::new()
        .compile("function early() return LATE end\nX = early()\nLATE = 1\n")
        .err()
        .expect("top-level code fails");
    assert!(
        matches!(err, grindlang::api::BuildError::Init(_)),
        "got {err:?}"
    );
}

/// Top-level code runs with the engine's host functions installed.
#[test]
fn engine_compile_runs_top_level_code_with_host_functions() {
    let mut engine = grindlang::Engine::new();
    engine.register_fn("base", || 40.0);
    let mut m = engine
        .compile("BASE = base() + 2\nfunction answer() return BASE end\n")
        .expect("compile");
    let answer: f64 = m.call_typed("answer", ()).expect("answer");
    assert_eq!(answer, 42.0);
}
