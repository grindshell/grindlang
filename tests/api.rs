//! Integration tests for the Phase 8 host embedding API ([`grindlang::api`]).
//!
//! This is the **tested public surface** the Phase 8 exit criterion asks for: it drives the
//! crate the way an embedder would — register host functions, declare memory, compile, and
//! call exports with typed Rust values.

#![cfg(feature = "jit")]

use std::collections::BTreeMap;

use grindlang::FnType;
use grindlang::RunError;
use grindlang::Type;
use grindlang::Value;
use grindlang::api::{CallError, Engine, MarshalError};

/// Typed args in, typed result out, plus a registered infallible host function.
#[test]
fn typed_call_with_host_fn() {
    let mut engine = Engine::new();
    engine.register_fn("double", |n: f64| n * 2.0);
    let mut m = engine
        .compile("function calc(x) return double(x) + 1 end")
        .expect("compile");
    let out: f64 = m.call_typed("calc", (20.0,)).expect("call");
    assert_eq!(out, 41.0);
}

/// Integer-typed marshaling (numbers are f64 underneath, exact for these magnitudes).
#[test]
fn integer_marshaling() {
    let mut m = Engine::new()
        .compile("function add(a, b) return a + b end")
        .expect("compile");
    let sum: i64 = m.call_typed("add", (40_i64, 2_i64)).expect("call");
    assert_eq!(sum, 42);
}

/// Marshaling a result into an integer Rust type: a finite non-integral value truncates toward
/// zero (trusted scripts may intend a fractional formula), but a non-finite value is rejected
/// rather than letting the saturating `as` cast turn `∞` into `i64::MAX` silently. As an `f64`
/// the same value passes through unchanged.
#[test]
fn integer_marshaling_truncates_finite_rejects_non_finite() {
    let mut m = Engine::new()
        .compile("function inf() return math.huge end\nfunction frac() return 3.9 end")
        .expect("compile");
    // f64 marshaling passes infinity through.
    let as_float: f64 = m.call_typed("inf", ()).expect("inf as f64");
    assert!(as_float.is_infinite());
    // i64 marshaling rejects it instead of saturating.
    let err = m
        .call_typed::<_, i64>("inf", ())
        .expect_err("inf into i64 must error");
    assert!(
        matches!(err, CallError::Result(MarshalError::NonFinite { .. })),
        "got {err:?}"
    );
    // A finite fractional result truncates toward zero.
    let truncated: i64 = m.call_typed("frac", ()).expect("frac into i64");
    assert_eq!(truncated, 3);
}

/// The host call boundary enforces exact arity (SPEC §5.5) rather than padding missing
/// arguments with `nil` or dropping surplus ones.
#[test]
fn call_with_wrong_arity_is_an_error() {
    let mut m = Engine::new()
        .compile("function add(a, b) return a + b end")
        .expect("compile");
    assert_eq!(m.call_typed::<_, f64>("add", (2.0, 3.0)).unwrap(), 5.0);
    let too_few = m
        .call_typed::<_, f64>("add", (2.0,))
        .expect_err("too few args");
    assert!(
        matches!(
            too_few,
            CallError::Run(RunError::ArityMismatch {
                expected: 2,
                got: 1,
                ..
            })
        ),
        "got {too_few:?}"
    );
    let too_many = m
        .call_typed::<_, f64>("add", (2.0, 3.0, 4.0))
        .expect_err("too many args");
    assert!(
        matches!(
            too_many,
            CallError::Run(RunError::ArityMismatch {
                expected: 2,
                got: 3,
                ..
            })
        ),
        "got {too_many:?}"
    );
}

/// A NaN operand to a relational compare must surface as a call error, not a silent `false`.
/// The JIT lowers `<` to a native `fcmp` (unordered → `false`); the `rt_check_compare` guard
/// latches the same error the tree-walking interpreter raises, so the divergence can't reach
/// a host (this runs under the default `jit`-only feature set, which `cargo test` exercises).
#[test]
fn nan_comparison_is_a_call_error() {
    let mut m = Engine::new()
        .compile("function lt(a, b) return a < b end")
        .expect("compile");
    let ok: bool = m.call_typed("lt", (1.0, 2.0)).expect("finite compare");
    assert!(ok);
    let err = m
        .call_typed::<_, bool>("lt", (f64::NAN, 1.0))
        .expect_err("NaN compare must error");
    assert!(matches!(err, CallError::Run(_)), "got {err:?}");
}

/// Runaway recursion used to overflow the native stack and abort the host process. It now
/// fails the call at the call-depth limit (`SPEC.md` §7.3), which runs under the default
/// `jit`-only feature set on the test thread's ordinary stack.
#[test]
fn runaway_recursion_is_a_call_error() {
    let mut m = Engine::new()
        .compile("function down(n) if n == 0 then return 0 end return 1 + down(n - 1) end")
        .expect("compile");
    let err = m
        .call_typed::<_, f64>("down", (1e7,))
        .expect_err("ten million nested calls must hit the limit");
    assert!(
        err.to_string().contains("call depth limit exceeded"),
        "got {err}"
    );

    // The default leaves ordinary recursion alone, and a host that knows its stack is bigger
    // can raise the limit.
    let depth = grindlang::value::DEFAULT_MAX_CALL_DEPTH;
    let n = f64::from(depth - 1);
    assert_eq!(m.call_typed::<_, f64>("down", (n,)).expect("fits"), n);
    assert!(m.call_typed::<_, f64>("down", (n + 1.0,)).is_err());
    m.set_max_call_depth(depth * 10);
    assert_eq!(
        m.call_typed::<_, f64>("down", (n + 1.0,)).expect("fits"),
        n + 1.0
    );
}

/// A panicking host function used to abort the whole process: the panic tried to unwind out of
/// an `extern "C"` runtime shim, which Rust turns into an abort. It now propagates out of `call`
/// like a panic from any other callback, and the module stays usable.
#[test]
fn a_panicking_host_function_panics_the_call() {
    let mut engine = Engine::new();
    engine.register_fn("check", |n: f64| {
        assert!(n >= 0.0, "negative input {n}");
        n
    });
    let mut m = engine
        .compile("function f(x) return check(x) + 1 end")
        .expect("compile");
    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        m.call_typed::<_, f64>("f", (-1.0,))
    }))
    .expect_err("the host function's panic must reach the caller");
    assert_eq!(
        payload.downcast_ref::<String>().map(String::as_str),
        Some("negative input -1")
    );
    assert_eq!(m.call_typed::<_, f64>("f", (2.0,)).expect("usable"), 3.0);
}

/// A bool-returning decision function.
#[test]
fn bool_result() {
    let mut m = Engine::new()
        .compile("function ok(hp) return hp > 0 end")
        .expect("compile");
    assert!(m.call_typed::<_, bool>("ok", (5.0,)).unwrap());
    assert!(!m.call_typed::<_, bool>("ok", (0.0,)).unwrap());
}

/// String marshaling through a builtin.
#[test]
fn string_result() {
    let mut m = Engine::new()
        .compile("function shout(s) return string.upper(s) end")
        .expect("compile");
    let out: String = m.call_typed("shout", ("hi".to_string(),)).unwrap();
    assert_eq!(out, "HI");
}

/// Array (`Vec<T>`) result marshaling.
#[test]
fn vec_result() {
    let src = "function grow(n)\n\
                 local out = { 1 }\n\
                 out[#out + 1] = n\n\
                 return out\n\
               end";
    let mut m = Engine::new().compile(src).expect("compile");
    let out: Vec<f64> = m.call_typed("grow", (9.0,)).unwrap();
    assert_eq!(out, vec![1.0, 9.0]);
}

/// A fallible host function: `Ok` flows through, `Err` becomes a runtime error.
#[test]
fn fallible_host_fn() {
    let mut engine = Engine::new();
    engine.register_try_fn("checked_sqrt", |x: f64| {
        if x < 0.0 {
            Err("negative input")
        } else {
            Ok(x.sqrt())
        }
    });
    let mut m = engine
        .compile("function f(x) return checked_sqrt(x) end")
        .expect("compile");

    let ok: f64 = m.call_typed("f", (16.0,)).unwrap();
    assert_eq!(ok, 4.0);

    let err = m.call_typed::<_, f64>("f", (-1.0,)).unwrap_err();
    match err {
        CallError::Run(e) => assert!(e.to_string().contains("negative input")),
        other => panic!("expected a run error, got {other:?}"),
    }
}

/// The raw escape hatch: dynamic `Value`-level host function + explicit signature.
#[test]
fn raw_host_fn() {
    use grindlang::{FnType, RunError};
    let mut engine = Engine::new();
    engine.register_fn_raw(
        "sum_all",
        FnType {
            params: vec![Type::array(Type::Number)],
            ret: Box::new(Type::Number),
        },
        |args: &[Value]| {
            let arr = args
                .first()
                .and_then(|v| v.as_array())
                .ok_or_else(|| RunError::Host("expected an array".into()))?;
            let total: f64 = arr.iter().filter_map(|v| v.as_f64()).sum();
            Ok(Value::Number(total))
        },
    );
    let mut m = engine
        .compile("function total(xs) return sum_all(xs) end")
        .expect("compile");
    let arr = Value::array(vec![
        Value::Number(1.0),
        Value::Number(2.0),
        Value::Number(3.0),
    ]);
    let out = m.call("total", vec![arr]).unwrap();
    assert_eq!(out.as_f64(), Some(6.0));
}

/// Host memory: declare its schema, bind a value, observe a script mutation.
#[test]
fn memory_read_write() {
    let mut engine = Engine::new();
    let mut rec = BTreeMap::new();
    rec.insert("gold".to_string(), Type::Number);
    engine.declare_memory("mem", Type::Record(rec));

    let src = "function spend(n)\n\
                 if mem.gold >= n then\n\
                   mem.gold = mem.gold - n\n\
                   return true\n\
                 end\n\
                 return false\n\
               end";
    let mut m = engine.compile(src).expect("compile");

    let mut initial = BTreeMap::new();
    initial.insert("gold".to_string(), Value::Number(100.0));
    m.set_memory("mem", Value::table(initial));

    assert!(m.call_typed::<_, bool>("spend", (30.0,)).unwrap());
    assert!(m.call_typed::<_, bool>("spend", (50.0,)).unwrap());
    assert!(!m.call_typed::<_, bool>("spend", (40.0,)).unwrap()); // only 20 left

    let gold = m.memory("mem").unwrap().field("gold").unwrap();
    assert_eq!(gold.as_f64(), Some(20.0));
}

/// Host-memory method (`mem:spend(n)`, SPEC §7.2): register the method impl on the engine,
/// call it from a script export, and observe that it mutates persistent memory. The impl
/// receives the receiver as its first argument.
#[test]
fn memory_method_registration_and_call() {
    let mut engine = Engine::new();
    let mut rec = BTreeMap::new();
    rec.insert("gold".to_string(), Type::Number);
    engine.declare_memory("mem", Type::Record(rec));
    engine.register_method_raw(
        "mem",
        "spend",
        FnType {
            params: vec![Type::Number],
            ret: Box::new(Type::Bool),
        },
        |a| {
            let amount = a[1].as_f64().unwrap();
            let gold = a[0].field("gold").and_then(|v| v.as_f64()).unwrap_or(0.0);
            if gold >= amount {
                if let Value::Table(t) = &a[0] {
                    t.borrow_mut()
                        .insert("gold".to_string(), Value::Number(gold - amount));
                }
                Ok(Value::Bool(true))
            } else {
                Ok(Value::Bool(false))
            }
        },
    );

    let src = "function pay(n) return mem:spend(n) end";
    let mut m = engine.compile(src).unwrap();

    let mut initial = BTreeMap::new();
    initial.insert("gold".to_string(), Value::Number(100.0));
    m.set_memory("mem", Value::table(initial));

    assert!(m.call_typed::<_, bool>("pay", (30.0,)).unwrap());
    assert!(m.call_typed::<_, bool>("pay", (50.0,)).unwrap());
    assert!(!m.call_typed::<_, bool>("pay", (40.0,)).unwrap()); // only 20 left

    let gold = m.memory("mem").unwrap().field("gold").unwrap();
    assert_eq!(gold.as_f64(), Some(20.0));
}

/// Export-signature introspection.
#[test]
fn export_introspection() {
    let m = Engine::new()
        .compile("K = 5\nfunction f(a, b) return a + b end")
        .expect("compile");
    assert_eq!(
        m.export_type("f").map(ToString::to_string).as_deref(),
        Some("fn(number, number) -> number")
    );
    assert_eq!(
        m.export_type("K").map(ToString::to_string).as_deref(),
        Some("number")
    );
    assert!(m.exports().contains_key("f"));
    assert!(m.export_type("nope").is_none());
}

/// A type error surfaces as `BuildError::Check`, not a panic. (Matched rather than
/// `unwrap_err`-ed because `Module` intentionally isn't `Debug`.)
#[test]
fn build_error_on_type_mismatch() {
    let result = Engine::new().compile("function f(x) return x + \"oops\" end");
    match result {
        Err(grindlang::api::BuildError::Check(_)) => {}
        Err(other) => panic!("expected a Check error, got {other:?}"),
        Ok(_) => panic!("expected the type mismatch to fail compilation"),
    }
}

/// Programs the IR can't express are rejected by the checker with a coded, located diagnostic,
/// never as a lowering error: every `BuildError::Lower` is an internal bug.
#[test]
fn inexpressible_programs_are_check_errors() {
    let mut engine = Engine::new();
    engine.register_fn("roll", || 4.0);
    let cases = [
        (
            "function helper() return 1 end\nfunction f() local g = helper return g() end",
            "E0434",
        ),
        ("function f() local g = roll return g() end", "E0434"),
        ("function f() return 1 end\nreturn { run = 1 }", "E0411"),
        ("function f() return 1 end\nreturn { run = roll }", "E0411"),
    ];
    for (src, code) in cases {
        match engine.compile(src) {
            Err(grindlang::api::BuildError::Check(d)) => {
                let d = d.iter().next().expect("a diagnostic");
                assert_eq!(d.code, code, "{src}");
                assert_ne!(
                    d.span,
                    grindlang::Span::dummy(),
                    "{src}: diagnostic has no location"
                );
            }
            Err(other) => panic!("{src}: expected a Check error, got {other:?}"),
            Ok(_) => panic!("{src}: expected compilation to fail"),
        }
    }
}

/// A result-type mismatch surfaces as `CallError::Result`.
#[test]
fn call_error_on_result_mismatch() {
    let mut m = Engine::new()
        .compile("function f() return 1 end")
        .expect("compile");
    // The function returns a number; ask for a String.
    let err = m.call_typed::<_, String>("f", ()).unwrap_err();
    assert!(matches!(err, CallError::Result(_)));
}

/// One engine compiles many independent modules sharing the same host functions.
#[test]
fn engine_compiles_many_modules() {
    let mut engine = Engine::new();
    engine.register_fn("bonus", || 10.0);
    let mut a = engine.compile("function f() return bonus() end").unwrap();
    let mut b = engine
        .compile("function g() return bonus() * 2 end")
        .unwrap();
    assert_eq!(a.call_typed::<_, f64>("f", ()).unwrap(), 10.0);
    assert_eq!(b.call_typed::<_, f64>("g", ()).unwrap(), 20.0);
}

/// Option marshaling: `tonumber` returns `number?`.
#[test]
fn optional_result() {
    let mut m = Engine::new()
        .compile("function parse(s) return tonumber(s) end")
        .expect("compile");
    let some: Option<f64> = m.call_typed("parse", ("3.5".to_string(),)).unwrap();
    assert_eq!(some, Some(3.5));
    let none: Option<f64> = m.call_typed("parse", ("nope".to_string(),)).unwrap();
    assert_eq!(none, None);
}

#[test]
fn returns_and_invokes_closure() {
    let mut m = Engine::new()
        .compile("function make_adder(n) return function(x) return x + n end end")
        .unwrap();
    // Obtain the closure as an opaque value, then host-invoke it with typed marshaling.
    let adder = m.call("make_adder", vec![Value::Number(10.0)]).unwrap();
    let sum: f64 = m.call_value_typed(adder, (5.0,)).unwrap();
    assert_eq!(sum, 15.0);
}

#[test]
fn host_held_counter_persists_upvalue() {
    let mut m = Engine::new()
        .compile(
            "function make_counter()\n\
               local n = 0\n\
               return function() n = n + 1 return n end\n\
             end",
        )
        .unwrap();
    let counter = m.call("make_counter", vec![]).unwrap();
    let a: f64 = m.call_value_typed(counter.clone(), ()).unwrap();
    let b: f64 = m.call_value_typed(counter.clone(), ()).unwrap();
    let c: f64 = m.call_value_typed(counter, ()).unwrap();
    assert_eq!((a, b, c), (1.0, 2.0, 3.0));
}
