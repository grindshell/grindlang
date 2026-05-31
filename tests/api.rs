//! Integration tests for the Phase 8 host embedding API ([`grindlang::api`]).
//!
//! This is the **tested public surface** the Phase 8 exit criterion asks for: it drives the
//! crate the way an embedder would — register host functions, declare memory, compile, and
//! call exports with typed Rust values.

#![cfg(feature = "jit")]

use std::collections::BTreeMap;

use grindlang::Type;
use grindlang::Value;
use grindlang::api::{CallError, Engine};

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
