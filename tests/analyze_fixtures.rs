//! Integration test: the spec's worked-example modules type-check end to end, producing
//! the expected export signatures.

use std::collections::BTreeMap;

use grindlang::{Type, TypeConfig};

fn read_fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

#[test]
fn stat_calc_fixture_type_checks() {
    let src = read_fixture("stat_calc.lua");
    let (_m, _res, info) = grindlang::analyze(&src, &TypeConfig::default())
        .unwrap_or_else(|d| panic!("typecheck failed:\n{}", d.render(&src)));

    assert_eq!(info.exports["ARMOR_K"], Type::Number);
    assert_eq!(
        info.exports["mitigated"].to_string(),
        "fn(number, number) -> number"
    );
    assert_eq!(
        info.exports["lethal"].to_string(),
        "fn(number, number, number) -> bool"
    );
}

#[test]
fn dialog_decision_fixture_type_checks() {
    let src = read_fixture("dialog_decision.lua");

    // Host memory schema: record { reputation: number, met_elder: bool }.
    let mut rec = BTreeMap::new();
    rec.insert("reputation".to_string(), Type::Number);
    rec.insert("met_elder".to_string(), Type::Bool);
    let mut memory = BTreeMap::new();
    memory.insert("mem".to_string(), Type::Record(rec));
    let cfg = TypeConfig {
        host_functions: BTreeMap::new(),
        memory,
    };

    let (_m, _res, info) = grindlang::analyze(&src, &cfg)
        .unwrap_or_else(|d| panic!("typecheck failed:\n{}", d.render(&src)));

    // Curated export table: greeting + choices only.
    assert_eq!(info.exports["greeting"].to_string(), "fn() -> string");
    assert_eq!(info.exports["choices"].to_string(), "fn() -> array<string>");
    assert!(!info.exports.contains_key("elder_greeting"));
}
