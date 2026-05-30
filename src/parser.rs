//! Recursive-descent + Pratt parser for the Grindlang grammar (`SPEC.md` §3).
//!
//! Two responsibilities beyond building the AST:
//!   * **Enforce the top-level constraint contract** structurally — the chunk is a list
//!     of function/const declarations plus an optional curated `return { ... }`. A
//!     top-level `local`, a stray statement, etc. are rejected here with a pointed
//!     diagnostic. (Deeper rules — const-ness of `const` RHS, no free globals, type
//!     checks — belong to later phases.)
//!   * **Reject unsupported-but-lexable constructs** (`repeat`/`until`, `goto`/labels,
//!     varargs `...`) where they appear, with a "not supported in Grindlang" message.
//!
//! The parser bails on the first error (no recovery) — adequate for the current phase;
//! multi-error recovery can be layered on later.

use crate::ast::*;
use crate::diagnostics::{Diagnostic, Diagnostics, Span};
use crate::lexer::{Token, TokenKind};

/// Parse a token stream (as produced by [`crate::lexer::lex`]) into a [`Module`].
pub fn parse(tokens: Vec<Token>) -> Result<Module, Diagnostics> {
    let mut p = Parser {
        tokens,
        pos: 0,
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
    pos: usize,
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

    fn parse_module(&mut self) -> PResult<Module> {
        let start = self.peek_span();
        let mut decls = Vec::new();
        let mut export = None;

        loop {
            match self.peek() {
                TokenKind::Eof => break,
                TokenKind::Function => {
                    decls.push(TopDecl::Function(self.parse_func_decl()?));
                }
                TokenKind::Name(_) if matches!(self.peek_at(1), TokenKind::Assign) => {
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
                TokenKind::Local => {
                    return self.err(
                        "E0103",
                        "top-level `local` variables are not allowed in Grindlang; \
                         declare module functions/constants at the top level and keep \
                         mutable locals inside function bodies",
                        self.peek_span(),
                    );
                }
                TokenKind::Name(_) => {
                    // A name not followed by `=` at top level — likely a stray call or
                    // an attempt at an executable statement.
                    return self.err(
                        "E0104",
                        "unexpected statement at module top level; the top level may only \
                         contain `function` declarations, `name = <const>` declarations, \
                         and an optional trailing `return { ... }`",
                        self.peek_span(),
                    );
                }
                other => {
                    let msg = format!(
                        "unexpected {} at module top level; expected a `function` or \
                         `name = <const>` declaration",
                        other.describe()
                    );
                    return self.err("E0105", msg, self.peek_span());
                }
            }
        }

        let end = self.peek_span();
        Ok(Module {
            decls,
            export,
            span: start.to(end),
        })
    }

    fn parse_func_decl(&mut self) -> PResult<FuncDecl> {
        let kw = self.expect(TokenKind::Function)?;
        let name = self.expect_name()?;
        let body = self.parse_func_body()?;
        let span = kw.span.to(body.span);
        Ok(FuncDecl { name, body, span })
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
        })
    }

    // ---- blocks & statements -------------------------------------------------

    /// Parse statements until a block terminator (`end`/`else`/`elseif`/EOF), including
    /// an optional trailing `return`.
    fn parse_block(&mut self) -> PResult<Block> {
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

    fn parse_stat(&mut self) -> PResult<Stat> {
        let span0 = self.peek_span();
        match self.peek() {
            TokenKind::Semi => {
                self.bump();
                Ok(Stat {
                    kind: StatKind::Empty,
                    span: span0,
                })
            }
            TokenKind::Local => self.parse_local(),
            TokenKind::Do => {
                self.bump();
                let block = self.parse_block()?;
                let end = self.expect(TokenKind::End)?;
                Ok(Stat {
                    kind: StatKind::Do(block),
                    span: span0.to(end.span),
                })
            }
            TokenKind::While => self.parse_while(),
            TokenKind::If => self.parse_if(),
            TokenKind::For => self.parse_for(),
            TokenKind::Break => {
                let t = self.bump();
                Ok(Stat {
                    kind: StatKind::Break,
                    span: t.span,
                })
            }
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
                Some(self.parse_expr()?)
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
    fn parse_expr_bp(&mut self, min_bp: u8) -> PResult<Expr> {
        // Prefix / unary.
        let mut lhs = if let Some(op) = unary_op(self.peek()) {
            let op_tok = self.bump();
            let r_bp = UNARY_BP;
            let operand = self.parse_expr_bp(r_bp)?;
            let span = op_tok.span.to(operand.span);
            Expr {
                kind: ExprKind::Unary {
                    op,
                    operand: Box::new(operand),
                },
                span,
            }
        } else {
            self.parse_primary()?
        };

        // Infix.
        while let Some(op) = binary_op(self.peek()) {
            let (l_bp, r_bp) = infix_bp(op);
            if l_bp < min_bp {
                break;
            }
            self.bump(); // operator
            let rhs = self.parse_expr_bp(r_bp)?;
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

    /// Primary expressions: literals, anonymous functions, table constructors, and
    /// suffixed prefix-expressions (names / parens followed by `.`/`[]`/calls).
    fn parse_primary(&mut self) -> PResult<Expr> {
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
            TokenKind::Name(_) | TokenKind::LParen => self.parse_suffixed_expr(),
            other => self.err(
                "E0206",
                format!("expected an expression, found {}", other.describe()),
                span,
            ),
        }
    }

    /// A prefix-expression (name or parenthesized expr) followed by any chain of
    /// suffixes: `.name`, `[expr]`, `(args)`, `{table}`, `"str"`, and `:method(args)`.
    fn parse_suffixed_expr(&mut self) -> PResult<Expr> {
        let mut e = self.parse_atom()?;
        loop {
            match self.peek() {
                TokenKind::Dot => {
                    self.bump();
                    let name = self.expect_name()?;
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
                    let index = self.parse_expr()?;
                    let close = self.expect(TokenKind::RBracket)?;
                    let span = e.span.to(close.span);
                    e = Expr {
                        kind: ExprKind::Index {
                            base: Box::new(e),
                            index: Box::new(index),
                        },
                        span,
                    };
                }
                TokenKind::Colon => {
                    self.bump();
                    let method = self.expect_name()?;
                    let (args, aspan) = self.parse_call_args()?;
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
                    let (args, aspan) = self.parse_call_args()?;
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
        parse(lex(src).unwrap())
    }

    fn parse_ok(src: &str) -> Module {
        parse_src(src).unwrap_or_else(|d| panic!("parse failed: {d}"))
    }

    fn err_code(src: &str) -> String {
        let d = parse_src(src).unwrap_err();
        d.0[0].code.to_string()
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
    fn rejects_top_level_local() {
        assert_eq!(err_code("local x = 1"), "E0103");
    }

    #[test]
    fn rejects_top_level_statement() {
        assert_eq!(err_code("foo()"), "E0104");
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
}
