//! Recursive-descent + Pratt parser for the Grindlang grammar (`SPEC.md` §3).
//!
//! Two responsibilities beyond building the AST:
//!   * **Shape the chunk** — function declarations, `NAME = expr` constants, and top-level
//!     statements in source order, plus an optional curated `return { ... }` that must come
//!     last. (Scoping, the no-free-globals rule, and type checks belong to later phases.)
//!   * **Reject unsupported-but-lexable constructs** (`repeat`/`until`, labels `::name::`,
//!     varargs `...`) where they appear, with a "not supported in Grindlang" message.
//!
//! The parser bails on the first error (no recovery) — adequate for the current phase;
//! multi-error recovery can be layered on later.
//!
//! ## Nesting limit
//!
//! Every later phase — resolver, checker, lowering, the interpreters, even dropping the AST —
//! walks the tree recursively, so a deep enough tree overflows the native stack and aborts the
//! process. The parser therefore refuses to build a tree more than `MAX_NESTING` levels deep
//! (`E0209`). It measures the tree's height rather than its own recursion: a left-associative
//! chain like `1 + 1 + … + 1` or a suffix chain like `a.b.c…` is parsed by a loop but still
//! builds a tree as deep as it is long.

use std::collections::HashSet;

use crate::ast::*;
use crate::diagnostics::{Diagnostic, Diagnostics, Span};
use crate::lexer::{DocComment, Token, TokenKind};
use crate::surface::Surface;

/// How many levels of nested expressions and blocks a script may have (`E0209`), and how deeply a
/// `---@` type annotation may nest. Lua and Luau limit their parsers to the same 200.
///
/// Source nested this deep compiles and runs on a 1 MiB stack (the Windows main thread's
/// default) in a release build. A debug build's unoptimized frames are several times larger, so
/// there it needs 2 MiB (Rust's default for spawned threads); on 1 MiB, nested tables, blocks,
/// and function literals can run out from about 100 levels. That is also why the recursive path
/// through the parser is kept to small dispatching frames, with nodes built in separate
/// functions.
pub(crate) const MAX_NESTING: u32 = 200;

/// The `E0209` error for a tree nested more than [`MAX_NESTING`] levels deep, which every
/// surface's parser reports the same way.
pub(crate) fn too_deeply_nested(span: Span) -> Diagnostic {
    Diagnostic::error(
        "E0209",
        format!(
            "this is nested too deeply (more than {MAX_NESTING} levels of expressions and \
             blocks); split it up, e.g. by moving parts into local variables"
        ),
        span,
    )
}

/// Parse a token stream plus the `---` doc comments (as produced by [`crate::lexer::lex`])
/// into a [`Module`]. Doc comments are associated with the `function` they precede and their
/// `@param`/`@return` directives attached as [`FnAnnotations`].
pub fn parse(tokens: Vec<Token>, docs: Vec<DocComment>) -> Result<Module, Diagnostics> {
    let mut p = Parser {
        tokens,
        docs,
        pos: 0,
        depth: 0,
        reached: 0,
        diags: Diagnostics::new(),
    };
    match p.parse_module() {
        Ok(m) if !p.diags.has_errors() => Ok(m),
        _ => Err(p.diags),
    }
}

/// Signals that parsing aborted after a diagnostic was recorded.
struct Aborted;
type PResult<T> = Result<T, Aborted>;

struct Parser {
    tokens: Vec<Token>,
    docs: Vec<DocComment>,
    pos: usize,
    /// The nesting level of the expression or block being parsed.
    depth: u32,
    /// The deepest level the tree built so far reaches, within the current
    /// [`measured`](Self::measured) scope.
    reached: u32,
    diags: Diagnostics,
}

impl Parser {
    // ---- token cursor helpers ------------------------------------------------

    fn peek(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    fn peek_at(&self, n: usize) -> &TokenKind {
        let i = (self.pos + n).min(self.tokens.len() - 1);
        &self.tokens[i].kind
    }

    fn peek_span(&self) -> Span {
        self.tokens[self.pos].span
    }

    fn at(&self, k: &TokenKind) -> bool {
        self.peek() == k
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), TokenKind::Eof)
    }

    fn bump(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if !matches!(t.kind, TokenKind::Eof) {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, k: &TokenKind) -> bool {
        if self.at(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, k: TokenKind) -> PResult<Token> {
        if self.at(&k) {
            Ok(self.bump())
        } else {
            self.err(
                "E0100",
                format!(
                    "expected {}, found {}",
                    k.describe(),
                    self.peek().describe()
                ),
                self.peek_span(),
            )
        }
    }

    fn err<T>(&mut self, code: &'static str, msg: impl Into<String>, span: Span) -> PResult<T> {
        self.diags.push(Diagnostic::error(code, msg, span));
        Err(Aborted)
    }

    // ---- nesting limit (see the module docs) -------------------------------------

    /// Open one level of nesting for the expression or block about to be parsed.
    fn enter(&mut self) -> PResult<()> {
        self.depth += 1;
        self.note_level(self.depth)
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// Record that the tree under construction reaches `level`, failing past [`MAX_NESTING`].
    fn note_level(&mut self, level: u32) -> PResult<()> {
        if level > MAX_NESTING {
            self.diags.push(too_deeply_nested(self.peek_span()));
            return Err(Aborted);
        }
        self.reached = self.reached.max(level);
        Ok(())
    }

    /// Start measuring how many levels below the current depth the tree parsed next reaches,
    /// finished by [`end_measure`](Self::end_measure) with the value this returns. A loop that
    /// stacks nodes on top of what it has parsed (a left-associative operator chain, a suffix
    /// chain) uses this to account for the levels its own nodes add.
    ///
    /// A pair of calls rather than one taking a closure, because every frame on the parser's
    /// recursive path counts: in a debug build a closure frame costs as much stack as the work
    /// it wraps.
    fn start_measure(&mut self) -> u32 {
        std::mem::replace(&mut self.reached, self.depth)
    }

    fn end_measure(&mut self, outer: u32) -> u32 {
        let height = self.reached - self.depth;
        self.reached = self.reached.max(outer);
        height
    }

    fn expect_name(&mut self) -> PResult<Ident> {
        let span = self.peek_span();
        match self.peek().clone() {
            TokenKind::Name(n) => {
                self.bump();
                Ok(Spanned::new(n, span))
            }
            other => self.err(
                "E0101",
                format!("expected an identifier, found {}", other.describe()),
                span,
            ),
        }
    }

    // ---- module / top level --------------------------------------------------

    /// The chunk: function declarations, constants, and top-level statements in any order, then
    /// an optional export table (`SPEC.md` §3.1).
    ///
    /// `NAME = expr` is a constant declaration unless `NAME` is a top-level `local` declared
    /// above it, in which case it assigns that module variable, as it would in Lua. Only the
    /// top-level locals seen so far can be in scope there, so the parser can tell the two apart.
    fn parse_module(&mut self) -> PResult<Module> {
        let start = self.peek_span();
        let mut decls = Vec::new();
        let mut export = None;
        let mut top_locals: HashSet<String> = HashSet::new();

        loop {
            match self.peek() {
                TokenKind::Eof => break,
                TokenKind::Function => {
                    decls.push(TopDecl::Function(self.parse_func_decl(false)?));
                }
                TokenKind::Local if matches!(self.peek_at(1), TokenKind::Function) => {
                    let f = self.parse_func_decl(true)?;
                    top_locals.insert(f.name.node.clone());
                    decls.push(TopDecl::Function(f));
                }
                TokenKind::Name(n)
                    if matches!(self.peek_at(1), TokenKind::Assign) && !top_locals.contains(n) =>
                {
                    decls.push(TopDecl::Const(self.parse_const_decl()?));
                }
                TokenKind::Return => {
                    export = Some(self.parse_export_table()?);
                    // The export table must be the last thing in the chunk.
                    if !self.at_eof() {
                        return self.err(
                            "E0102",
                            "the `return` export table must be the last item in a module",
                            self.peek_span(),
                        );
                    }
                    break;
                }
                TokenKind::End | TokenKind::Else | TokenKind::Elseif | TokenKind::Until => {
                    let msg = format!(
                        "unexpected {} at module top level; it does not close anything",
                        self.peek().describe()
                    );
                    return self.err("E0105", msg, self.peek_span());
                }
                _ => {
                    let stat = self.parse_stat()?;
                    if let StatKind::Local { names, .. } = &stat.kind {
                        top_locals.extend(names.iter().map(|n| n.node.clone()));
                    }
                    if !matches!(stat.kind, StatKind::Empty) {
                        decls.push(TopDecl::Stat(Box::new(stat)));
                    }
                }
            }
        }

        let end = self.peek_span();
        Ok(Module {
            decls,
            export,
            span: start.to(end),
            surface: Surface::Lua,
        })
    }

    /// A top-level `function name(...)`, or with `local`, a `local function name(...)`.
    fn parse_func_decl(&mut self, local: bool) -> PResult<FuncDecl> {
        // Doc comments sitting between the previous token and the declaration annotate it
        // (`SPEC.md` §5.6).
        let annotations = self.gather_annotations_before(self.peek_span().start);
        let start = self.peek_span();
        if local {
            self.expect(TokenKind::Local)?;
        }
        self.expect(TokenKind::Function)?;
        let name = self.expect_name()?;
        let body = self.parse_func_body()?;
        let span = start.to(body.span);
        Ok(FuncDecl {
            name,
            local,
            body,
            annotations,
            span,
        })
    }

    /// Collect the run of `---` doc comments in the source gap `[prev_token_end, func_start)`
    /// and parse their `@param`/`@return` directives. Diagnostics from malformed directives are
    /// recorded; the returned annotations are best-effort.
    fn gather_annotations_before(&mut self, func_start: u32) -> FnAnnotations {
        let prev_end = if self.pos == 0 {
            0
        } else {
            self.tokens[self.pos - 1].span.end
        };
        let block: Vec<DocComment> = self
            .docs
            .iter()
            .filter(|d| d.span.start >= prev_end && d.span.end <= func_start)
            .cloned()
            .collect();
        if block.is_empty() {
            return FnAnnotations::default();
        }
        let (ann, diags) = crate::annotations::parse_doc_block(&block, Surface::Lua);
        for d in diags {
            self.diags.push(d);
        }
        ann
    }

    fn parse_const_decl(&mut self) -> PResult<ConstDecl> {
        let name = self.expect_name()?;
        self.expect(TokenKind::Assign)?;
        let value = self.parse_expr()?;
        let span = name.span.to(value.span);
        Ok(ConstDecl { name, value, span })
    }

    fn parse_export_table(&mut self) -> PResult<Spanned<Vec<Field>>> {
        let kw = self.expect(TokenKind::Return)?;
        if !self.at(&TokenKind::LBrace) {
            return self.err(
                "E0106",
                "a module's `return` must be a table constructor `{ ... }` of exports",
                self.peek_span(),
            );
        }
        let (fields, tspan) = self.parse_table_fields()?;
        self.eat(&TokenKind::Semi);
        Ok(Spanned::new(fields, kw.span.to(tspan)))
    }

    /// `function` params + body + `end`. Assumes `function` already consumed.
    fn parse_func_body(&mut self) -> PResult<FuncBody> {
        let lparen = self.expect(TokenKind::LParen)?;
        let mut params = Vec::new();
        if !self.at(&TokenKind::RParen) {
            loop {
                if self.at(&TokenKind::Ellipsis) {
                    return self.err(
                        "E0200",
                        "varargs `...` are not supported in Grindlang",
                        self.peek_span(),
                    );
                }
                params.push(self.expect_name()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(TokenKind::RParen)?;
        let block = self.parse_block()?;
        let end = self.expect(TokenKind::End)?;
        Ok(FuncBody {
            params,
            block,
            span: lparen.span.to(end.span),
            fallthrough: Fallthrough::Error,
        })
    }

    // ---- blocks & statements -------------------------------------------------

    /// Parse statements until a block terminator (`end`/`else`/`elseif`/EOF), including
    /// an optional trailing `return`.
    fn parse_block(&mut self) -> PResult<Block> {
        self.enter()?;
        let start = self.peek_span();
        let mut stats = Vec::new();
        let mut ret = None;

        loop {
            match self.peek() {
                TokenKind::End | TokenKind::Else | TokenKind::Elseif | TokenKind::Eof => break,
                TokenKind::Until => {
                    return self.err(
                        "E0201",
                        "`repeat ... until` loops are not supported in Grindlang; use `while`",
                        self.peek_span(),
                    );
                }
                TokenKind::Return => {
                    ret = Some(self.parse_ret_stat()?);
                    break;
                }
                _ => {
                    let s = self.parse_stat()?;
                    // Skip pure `;` separators in the statement list.
                    if !matches!(s.kind, StatKind::Empty) {
                        stats.push(s);
                    }
                }
            }
        }

        let end = self.peek_span();
        self.leave();
        Ok(Block {
            stats,
            ret,
            span: start.to(end),
        })
    }

    fn parse_ret_stat(&mut self) -> PResult<RetStat> {
        let kw = self.expect(TokenKind::Return)?;
        let mut exprs = Vec::new();
        // `return` may be empty or followed by an expression list.
        if !self.block_follows() && !self.at(&TokenKind::Semi) {
            exprs = self.parse_expr_list()?;
        }
        let mut span = kw.span;
        if let Some(last) = exprs.last() {
            span = span.to(last.span);
        }
        self.eat(&TokenKind::Semi);
        Ok(RetStat { exprs, span })
    }

    fn block_follows(&self) -> bool {
        matches!(
            self.peek(),
            TokenKind::End | TokenKind::Else | TokenKind::Elseif | TokenKind::Eof
        )
    }

    /// Only dispatches: every nested block recurses through here, so each statement is built in
    /// its own function rather than in this frame (see [`MAX_NESTING`]).
    fn parse_stat(&mut self) -> PResult<Stat> {
        let span0 = self.peek_span();
        match self.peek() {
            TokenKind::Semi | TokenKind::Break => self.parse_keyword_stat(),
            TokenKind::Local => self.parse_local(),
            TokenKind::Do => self.parse_do(),
            TokenKind::While => self.parse_while(),
            TokenKind::If => self.parse_if(),
            TokenKind::For => self.parse_for(),
            TokenKind::Repeat => self.err(
                "E0201",
                "`repeat ... until` loops are not supported in Grindlang; use `while`",
                span0,
            ),
            TokenKind::DoubleColon => self.err(
                "E0202",
                "labels (`::name::`) and `goto` are not supported in Grindlang",
                span0,
            ),
            _ => self.parse_expr_stat(),
        }
    }

    /// `;` (an empty statement) or `break`.
    fn parse_keyword_stat(&mut self) -> PResult<Stat> {
        let t = self.bump();
        let kind = match t.kind {
            TokenKind::Break => StatKind::Break,
            _ => StatKind::Empty,
        };
        Ok(Stat { kind, span: t.span })
    }

    fn parse_do(&mut self) -> PResult<Stat> {
        let kw = self.expect(TokenKind::Do)?;
        let block = self.parse_block()?;
        let end = self.expect(TokenKind::End)?;
        Ok(Stat {
            kind: StatKind::Do(block),
            span: kw.span.to(end.span),
        })
    }

    fn parse_local(&mut self) -> PResult<Stat> {
        let kw = self.expect(TokenKind::Local)?;
        if self.at(&TokenKind::Function) {
            self.bump();
            let name = self.expect_name()?;
            let body = self.parse_func_body()?;
            let span = kw.span.to(body.span);
            return Ok(Stat {
                kind: StatKind::LocalFunction { name, body },
                span,
            });
        }
        let mut names = vec![self.expect_name()?];
        while self.eat(&TokenKind::Comma) {
            names.push(self.expect_name()?);
        }
        let mut exprs = Vec::new();
        if self.eat(&TokenKind::Assign) {
            exprs = self.parse_expr_list()?;
        }
        let mut span = kw.span;
        if let Some(last) = exprs.last() {
            span = span.to(last.span);
        } else if let Some(last) = names.last() {
            span = span.to(last.span);
        }
        Ok(Stat {
            kind: StatKind::Local { names, exprs },
            span,
        })
    }

    fn parse_while(&mut self) -> PResult<Stat> {
        let kw = self.expect(TokenKind::While)?;
        let cond = self.parse_expr()?;
        self.expect(TokenKind::Do)?;
        let body = self.parse_block()?;
        let end = self.expect(TokenKind::End)?;
        Ok(Stat {
            kind: StatKind::While { cond, body },
            span: kw.span.to(end.span),
        })
    }

    fn parse_if(&mut self) -> PResult<Stat> {
        let kw = self.expect(TokenKind::If)?;
        let mut arms = Vec::new();
        let cond = self.parse_expr()?;
        self.expect(TokenKind::Then)?;
        let body = self.parse_block()?;
        arms.push((cond, body));

        while self.at(&TokenKind::Elseif) {
            self.bump();
            let c = self.parse_expr()?;
            self.expect(TokenKind::Then)?;
            let b = self.parse_block()?;
            arms.push((c, b));
        }

        let else_block = if self.eat(&TokenKind::Else) {
            Some(self.parse_block()?)
        } else {
            None
        };

        let end = self.expect(TokenKind::End)?;
        Ok(Stat {
            kind: StatKind::If { arms, else_block },
            span: kw.span.to(end.span),
        })
    }

    fn parse_for(&mut self) -> PResult<Stat> {
        let kw = self.expect(TokenKind::For)?;
        let first = self.expect_name()?;

        if self.at(&TokenKind::Assign) {
            // numeric for
            self.bump();
            let start = self.parse_expr()?;
            self.expect(TokenKind::Comma)?;
            let end_e = self.parse_expr()?;
            let step = if self.eat(&TokenKind::Comma) {
                Some(Box::new(self.parse_expr()?))
            } else {
                None
            };
            self.expect(TokenKind::Do)?;
            let body = self.parse_block()?;
            let endt = self.expect(TokenKind::End)?;
            Ok(Stat {
                kind: StatKind::NumericFor {
                    var: first,
                    start,
                    end: end_e,
                    step,
                    body,
                },
                span: kw.span.to(endt.span),
            })
        } else {
            // generic for
            let mut names = vec![first];
            while self.eat(&TokenKind::Comma) {
                names.push(self.expect_name()?);
            }
            self.expect(TokenKind::In)?;
            let iter = self.parse_iter_expr()?;
            self.expect(TokenKind::Do)?;
            let body = self.parse_block()?;
            let endt = self.expect(TokenKind::End)?;
            Ok(Stat {
                kind: StatKind::GenericFor { names, iter, body },
                span: kw.span.to(endt.span),
            })
        }
    }

    /// Generic-for iterator: only `ipairs(e)` / `pairs(e)` are allowed (SPEC §3.3).
    fn parse_iter_expr(&mut self) -> PResult<IterExpr> {
        let span0 = self.peek_span();
        let name = match self.peek().clone() {
            TokenKind::Name(n) => n,
            other => {
                return self.err(
                    "E0203",
                    format!(
                        "generic `for` may only iterate with `ipairs(...)` or `pairs(...)`, \
                         found {}",
                        other.describe()
                    ),
                    span0,
                );
            }
        };
        match name.as_str() {
            "ipairs" | "pairs" => {
                self.bump();
                self.expect(TokenKind::LParen)?;
                let arg = self.parse_expr()?;
                let close = self.expect(TokenKind::RParen)?;
                let span = span0.to(close.span);
                Ok(if name == "ipairs" {
                    IterExpr::IPairs { arg, span }
                } else {
                    IterExpr::Pairs { arg, span }
                })
            }
            _ => self.err(
                "E0203",
                "generic `for` may only iterate with `ipairs(...)` or `pairs(...)`",
                span0,
            ),
        }
    }

    /// A statement that begins with an expression: either an assignment or a call.
    fn parse_expr_stat(&mut self) -> PResult<Stat> {
        let first = self.parse_suffixed_expr()?;

        if self.at(&TokenKind::Assign) || self.at(&TokenKind::Comma) {
            let mut targets = vec![first];
            while self.eat(&TokenKind::Comma) {
                targets.push(self.parse_suffixed_expr()?);
            }
            // Validate that every target is assignable.
            for t in &targets {
                if !is_assignable(t) {
                    return self.err(
                        "E0204",
                        "cannot assign to this expression; assignment targets must be a \
                         name, field access, or index",
                        t.span,
                    );
                }
            }
            self.expect(TokenKind::Assign)?;
            let exprs = self.parse_expr_list()?;
            let mut span = targets[0].span;
            if let Some(last) = exprs.last() {
                span = span.to(last.span);
            }
            Ok(Stat {
                kind: StatKind::Assign { targets, exprs },
                span,
            })
        } else {
            // Must be a call to be a valid statement.
            if matches!(
                first.kind,
                ExprKind::Call { .. } | ExprKind::MethodCall { .. }
            ) {
                let span = first.span;
                Ok(Stat {
                    kind: StatKind::Call(first),
                    span,
                })
            } else if matches!(&first.kind, ExprKind::Name(n) if n == "continue") {
                // Luau's `continue` is no reserved word: a statement starting with the name, which
                // doesn't call or assign it, is the statement.
                Ok(Stat {
                    kind: StatKind::Continue,
                    span: first.span,
                })
            } else {
                self.err(
                    "E0205",
                    "this expression is not a statement; only function calls and \
                     assignments may appear as statements",
                    first.span,
                )
            }
        }
    }

    // ---- expressions (Pratt) -------------------------------------------------

    fn parse_expr_list(&mut self) -> PResult<Vec<Expr>> {
        let mut list = vec![self.parse_expr()?];
        while self.eat(&TokenKind::Comma) {
            list.push(self.parse_expr()?);
        }
        Ok(list)
    }

    fn parse_expr(&mut self) -> PResult<Expr> {
        self.parse_expr_bp(0)
    }

    /// Pratt expression parser. `min_bp` is the minimum left binding power that an infix
    /// operator must have to be consumed at this level (see SPEC §3.5).
    ///
    /// Nested parentheses, unary operators and right-associative operators all recurse through
    /// here, so it only opens the nesting level and parses the first operand; the infix loop
    /// runs in [`parse_infix`](Self::parse_infix), whose larger frame is then not on the stack
    /// once per level while the operand is parsed.
    fn parse_expr_bp(&mut self, min_bp: u8) -> PResult<Expr> {
        self.enter()?;
        let outer = self.start_measure();
        let lhs = self.parse_operand()?;
        let height = self.end_measure(outer);
        let e = self.parse_infix(lhs, height, min_bp)?;
        self.leave();
        Ok(e)
    }

    /// Fold infix operators onto `lhs`, which reaches `height` levels below the current depth.
    /// Each operator folded puts everything parsed so far one level deeper.
    fn parse_infix(&mut self, mut lhs: Expr, mut height: u32, min_bp: u8) -> PResult<Expr> {
        while let Some(op) = binary_op(self.peek()) {
            let (l_bp, r_bp) = infix_bp(op);
            if l_bp < min_bp {
                break;
            }
            self.bump(); // operator
            let outer = self.start_measure();
            let rhs = self.parse_expr_bp(r_bp)?;
            height = height.max(self.end_measure(outer)) + 1;
            self.note_level(self.depth + height)?;
            let span = lhs.span.to(rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            };
        }
        Ok(lhs)
    }

    /// A unary operation or a primary expression: the operand an infix chain starts from.
    fn parse_operand(&mut self) -> PResult<Expr> {
        let Some(op) = unary_op(self.peek()) else {
            return self.parse_primary();
        };
        let op_tok = self.bump();
        let operand = self.parse_expr_bp(UNARY_BP)?;
        let span = op_tok.span.to(operand.span);
        Ok(Expr {
            kind: ExprKind::Unary {
                op,
                operand: Box::new(operand),
            },
            span,
        })
    }

    /// Primary expressions: literals, anonymous functions, table constructors, and
    /// suffixed prefix-expressions (names / parens followed by `.`/`[]`/calls).
    ///
    /// Only dispatches, so that a parenthesized expression recursing through here doesn't carry
    /// the frame of [`parse_literal`](Self::parse_literal) once per level.
    fn parse_primary(&mut self) -> PResult<Expr> {
        match self.peek() {
            TokenKind::Name(_) | TokenKind::LParen => self.parse_suffixed_expr(),
            _ => self.parse_literal(),
        }
    }

    /// A primary expression that isn't a prefix-expression: a literal, an anonymous function,
    /// or a table constructor.
    fn parse_literal(&mut self) -> PResult<Expr> {
        let span = self.peek_span();
        match self.peek().clone() {
            TokenKind::Nil => {
                self.bump();
                Ok(Expr {
                    kind: ExprKind::Nil,
                    span,
                })
            }
            TokenKind::True => {
                self.bump();
                Ok(Expr {
                    kind: ExprKind::Bool(true),
                    span,
                })
            }
            TokenKind::False => {
                self.bump();
                Ok(Expr {
                    kind: ExprKind::Bool(false),
                    span,
                })
            }
            TokenKind::Number(n) => {
                self.bump();
                Ok(Expr {
                    kind: ExprKind::Number(n),
                    span,
                })
            }
            TokenKind::Str(s) => {
                self.bump();
                Ok(Expr {
                    kind: ExprKind::Str(s),
                    span,
                })
            }
            TokenKind::Ellipsis => self.err(
                "E0200",
                "varargs `...` are not supported in Grindlang",
                span,
            ),
            TokenKind::Function => {
                self.bump();
                let body = self.parse_func_body()?;
                let fspan = span.to(body.span);
                Ok(Expr {
                    kind: ExprKind::Function(body),
                    span: fspan,
                })
            }
            TokenKind::LBrace => {
                let (fields, tspan) = self.parse_table_fields()?;
                Ok(Expr {
                    kind: ExprKind::Table(fields),
                    span: tspan,
                })
            }
            other => self.err(
                "E0206",
                format!("expected an expression, found {}", other.describe()),
                span,
            ),
        }
    }

    /// A prefix-expression (name or parenthesized expr) followed by any chain of
    /// suffixes: `.name`, `[expr]`, `(args)`, `{table}`, `"str"`, and `:method(args)`.
    ///
    /// Like an infix chain, each suffix puts everything parsed so far one level deeper. The
    /// suffix loop runs in [`parse_suffixes`](Self::parse_suffixes), after the atom, for the
    /// same reason [`parse_expr_bp`](Self::parse_expr_bp) splits off its infix loop.
    fn parse_suffixed_expr(&mut self) -> PResult<Expr> {
        let outer = self.start_measure();
        let atom = self.parse_atom()?;
        let height = self.end_measure(outer);
        self.parse_suffixes(atom, height)
    }

    /// Apply any suffixes following `e`, which reaches `height` levels below the current depth.
    fn parse_suffixes(&mut self, mut e: Expr, mut height: u32) -> PResult<Expr> {
        loop {
            match self.peek() {
                TokenKind::Dot => {
                    self.bump();
                    let name = self.expect_name()?;
                    height += 1;
                    self.note_level(self.depth + height)?;
                    let span = e.span.to(name.span);
                    e = Expr {
                        kind: ExprKind::Field {
                            base: Box::new(e),
                            name,
                        },
                        span,
                    };
                }
                TokenKind::LBracket => {
                    self.bump();
                    let outer = self.start_measure();
                    let index = self.parse_expr()?;
                    height = height.max(self.end_measure(outer)) + 1;
                    let close = self.expect(TokenKind::RBracket)?;
                    self.note_level(self.depth + height)?;
                    let span = e.span.to(close.span);
                    e = Expr {
                        kind: ExprKind::Index {
                            base: Box::new(e),
                            index: Box::new(index),
                            zero_based: false,
                        },
                        span,
                    };
                }
                TokenKind::Colon => {
                    self.bump();
                    let method = self.expect_name()?;
                    let outer = self.start_measure();
                    let (args, aspan) = self.parse_call_args()?;
                    height = height.max(self.end_measure(outer)) + 1;
                    self.note_level(self.depth + height)?;
                    let span = e.span.to(aspan);
                    e = Expr {
                        kind: ExprKind::MethodCall {
                            receiver: Box::new(e),
                            method,
                            args,
                        },
                        span,
                    };
                }
                TokenKind::LParen | TokenKind::LBrace | TokenKind::Str(_) => {
                    let outer = self.start_measure();
                    let (args, aspan) = self.parse_call_args()?;
                    height = height.max(self.end_measure(outer)) + 1;
                    self.note_level(self.depth + height)?;
                    let span = e.span.to(aspan);
                    e = Expr {
                        kind: ExprKind::Call {
                            callee: Box::new(e),
                            args,
                        },
                        span,
                    };
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn parse_atom(&mut self) -> PResult<Expr> {
        let span = self.peek_span();
        match self.peek().clone() {
            TokenKind::Name(n) => {
                self.bump();
                Ok(Expr {
                    kind: ExprKind::Name(n),
                    span,
                })
            }
            TokenKind::LParen => {
                self.bump();
                let inner = self.parse_expr()?;
                let close = self.expect(TokenKind::RParen)?;
                Ok(Expr {
                    kind: ExprKind::Paren(Box::new(inner)),
                    span: span.to(close.span),
                })
            }
            other => self.err(
                "E0207",
                format!("expected a name or `(`, found {}", other.describe()),
                span,
            ),
        }
    }

    /// Call arguments in any of the three Lua forms: `(a, b)`, `{table}`, or `"str"`.
    fn parse_call_args(&mut self) -> PResult<(Vec<Expr>, Span)> {
        match self.peek().clone() {
            TokenKind::LParen => {
                let open = self.bump();
                let mut args = Vec::new();
                if !self.at(&TokenKind::RParen) {
                    args = self.parse_expr_list()?;
                }
                let close = self.expect(TokenKind::RParen)?;
                Ok((args, open.span.to(close.span)))
            }
            TokenKind::LBrace => {
                let (fields, tspan) = self.parse_table_fields()?;
                Ok((
                    vec![Expr {
                        kind: ExprKind::Table(fields),
                        span: tspan,
                    }],
                    tspan,
                ))
            }
            TokenKind::Str(s) => {
                let t = self.bump();
                Ok((
                    vec![Expr {
                        kind: ExprKind::Str(s),
                        span: t.span,
                    }],
                    t.span,
                ))
            }
            other => self.err(
                "E0208",
                format!("expected call arguments, found {}", other.describe()),
                self.peek_span(),
            ),
        }
    }

    /// `{ field, field; field }` — returns the fields and the whole-table span.
    fn parse_table_fields(&mut self) -> PResult<(Vec<Field>, Span)> {
        let open = self.expect(TokenKind::LBrace)?;
        let mut fields = Vec::new();
        while !self.at(&TokenKind::RBrace) && !self.at_eof() {
            let field = self.parse_field()?;
            fields.push(field);
            // Separator: `,` or `;`. If neither, the table must be closing.
            if !self.eat(&TokenKind::Comma) && !self.eat(&TokenKind::Semi) {
                break;
            }
        }
        let close = self.expect(TokenKind::RBrace)?;
        Ok((fields, open.span.to(close.span)))
    }

    fn parse_field(&mut self) -> PResult<Field> {
        match self.peek() {
            // `[key] = value`
            TokenKind::LBracket => {
                self.bump();
                let key = self.parse_expr()?;
                self.expect(TokenKind::RBracket)?;
                self.expect(TokenKind::Assign)?;
                let value = self.parse_expr()?;
                Ok(Field::Keyed { key, value })
            }
            // `name = value` (only when the `=` follows; otherwise it's a positional
            // expression that happens to start with a name).
            TokenKind::Name(_) if matches!(self.peek_at(1), TokenKind::Assign) => {
                let name = self.expect_name()?;
                self.expect(TokenKind::Assign)?;
                let value = self.parse_expr()?;
                Ok(Field::Named { name, value })
            }
            // positional
            _ => {
                let value = self.parse_expr()?;
                Ok(Field::Positional(value))
            }
        }
    }
}

/// Whether an expression is a legal assignment target (name, field, or index).
fn is_assignable(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Name(_) | ExprKind::Field { .. } | ExprKind::Index { .. }
    )
}

fn unary_op(k: &TokenKind) -> Option<UnOp> {
    Some(match k {
        TokenKind::Minus => UnOp::Neg,
        TokenKind::Not => UnOp::Not,
        TokenKind::Hash => UnOp::Len,
        _ => return None,
    })
}

fn binary_op(k: &TokenKind) -> Option<BinOp> {
    Some(match k {
        TokenKind::Plus => BinOp::Add,
        TokenKind::Minus => BinOp::Sub,
        TokenKind::Star => BinOp::Mul,
        TokenKind::Slash => BinOp::Div,
        TokenKind::DoubleSlash => BinOp::FloorDiv,
        TokenKind::Percent => BinOp::Mod,
        TokenKind::Caret => BinOp::Pow,
        TokenKind::DotDot => BinOp::Concat,
        TokenKind::Eq => BinOp::Eq,
        TokenKind::Ne => BinOp::Ne,
        TokenKind::Lt => BinOp::Lt,
        TokenKind::Le => BinOp::Le,
        TokenKind::Gt => BinOp::Gt,
        TokenKind::Ge => BinOp::Ge,
        TokenKind::And => BinOp::And,
        TokenKind::Or => BinOp::Or,
        _ => return None,
    })
}

/// Unary binding power: tighter than `*`/`/` but looser than `^` (SPEC §3.5).
const UNARY_BP: u8 = 13;

/// `(left_bp, right_bp)` for each infix operator. Right-associative operators have
/// `right_bp < left_bp` (`..` and `^`).
fn infix_bp(op: BinOp) -> (u8, u8) {
    match op {
        BinOp::Or => (1, 2),
        BinOp::And => (3, 4),
        BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne => (5, 6),
        BinOp::Concat => (8, 7), // right-assoc
        BinOp::Add | BinOp::Sub => (9, 10),
        BinOp::Mul | BinOp::Div | BinOp::FloorDiv | BinOp::Mod => (11, 12),
        BinOp::Pow => (16, 15), // right-assoc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn parse_src(src: &str) -> Result<Module, Diagnostics> {
        let (tokens, docs) = lex(src).unwrap();
        parse(tokens, docs)
    }

    fn parse_ok(src: &str) -> Module {
        parse_src(src).unwrap_or_else(|d| panic!("parse failed: {d}"))
    }

    fn err_code(src: &str) -> String {
        let d = parse_src(src).unwrap_err();
        d.0[0].code.to_string()
    }

    fn func_decl(m: &Module, idx: usize) -> &FuncDecl {
        match &m.decls[idx] {
            TopDecl::Function(f) => f,
            _ => panic!("decl {idx} is not a function"),
        }
    }

    #[test]
    fn doc_comments_attach_to_following_function() {
        let m = parse_ok(
            "---@param base number\n\
             ---@return number\n\
             function f(base) return base end",
        );
        let f = func_decl(&m, 0);
        assert_eq!(f.annotations.params.len(), 1);
        assert_eq!(f.annotations.params[0].name.node, "base");
        assert_eq!(f.annotations.ret.len(), 1);
    }

    #[test]
    fn doc_comments_bind_only_the_next_function() {
        // The annotation precedes `f`; `g` must see no annotations.
        let m = parse_ok(
            "---@param x number\n\
             function f(x) return x end\n\
             function g(y) return y end",
        );
        assert_eq!(func_decl(&m, 0).annotations.params.len(), 1);
        assert!(func_decl(&m, 1).annotations.is_empty());
    }

    #[test]
    fn plain_comments_are_not_annotations() {
        // A `--` (two-dash) comment is not a doc comment and attaches nothing.
        let m = parse_ok("-- @param x number\nfunction f(x) return x + 0 end");
        assert!(func_decl(&m, 0).annotations.is_empty());
    }

    #[test]
    fn malformed_annotation_is_a_parse_error() {
        assert_eq!(
            err_code("---@param x number[\nfunction f(x) return 1 end"),
            "E0110",
        );
    }

    #[test]
    fn parses_function_and_const_exports() {
        let m = parse_ok(
            "MAX = 99\n\
             function f(a, b)\n  return a + b\nend",
        );
        assert_eq!(m.decls.len(), 2);
        assert!(matches!(m.decls[0], TopDecl::Const(_)));
        assert!(matches!(m.decls[1], TopDecl::Function(_)));
        assert!(m.export.is_none());
    }

    #[test]
    fn parses_curated_export_table() {
        let m = parse_ok(
            "function f() return 1 end\n\
             return { g = f }",
        );
        let export = m.export.expect("export table");
        assert_eq!(export.node.len(), 1);
    }

    #[test]
    fn precedence_pow_is_right_assoc_and_tighter_than_unary() {
        // -x^2 parses as -(x^2)
        let m = parse_ok("function f(x) return -x^2 end");
        let TopDecl::Function(fd) = &m.decls[0] else {
            panic!()
        };
        let ret = fd.body.block.ret.as_ref().unwrap();
        let ExprKind::Unary { op, operand } = &ret.exprs[0].kind else {
            panic!("expected unary, got {:?}", ret.exprs[0].kind)
        };
        assert_eq!(*op, UnOp::Neg);
        assert!(matches!(
            operand.kind,
            ExprKind::Binary { op: BinOp::Pow, .. }
        ));
    }

    #[test]
    fn precedence_mul_binds_tighter_than_add() {
        let m = parse_ok("function f() return 1 + 2 * 3 end");
        let TopDecl::Function(fd) = &m.decls[0] else {
            panic!()
        };
        let ret = fd.body.block.ret.as_ref().unwrap();
        let ExprKind::Binary { op, rhs, .. } = &ret.exprs[0].kind else {
            panic!()
        };
        assert_eq!(*op, BinOp::Add);
        assert!(matches!(rhs.kind, ExprKind::Binary { op: BinOp::Mul, .. }));
    }

    #[test]
    fn parses_control_flow_and_calls() {
        parse_ok(
            "function f(t)\n\
               local sum = 0\n\
               for i = 1, 10 do\n\
                 if i % 2 == 0 then\n\
                   sum = sum + i\n\
                 end\n\
               end\n\
               for _, v in ipairs(t) do\n\
                 sum = sum + v\n\
               end\n\
               return sum\n\
             end",
        );
    }

    #[test]
    fn parses_top_level_statements_in_order() {
        let m = parse_ok(
            "local rate = 2\n\
             for i = 1, 3 do rate = rate + i end\n\
             function f(x) return x * rate end\n\
             setup()\n\
             TOTAL = f(1)",
        );
        assert!(
            matches!(&m.decls[0], TopDecl::Stat(s) if matches!(s.kind, StatKind::Local { .. }))
        );
        assert!(
            matches!(&m.decls[1], TopDecl::Stat(s) if matches!(s.kind, StatKind::NumericFor { .. }))
        );
        assert!(matches!(m.decls[2], TopDecl::Function(_)));
        assert!(matches!(&m.decls[3], TopDecl::Stat(s) if matches!(s.kind, StatKind::Call(_))));
        assert!(matches!(m.decls[4], TopDecl::Const(_)));
    }

    /// `x = …` assigns a top-level local declared above it, as in Lua; with no such local it
    /// declares a constant.
    #[test]
    fn a_name_assignment_is_a_constant_unless_a_top_level_local_is_in_scope() {
        let m = parse_ok("X = 1\nlocal y = 1\ny = 2");
        assert!(matches!(m.decls[0], TopDecl::Const(_)));
        assert!(
            matches!(&m.decls[2], TopDecl::Stat(s) if matches!(s.kind, StatKind::Assign { .. }))
        );
    }

    #[test]
    fn a_top_level_local_function_is_a_private_declaration() {
        let m = parse_ok("local function helper(x) return x end");
        assert!(func_decl(&m, 0).local);
    }

    #[test]
    fn rejects_a_stray_block_closer_at_top_level() {
        assert_eq!(err_code("end"), "E0105");
    }

    #[test]
    fn rejects_repeat() {
        assert_eq!(
            err_code("function f() repeat local x = 1 until true end"),
            "E0201"
        );
    }

    #[test]
    fn rejects_varargs() {
        assert_eq!(err_code("function f(...) return 1 end"), "E0200");
    }

    #[test]
    fn rejects_goto_label() {
        assert_eq!(err_code("function f() ::top:: end"), "E0202");
    }

    #[test]
    fn rejects_bad_iterator() {
        assert_eq!(
            err_code("function f(t) for k in next(t) do end end"),
            "E0203"
        );
    }

    #[test]
    fn rejects_bad_assignment_target() {
        // A call result is a valid prefix-expression but not an assignable target.
        assert_eq!(err_code("function f() g() = 2 end"), "E0204");
    }

    #[test]
    fn rejects_non_statement_expression() {
        // A parenthesized expression is neither a call nor an assignment.
        assert_eq!(err_code("function f(x) (x) end"), "E0205");
    }

    // ---- nesting limit (E0209) ----

    fn nested(open: &str, inner: &str, close: &str, n: usize) -> String {
        format!("{}{inner}{}", open.repeat(n), close.repeat(n))
    }

    #[test]
    fn rejects_deeply_nested_parentheses() {
        assert_eq!(
            err_code(&format!("X = {}", nested("(", "1", ")", 300))),
            "E0209"
        );
    }

    #[test]
    fn rejects_deeply_nested_blocks() {
        let body = nested("do ", "", "end ", 300);
        assert_eq!(err_code(&format!("function f() {body}end")), "E0209");
    }

    #[test]
    fn rejects_deeply_nested_tables_and_calls() {
        assert_eq!(
            err_code(&format!("X = {}", nested("{", "1", "}", 300))),
            "E0209"
        );
        let call = nested("f(", "1", ")", 300);
        assert_eq!(
            err_code(&format!("function f(x) return {call} end")),
            "E0209"
        );
    }

    #[test]
    fn rejects_a_long_operator_chain() {
        // Parsed by a loop, not by recursion, but the tree is still 10,000 levels deep.
        let chain = " + 1".repeat(10_000);
        assert_eq!(err_code(&format!("X = 1{chain}")), "E0209");
    }

    #[test]
    fn rejects_a_long_suffix_chain() {
        let chain = ".a".repeat(10_000);
        assert_eq!(
            err_code(&format!("function f(t) return t{chain} end")),
            "E0209"
        );
    }

    #[test]
    fn a_chain_counts_the_depth_of_its_operands() {
        // Every `+ 1` folded after the parentheses pushes them one level deeper, so 150 levels
        // of parentheses plus 100 operators is 250 levels, although neither alone is too deep.
        let parens = nested("(", "1", ")", 150);
        assert_eq!(
            err_code(&format!("X = 1 + {parens}{}", " + 1".repeat(100))),
            "E0209"
        );
        parse_ok(&format!("X = 1 + {parens}{}", " + 1".repeat(20)));
    }

    #[test]
    fn accepts_nesting_within_the_limit() {
        parse_ok(&format!("X = {}", nested("(", "1", ")", 190)));
        parse_ok(&format!("X = 1{}", " + 1".repeat(190)));
        parse_ok(&format!(
            "function f() {}end",
            nested("do ", "", "end ", 190)
        ));
    }
}
