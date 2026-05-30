//! Integration test: the spec's worked-example modules execute end to end through the
//! reference interpreter and produce the expected runtime results.

#![cfg(feature = "interp")]

use std::collections::BTreeMap;

use grindlang::{Interpreter, ResolveConfig, Value};

fn read_fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

#[test]
fn stat_calc_fixture_runs() {
    let src = read_fixture("stat_calc.lua");
    let module = grindlang::parse(&src).unwrap();
    let res = grindlang::resolve::resolve(&module, &ResolveConfig::default()).unwrap();
    let mut it = Interpreter::new(&module, &res).unwrap();

    // mitigated(100, 100) = 100 * (100 / 200) = 50
    let mitigated = it
        .call(
            "mitigated",
            vec![Value::Number(100.0), Value::Number(100.0)],
        )
        .unwrap();
    assert_eq!(mitigated.as_f64(), Some(50.0));

    // lethal(100, 100, 40) -> 50 >= 40 -> true
    let lethal = it
        .call(
            "lethal",
            vec![
                Value::Number(100.0),
                Value::Number(100.0),
                Value::Number(40.0),
            ],
        )
        .unwrap();
    assert_eq!(lethal.as_bool(), Some(true));
}

#[test]
fn dialog_decision_fixture_runs_with_memory() {
    let src = read_fixture("dialog_decision.lua");
    let module = grindlang::parse(&src).unwrap();
    let res = grindlang::resolve::resolve(&module, &ResolveConfig::with_memory("mem")).unwrap();
    let mut it = Interpreter::new(&module, &res).unwrap();

    let mut mem = BTreeMap::new();
    mem.insert("reputation".to_string(), Value::Number(10.0));
    mem.insert("met_elder".to_string(), Value::Bool(false));
    it.set_memory("mem", Value::table(mem));

    // First greeting: not met yet -> "intro", and met_elder flips to true.
    assert_eq!(
        it.call("greeting", vec![]).unwrap().as_string().as_deref(),
        Some("intro")
    );
    assert_eq!(
        it.memory("mem")
            .unwrap()
            .field("met_elder")
            .unwrap()
            .as_bool(),
        Some(true)
    );

    // Second greeting: met, low reputation -> "neutral".
    assert_eq!(
        it.call("greeting", vec![]).unwrap().as_string().as_deref(),
        Some("neutral")
    );

    // Low-reputation choices: just the two base options.
    let choices = it.call("choices", vec![]).unwrap().as_array().unwrap();
    assert_eq!(choices.len(), 2);

    // Raise reputation; greeting becomes "warm" and a third choice appears.
    it.set_memory("mem", {
        let mut m = BTreeMap::new();
        m.insert("reputation".to_string(), Value::Number(80.0));
        m.insert("met_elder".to_string(), Value::Bool(true));
        Value::table(m)
    });
    assert_eq!(
        it.call("greeting", vec![]).unwrap().as_string().as_deref(),
        Some("warm")
    );
    let choices = it.call("choices", vec![]).unwrap().as_array().unwrap();
    assert_eq!(choices.len(), 3);
    assert_eq!(choices[2].as_string().as_deref(), Some("ask_favor"));
}
