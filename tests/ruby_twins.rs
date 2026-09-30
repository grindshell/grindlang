//! The twin corpus (`PLAN.md` Phase 10, R2): each `tests/ruby_twins/NAME.rb` has a Lua twin,
//! `NAME.lua`, written the way SPEC-RUBY.md §4 says the Ruby translates. The two must lower to
//! the same IR, which tests the translator directly: the rest of the pipeline is shared.

#![cfg(feature = "ruby")]

use std::collections::BTreeMap;

use grindlang::ruby::{RubyConfig, parse_module};
use grindlang::{FnType, Type, TypeConfig};

/// The IR `module` lowers to, as text. It holds no spans or names of locals, so twins written
/// in different syntaxes can match exactly.
fn lowered(module: &grindlang::Module, cfg: &TypeConfig, src: &str) -> String {
    let (res, info) = grindlang::analyze_module(module, cfg)
        .unwrap_or_else(|d| panic!("analyze failed:\n{}", d.render(src)));
    let program = grindlang::ir::lower(module, &res, &info, cfg).expect("lower");
    format!("{program:#?}")
}

#[track_caller]
fn assert_twins(name: &str, cfg: &TypeConfig, ruby: &RubyConfig) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ruby_twins");
    let rb = std::fs::read_to_string(format!("{dir}/{name}.rb")).expect("read .rb");
    let lua = std::fs::read_to_string(format!("{dir}/{name}.lua")).expect("read .lua");
    let from_ruby = parse_module(&rb, ruby, cfg)
        .unwrap_or_else(|d| panic!("translating {name}.rb failed:\n{}", d.render(&rb)));
    let from_lua = grindlang::parse(&lua)
        .unwrap_or_else(|d| panic!("parsing {name}.lua failed:\n{}", d.render(&lua)));
    let (a, b) = (lowered(&from_ruby, cfg, &rb), lowered(&from_lua, cfg, &lua));
    if a != b {
        let line = a
            .lines()
            .zip(b.lines())
            .position(|(x, y)| x != y)
            .unwrap_or_else(|| a.lines().count().min(b.lines().count()));
        let context = |s: &str| {
            s.lines()
                .skip(line.saturating_sub(12))
                .take(18)
                .collect::<Vec<_>>()
                .join("\n")
        };
        panic!(
            "{name}.rb and {name}.lua lower differently, from line {line} of the IR:\n\
             --- from Ruby\n{}\n--- from Lua\n{}",
            context(&a),
            context(&b)
        );
    }
}

#[test]
fn stats() {
    assert_twins("stats", &TypeConfig::default(), &RubyConfig::new());
}

#[test]
fn loops() {
    assert_twins("loops", &TypeConfig::default(), &RubyConfig::new());
}

#[test]
fn control() {
    assert_twins("control", &TypeConfig::default(), &RubyConfig::new());
}

#[test]
fn data() {
    let mut fields = BTreeMap::new();
    fields.insert("hp".to_string(), Type::Number);
    fields.insert("gold".to_string(), Type::Number);
    let mut memory = BTreeMap::new();
    memory.insert("mem".to_string(), Type::Record(fields));
    let mut mem_methods = BTreeMap::new();
    mem_methods.insert(
        "add_gold".to_string(),
        FnType {
            params: vec![Type::Number],
            ret: Box::new(Type::Number),
        },
    );
    let mut methods = BTreeMap::new();
    methods.insert("mem".to_string(), mem_methods);
    let cfg = TypeConfig {
        host_functions: BTreeMap::new(),
        memory,
        methods,
    };
    assert_twins("data", &cfg, &RubyConfig::new().ivar_memory("mem"));
}
