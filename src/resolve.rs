//! Resolver: name binding + the deeper constraint rules (`PLAN.md` Phase 2).
//!
//! The parser already enforced the *structural* contract (no `repeat`/varargs/goto,
//! generic-`for` iterators restricted, assignable targets). The resolver enforces the rules
//! that need a symbol table:
//!
//!   * **No free globals.** Every name must resolve to a local/param/loop-var, a captured
//!     upvalue, a module variable, another top-level declaration, a builtin, a
//!     host-registered function, or the host memory binding. Anything else is an error —
//!     there is no ambient global namespace (this is also how the "no ambient stdlib /
//!     determinism" rule is enforced, SPEC §1, §6, §7).
//!   * **Lua scoping at the top level** (SPEC §3.1). A `function` or constant is in scope
//!     everywhere; a top-level `local` or `local function` only from its declaration on,
//!     including inside the functions declared after it.
//!   * **Immutable bindings.** Top-level functions/constants, builtins, host functions, and
//!     the memory handle itself cannot be assigned to. (`mem.field = …` is fine — that's a
//!     field write, not a rebind.)
//!   * **Only top-level code changes module values.** A module variable is assigned, and a
//!     module value written through, only by top-level code; a function body that tries is
//!     rejected (`E0308`, `E0307`), since by the time a call runs the module has initialized
//!     and frozen them.
//!   * **Top-level code** may not `return` (`E0309`) or touch host memory (`E0310`), which
//!     isn't bound until after the module initializes.
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

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::diagnostics::{Diagnostic, Diagnostics, Span};
use crate::surface::Surface;

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
    /// A top-level `local`: a module variable, kept in a module slot rather than a frame.
    Module,
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
    /// A top-level `local` — a module variable. Read and written through its module slot, never
    /// captured: every function that can see it reads the slot directly.
    Module(SymbolId),
    /// A top-level function declaration (a `function` or a top-level `local function`).
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
    /// Each symbol *definition* site (param/local/loop-var name span) mapped to its
    /// [`SymbolId`]. Lets later phases recover the symbol introduced at a declaration
    /// without re-deriving scope.
    pub defs: HashMap<Span, SymbolId>,
    /// All in-function symbol definitions, indexed by [`SymbolId`].
    pub symbols: Vec<SymbolInfo>,
    /// The module's **slots**, in the order top-level code sets them: one for each constant
    /// and each top-level `local`. A module instance keeps each value its top-level code
    /// builds in the slot of the name it is bound to (`SPEC.md` §3.1). Top-level names are
    /// unique, so a slot is identified by its name.
    pub globals: Vec<String>,
}

impl Resolution {
    /// Look up the binding recorded for a name use at `span`.
    pub fn binding(&self, span: Span) -> Option<&Binding> {
        self.bindings.get(&span)
    }

    /// Look up the [`SymbolId`] introduced at the definition site `span`.
    pub fn def(&self, span: Span) -> Option<SymbolId> {
        self.defs.get(&span).copied()
    }

    /// The slot of the constant or module variable named `name`.
    pub fn global_slot(&self, name: &str) -> Option<u32> {
        self.globals
            .iter()
            .position(|g| g == name)
            .map(|i| i as u32)
    }

    /// The slot a [`Binding::Module`] or [`Binding::TopConst`] refers to; `None` for any other
    /// binding.
    pub fn slot_of(&self, binding: &Binding) -> Option<u32> {
        match binding {
            Binding::Module(id) => self.global_slot(&self.symbols[*id as usize].name),
            Binding::TopConst(name) => self.global_slot(name),
            _ => None,
        }
    }
}

/// Resolve `module` against host configuration `cfg`.
///
/// Returns the [`Resolution`] on success, or every collected diagnostic on failure.
pub fn resolve(module: &Module, cfg: &ResolveConfig) -> Result<Resolution, Diagnostics> {
    let mut r = Resolver::new(cfg, module.surface);
    r.resolve_module(module);
    if r.diags.has_errors() {
        Err(r.diags)
    } else {
        Ok(Resolution {
            bindings: r.bindings,
            defs: r.defs,
            symbols: r.symbols,
            globals: r.globals,
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
    /// The syntax diagnostics quote code in.
    surface: Surface,
    /// The top-level functions in scope: every `function`, plus each `local function` once its
    /// declaration is reached.
    top_funcs: HashMap<String, Span>,
    top_consts: HashMap<String, Span>,
    /// The module variables (top-level `local`s) declared so far.
    module_scope: HashMap<String, SymbolId>,
    globals: Vec<String>,
    /// Whether the code being resolved is top-level code itself, rather than a function body
    /// (including a function literal written in top-level code).
    in_chunk: bool,
    func_stack: Vec<FuncScope>,
    symbols: Vec<SymbolInfo>,
    bindings: HashMap<Span, Binding>,
    defs: HashMap<Span, SymbolId>,
    loop_depth: usize,
    diags: Diagnostics,
}

impl<'a> Resolver<'a> {
    fn new(cfg: &'a ResolveConfig, surface: Surface) -> Self {
        Resolver {
            cfg,
            surface,
            top_funcs: HashMap::new(),
            top_consts: HashMap::new(),
            module_scope: HashMap::new(),
            globals: Vec::new(),
            in_chunk: false,
            func_stack: Vec::new(),
            symbols: Vec::new(),
            bindings: HashMap::new(),
            defs: HashMap::new(),
            loop_depth: 0,
            diags: Diagnostics::new(),
        }
    }

    fn error(&mut self, code: &'static str, msg: impl Into<String>, span: Span) {
        self.diags.push(Diagnostic::error(code, msg, span));
    }

    // ---- top level -----------------------------------------------------------

    fn resolve_module(&mut self, module: &Module) {
        // Pass 1: check every top-level name for duplicates and reserved-name shadowing, and
        // bring the ones in scope everywhere — functions and constants — into scope, so
        // functions are mutually in scope.
        let mut seen: HashSet<&str> = HashSet::new();
        for decl in &module.decls {
            let names: Vec<&Ident> = match decl {
                TopDecl::Function(f) => vec![&f.name],
                TopDecl::Const(c) => vec![&c.name],
                TopDecl::Stat(stat) => match &stat.kind {
                    StatKind::Local { names, .. } => names.iter().collect(),
                    _ => Vec::new(),
                },
            };
            for ident in names {
                let (name, span) = (&ident.node, ident.span);
                if let Some(reason) = self.reserved_reason(name) {
                    self.error(
                        "E0304",
                        format!("top-level declaration `{name}` shadows {reason}"),
                        span,
                    );
                    // Still register it so uses don't also cascade into free-global errors.
                }
                if !seen.insert(name) {
                    self.error(
                        "E0305",
                        format!("duplicate top-level declaration `{name}`"),
                        span,
                    );
                }
            }
            match decl {
                TopDecl::Function(f) if !f.local => {
                    self.top_funcs.insert(f.name.node.clone(), f.name.span);
                }
                TopDecl::Const(c) => {
                    self.top_consts.insert(c.name.node.clone(), c.name.span);
                }
                _ => {}
            }
        }

        // Pass 2: resolve every item in order, so a top-level `local` is in scope from its
        // declaration on. Top-level code runs in one function scope of its own: the locals of
        // its nested blocks are that scope's, and a function literal in it captures them.
        self.in_chunk = true;
        self.enter_function();
        for decl in &module.decls {
            match decl {
                TopDecl::Function(f) => {
                    if f.local {
                        self.top_funcs.insert(f.name.node.clone(), f.name.span);
                    }
                    // A declared function is outermost: it captures nothing from top-level code
                    // (a module variable is read through its slot, not captured).
                    let chunk = std::mem::take(&mut self.func_stack);
                    self.resolve_func_body(&f.body);
                    self.func_stack = chunk;
                }
                TopDecl::Const(c) => {
                    self.resolve_expr(&c.value);
                    self.globals.push(c.name.node.clone());
                }
                TopDecl::Stat(stat) => match &stat.kind {
                    StatKind::Local { names, exprs } => {
                        // Initializers see the scope before the new variables.
                        for e in exprs {
                            self.resolve_expr(e);
                        }
                        for n in names {
                            self.declare_module_var(n);
                        }
                    }
                    _ => self.resolve_stat(stat),
                },
            }
        }
        self.exit_function();
        self.in_chunk = false;

        // Pass 3: the curated export table references top-level names.
        if let Some(export) = &module.export {
            for field in &export.node {
                self.resolve_export_field(field);
            }
        }
    }

    /// Declare a top-level `local`: a module variable, with a slot of its own.
    fn declare_module_var(&mut self, name: &Ident) {
        let id = self.symbols.len() as SymbolId;
        self.symbols.push(SymbolInfo {
            name: name.node.clone(),
            def_span: name.span,
            kind: SymbolKind::Module,
            func_depth: 0,
        });
        self.defs.insert(name.span, id);
        self.module_scope.insert(name.node.clone(), id);
        self.globals.push(name.node.clone());
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
        self.defs.insert(span, id);
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
        if let Some(&id) = self.module_scope.get(name) {
            return Some(Binding::Module(id));
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
            Some(Binding::Memory(m)) if self.in_chunk => self.error(
                "E0310",
                format!(
                    "top-level code cannot use host memory `{m}`: it runs once, when the module \
                     initializes, before any memory is bound. Read memory inside a function."
                ),
                span,
            ),
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
        // A function body is not top-level code, even when written in top-level code: it runs
        // when called, which may be after the module has initialized.
        let saved_in_chunk = std::mem::replace(&mut self.in_chunk, false);
        self.resolve_stats(&body.block);
        self.in_chunk = saved_in_chunk;
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
            if self.in_chunk {
                let why = self.surface.top_level_return();
                self.error(
                    "E0309",
                    format!("top-level code cannot `return`; {why}"),
                    ret.span,
                );
            }
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

    /// The bare name a field/index chain is rooted in (`a.b[c].d` → `a`), or `None` if the
    /// chain bottoms out in something else (a call result, a table literal).
    fn root_name(expr: &Expr) -> Option<&str> {
        match &expr.kind {
            ExprKind::Name(n) => Some(n),
            ExprKind::Field { base, .. } | ExprKind::Index { base, .. } => Self::root_name(base),
            _ => None,
        }
    }

    fn resolve_assign_target(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Name(n) => match self.lookup(n) {
                Some(b @ (Binding::Local(_) | Binding::Upvalue(_))) => {
                    self.bindings.insert(expr.span, b);
                }
                Some(b @ Binding::Module(_)) if self.in_chunk => {
                    self.bindings.insert(expr.span, b);
                }
                Some(Binding::Module(_)) => self.error(
                    "E0308",
                    format!(
                        "cannot assign to module variable `{n}` in a function: only top-level \
                         code sets module variables, and they are frozen once the module has \
                         initialized. Use host memory (`SPEC.md` §7) for state that changes \
                         between calls."
                    ),
                    expr.span,
                ),
                Some(Binding::Memory(m)) if self.in_chunk => self.error(
                    "E0310",
                    format!(
                        "top-level code cannot use host memory `{m}`: it runs once, when the \
                         module initializes, before any memory is bound"
                    ),
                    expr.span,
                ),
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
            ExprKind::Field { base, .. } => {
                self.reject_const_write(expr);
                self.resolve_expr(base)
            }
            ExprKind::Index { base, index } => {
                self.reject_const_write(expr);
                self.resolve_expr(base);
                self.resolve_expr(index);
            }
            // The parser guarantees targets are name/field/index; anything else is a bug.
            _ => self.resolve_expr(expr),
        }
    }

    /// Reject a write *through* a module value (`C.x = …`, `C.a[1] = …`) in a function body.
    ///
    /// What top-level code builds is frozen once the module has initialized (`SPEC.md` §3.1),
    /// and a function only ever runs after that — so the write would fail every time. Mutating
    /// it would be module-level state surviving between calls, which §1 reserves for host memory
    /// alone. Top-level code itself may write through module values: it is what builds them.
    ///
    /// Field writes through **memory** stay legal — that is the whole point of memory (§7) —
    /// so this checks the root binding rather than blanket-rejecting field targets.
    fn reject_const_write(&mut self, target: &Expr) {
        if self.in_chunk {
            return;
        }
        let Some(root) = Self::root_name(target) else {
            return;
        };
        let what = match self.lookup(root) {
            Some(Binding::TopConst(_)) => "a top-level constant",
            Some(Binding::Module(_)) => "a module variable",
            _ => return,
        };
        self.error(
            "E0307",
            format!(
                "cannot assign through `{root}` in a function: it is {what}, and what top-level \
                 code builds is frozen once the module has initialized. Use host memory \
                 (`SPEC.md` §7) for state that changes between calls."
            ),
            target.span,
        );
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
    fn a_constant_may_be_computed_from_names_and_calls() {
        ok(
            "A = 1\nB = A + f(2)\nfunction f(x) return x * 2 end",
            &empty(),
        );
    }

    #[test]
    fn a_constant_is_in_scope_before_its_declaration() {
        // Like a Lua global: whether it has been set yet is a runtime question.
        ok("function f() return LATE end\nLATE = 1", &empty());
    }

    #[test]
    fn a_top_level_local_is_in_scope_only_after_its_declaration() {
        ok("local k = 2\nfunction f(x) return x * k end", &empty());
        assert_eq!(
            err_code("function f(x) return x * k end\nlocal k = 2", &empty()),
            "E0300"
        );
        assert_eq!(err_code("local a = b\nlocal b = 1", &empty()), "E0300");
    }

    #[test]
    fn a_top_level_local_function_is_in_scope_only_after_its_declaration() {
        ok(
            "local function helper(x) return x + 1 end\nfunction f() return helper(1) end",
            &empty(),
        );
        assert_eq!(
            err_code(
                "function f() return helper(1) end\nlocal function helper(x) return x end",
                &empty()
            ),
            "E0300"
        );
    }

    #[test]
    fn module_variables_are_slots_not_upvalues() {
        let r = ok(
            "local k = 2\nfunction f(x) return x * k end\nK = k + 1",
            &empty(),
        );
        assert!(r.bindings.values().any(|b| matches!(b, Binding::Module(_))));
        assert!(
            !r.bindings
                .values()
                .any(|b| matches!(b, Binding::Upvalue(_)))
        );
        assert_eq!(r.globals, ["k", "K"]);
    }

    #[test]
    fn a_closure_in_top_level_code_captures_a_block_local() {
        let r = ok(
            "local t = {}\ndo\n  local n = 0\n  t[1] = function() return n end\nend",
            &empty(),
        );
        assert!(
            r.bindings
                .values()
                .any(|b| matches!(b, Binding::Upvalue(_)))
        );
    }

    #[test]
    fn top_level_code_may_assign_module_variables() {
        ok(
            "local total = 0\nfor i = 1, 3 do total = total + i end\ntotal = total * 2",
            &empty(),
        );
        ok("C = {}\nC.x = 1\nlocal t = {}\nt[1] = 2", &empty());
    }

    #[test]
    fn a_function_cannot_assign_a_module_variable() {
        assert_eq!(
            err_code("local n = 0\nfunction f() n = n + 1 end", &empty()),
            "E0308"
        );
        // A closure written in top-level code is a function too.
        assert_eq!(
            err_code("local n = 0\nlocal g = function() n = 1 end", &empty()),
            "E0308"
        );
    }

    #[test]
    fn a_function_cannot_write_through_a_module_value() {
        assert_eq!(
            err_code("local t = {}\nfunction f() t.x = 1 end", &empty()),
            "E0307"
        );
        assert_eq!(
            err_code("C = {}\nfunction f() C.x = 1 end", &empty()),
            "E0307"
        );
    }

    #[test]
    fn top_level_code_cannot_return() {
        assert_eq!(err_code("if true then return end", &empty()), "E0309");
        // A function written in top-level code may.
        ok("local g = function() return 1 end", &empty());
    }

    #[test]
    fn top_level_code_cannot_use_memory() {
        let cfg = ResolveConfig::with_memory("mem");
        assert_eq!(err_code("local g = mem.gold", &cfg), "E0310");
        assert_eq!(err_code("mem.gold = 1", &cfg), "E0310");
        // A function reads it when called, after memory is bound.
        ok("local g = function() return mem.gold end", &cfg);
    }

    #[test]
    fn rejects_duplicate_top_level_local() {
        assert_eq!(err_code("local x = 1\nlocal x = 2", &empty()), "E0305");
        assert_eq!(err_code("function x() end\nlocal x = 2", &empty()), "E0305");
        assert_eq!(err_code("local math = 1", &empty()), "E0304");
    }

    #[test]
    fn a_block_local_in_top_level_code_may_shadow() {
        ok("local x = 1\ndo local x = 2 end", &empty());
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
