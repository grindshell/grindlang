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

#[test]
fn the_worked_example_translates_and_checks() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ruby/stat_calc.rb"
    );
    let src = std::fs::read_to_string(path).expect("read stat_calc.rb");
    diagnose(&src, false).unwrap_or_else(|d| panic!("{}", d.render(&src)));
}
