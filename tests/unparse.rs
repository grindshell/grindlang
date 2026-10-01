//! Printing a module as Lua (`grindlang::unparse`): Lua source prints back as the same tree, and
//! a translated Ruby script prints with a note on each kind of thing in it Lua can't write.

use grindlang::unparse::to_lua;

/// `module`'s tree without its spans, which printing doesn't keep.
fn shape(module: &grindlang::Module) -> String {
    let tree = format!("{module:?}");
    let mut out = String::with_capacity(tree.len());
    let mut rest = tree.as_str();
    while let Some(at) = rest.find("Span { start: ") {
        out.push_str(&rest[..at]);
        out.push_str("Span");
        rest = &rest[at..];
        rest = &rest[rest.find('}').expect("a span's end") + 1..];
    }
    out.push_str(rest);
    out
}

#[test]
fn lua_prints_back_as_the_same_tree() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests");
    for file in [
        "fixtures/stat_calc.lua",
        "fixtures/dialog_decision.lua",
        "fixtures/closures.lua",
        "ruby_twins/stats.lua",
        "ruby_twins/loops.lua",
        "ruby_twins/control.lua",
        "ruby_twins/collect.lua",
        "ruby_twins/data.lua",
    ] {
        let src = std::fs::read_to_string(format!("{dir}/{file}")).expect("read");
        let module =
            grindlang::parse(&src).unwrap_or_else(|d| panic!("{file}:\n{}", d.render(&src)));
        let printed = to_lua(&module);
        let again = grindlang::parse(&printed).unwrap_or_else(|d| {
            panic!(
                "{file} printed as Lua that doesn't parse:\n{}\n{printed}",
                d.render(&printed)
            )
        });
        assert_eq!(
            shape(&again),
            shape(&module),
            "{file} printed as\n{printed}"
        );
        assert_eq!(
            to_lua(&again),
            printed,
            "{file} prints differently a second time"
        );
    }
}

#[cfg(feature = "ruby")]
#[test]
fn ruby_prints_with_notes_on_what_lua_cant_write() {
    use std::collections::BTreeMap;

    use grindlang::ruby::{RubyConfig, parse_module};
    use grindlang::{Type, TypeConfig};

    let mut fields = BTreeMap::new();
    fields.insert("hp".to_string(), Type::Number);
    let mut cfg = TypeConfig::default();
    cfg.memory.insert("mem".to_string(), Type::Record(fields));
    let mut ruby = RubyConfig::new().ivar_memory("mem");
    ruby.handler("on", "ON")
        .block(&[Type::Number], Type::Unit)
        .dispatch("on_event");

    let src = "\
LIMIT = 3

on :hit do |dmg|
  @hp -= dmg
end

def grade(xs)
  big = xs.select { |x| x > LIMIT }
  first = xs[0]
  label = first.nil? ? \"none\" : \"#{first}\"
  \"#{label}: #{big.length}\"
end
";
    let module = parse_module(src, &ruby, &cfg).unwrap_or_else(|d| panic!("{}", d.render(src)));
    grindlang::analyze_module(&module, &cfg).unwrap_or_else(|d| panic!("{}", d.render(src)));
    insta::assert_snapshot!("unparse_ruby", to_lua(&module));
}
