//! Where each Ruby local is declared (`SPEC-RUBY.md` §4.3).
//!
//! Ruby has no declarations: a name is a local from the first assignment to it that the parser
//! has seen, and until then the same name is a method call. A `def` opens a scope that sees
//! nothing outside it; a block or lambda opens one that sees its enclosing scope's locals, and a
//! local first assigned inside it belongs to it. [`analyze`] walks a program in source order,
//! applying that rule, and records for the translator:
//!
//! - which `Name` expressions read a local (the rest are calls with no arguments);
//! - which first assignments can declare their local in place, as `local x = …`: those whose
//!   every other use is inside the Lua block the assignment lands in, so the declaration is in
//!   scope wherever the local is used;
//! - the rest, which a bare `local x` at the start of the owning scope declares instead, typed
//!   as optional, since it holds `nil` until assigned.
//!
//! A first assignment is also declared at the start of its scope when it reads the local itself
//! (`x ||= 1`, or a lambda that calls itself), or shares a multiple assignment with a target
//! that isn't declared there, since one `local` can't declare some targets and assign others.
//!
//! A local first assigned directly in a `while` body is declared in that body, so it is a new
//! variable on each pass, where Ruby has one for the whole method; only a lambda made in the loop
//! can tell (`SPEC-RUBY.md` §4.3).

use std::collections::{HashMap, HashSet};

use super::ast::*;
use crate::ast::Spanned;
use crate::diagnostics::Span;

/// What the translator needs to know about a program's locals.
#[derive(Debug, Default)]
pub(super) struct Scopes {
    /// The span of every `Name` expression that reads a local, of every assignment target
    /// naming one, and of the name of every call `f(…)` whose `f` is a local.
    pub locals: HashSet<Span>,
    /// First assignments that declare their local in place, `local x = …`, by the span of the
    /// assigned name.
    pub in_place: HashSet<Span>,
    /// The locals each scope declares at its start with a bare `local`, by the span of what
    /// opens the scope (the program, a `def`, a block, or a lambda), each named with the span
    /// of its first assignment.
    pub hoisted: HashMap<Span, Vec<Ident>>,
    /// The names of the program's own top-level locals, which a `def` can't read.
    pub top_level: HashSet<String>,
}

/// Find the scope of every local in `program`.
pub(super) fn analyze(program: &Program) -> Scopes {
    let mut w = Walker::default();
    w.open_scope(program.span, ScopeKind::Top);
    w.enter_body();
    for item in &program.items {
        match item {
            Item::Def(def) => w.def(def),
            Item::Visibility(_) => {}
            Item::Stmt(stmt) => w.stmt(stmt),
        }
    }
    w.leave_body();
    w.close_scope();
    w.finish()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Top,
    /// A `def`, which sees nothing outside it.
    Def,
    /// A block or lambda, which sees its enclosing scope.
    Block,
}

struct Scope {
    owner: Span,
    kind: ScopeKind,
    names: HashMap<String, usize>,
}

/// A local variable, or a parameter (which never needs a declaration).
struct Var {
    name: String,
    /// The span of the name in its first assignment; `None` for a parameter.
    first: Option<Span>,
    /// The Lua block the first assignment lands in.
    body: usize,
    /// The scope that owns it.
    owner: Span,
    /// Whether it may be declared at its first assignment.
    in_place: bool,
}

#[derive(Default)]
struct Walker {
    scopes: Vec<Scope>,
    vars: Vec<Var>,
    /// The Lua blocks enclosing the current point, innermost last. A Ruby body is one, and so
    /// is the block a statement modifier wraps its statement in.
    bodies: Vec<usize>,
    next_body: usize,
    /// Locals whose first assignment is being walked: a use of one inside it comes before its
    /// declaration could.
    pending: HashSet<usize>,
    /// The locals each multiple assignment declares, beside whether it assigns anything else.
    multi: Vec<(Vec<usize>, bool)>,
    out: Scopes,
}

impl Walker {
    fn open_scope(&mut self, owner: Span, kind: ScopeKind) {
        self.scopes.push(Scope {
            owner,
            kind,
            names: HashMap::new(),
        });
    }

    fn close_scope(&mut self) {
        self.scopes.pop();
    }

    fn enter_body(&mut self) {
        self.bodies.push(self.next_body);
        self.next_body += 1;
    }

    fn leave_body(&mut self) {
        self.bodies.pop();
    }

    /// The local `name` names here, if any: one of the innermost scope's, or while that is a
    /// block, of the scopes enclosing it up to the nearest `def` or the top level.
    fn lookup(&self, name: &str) -> Option<usize> {
        for scope in self.scopes.iter().rev() {
            if let Some(&v) = scope.names.get(name) {
                return Some(v);
            }
            if scope.kind != ScopeKind::Block {
                return None;
            }
        }
        None
    }

    fn declare(&mut self, name: &str, first: Option<Span>, in_place: bool) -> usize {
        let scope = self.scopes.last_mut().expect("a scope is open");
        let v = self.vars.len();
        self.vars.push(Var {
            name: name.to_string(),
            first,
            body: *self.bodies.last().expect("a body is open"),
            owner: scope.owner,
            in_place,
        });
        scope.names.insert(name.to_string(), v);
        if scope.kind == ScopeKind::Top && first.is_some() {
            self.out.top_level.insert(name.to_string());
        }
        v
    }

    /// A use of local `v` other than its first assignment.
    fn occurrence(&mut self, v: usize) {
        let var = &mut self.vars[v];
        if self.pending.contains(&v) || !self.bodies.contains(&var.body) {
            var.in_place = false;
        }
    }

    fn params(&mut self, params: &[Ident]) {
        for p in params {
            self.declare(&p.node, None, false);
        }
    }

    fn def(&mut self, def: &Def) {
        self.open_scope(def.span, ScopeKind::Def);
        self.enter_body();
        self.params(&def.params);
        self.stmts(&def.body.stmts);
        self.leave_body();
        self.close_scope();
    }

    /// A block or lambda: a scope of its own, whose parameters shadow outer names.
    fn block(&mut self, block: &Block) {
        self.open_scope(block.span, ScopeKind::Block);
        self.enter_body();
        self.params(&block.params);
        self.stmts(&block.body.stmts);
        self.leave_body();
        self.close_scope();
    }

    fn body(&mut self, body: &Body) {
        self.enter_body();
        self.stmts(&body.stmts);
        self.leave_body();
    }

    fn stmts(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::Assign { targets, values } => {
                // As in Ruby, the targets are locals from here on, so the values can already
                // use them (and a use there means they can't be declared in place).
                let mut declared = Vec::new();
                let mut assigns_other = false;
                for t in targets {
                    match self.target(t, true) {
                        Some(v) => declared.push(v),
                        None => assigns_other = true,
                    }
                }
                self.pending.extend(declared.iter().copied());
                for v in values {
                    self.expr(v);
                }
                for v in &declared {
                    self.pending.remove(v);
                }
                if targets.len() > 1 {
                    self.multi.push((declared, assigns_other));
                }
            }
            StmtKind::OpAssign { target, value, .. } => {
                // `x += e` reads `x` before assigning it, so a first one isn't in place.
                self.target(target, false);
                self.expr(value);
            }
            StmtKind::While { cond, body, .. } => {
                self.expr(cond);
                self.body(body);
            }
            StmtKind::Loop(body) => self.body(body),
            StmtKind::Break | StmtKind::Next => {}
            StmtKind::Return(values) => {
                for v in values {
                    self.expr(v);
                }
            }
            StmtKind::Modified {
                stmt: inner, cond, ..
            } => {
                // `stmt if c`: the condition runs first, and the statement lands in a block.
                self.expr(cond);
                self.enter_body();
                self.stmt(inner);
                self.leave_body();
            }
        }
    }

    /// An assignment target. Returns the local it declares, if this is its first assignment;
    /// `in_place` is whether that may be where it is declared.
    fn target(&mut self, target: &Expr, in_place: bool) -> Option<usize> {
        match &target.kind {
            ExprKind::Name(name) => {
                self.out.locals.insert(target.span);
                match self.lookup(name) {
                    Some(v) => {
                        self.occurrence(v);
                        None
                    }
                    None => Some(self.declare(name, Some(target.span), in_place)),
                }
            }
            _ => {
                self.expr(target);
                None
            }
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Nil
            | ExprKind::Bool(_)
            | ExprKind::Number(_)
            | ExprKind::Symbol(_)
            | ExprKind::Const(_)
            | ExprKind::Ivar(_) => {}
            ExprKind::Str(parts) => {
                for part in parts {
                    if let StrPart::Interp(e) = part {
                        self.expr(e);
                    }
                }
            }
            ExprKind::Name(name) => {
                if let Some(v) = self.lookup(name) {
                    self.out.locals.insert(expr.span);
                    self.occurrence(v);
                }
            }
            ExprKind::Array(items) => {
                for e in items {
                    self.expr(e);
                }
            }
            ExprKind::Hash(pairs) => {
                for pair in pairs {
                    if let Key::Expr(k) = &pair.key {
                        self.expr(k);
                    }
                    self.expr(&pair.value);
                }
            }
            ExprKind::Lambda(block) => self.block(block),
            ExprKind::Range { lo, hi, .. } => {
                self.expr(lo);
                self.expr(hi);
            }
            ExprKind::Call { name, args, block } => {
                if self.lookup(&name.node).is_some() {
                    self.out.locals.insert(name.span);
                }
                self.args(args);
                if let Some(b) = block {
                    self.block(b);
                }
            }
            ExprKind::MethodCall {
                recv, args, block, ..
            } => {
                self.expr(recv);
                self.args(args);
                if let Some(b) = block {
                    self.block(b);
                }
            }
            ExprKind::Field { recv, .. } => self.expr(recv),
            ExprKind::CallValue { callee, args } => {
                self.expr(callee);
                self.args(args);
            }
            ExprKind::Index { base, index } => {
                self.expr(base);
                self.expr(index);
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Unary { operand, .. } => self.expr(operand),
            ExprKind::Ternary { cond, then, els } => {
                self.expr(cond);
                self.expr(then);
                self.expr(els);
            }
            ExprKind::If(if_) => {
                for (cond, body) in &if_.arms {
                    self.expr(cond);
                    self.body(body);
                }
                if let Some(body) = &if_.else_body {
                    self.body(body);
                }
            }
            ExprKind::Case(case) => {
                self.expr(&case.subject);
                for when in &case.whens {
                    for v in &when.values {
                        self.expr(v);
                    }
                    self.body(&when.body);
                }
                if let Some(body) = &case.else_body {
                    self.body(body);
                }
            }
            ExprKind::Paren(inner) => self.expr(inner),
        }
    }

    fn args(&mut self, args: &[Arg]) {
        for arg in args {
            match arg {
                Arg::Pos(e) | Arg::Label { value: e, .. } => self.expr(e),
            }
        }
    }

    fn finish(mut self) -> Scopes {
        // One `local` declares every target of a multiple assignment or none of them.
        for (declared, assigns_other) in std::mem::take(&mut self.multi) {
            if assigns_other || declared.iter().any(|&v| !self.vars[v].in_place) {
                for v in declared {
                    self.vars[v].in_place = false;
                }
            }
        }
        for var in &self.vars {
            let Some(first) = var.first else { continue };
            if var.in_place {
                self.out.in_place.insert(first);
            } else {
                self.out
                    .hoisted
                    .entry(var.owner)
                    .or_default()
                    .push(Spanned::new(var.name.clone(), first));
            }
        }
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(src: &str) -> (Scopes, String) {
        let program = crate::ruby::parse(src).unwrap_or_else(|d| panic!("{}", d.render(src)));
        (analyze(&program), src.to_string())
    }

    /// The text of each span in `set`, sorted by position.
    fn texts(src: &str, set: &HashSet<Span>) -> Vec<String> {
        let mut spans: Vec<_> = set.iter().copied().collect();
        spans.sort_by_key(|s| s.start);
        spans
            .iter()
            .map(|s| format!("{}@{}", &src[s.start as usize..s.end as usize], s.start))
            .collect()
    }

    fn hoisted(s: &Scopes) -> Vec<String> {
        let mut out: Vec<String> = s
            .hoisted
            .values()
            .flatten()
            .map(|i| i.node.clone())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn a_name_is_a_local_after_its_first_assignment() {
        let src = "def f\n  x\n  x = 1\n  x\nend\n";
        let (s, src) = scopes(src);
        // The first `x` is a call; the target and the last read are the local.
        assert_eq!(texts(&src, &s.locals), ["x@12", "x@20"]);
        assert_eq!(texts(&src, &s.in_place), ["x@12"]);
        assert!(s.hoisted.is_empty());
    }

    #[test]
    fn a_use_outside_the_first_assignments_block_hoists() {
        let (s, _) = scopes("def f(c)\n  if c\n    x = 1\n  end\n  x\nend\n");
        assert!(s.in_place.is_empty());
        assert_eq!(hoisted(&s), ["x"]);
        // A use in a nested block of the same body doesn't.
        let (s, _) = scopes("def f(c)\n  x = 1\n  if c\n    x += 1\n  end\nend\n");
        assert_eq!(s.in_place.len(), 1);
        assert!(s.hoisted.is_empty());
    }

    #[test]
    fn a_modifier_puts_its_statement_in_a_block() {
        let (s, _) = scopes("def f(c)\n  x = 1 if c\n  x\nend\n");
        assert_eq!(hoisted(&s), ["x"]);
    }

    #[test]
    fn op_assign_and_self_reference_hoist() {
        let (s, _) = scopes("def f\n  x ||= 1\n  x\nend\n");
        assert_eq!(hoisted(&s), ["x"]);
        let (s, _) = scopes("def f\n  g = -> { g.() }\nend\n");
        assert_eq!(hoisted(&s), ["g"]);
    }

    #[test]
    fn a_multiple_assignment_declares_all_or_none() {
        let (s, _) = scopes("def f\n  a, b = 1, 2\n  a + b\nend\n");
        assert_eq!(s.in_place.len(), 2);
        let (s, _) = scopes("def f(a)\n  a, b = 1, 2\n  b\nend\n");
        assert_eq!(hoisted(&s), ["b"]);
    }

    #[test]
    fn a_def_sees_nothing_outside_it() {
        let (s, src) = scopes("x = 1\ndef f\n  x\nend\n");
        assert_eq!(texts(&src, &s.locals), ["x@0"]);
        assert!(s.top_level.contains("x"));
    }

    #[test]
    fn a_block_sees_outer_locals_and_keeps_its_own() {
        let src = "def f(xs)\n  t = 0\n  xs.each do |x|\n    y = x\n    t += y\n  end\n  y\nend\n";
        let (s, src) = scopes(src);
        // `y` after the block is a call: the block's `y` ended with it.
        let reads: Vec<_> = texts(&src, &s.locals)
            .into_iter()
            .filter(|t| t.starts_with('y'))
            .collect();
        assert_eq!(reads.len(), 2);
        assert_eq!(s.in_place.len(), 2);
    }
}
