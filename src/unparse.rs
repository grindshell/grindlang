//! Printing a module as Lua source, whichever surface it was parsed from.
//!
//! [`to_lua`] is for reading what a module became: the `grindlang` CLI's `--print-lua` shows what
//! the Ruby surface translated a script into (`SPEC-RUBY.md`). A module parsed from Lua prints
//! back as source that parses to the same tree, up to spans, comments, and layout. A translated
//! module can hold what Lua has no spelling for, which prints in a form a reader can follow but
//! the Lua parser doesn't take back as the same program, and a comment at the top explains each
//! kind the module holds:
//!
//! - a conditional expression (`SPEC.md` §3.5) prints as Luau's `(if c then a else b)`;
//! - an index that counts from 0 (C9) as `xs[i --[[from 0]]]`;
//! - a function that returns `nil` where it falls off its end (C4) as any other;
//! - a name the translator made up, which holds a `#` (`map#1`), as it is;
//! - a function type in an annotation, which no source spells, as the checker writes it,
//!   `fn(number) -> bool`.

use crate::ast::{
    BinOp, Block, Fallthrough, Field, FnAnnotations, FuncBody, FuncDecl, IterExpr, Module, Stat,
    StatKind, TopDecl, TypeAnn, UnOp,
};
use crate::ast::{Expr, ExprKind};
use crate::surface::Surface;

/// Print `module` as Lua source.
///
/// ```
/// let module = grindlang::parse("function f(x) return x*2+1 end").unwrap();
/// assert_eq!(grindlang::unparse::to_lua(&module), "function f(x)\n  return x * 2 + 1\nend\n");
/// ```
pub fn to_lua(module: &Module) -> String {
    let mut p = Printer::default();
    for (i, decl) in module.decls.iter().enumerate() {
        let is_fn = matches!(decl, TopDecl::Function(_));
        let after_fn = i > 0 && matches!(module.decls[i - 1], TopDecl::Function(_));
        if i > 0 && (is_fn || after_fn) {
            p.out.push('\n');
        }
        match decl {
            TopDecl::Function(f) => p.function(f),
            TopDecl::Const(c) => {
                let name = p.name(&c.name.node);
                let value = p.expr(&c.value, 0);
                p.line(0, &format!("{name} = {value}"));
            }
            TopDecl::Stat(s) => p.stat(s, 0),
        }
    }
    if let Some(export) = &module.export {
        if !module.decls.is_empty() {
            p.out.push('\n');
        }
        let table = p.table(&export.node, 0);
        p.line(0, &format!("return {table}"));
    }
    p.notes(module.surface) + &p.out
}

/// Binding strength, as the Lua parser reads it: an operand binding less tightly than its
/// position needs is parenthesized.
const OR: u8 = 1;
const AND: u8 = 2;
const COMPARE: u8 = 3;
const CONCAT: u8 = 4;
const ADD: u8 = 5;
const MUL: u8 = 6;
const UNARY: u8 = 7;
const POW: u8 = 8;
const ATOM: u8 = 9;

const INDENT: &str = "  ";

/// A table whose fields fit in this many characters prints on one line.
const INLINE_TABLE: usize = 72;

#[derive(Default)]
struct Printer {
    out: String,
    /// What the module holds that Lua has no spelling for, for the comment at the top.
    cond: bool,
    zero_based: bool,
    nil_fallthrough: bool,
    made_names: bool,
    fn_types: bool,
}

impl Printer {
    /// The comment explaining what in the module Lua can't write, if anything.
    fn notes(&self, surface: Surface) -> String {
        let mut notes = Vec::new();
        if self.cond {
            notes.push("`(if c then a else b)` is a conditional expression (SPEC.md §3.5)");
        }
        if self.zero_based {
            notes.push(
                "`xs[i --[[from 0]]]` counts from 0 when `xs` is an array (SPEC-RUBY.md §4.9)",
            );
        }
        if self.nil_fallthrough {
            notes.push("a function returns nil where it falls off its end (SPEC-RUBY.md §4.2)");
        }
        if self.made_names {
            notes.push("a name with a `#` is one the translator made up");
        }
        if self.fn_types {
            notes.push("`fn(T) -> R` in an annotation is a function type, which Lua can't write");
        }
        let mut out = String::new();
        if surface == Surface::Ruby {
            out.push_str("-- Translated from Ruby (SPEC-RUBY.md).\n");
        }
        if !notes.is_empty() {
            out.push_str("-- Parsed as Lua, this isn't the same program:\n");
            for note in notes {
                out.push_str(&format!("-- - {note}\n"));
            }
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }

    fn line(&mut self, depth: usize, text: &str) {
        self.out.push_str(&INDENT.repeat(depth));
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn name(&mut self, name: &str) -> String {
        self.made_names |= name.contains('#');
        name.to_string()
    }

    fn names<'a>(&mut self, names: impl IntoIterator<Item = &'a String>) -> String {
        let names: Vec<String> = names.into_iter().map(|n| self.name(n)).collect();
        names.join(", ")
    }

    // ---- declarations and statements -------------------------------------------------------

    fn function(&mut self, f: &FuncDecl) {
        self.annotations(&f.annotations);
        let local = if f.local { "local " } else { "" };
        let name = self.name(&f.name.node);
        let body = self.body(&f.body, 0);
        self.line(0, &format!("{local}function {name}{body}"));
    }

    fn annotations(&mut self, ann: &FnAnnotations) {
        for p in &ann.params {
            let ty = self.type_ann(&p.ty.node);
            let name = self.name(&p.name.node);
            self.line(0, &format!("---@param {name} {ty}"));
        }
        for r in &ann.ret {
            let ty = self.type_ann(&r.node);
            self.line(0, &format!("---@return {ty}"));
        }
    }

    /// A function's parameters, body, and `end`, the body indented one past `depth`.
    fn body(&mut self, body: &FuncBody, depth: usize) -> String {
        self.nil_fallthrough |= body.fallthrough == Fallthrough::Nil;
        let params = self.names(body.params.iter().map(|p| &p.node));
        let block = self.nested(&body.block, depth + 1);
        format!("({params})\n{block}{}end", INDENT.repeat(depth))
    }

    /// `block`'s lines at `depth`, as text.
    fn nested(&mut self, block: &Block, depth: usize) -> String {
        let outer = std::mem::take(&mut self.out);
        self.block(block, depth);
        std::mem::replace(&mut self.out, outer)
    }

    fn block(&mut self, block: &Block, depth: usize) {
        for s in &block.stats {
            self.stat(s, depth);
        }
        if let Some(ret) = &block.ret {
            let values = self.list(&ret.exprs, depth);
            if values.is_empty() {
                self.line(depth, "return");
            } else {
                self.line(depth, &format!("return {values}"));
            }
        }
    }

    fn stat(&mut self, stat: &Stat, depth: usize) {
        match &stat.kind {
            StatKind::Empty => self.line(depth, ";"),
            StatKind::Local { names, exprs } => {
                let names = self.names(names.iter().map(|n| &n.node));
                if exprs.is_empty() {
                    self.line(depth, &format!("local {names}"));
                } else {
                    let values = self.list(exprs, depth);
                    self.line(depth, &format!("local {names} = {values}"));
                }
            }
            StatKind::LocalFunction { name, body } => {
                let name = self.name(&name.node);
                let body = self.body(body, depth);
                self.line(depth, &format!("local function {name}{body}"));
            }
            StatKind::Assign { targets, exprs } => {
                let targets = self.list(targets, depth);
                let values = self.list(exprs, depth);
                self.line(depth, &format!("{targets} = {values}"));
            }
            StatKind::Call(e) => {
                let call = self.expr(e, depth);
                self.line(depth, &call);
            }
            StatKind::Do(b) => {
                self.line(depth, "do");
                self.block(b, depth + 1);
                self.line(depth, "end");
            }
            StatKind::While { cond, body } => {
                let cond = self.expr(cond, depth);
                self.line(depth, &format!("while {cond} do"));
                self.block(body, depth + 1);
                self.line(depth, "end");
            }
            StatKind::If { arms, else_block } => {
                for (i, (cond, body)) in arms.iter().enumerate() {
                    let cond = self.expr(cond, depth);
                    let keyword = if i == 0 { "if" } else { "elseif" };
                    self.line(depth, &format!("{keyword} {cond} then"));
                    self.block(body, depth + 1);
                }
                if let Some(b) = else_block {
                    self.line(depth, "else");
                    self.block(b, depth + 1);
                }
                self.line(depth, "end");
            }
            StatKind::NumericFor {
                var,
                start,
                end,
                step,
                body,
            } => {
                let var = self.name(&var.node);
                let mut range = format!("{}, {}", self.expr(start, depth), self.expr(end, depth));
                if let Some(step) = step {
                    range = format!("{range}, {}", self.expr(step, depth));
                }
                self.line(depth, &format!("for {var} = {range} do"));
                self.block(body, depth + 1);
                self.line(depth, "end");
            }
            StatKind::GenericFor { names, iter, body } => {
                let names = self.names(names.iter().map(|n| &n.node));
                let iter = match iter {
                    IterExpr::IPairs { arg, .. } => format!("ipairs({})", self.expr(arg, depth)),
                    IterExpr::Pairs { arg, .. } => format!("pairs({})", self.expr(arg, depth)),
                };
                self.line(depth, &format!("for {names} in {iter} do"));
                self.block(body, depth + 1);
                self.line(depth, "end");
            }
            StatKind::Break => self.line(depth, "break"),
            StatKind::Continue => self.line(depth, "continue"),
        }
    }

    // ---- expressions -----------------------------------------------------------------------

    /// Expressions separated by commas.
    fn list(&mut self, exprs: &[Expr], depth: usize) -> String {
        let parts: Vec<String> = exprs.iter().map(|e| self.expr(e, depth)).collect();
        parts.join(", ")
    }

    /// `e` as text whose first line continues one at `depth`, and whose later lines (a function's
    /// body, a long table's fields) are indented from `depth`.
    fn expr(&mut self, e: &Expr, depth: usize) -> String {
        match &e.kind {
            ExprKind::Nil => "nil".into(),
            ExprKind::Bool(b) => b.to_string(),
            ExprKind::Number(n) => number(*n),
            ExprKind::Str(s) => string(s),
            ExprKind::Function(body) => format!("function{}", self.body(body, depth)),
            ExprKind::Name(n) => self.name(n),
            ExprKind::Index {
                base,
                index,
                zero_based,
            } => {
                let base = self.prefix(base, depth);
                let index = self.expr(index, depth);
                self.zero_based |= *zero_based;
                let from = if *zero_based { " --[[from 0]]" } else { "" };
                format!("{base}[{index}{from}]")
            }
            ExprKind::Field { base, name } => {
                let base = self.prefix(base, depth);
                format!("{base}.{}", self.name(&name.node))
            }
            ExprKind::Call { callee, args } => {
                let callee = self.prefix(callee, depth);
                format!("{callee}({})", self.list(args, depth))
            }
            ExprKind::MethodCall {
                receiver,
                method,
                args,
            } => {
                let receiver = self.prefix(receiver, depth);
                let method = self.name(&method.node);
                format!("{receiver}:{method}({})", self.list(args, depth))
            }
            ExprKind::Table(fields) => self.table(fields, depth),
            ExprKind::Binary { op, lhs, rhs } => {
                let p = binop_strength(*op);
                let (left, right) = match op {
                    BinOp::Concat | BinOp::Pow => (p + 1, p),
                    _ => (p, p + 1),
                };
                let lhs = self.operand(lhs, left, depth);
                // A unary operation on the right reads as one however tightly the operator binds:
                // the parser takes an operator's right side as a whole unary operation first.
                let rhs = if strength(rhs) == UNARY {
                    self.expr(rhs, depth)
                } else {
                    self.operand(rhs, right, depth)
                };
                format!("{lhs} {} {rhs}", op.symbol())
            }
            ExprKind::Unary { op, operand } => {
                let operand = self.operand(operand, UNARY, depth);
                match op {
                    UnOp::Not => format!("not {operand}"),
                    // `--` would start a comment.
                    UnOp::Neg if operand.starts_with('-') => format!("- {operand}"),
                    _ => format!("{}{operand}", op.symbol()),
                }
            }
            ExprKind::Paren(inner) => format!("({})", self.expr(inner, depth)),
            ExprKind::Cond { cond, then, els } => {
                self.cond = true;
                let cond = self.expr(cond, depth);
                let then = self.expr(then, depth);
                let els = self.expr(els, depth);
                format!("(if {cond} then {then} else {els})")
            }
        }
    }

    /// `e` where it must bind at least as tightly as `min`.
    fn operand(&mut self, e: &Expr, min: u8, depth: usize) -> String {
        let text = self.expr(e, depth);
        if strength(e) < min {
            format!("({text})")
        } else {
            text
        }
    }

    /// `e` where Lua takes only a name or a parenthesized expression, with suffixes: what is
    /// indexed or called.
    fn prefix(&mut self, e: &Expr, depth: usize) -> String {
        let text = self.expr(e, depth);
        match e.kind {
            ExprKind::Name(_)
            | ExprKind::Paren(_)
            | ExprKind::Index { .. }
            | ExprKind::Field { .. }
            | ExprKind::Call { .. }
            | ExprKind::MethodCall { .. }
            | ExprKind::Cond { .. } => text,
            _ => format!("({text})"),
        }
    }

    fn table(&mut self, fields: &[Field], depth: usize) -> String {
        if fields.is_empty() {
            return "{}".into();
        }
        let fields: Vec<String> = fields
            .iter()
            .map(|f| match f {
                Field::Positional(e) => self.expr(e, depth + 1),
                Field::Named { name, value } => {
                    let name = self.name(&name.node);
                    format!("{name} = {}", self.expr(value, depth + 1))
                }
                Field::Keyed { key, value } => {
                    let key = self.expr(key, depth + 1);
                    format!("[{key}] = {}", self.expr(value, depth + 1))
                }
            })
            .collect();
        let width: usize = fields.iter().map(|f| f.len() + 2).sum();
        if width <= INLINE_TABLE && !fields.iter().any(|f| f.contains('\n')) {
            return format!("{{{}}}", fields.join(", "));
        }
        let inner = INDENT.repeat(depth + 1);
        let lines: String = fields.iter().map(|f| format!("{inner}{f},\n")).collect();
        format!("{{\n{lines}{}}}", INDENT.repeat(depth))
    }

    fn type_ann(&mut self, ty: &TypeAnn) -> String {
        match ty {
            TypeAnn::Named(n) => n.clone(),
            TypeAnn::Optional(t) => format!("{}?", self.type_postfix(t)),
            TypeAnn::Array(t) => format!("{}[]", self.type_postfix(t)),
            TypeAnn::Map(t) => format!("{{ [string]: {} }}", self.type_ann(t)),
            TypeAnn::Record(fields) => {
                let fields: Vec<String> = fields
                    .iter()
                    .map(|(k, t)| format!("{k}: {}", self.type_ann(t)))
                    .collect();
                format!("{{ {} }}", fields.join(", "))
            }
            TypeAnn::Function { params, ret } => {
                self.fn_types = true;
                let params: Vec<String> = params.iter().map(|t| self.type_ann(t)).collect();
                let ret = match ret {
                    Some(t) => self.type_ann(t),
                    None => "()".into(),
                };
                format!("fn({}) -> {ret}", params.join(", "))
            }
        }
    }

    /// A type that `?` or `[]` follows: a function type is parenthesized, since its return type
    /// would take them otherwise.
    fn type_postfix(&mut self, ty: &TypeAnn) -> String {
        let text = self.type_ann(ty);
        if matches!(ty, TypeAnn::Function { .. }) {
            format!("({text})")
        } else {
            text
        }
    }
}

fn binop_strength(op: BinOp) -> u8 {
    match op {
        BinOp::Or => OR,
        BinOp::And => AND,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => COMPARE,
        BinOp::Concat => CONCAT,
        BinOp::Add | BinOp::Sub => ADD,
        BinOp::Mul | BinOp::Div | BinOp::FloorDiv | BinOp::Mod => MUL,
        BinOp::Pow => POW,
    }
}

/// How tightly `e` binds as printed.
fn strength(e: &Expr) -> u8 {
    match &e.kind {
        ExprKind::Binary { op, .. } => binop_strength(*op),
        ExprKind::Unary { .. } => UNARY,
        // A negative number prints with a `-`, which Lua reads as negation.
        ExprKind::Number(n) if !n.is_nan() && n.is_sign_negative() => UNARY,
        _ => ATOM,
    }
}

/// A number as a literal Lua reads back as the same `f64`.
fn number(n: f64) -> String {
    if n.is_nan() {
        "(0/0)".into()
    } else if n.is_infinite() {
        // Too big for an `f64`, so it reads as infinity.
        String::from(if n > 0.0 { "1e999" } else { "-1e999" })
    } else if n == 0.0 && n.is_sign_negative() {
        "-0".into()
    } else if n == n.trunc() && n.abs() < 1e15 {
        (n as i64).to_string()
    } else {
        format!("{n:?}")
    }
}

/// A string literal with what Lua must escape escaped.
fn string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Three digits, so a digit after it isn't read as part of it.
            c if c.is_ascii_control() => out.push_str(&format!("\\{:03}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(src: &str) -> String {
        to_lua(&crate::parse(src).unwrap_or_else(|d| panic!("{}", d.render(src))))
    }

    #[test]
    fn operators_keep_their_grouping() {
        let src = "function f(a, b, c)\n  \
                   return (a + b) * c, a - (b - c), a ^ b ^ c, (a ^ b) ^ c, -a ^ 2, (-a) ^ 2, \
                   a * -b, - -a, not (a == b), #\"x\" .. \"y\" .. (\"z\" .. \"w\")\nend\n";
        assert_eq!(round_trip(src), src);
    }

    #[test]
    fn numbers_and_strings_read_back_the_same() {
        for n in [
            0.5,
            1e-7,
            1.5e300,
            123456789.25,
            1e20,
            -0.0,
            -3.0,
            f64::INFINITY,
        ] {
            let text = number(n);
            let src = format!("X = {text}");
            let module = crate::parse(&src).unwrap();
            let TopDecl::Const(c) = &module.decls[0] else {
                panic!("a constant")
            };
            let back = match &c.value.kind {
                ExprKind::Number(m) => *m,
                ExprKind::Unary {
                    op: UnOp::Neg,
                    operand,
                } => match operand.kind {
                    ExprKind::Number(m) => -m,
                    _ => panic!("{text}"),
                },
                _ => panic!("{text}"),
            };
            assert_eq!(back.to_bits(), n.to_bits(), "{text}");
        }
        let s = "a \"quote\" \\ back\nslash\t\u{1}9 é";
        let module = crate::parse(&format!("X = {}", string(s))).unwrap();
        let TopDecl::Const(c) = &module.decls[0] else {
            panic!("a constant")
        };
        assert_eq!(c.value.kind, ExprKind::Str(s.into()));
    }

    #[test]
    fn statements_and_tables_print_as_written() {
        let src = "\
---@param xs number[]?
---@param m { [string]: { a: number, b: string? } }
---@return number
local function total(xs, m)
  local s = 0
  local t
  if xs == nil then
    return 0
  elseif #xs > 2 then
    s = 1
  else
  end
  for i = 1, 10, 2 do
    while true do
      break
    end
    continue
  end
  for _, x in ipairs(xs) do
    do
      local function g(y)
        return y
      end
      s = s + g(x)
    end
  end
  for k, v in pairs(m) do
    t = {
      1,
      {a = k},
      [\"key\"] = v.a,
      f = function(z)
        return z
      end,
    }
  end
  mem:add(s, m[\"x\"])
  return s
end

local n
K = 1

return {total = total, K = K}
";
        assert_eq!(round_trip(src), src);
    }

    #[test]
    fn a_long_table_prints_a_field_per_line() {
        let src = "T = {alpha = \"a long string value\", beta = \"another long string\", \
                   gamma = \"and one more\"}";
        assert_eq!(
            round_trip(src),
            "T = {\n  alpha = \"a long string value\",\n  beta = \"another long string\",\n  \
             gamma = \"and one more\",\n}\n"
        );
    }
}
