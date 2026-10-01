//! Translating the Ruby surface (`PLAN.md` Phase 10, step R2): what the translator rejects,
//! with its code, and how the checker words its errors for a Ruby script. Each group's rendered
//! diagnostics are pinned in one snapshot.

#![cfg(feature = "ruby")]

use std::collections::BTreeMap;

use grindlang::ruby::{RubyConfig, parse_module};
use grindlang::{Diagnostics, Type, TypeConfig};

/// Translate and check `src`, with `@name` bound to a memory `mem` when `ivars` is set.
fn diagnose(src: &str, ivars: bool) -> Result<(), Diagnostics> {
    let mut cfg = TypeConfig::default();
    let mut ruby = RubyConfig::new();
    if ivars {
        let mut fields = BTreeMap::new();
        fields.insert("hp".to_string(), Type::Number);
        cfg.memory.insert("mem".to_string(), Type::Record(fields));
        ruby = ruby.ivar_memory("mem");
    }
    let module = parse_module(src, &ruby, &cfg)?;
    grindlang::analyze_module(&module, &cfg).map(|_| ())
}

/// Each group, with a source for every case and the code its first diagnostic has.
const GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "translation",
        &[
            ("def f(x)\n  x\nend\nf(1) { |y| y }\n", "E0521"),
            ("def f\n  g(a: 1)\nend\n", "E0523"),
            ("def f\n  @hp\nend\n", "E0524"),
            ("limit = 3\ndef f\n  limit\nend\n", "E0524"),
            ("def f(xs)\n  xs[-1]\nend\n", "E0525"),
        ],
    ),
    (
        "semantics",
        &[
            ("def f(name)\n  \"hi \" + name\nend\n", "E0600"),
            ("def f(xs)\n  xs.sort()\nend\n", "E0601"),
            ("def f(x)\n  x.even?\nend\n", "E0601"),
            ("def f(x)\n  Math.tan(x)\nend\n", "E0601"),
            ("def f(x)\n  x.abs(2)\nend\n", "E0602"),
            ("def f(xs)\n  xs.each\nend\n", "E0602"),
            ("def f(xs)\n  y = xs.each { |x| x }\nend\n", "E0602"),
            ("def f(xs)\n  xs.each { |a, b, c| a }\nend\n", "E0602"),
            ("def f(xs, x)\n  y = xs << x\nend\n", "E0602"),
            ("def f(xs)\n  n = xs.push(1)\nend\n", "E0602"),
            ("def f\n  g.last\nend\n", "E0602"),
            ("def f\n  g[0] += 1\nend\n", "E0602"),
            ("def f\n  r = 1..3\nend\n", "E0603"),
            ("def f\n  X = 1\nend\n", "E0604"),
            ("if true\n  X = 1\nend\n", "E0604"),
            ("A, B = 1, 2\n", "E0604"),
            ("X = 1, 2\n", "E0604"),
            ("def f(x)\n  x + 1\n  x\nend\n", "E0605"),
            ("def f(x)\n  return x\n  x + 1\nend\n", "E0606"),
            (
                "def f(xs)\n  xs.each do |x|\n    next\n    x\n  end\nend\n",
                "E0606",
            ),
            ("def f\n  g = ->(x) { x }\n  g(1)\nend\n", "E0607"),
            ("def f\n  h = {}\nend\n", "E0608"),
            (
                "def f(c)\n  1 + (if c then y = 1; y else 2 end)\nend\n",
                "E0609",
            ),
            (
                "def f\n  1 + (case g when 1 then 2 else 3 end)\nend\n",
                "E0609",
            ),
            (
                "def f(c)\n  x = if c\n    y = 1\n  else\n    2\n  end\n  x\nend\n",
                "E0609",
            ),
            ("X = if true\n  y = 1\n  y\nend\n", "E0609"),
        ],
    ),
    (
        "collections",
        &[
            ("def f(xs)\n  1 + xs.sum\nend\n", "E0610"),
            ("def f(xs)\n  g(xs.map { |x| x })\nend\n", "E0610"),
            ("def f(xs)\n  xs.map { |x| x }\n  1\nend\n", "E0610"),
            ("def f(xs)\n  t = 0\n  t += xs.sum\nend\n", "E0610"),
            (
                "def f(xs)\n  if xs.any? { |x| x > 1 }\n    1\n  end\nend\n",
                "E0610",
            ),
            ("def f(xs)\n  n = xs.count\nend\n", "E0602"),
            ("def f(xs)\n  ys = xs.map(1) { |x| x }\nend\n", "E0602"),
            ("def f(xs)\n  ys = xs.map { |a, b| a }\nend\n", "E0602"),
            ("def f(xs)\n  ys = xs.map { }\nend\n", "E0611"),
            ("def f(xs)\n  ys = xs.map { |x| y = x }\nend\n", "E0611"),
            (
                "def f(xs)\n  ys = xs.select do |x|\n    next if x > 1\n    true\n  end\nend\n",
                "E0611",
            ),
            ("def f(xs)\n  y = xs.find { |x| break }\nend\n", "E0611"),
            (
                "def f(xs)\n  ys = xs.map do |x|\n    return 1\n    x\n  end\nend\n",
                "E0606",
            ),
        ],
    ),
    (
        "checker_in_ruby",
        &[
            (
                "# @param m { [string]: number }\ndef f(m)\n  t = 0\n  m.each { |v| t += v }\n  t\nend\n",
                "E0412",
            ),
            (
                "# @param m { [string]: number }\ndef f(m)\n  m.sum\nend\n",
                "E0412",
            ),
            ("def f(xs)\n  xs.select { |x| x.to_f }\nend\n", "E0400"),
            (
                "# @param a string\n# @param b string\ndef f(a, b)\n  a + b\nend\n",
                "E0401",
            ),
            ("def f(s)\n  s.to_f + 1\nend\n", "E0401"),
            (
                "def f(s)\n  n = s.to_f\n  if n\n    1\n  else\n    0\n  end\nend\n",
                "E0400",
            ),
            (
                "def two\n  return 1, 2\nend\ndef f\n  two + 1\nend\n",
                "E0415",
            ),
            ("return 1\n", "E0309"),
            ("def f\n  next\nend\n", "E0311"),
            ("# @param x nope[\ndef f(x)\n  x\nend\n", "E0110"),
            ("def f(c)\n  x = c ? 1 : \"one\"\nend\n", "E0455"),
            // Reaching the end returns `nil`, which a declared `number` can't hold.
            ("# @return number\ndef f(c)\n  1 if c\nend\n", "E0480"),
        ],
    ),
];

#[test]
fn rejected_forms_report_their_codes() {
    for (group, cases) in GROUPS {
        for (src, code) in *cases {
            let ivars = src.contains('@') && !src.contains("@param");
            let diags = diagnose(src, false).err().unwrap_or_else(|| {
                panic!("{group}: expected {code}, but this translated and checked:\n{src}")
            });
            let first = diags.iter().next().expect("a diagnostic").code;
            assert_eq!(first, *code, "{group}:\n{src}\n{}", diags.render(src));
            // With `@` bound, only the missing binding goes away.
            if ivars && *code == "E0524" {
                diagnose(src, true).unwrap_or_else(|d| panic!("{}", d.render(src)));
            }
        }
    }
}

#[test]
fn snapshot_translation_diagnostics() {
    for (group, cases) in GROUPS {
        let rendered = cases
            .iter()
            .map(|(src, _)| match diagnose(src, false) {
                Ok(()) => format!("{src}\n(translated and checked with no diagnostics)"),
                Err(d) => d.render(src),
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        insta::assert_snapshot!(format!("ruby_translate_{group}"), rendered);
    }
}

/// The keywords the `dsl` cases use, with `@name` the memory `mem`.
fn dsl_config() -> (TypeConfig, RubyConfig) {
    let mut cfg = TypeConfig::default();
    let mut fields = BTreeMap::new();
    fields.insert("hp".to_string(), Type::Number);
    fields.insert("name".to_string(), Type::String);
    cfg.memory.insert("mem".to_string(), Type::Record(fields));
    let mut ruby = RubyConfig::new().ivar_memory("mem");
    ruby.handler("on", "ON")
        .block(&[Type::Number], Type::Unit)
        .dispatch("on_event");
    ruby.data("node", "NODES")
        .list("say", "lines", Type::String)
        .child("choice", "choices", |c| {
            c.positional("text", Type::String).keyword(
                "if",
                "cond",
                Type::optional(Type::function(vec![], Type::Bool)),
            )
        });
    ruby.data("item", "ITEMS")
        .positional("name", Type::String)
        .keyword("cost", "cost", Type::Number)
        .block("use", &[Type::Number], Type::Bool);
    (cfg, ruby)
}

fn diagnose_dsl(src: &str) -> Result<grindlang::ast::Module, Diagnostics> {
    let (cfg, ruby) = dsl_config();
    let module = parse_module(src, &ruby, &cfg)?;
    grindlang::analyze_module(&module, &cfg)?;
    Ok(module)
}

/// Each source using the keywords of [`dsl_config`] wrongly, with the code its first diagnostic
/// has.
const DSL: &[(&str, &str)] = &[
    // A keyword where it can't be, or a name a keyword takes.
    ("def f\n  on :hit do end\nend\n", "E0612"),
    ("on :hit do end if true\n", "E0612"),
    ("say \"hi\"\n", "E0612"),
    ("def f\n  node\nend\n", "E0612"),
    ("def on_event(id, x)\n  x\nend\n", "E0612"),
    ("def node\n  1\nend\n", "E0612"),
    ("ON = 1\n", "E0612"),
    // Unknown keywords and named arguments.
    ("node :a do\n  x = 1\nend\n", "E0613"),
    ("node :a do\n  on :hit do end\nend\n", "E0613"),
    ("node :a do\n  choice \"x\", when: 1\nend\n", "E0613"),
    ("on :hit, if: 1 do |d| end\n", "E0613"),
    ("node :a do\n  say \"x\", loud: true\nend\n", "E0613"),
    // Arguments that don't fit.
    ("on hit do |d| end\n", "E0614"),
    ("on \"#{1}\" do |d| end\n", "E0614"),
    ("on :hit, 2 do |d| end\n", "E0614"),
    ("on :hit do |a, b| end\n", "E0614"),
    ("node :a, \"x\"\n", "E0614"),
    ("node :a do\n  say \"x\", \"y\"\nend\n", "E0614"),
    ("node :a do\n  say \"x\" do end\nend\n", "E0614"),
    ("node :a do |x|\n  say \"x\"\nend\n", "E0614"),
    (
        "node :a do\n  choice \"x\", if: -> { true }, if: -> { false }\nend\n",
        "E0614",
    ),
    (
        "node :a do\n  choice \"x\", if: ->(a) { true }\nend\n",
        "E0614",
    ),
    // Missing ids and fields.
    ("on do |d| end\n", "E0615"),
    ("on :hit\n", "E0615"),
    ("node :a do\n  choice\nend\n", "E0615"),
    ("node :a do\n  say\nend\n", "E0615"),
    ("item :a, \"Sword\" do |n| true end\n", "E0615"),
    ("item :a, \"Sword\", cost: 1\n", "E0615"),
    // A block that doesn't fit is reported once, not as missing too.
    ("item :a, \"Sword\", cost: 1 do |n, m| true end\n", "E0614"),
    // Repeated ids.
    ("on :hit do end\non :hit do end\n", "E0616"),
    ("node :a\nnode \"a\"\n", "E0616"),
    // The checker, against the declared types, where the script wrote the mistake.
    ("on :hit do |d|\n  @name = d\nend\n", "E0402"),
    ("node :a do\n  say 5\nend\n", "E0402"),
    ("node :a do\n  choice \"x\", if: -> { 1 }\nend\n", "E0413"),
    ("item :a, \"Sword\", cost: 1 do |n|\n  n\nend\n", "E0413"),
    // A keyword's arguments are top-level code; its blocks run later.
    ("node :a do\n  say @name\nend\n", "E0310"),
];

#[test]
fn keyword_misuse_reports_its_codes() {
    for (src, code) in DSL {
        let diags = diagnose_dsl(src)
            .err()
            .unwrap_or_else(|| panic!("expected {code}, but this translated and checked:\n{src}"));
        let first = diags.iter().next().expect("a diagnostic").code;
        assert_eq!(first, *code, "{src}\n{}", diags.render(src));
    }
    let rendered = DSL
        .iter()
        .map(|(src, _)| match diagnose_dsl(src) {
            Ok(_) => format!("{src}\n(translated and checked with no diagnostics)"),
            Err(d) => d.render(src),
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    insta::assert_snapshot!("ruby_translate_dsl", rendered);
}

#[test]
fn keywords_export_their_tables_and_dispatch_functions() {
    let src = "on :hit do |d| @hp -= d end\nnode :a do\n  say \"hi\"\n  choice \"x\"\nend\n\
               item :s, \"Sword\", cost: 2 do |n| n > 1 end\n";
    let (cfg, ruby) = dsl_config();
    let module = parse_module(src, &ruby, &cfg).unwrap_or_else(|d| panic!("{}", d.render(src)));
    let (_, info) =
        grindlang::analyze_module(&module, &cfg).unwrap_or_else(|d| panic!("{}", d.render(src)));
    let exports: Vec<String> = info
        .exports
        .iter()
        .map(|(name, ty)| format!("{name}: {ty}"))
        .collect();
    assert_eq!(
        exports,
        [
            "ITEMS: map<string, record { cost: number, name: string, use: fn(number) -> bool }>",
            "NODES: map<string, record { choices: array<record { cond: fn() -> bool?, text: \
             string }>, lines: array<string> }>",
            "ON: map<string, fn(number) -> ()>",
            "on_event: fn(string, number) -> ()",
        ]
    );
    // A private `def` makes the module list its exports, which keep the keywords'.
    let private = format!("{src}private def helper\n  1\nend\n");
    let module = parse_module(&private, &ruby, &cfg).unwrap();
    let (_, info) = grindlang::analyze_module(&module, &cfg).unwrap();
    let names: Vec<&str> = info.exports.keys().map(String::as_str).collect();
    assert_eq!(names, ["ITEMS", "NODES", "ON", "on_event"]);
    // With no handler, the dispatch function does nothing, and there is no table.
    let module = parse_module("", &ruby, &cfg).unwrap();
    let (_, info) = grindlang::analyze_module(&module, &cfg).unwrap();
    assert_eq!(
        info.exports["on_event"].to_string(),
        "fn(string, number) -> ()"
    );
    assert!(!info.exports.contains_key("ON"));
}

#[test]
fn the_worked_example_translates_and_checks() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ruby/stat_calc.rb"
    );
    let src = std::fs::read_to_string(path).expect("read stat_calc.rb");
    diagnose(&src, false).unwrap_or_else(|d| panic!("{}", d.render(&src)));
}
