//! Resolver: name binding + the deeper constraint rules (`PLAN.md` Phase 2).
//!
//! The parser already enforced the *structural* contract (no top-level locals, no
//! `repeat`/varargs/goto, generic-`for` iterators restricted, assignable targets). The
//! resolver enforces the rules that need a symbol table:
//!
//!   * **No free globals.** Every name must resolve to a local/param/loop-var, a captured
//!     upvalue, another top-level declaration, a builtin, a host-registered function, or
//!     the host memory binding. Anything else is an error — there is no ambient global
//!     namespace (this is also how the "no ambient stdlib / determinism" rule is enforced,
//!     SPEC §1, §6, §7).
//!   * **Immutable bindings.** Top-level functions/constants, builtins, host functions, and
//!     the memory handle itself cannot be assigned to. (`mem.field = …` is fine — that's a
//!     field write, not a rebind.)
//!   * **Constant expressions.** A top-level `name = <expr>` RHS must be compile-time
//!     constant: literals, table constructors of constants, and operators over them. No
//!     names, calls, or indexing (SPEC §3.1).
//!   * **`break` only inside a loop.**
//!   * **No shadowing reserved names** by a top-level declaration (SPEC §7).
//!   * **Duplicate top-level declarations** are rejected.
//!
//! Output is a [`Resolution`]: a map from each name *use* (by [`Span`]) to its [`Binding`],
//! plus the table of in-function [`SymbolInfo`] definitions. Later phases (type checker,
//! interpreter, codegen) consume this instead of re-deriving scope.
//!
//! Unlike the parser, the resolver does **not** bail on the first error — it collects as
//! many diagnostics as it can in one pass.

use std::collections::HashMap;

use crate::ast::*;
use crate::diagnostics::{Diagnostic, Diagnostics, Span};

/// Builtin value names available in every script (SPEC §6). Namespaces (`math`, `string`)
/// are accessed via field expressions (`math.floor`); their membership is checked later by
/// the type checker. `ipairs`/`pairs` are intentionally absent — they are only legal as
/// generic-`for` iterators (enforced by the parser) and produce a targeted error if used
/// as a value here.
pub const BUILTINS: &[&str] = &["math", "string", "tostring", "tonumber"];

/// Host-supplied names injected into a script's environment (SPEC §7).
#[derive(Clone, Debug, Default)]
pub struct ResolveConfig {
    /// Names of host-registered functions callable from the script.
    pub host_functions: Vec<String>,
    /// Names bound to host memory handles (commonly just `["mem".into()]`).
    pub memory: Vec<String>,
}

impl ResolveConfig {
    /// A config with a single memory binding named `mem` and no host functions.
    pub fn with_memory(name: impl Into<String>) -> Self {
        ResolveConfig {
            host_functions: Vec::new(),
            memory: vec![name.into()],
        }
    }
}

/// An identifier for an in-function symbol (param, local, or loop variable).
pub type SymbolId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolKind {
    Param,
    Local,
    LoopVar,
}

/// A definition of an in-function symbol.
#[derive(Clone, Debug)]
pub struct SymbolInfo {
    pub name: String,
    pub def_span: Span,
    pub kind: SymbolKind,
    /// 0 = outermost function, increasing inward (for closures).
    pub func_depth: usize,
}

/// What a resolved name use refers to.
#[derive(Clone, Debug, PartialEq)]
pub enum Binding {
    /// A param/local/loop-var defined in the *current* function.
    Local(SymbolId),
    /// A symbol captured from an enclosing function (closure upvalue).
    Upvalue(SymbolId),
    /// A top-level function declaration.
    TopFunction(String),
    /// A top-level constant declaration.
    TopConst(String),
    /// A language builtin (see [`BUILTINS`]).
    Builtin(&'static str),
    /// A host-registered function.
    HostFunction(String),
    /// A host memory handle.
    Memory(String),
}

/// The result of resolving a [`Module`].
#[derive(Clone, Debug, Default)]
pub struct Resolution {
    /// Each name-use span mapped to its binding.
    pub bindings: HashMap<Span, Binding>,
    /// All in-function symbol definitions, indexed by [`SymbolId`].
    pub symbols: Vec<SymbolInfo>,
}

impl Resolution {
    /// Look up the binding recorded for a name use at `span`.
    pub fn binding(&self, span: Span) -> Option<&Binding> {
        self.bindings.get(&span)
    }
}

/// Resolve `module` against host configuration `cfg`.
///
/// Returns the [`Resolution`] on success, or every collected diagnostic on failure.
pub fn resolve(module: &Module, cfg: &ResolveConfig) -> Result<Resolution, Diagnostics> {
    let mut r = Resolver::new(cfg);
    r.resolve_module(module);
    if r.diags.has_errors() {
        Err(r.diags)
    } else {
        Ok(Resolution {
            bindings: r.bindings,
            symbols: r.symbols,
        })
    }
}

struct BlockScope {
    names: HashMap<String, SymbolId>,
}

impl BlockScope {
    fn new() -> Self {
        BlockScope {
            names: HashMap::new(),
        }
    }
}

struct FuncScope {
    blocks: Vec<BlockScope>,
}

struct Resolver<'a> {
    cfg: &'a ResolveConfig,
    top_funcs: HashMap<String, Span>,
    top_consts: HashMap<String, Span>,
    func_stack: Vec<FuncScope>,
    symbols: Vec<SymbolInfo>,
    bindings: HashMap<Span, Binding>,
    loop_depth: usize,
    diags: Diagnostics,
}

impl<'a> Resolver<'a> {
    fn new(cfg: &'a ResolveConfig) -> Self {
        Resolver {
            cfg,
            top_funcs: HashMap::new(),
            top_consts: HashMap::new(),
            func_stack: Vec::new(),
            symbols: Vec::new(),
            bindings: HashMap::new(),
            loop_depth: 0,
            diags: Diagnostics::new(),
        }
    }

    fn error(&mut self, code: &'static str, msg: impl Into<String>, span: Span) {
        self.diags.push(Diagnostic::error(code, msg, span));
    }

    // ---- top level -----------------------------------------------------------

    fn resolve_module(&mut self, module: &Module) {
        // Pass 1: collect every top-level name so functions are mutually in scope, while
        // rejecting duplicates and reserved-name shadowing.
        for decl in &module.decls {
            let (name, span) = match decl {
                TopDecl::Function(f) => (&f.name.node, f.name.span),
                TopDecl::Const(c) => (&c.name.node, c.name.span),
            };

            if let Some(reason) = self.reserved_reason(name) {
                self.error(
                    "E0304",
                    format!("top-level declaration `{name}` shadows {reason}"),
                    span,
                );
                // Still register it below so uses don't also cascade into free-global errors.
            }

            let dup = self.top_funcs.contains_key(name) || self.top_consts.contains_key(name);
            if dup {
                self.error(
                    "E0305",
                    format!("duplicate top-level declaration `{name}`"),
                    span,
                );
            }

            match decl {
                TopDecl::Function(_) => {
                    self.top_funcs.insert(name.clone(), span);
                }
                TopDecl::Const(_) => {
                    self.top_consts.insert(name.clone(), span);
                }
            }
        }

        // Pass 2: validate const RHSs and resolve function bodies.
        for decl in &module.decls {
            match decl {
                TopDecl::Const(c) => self.check_const_expr(&c.value),
                TopDecl::Function(f) => self.resolve_func_body(&f.body),
            }
        }

        // Pass 3: the curated export table references top-level names.
        if let Some(export) = &module.export {
            for field in &export.node {
                self.resolve_export_field(field);
            }
        }
    }

    /// Why `name` is reserved (for the shadowing diagnostic), or `None`.
    fn reserved_reason(&self, name: &str) -> Option<String> {
        if BUILTINS.contains(&name) || name == "ipairs" || name == "pairs" {
            Some(format!("the builtin `{name}`"))
        } else if self.cfg.host_functions.iter().any(|h| h == name) {
            Some(format!("the host function `{name}`"))
        } else if self.cfg.memory.iter().any(|m| m == name) {
            Some(format!("the host memory binding `{name}`"))
        } else {
            None
        }
    }

    fn resolve_export_field(&mut self, field: &Field) {
        match field {
            Field::Positional(e) => self.resolve_expr(e),
            Field::Named { value, .. } => self.resolve_expr(value),
            Field::Keyed { key, value } => {
                self.resolve_expr(key);
                self.resolve_expr(value);
            }
        }
    }

    // ---- constant expressions ------------------------------------------------

    /// Validate that a top-level constant initializer is compile-time constant.
    fn check_const_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Nil | ExprKind::Bool(_) | ExprKind::Number(_) | ExprKind::Str(_) => {}
            ExprKind::Paren(inner) => self.check_const_expr(inner),
            ExprKind::Unary { operand, .. } => self.check_const_expr(operand),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.check_const_expr(lhs);
                self.check_const_expr(rhs);
            }
            ExprKind::Table(fields) => {
                for field in fields {
                    match field {
                        Field::Positional(e) => self.check_const_expr(e),
                        Field::Named { value, .. } => self.check_const_expr(value),
                        Field::Keyed { key, value } => {
                            self.check_const_expr(key);
                            self.check_const_expr(value);
                        }
                    }
                }
            }
            _ => {
                self.error(
                    "E0303",
                    "constant declarations may only contain literal values, table \
                     constructors of constants, and operators over them; names, calls, \
                     and indexing are not allowed",
                    expr.span,
                );
            }
        }
    }

    // ---- scopes --------------------------------------------------------------

    fn enter_function(&mut self) {
        self.func_stack.push(FuncScope {
            blocks: vec![BlockScope::new()],
        });
    }

    fn exit_function(&mut self) {
        self.func_stack.pop();
    }

    fn push_block(&mut self) {
        self.func_stack
            .last_mut()
            .expect("block pushed outside a function")
            .blocks
            .push(BlockScope::new());
    }

    fn pop_block(&mut self) {
        self.func_stack
            .last_mut()
            .expect("block popped outside a function")
            .blocks
            .pop();
    }

    fn declare(&mut self, name: &str, span: Span, kind: SymbolKind) -> SymbolId {
        let func_depth = self.func_stack.len().saturating_sub(1);
        let id = self.symbols.len() as SymbolId;
        self.symbols.push(SymbolInfo {
            name: name.to_string(),
            def_span: span,
            kind,
            func_depth,
        });
        // Redeclaration in the same block shadows the earlier binding (Lua-compatible).
        self.func_stack
            .last_mut()
            .expect("declare outside a function")
            .blocks
            .last_mut()
            .expect("declare with no block")
            .names
            .insert(name.to_string(), id);
        id
    }

    /// Resolve a name to a binding without recording or reporting anything.
    fn lookup(&self, name: &str) -> Option<Binding> {
        let last = self.func_stack.len().saturating_sub(1);
        for (fi, fscope) in self.func_stack.iter().enumerate().rev() {
            for block in fscope.blocks.iter().rev() {
                if let Some(&id) = block.names.get(name) {
                    return Some(if fi == last {
                        Binding::Local(id)
                    } else {
                        Binding::Upvalue(id)
                    });
                }
            }
        }
        if self.top_funcs.contains_key(name) {
            return Some(Binding::TopFunction(name.to_string()));
        }
        if self.top_consts.contains_key(name) {
            return Some(Binding::TopConst(name.to_string()));
        }
        if let Some(b) = BUILTINS.iter().find(|&&b| b == name) {
            return Some(Binding::Builtin(b));
        }
        if self.cfg.host_functions.iter().any(|h| h == name) {
            return Some(Binding::HostFunction(name.to_string()));
        }
        if self.cfg.memory.iter().any(|m| m == name) {
            return Some(Binding::Memory(name.to_string()));
        }
        None
    }

    fn resolve_name(&mut self, name: &str, span: Span) {
        match self.lookup(name) {
            Some(b) => {
                self.bindings.insert(span, b);
            }
            None if name == "ipairs" || name == "pairs" => self.error(
                "E0301",
                format!("`{name}` may only be used as a generic `for` iterator"),
                span,
            ),
            None => self.error(
                "E0300",
                format!("cannot find name `{name}` in this scope"),
                span,
            ),
        }
    }

    // ---- functions / blocks / statements -------------------------------------

    fn resolve_func_body(&mut self, body: &FuncBody) {
        self.enter_function();
        for p in &body.params {
            self.declare(&p.node, p.span, SymbolKind::Param);
        }
        // `break` cannot cross a function boundary.
        let saved_loop_depth = self.loop_depth;
        self.loop_depth = 0;
        self.resolve_stats(&body.block);
        self.loop_depth = saved_loop_depth;
        self.exit_function();
    }

    /// Resolve a block's statements in a fresh nested scope.
    fn resolve_block(&mut self, block: &Block) {
        self.push_block();
        self.resolve_stats(block);
        self.pop_block();
    }

    /// Resolve a block's statements in the *current* scope (no new block pushed).
    fn resolve_stats(&mut self, block: &Block) {
        for stat in &block.stats {
            self.resolve_stat(stat);
        }
        if let Some(ret) = &block.ret {
            for e in &ret.exprs {
                self.resolve_expr(e);
            }
        }
    }

    fn resolve_stat(&mut self, stat: &Stat) {
        match &stat.kind {
            StatKind::Empty => {}
            StatKind::Local { names, exprs } => {
                // Initializers see the *outer* scope, so resolve before declaring.
                for e in exprs {
                    self.resolve_expr(e);
                }
                for n in names {
                    self.declare(&n.node, n.span, SymbolKind::Local);
                }
            }
            StatKind::LocalFunction { name, body } => {
                // Declare first so the body can recurse.
                self.declare(&name.node, name.span, SymbolKind::Local);
                self.resolve_func_body(body);
            }
            StatKind::Assign { targets, exprs } => {
                for e in exprs {
                    self.resolve_expr(e);
                }
                for t in targets {
                    self.resolve_assign_target(t);
                }
            }
            StatKind::Call(e) => self.resolve_expr(e),
            StatKind::Do(block) => self.resolve_block(block),
            StatKind::While { cond, body } => {
                self.resolve_expr(cond);
                self.loop_depth += 1;
                self.resolve_block(body);
                self.loop_depth -= 1;
            }
            StatKind::If { arms, else_block } => {
                for (cond, block) in arms {
                    self.resolve_expr(cond);
                    self.resolve_block(block);
                }
                if let Some(block) = else_block {
                    self.resolve_block(block);
                }
            }
            StatKind::NumericFor {
                var,
                start,
                end,
                step,
                body,
            } => {
                self.resolve_expr(start);
                self.resolve_expr(end);
                if let Some(step) = step {
                    self.resolve_expr(step);
                }
                self.push_block();
                self.declare(&var.node, var.span, SymbolKind::LoopVar);
                self.loop_depth += 1;
                self.resolve_block(body);
                self.loop_depth -= 1;
                self.pop_block();
            }
            StatKind::GenericFor { names, iter, body } => {
                match iter {
                    IterExpr::IPairs { arg, .. } | IterExpr::Pairs { arg, .. } => {
                        self.resolve_expr(arg)
                    }
                }
                self.push_block();
                for n in names {
                    self.declare(&n.node, n.span, SymbolKind::LoopVar);
                }
                self.loop_depth += 1;
                self.resolve_block(body);
                self.loop_depth -= 1;
                self.pop_block();
            }
            StatKind::Break => {
                if self.loop_depth == 0 {
                    self.error("E0306", "`break` outside of a loop", stat.span);
                }
            }
        }
    }

    fn resolve_assign_target(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Name(n) => match self.lookup(n) {
                Some(b @ (Binding::Local(_) | Binding::Upvalue(_))) => {
                    self.bindings.insert(expr.span, b);
                }
                Some(_) => self.error(
                    "E0302",
                    format!(
                        "cannot assign to `{n}`: it is an immutable binding (top-level \
                         declaration, builtin, host function, or memory handle)"
                    ),
                    expr.span,
                ),
                None if n == "ipairs" || n == "pairs" => self.error(
                    "E0301",
                    format!("`{n}` may only be used as a generic `for` iterator"),
                    expr.span,
                ),
                None => self.error(
                    "E0300",
                    format!(
                        "cannot find name `{n}` in this scope; assigning to an undeclared \
                         name would create a global, which is not allowed"
                    ),
                    expr.span,
                ),
            },
            ExprKind::Field { base, .. } => self.resolve_expr(base),
            ExprKind::Index { base, index } => {
                self.resolve_expr(base);
                self.resolve_expr(index);
            }
            // The parser guarantees targets are name/field/index; anything else is a bug.
            _ => self.resolve_expr(expr),
        }
    }

    // ---- expressions ---------------------------------------------------------

    fn resolve_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Nil | ExprKind::Bool(_) | ExprKind::Number(_) | ExprKind::Str(_) => {}
            ExprKind::Name(n) => self.resolve_name(n, expr.span),
            ExprKind::Index { base, index } => {
                self.resolve_expr(base);
                self.resolve_expr(index);
            }
            // Field/method names are not variables; only the base/receiver resolves.
            ExprKind::Field { base, .. } => self.resolve_expr(base),
            ExprKind::Call { callee, args } => {
                self.resolve_expr(callee);
                for a in args {
                    self.resolve_expr(a);
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                self.resolve_expr(receiver);
                for a in args {
                    self.resolve_expr(a);
                }
            }
            ExprKind::Table(fields) => {
                for field in fields {
                    match field {
                        Field::Positional(e) => self.resolve_expr(e),
                        Field::Named { value, .. } => self.resolve_expr(value),
                        Field::Keyed { key, value } => {
                            self.resolve_expr(key);
                            self.resolve_expr(value);
                        }
                    }
                }
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.resolve_expr(lhs);
                self.resolve_expr(rhs);
            }
            ExprKind::Unary { operand, .. } => self.resolve_expr(operand),
            ExprKind::Paren(inner) => self.resolve_expr(inner),
            // A nested anonymous function: a closure that may capture enclosing locals.
            ExprKind::Function(body) => self.resolve_func_body(body),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_src(src: &str, cfg: &ResolveConfig) -> Result<Resolution, Diagnostics> {
        let module = crate::parse(src).expect("parse should succeed for resolver tests");
        resolve(&module, cfg)
    }

    fn ok(src: &str, cfg: &ResolveConfig) -> Resolution {
        resolve_src(src, cfg).unwrap_or_else(|d| panic!("resolve failed: {d}"))
    }

    fn err_code(src: &str, cfg: &ResolveConfig) -> String {
        let d = resolve_src(src, cfg).unwrap_err();
        d.0[0].code.to_string()
    }

    fn empty() -> ResolveConfig {
        ResolveConfig::default()
    }

    #[test]
    fn resolves_params_locals_and_mutual_recursion() {
        ok(
            "function even(n)\n\
               if n == 0 then return true end\n\
               return odd(n - 1)\n\
             end\n\
             function odd(n)\n\
               if n == 0 then return false end\n\
               return even(n - 1)\n\
             end",
            &empty(),
        );
    }

    #[test]
    fn resolves_builtins_and_const_reference_in_body() {
        ok(
            "K = 100\n\
             function f(x)\n\
               return math.floor(x) + K\n\
             end",
            &empty(),
        );
    }

    #[test]
    fn closure_captures_enclosing_local_as_upvalue() {
        let r = ok(
            "function make(base)\n\
               local add = function(x) return x + base end\n\
               return add(1)\n\
             end",
            &empty(),
        );
        // Somewhere a use of `base` resolved to an upvalue.
        assert!(
            r.bindings
                .values()
                .any(|b| matches!(b, Binding::Upvalue(_))),
            "expected an upvalue binding, got {:?}",
            r.bindings.values().collect::<Vec<_>>()
        );
    }

    #[test]
    fn resolves_memory_field_access_and_write() {
        let cfg = ResolveConfig::with_memory("mem");
        ok(
            "function spend(n)\n\
               if mem.gold >= n then\n\
                 mem.gold = mem.gold - n\n\
                 return true\n\
               end\n\
               return false\n\
             end",
            &cfg,
        );
    }

    #[test]
    fn rejects_free_global() {
        assert_eq!(err_code("function f() return ghost end", &empty()), "E0300");
    }

    #[test]
    fn rejects_implicit_global_write() {
        assert_eq!(
            err_code("function f() undeclared = 1 end", &empty()),
            "E0300"
        );
    }

    #[test]
    fn rejects_memory_use_when_not_configured() {
        // Without a memory binding, `mem` is just an unknown name.
        assert_eq!(err_code("function f() return mem.x end", &empty()), "E0300");
    }

    #[test]
    fn rejects_assignment_to_const() {
        assert_eq!(err_code("K = 1\nfunction f() K = 2 end", &empty()), "E0302");
    }

    #[test]
    fn rejects_assignment_to_function() {
        assert_eq!(
            err_code("function g() end\nfunction f() g = 1 end", &empty()),
            "E0302"
        );
    }

    #[test]
    fn rejects_rebinding_memory_handle() {
        let cfg = ResolveConfig::with_memory("mem");
        assert_eq!(err_code("function f() mem = 1 end", &cfg), "E0302");
    }

    #[test]
    fn rejects_break_outside_loop() {
        assert_eq!(err_code("function f() break end", &empty()), "E0306");
    }

    #[test]
    fn break_inside_loop_is_ok() {
        ok(
            "function f(n)\n\
               while n > 0 do\n\
                 if n == 3 then break end\n\
                 n = n - 1\n\
               end\n\
               return n\n\
             end",
            &empty(),
        );
    }

    #[test]
    fn rejects_break_across_closure_boundary() {
        // The `break` is inside a closure nested in a loop — it must not see the loop.
        assert_eq!(
            err_code(
                "function f()\n\
                   while true do\n\
                     local g = function() break end\n\
                   end\n\
                 end",
                &empty()
            ),
            "E0306"
        );
    }

    #[test]
    fn rejects_non_constant_const_rhs_name() {
        assert_eq!(err_code("A = 1\nB = A", &empty()), "E0303");
    }

    #[test]
    fn rejects_non_constant_const_rhs_call() {
        assert_eq!(err_code("X = f()", &empty()), "E0303");
    }

    #[test]
    fn allows_constant_arithmetic_and_table_const() {
        ok("A = 1 + 2 * 3\nB = { 1, 2, 3 }\nC = -4", &empty());
    }

    #[test]
    fn rejects_duplicate_top_level() {
        assert_eq!(
            err_code("function f() end\nfunction f() end", &empty()),
            "E0305"
        );
    }

    #[test]
    fn rejects_top_level_shadowing_builtin() {
        assert_eq!(err_code("function math() end", &empty()), "E0304");
    }

    #[test]
    fn rejects_ipairs_as_value() {
        assert_eq!(
            err_code("function f() return ipairs end", &empty()),
            "E0301"
        );
    }

    #[test]
    fn resolves_generic_for_loop_var() {
        ok(
            "function sum(t)\n\
               local s = 0\n\
               for _, v in ipairs(t) do\n\
                 s = s + v\n\
               end\n\
               return s\n\
             end",
            &empty(),
        );
    }

    #[test]
    fn loop_var_not_visible_after_loop() {
        assert_eq!(
            err_code(
                "function f()\n\
                   for i = 1, 10 do end\n\
                   return i\n\
                 end",
                &empty()
            ),
            "E0300"
        );
    }

    #[test]
    fn resolves_host_function_call() {
        let cfg = ResolveConfig {
            host_functions: vec!["roll_dice".into()],
            memory: vec![],
        };
        ok("function f() return roll_dice(6) end", &cfg);
    }
}
