//! Differential test: the IR is a second semantics oracle. For a corpus of programs, run
//! each through both the AST interpreter (`interp`) and the IR interpreter (`ir::Vm`) and
//! assert identical results. Any divergence is a lowering bug.
//!
//! Values are compared by their `Display` form, which both interpreters implement
//! identically and deterministically (sorted table keys, integer-formatted whole numbers).

#![cfg(feature = "interp")]

use std::collections::BTreeMap;

use grindlang::interp::Interpreter;
use grindlang::ir::Vm;
use grindlang::{TypeConfig, Value};

/// Build everything needed to run a program through both oracles.
fn both(src: &str, cfg: &TypeConfig) -> (Interpreter<'static>, Vm<'static>) {
    // Leak the analyzed artifacts so both interpreters can borrow them for 'static — fine
    // for a test harness.
    let (module, res, info) = grindlang::analyze(src, cfg).expect("analyze");
    let module = Box::leak(Box::new(module));
    let res = Box::leak(Box::new(res));
    let info = Box::leak(Box::new(info));
    let program = Box::leak(Box::new(
        grindlang::ir::lower(module, res, info, cfg).expect("lower"),
    ));
    grindlang::ir::verify(program).expect("verify");

    let interp = Interpreter::new(module, res).expect("interp");
    let vm = Vm::new(program);
    (interp, vm)
}

fn assert_same(src: &str, func: &str, args: Vec<Value>) {
    let cfg = TypeConfig::default();
    let (mut interp, mut vm) = both(src, &cfg);
    let a = interp.call(func, args.clone());
    let b = vm.call(func, args);
    match (a, b) {
        (Ok(av), Ok(bv)) => assert_eq!(
            format!("{av}"),
            format!("{bv}"),
            "AST vs IR result mismatch for `{func}` in:\n{src}"
        ),
        (Err(ae), Err(be)) => {
            // Both error — acceptable for the oracle as long as both agree they failed.
            let _ = (ae, be);
        }
        (a, b) => panic!("AST vs IR disagreement for `{func}`: {a:?} vs {b:?}\n{src}"),
    }
}

#[test]
fn arithmetic_and_precedence() {
    assert_same(
        "function f(a, b) return a + b * 2 - 1 end",
        "f",
        vec![Value::Number(3.0), Value::Number(4.0)],
    );
}

#[test]
fn floor_div_and_mod_and_pow() {
    assert_same(
        "function f(a, b) return a // b + a % b end",
        "f",
        vec![Value::Number(17.0), Value::Number(5.0)],
    );
    assert_same(
        "function f(x) return x ^ 3 end",
        "f",
        vec![Value::Number(2.0)],
    );
}

#[test]
fn branches_and_comparison() {
    let src = "function classify(n)\n\
                 if n < 0 then return \"neg\" end\n\
                 if n == 0 then return \"zero\" end\n\
                 return \"pos\"\n\
               end";
    for n in [-5.0, 0.0, 9.0] {
        assert_same(src, "classify", vec![Value::Number(n)]);
    }
}

#[test]
fn while_loop_accumulation() {
    let src = "function fact(n)\n\
                 local acc = 1\n\
                 local i = 2\n\
                 while i <= n do\n\
                   acc = acc * i\n\
                   i = i + 1\n\
                 end\n\
                 return acc\n\
               end";
    for n in [0.0, 1.0, 5.0, 8.0] {
        assert_same(src, "fact", vec![Value::Number(n)]);
    }
}

#[test]
fn numeric_for_with_negative_step() {
    let src = "function countdown(n)\n\
                 local s = 0\n\
                 for i = n, 1, -1 do\n\
                   s = s + i\n\
                 end\n\
                 return s\n\
               end";
    for n in [0.0, 1.0, 6.0] {
        assert_same(src, "countdown", vec![Value::Number(n)]);
    }
}

#[test]
fn generic_for_ipairs() {
    let src = "function total(xs)\n\
                 local s = 0\n\
                 for _, v in ipairs(xs) do\n\
                   s = s + v\n\
                 end\n\
                 return s\n\
               end";
    let arr = Value::array(vec![
        Value::Number(2.0),
        Value::Number(4.0),
        Value::Number(6.0),
    ]);
    assert_same(src, "total", vec![arr]);
}

#[test]
fn short_circuit_and_or() {
    // Operand types are pinned by the `==` comparisons, so `and`/`or` are well-typed.
    let and_src = "function f(a, b) return (a == true) and (b == true) end";
    let or_src = "function f(a, b) return (a == true) or (b == true) end";
    for a in [true, false] {
        for b in [true, false] {
            assert_same(and_src, "f", vec![Value::Bool(a), Value::Bool(b)]);
            assert_same(or_src, "f", vec![Value::Bool(a), Value::Bool(b)]);
        }
    }
}

#[test]
fn break_in_loop() {
    let src = "function firsthit(xs)\n\
                 local found = 0\n\
                 for _, v in ipairs(xs) do\n\
                   if v > 2 then\n\
                     found = v\n\
                     break\n\
                   end\n\
                 end\n\
                 return found\n\
               end";
    let arr = Value::array(vec![
        Value::Number(1.0),
        Value::Number(3.0),
        Value::Number(5.0),
    ]);
    assert_same(src, "firsthit", vec![arr]);
}

#[test]
fn string_builtins_and_concat() {
    assert_same(
        "function greet(name) return \"hi \" .. string.upper(name) end",
        "greet",
        vec![Value::string("bob")],
    );
}

#[test]
fn math_builtins_and_const() {
    let src = "K = 10\nfunction f(x) return math.floor(x) + K end";
    assert_same(src, "f", vec![Value::Number(3.7)]);
}

#[test]
fn array_build_and_append() {
    let src = "function grow(n)\n\
                 local out = { 1 }\n\
                 out[#out + 1] = n\n\
                 return out\n\
               end";
    assert_same(src, "grow", vec![Value::Number(9.0)]);
}

#[test]
fn recursion() {
    let src = "function fib(n)\n\
                 if n < 2 then return n end\n\
                 return fib(n - 1) + fib(n - 2)\n\
               end";
    for n in [0.0, 1.0, 7.0, 10.0] {
        assert_same(src, "fib", vec![Value::Number(n)]);
    }
}

#[test]
fn stat_calc_fixture_matches() {
    let path = format!(
        "{}/tests/fixtures/stat_calc.lua",
        env!("CARGO_MANIFEST_DIR")
    );
    let src = std::fs::read_to_string(path).unwrap();
    assert_same(
        &src,
        "mitigated",
        vec![Value::Number(120.0), Value::Number(80.0)],
    );
    assert_same(
        &src,
        "lethal",
        vec![
            Value::Number(120.0),
            Value::Number(80.0),
            Value::Number(50.0),
        ],
    );
}

#[test]
fn memory_and_pairs_match() {
    // Exercises memory read/write through both oracles and `pairs` over a map.
    let src = "function tally()\n\
                 local sum = 0\n\
                 for k, v in pairs(mem) do\n\
                   sum = sum + v\n\
                 end\n\
                 mem.total = sum\n\
                 return sum\n\
               end";

    let mut tc_mem = BTreeMap::new();
    tc_mem.insert(
        "mem".to_string(),
        grindlang::Type::Map(Box::new(grindlang::Type::Number)),
    );
    let cfg = TypeConfig {
        host_functions: BTreeMap::new(),
        memory: tc_mem,
    };

    let (module, res, info) = grindlang::analyze(src, &cfg).expect("analyze");
    let program = grindlang::ir::lower(&module, &res, &info, &cfg).expect("lower");
    grindlang::ir::verify(&program).expect("verify");

    let make_mem = || {
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), Value::Number(10.0));
        m.insert("b".to_string(), Value::Number(20.0));
        Value::table(m)
    };

    let mut interp = Interpreter::new(&module, &res).unwrap();
    interp.set_memory("mem", make_mem());
    let ar = interp.call("tally", vec![]).unwrap();

    let mut vm = Vm::new(&program);
    vm.set_memory("mem", make_mem());
    let br = vm.call("tally", vec![]).unwrap();

    assert_eq!(format!("{ar}"), format!("{br}"));
    // Memory writes agree too.
    assert_eq!(
        format!("{}", interp.memory("mem").unwrap()),
        format!("{}", vm.memory("mem").unwrap())
    );
}

#[test]
fn host_function_matches() {
    let src = "function f(n) return roll(n) + 1 end";
    let mut host = BTreeMap::new();
    host.insert(
        "roll".to_string(),
        grindlang::FnType {
            params: vec![grindlang::Type::Number],
            ret: Box::new(grindlang::Type::Number),
        },
    );
    let cfg = TypeConfig {
        host_functions: host,
        memory: BTreeMap::new(),
    };

    let (module, res, info) = grindlang::analyze(src, &cfg).expect("analyze");
    let program = grindlang::ir::lower(&module, &res, &info, &cfg).expect("lower");

    let mut interp = Interpreter::new(&module, &res).unwrap();
    interp.set_host_function("roll", |a| Ok(Value::Number(a[0].as_f64().unwrap() * 2.0)));
    let ar = interp.call("f", vec![Value::Number(3.0)]).unwrap();

    let mut vm = Vm::new(&program);
    vm.set_host_function("roll", |a| Ok(Value::Number(a[0].as_f64().unwrap() * 2.0)));
    let br = vm.call("f", vec![Value::Number(3.0)]).unwrap();

    assert_eq!(format!("{ar}"), format!("{br}"));
}
