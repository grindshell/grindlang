//! Parser for the Ruby surface (`SPEC-RUBY.md` §3): tokens to a [`Program`].
//!
//! Recursive descent for statements, Pratt parsing for operators. Beyond building the tree it
//! enforces the rules that keep Ruby's syntax unambiguous without knowing which names are
//! locals:
//!
//! * **Two expression levels**, as in Ruby's grammar. `not`, `and`, and `or` bind more loosely
//!   than `=` and argument commas, so they may appear only in conditions, in parentheses, and in
//!   an expression statement ([`parse_expr`](Parser::parse_expr)); everywhere else takes an
//!   `arg` ([`parse_arg`](Parser::parse_arg)). Where Ruby would read `x = a or b` as
//!   `(x = a) or b`, the parser rejects it (`E0528`).
//! * **Calls without parentheses** only at the start of a statement (§3.4). A name followed by
//!   what could be its argument anywhere else is `E0527`, as is a name followed by a spaced
//!   `-`, `*`, `[`, or `(` with no space after (`foo -1`), which Ruby reads by whitespace.
//! * **`do` blocks** attach to the call a statement starts with when that call has arguments
//!   without parentheses (`on :hit do … end`), and never to a `while` condition's last call;
//!   otherwise to the call just before them. A `{` right after a call is always its block.
//! * **Rejections.** Every construct of `SPEC-RUBY.md` §6 that shows in the syntax gets a
//!   targeted diagnostic (`E0512`–`E0528`); the lexer rejects the literals.
//!
//! Like the Lua parser it stops at the first error, and it refuses to build a tree nested more
//! than [`MAX_NESTING`] levels deep (`E0209`; see [`crate::parser`]).

use crate::ast::Spanned;
use crate::diagnostics::{Diagnostic, Diagnostics, Span};
use crate::lexer::DocComment;
use crate::parser::{MAX_NESTING, too_deeply_nested};

use super::ast::*;
use super::lexer::{Token, TokenKind};

/// Parse the tokens and comments [`super::lexer::lex`] produced from `src` into a [`Program`].
pub fn parse(src: &str, tokens: Vec<Token>, docs: Vec<DocComment>) -> Result<Program, Diagnostics> {
    let mut p = Parser {
        src,
        tokens,
        docs,
        pos: 0,
        depth: 0,
        reached: 0,
        no_do: false,
        in_ternary: false,
        diags: Diagnostics::new(),
    };
    match p.parse_program() {
        Ok(program) => Ok(program),
        Err(Aborted) => Err(p.diags),
    }
}

/// Signals that parsing aborted after a diagnostic was recorded.
struct Aborted;
type PResult<T> = Result<T, Aborted>;

/// Binding powers of the operators below the unary ones, loosest first (`SPEC-RUBY.md` §3.5).
const TERNARY_BP: u8 = 2;
const RANGE_BP: u8 = 4;
/// Unary minus binds tighter than `*` but looser than `**`: `-x ** 2` is `-(x ** 2)`.
const NEG_BP: u8 = 20;
const POW_BP: u8 = 22;
/// `!` binds tighter than everything: `!a == b` is `(!a) == b`.
const NOT_BP: u8 = 23;

/// An infix operator the Pratt loop folds.
#[derive(Clone, Copy)]
enum Infix {
    Binary(BinOp),
    Range { exclusive: bool },
    Ternary,
}

impl Infix {
    fn of(k: &TokenKind) -> Option<Infix> {
        use TokenKind::*;
        let op = match k {
            OrOr => BinOp::Or,
            AndAnd => BinOp::And,
            EqEq => BinOp::Eq,
            NotEq => BinOp::Ne,
            Lt => BinOp::Lt,
            Le => BinOp::Le,
            Gt => BinOp::Gt,
            Ge => BinOp::Ge,
            Shl => BinOp::Append,
            Plus => BinOp::Add,
            Minus => BinOp::Sub,
            Star => BinOp::Mul,
            Slash => BinOp::Div,
            Percent => BinOp::Mod,
            Pow => BinOp::Pow,
            DotDot => return Some(Infix::Range { exclusive: false }),
            DotDotDot => return Some(Infix::Range { exclusive: true }),
            Question => return Some(Infix::Ternary),
            _ => return None,
        };
        Some(Infix::Binary(op))
    }

    /// `(left, right)` binding powers; a right-associative operator's right is lower.
    fn binding_power(self) -> (u8, u8) {
        match self {
            Infix::Ternary => (TERNARY_BP, TERNARY_BP),
            Infix::Range { .. } => (RANGE_BP, RANGE_BP + 1),
            Infix::Binary(op) => match op {
                BinOp::Or => (6, 7),
                BinOp::And => (8, 9),
                BinOp::Eq | BinOp::Ne => (10, 11),
                BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => (12, 13),
                BinOp::Append => (14, 15),
                BinOp::Add | BinOp::Sub => (16, 17),
                BinOp::Mul | BinOp::Div | BinOp::Mod => (18, 19),
                BinOp::Pow => (POW_BP, POW_BP - 1),
            },
        }
    }
}

/// The diagnostic for an operator Ruby has and Grindlang doesn't, where an infix operator
/// would be read.
fn rejected_infix(k: &TokenKind) -> Option<(&'static str, &'static str)> {
    use TokenKind::*;
    Some(match k {
        Amp | Pipe | Caret | Shr => ("E0526", "bitwise operators are not supported in Grindlang"),
        Spaceship => (
            "E0526",
            "`<=>` is not supported in Grindlang; compare with `<`, `==`, and `>`",
        ),
        EqEqEq => (
            "E0526",
            "`===` is not supported in Grindlang; use `==`, or `case … when`",
        ),
        Match | NotMatch => (
            "E0526",
            "`=~` and `!~` are not supported in Grindlang: there are no regular expressions",
        ),
        _ => return None,
    })
}

/// Whether a token can begin a call's first argument when the call has no parentheses
/// (`SPEC-RUBY.md` §3.4). `-`, `[`, `(`, `*`, `&`, and `{` are left out: after a name they are
/// an operator, an index, the call's own parentheses, or its block.
fn starts_argument(k: &TokenKind) -> bool {
    use TokenKind::*;
    matches!(
        k,
        Number(_)
            | Str(_)
            | StrBegin(_)
            | Symbol(_)
            | Name(_)
            | Const(_)
            | Ivar(_)
            | Label(_)
            | Nil
            | True
            | False
            | Arrow
            | Bang
            | Reserved(_)
    )
}

/// Whether an expression could be a call written without parentheses: `foo` or `x.foo`.
fn bare_call(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Name(_) | ExprKind::Field { .. })
}

fn can_take_block(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Name(_)
            | ExprKind::Field { .. }
            | ExprKind::Call { block: None, .. }
            | ExprKind::MethodCall { block: None, .. }
    )
}

fn is_negative_literal(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Number(n) => *n < 0.0,
        ExprKind::Unary {
            op: UnOp::Neg,
            operand,
        } => matches!(operand.kind, ExprKind::Number(_)),
        _ => false,
    }
}

fn push_text(parts: &mut Vec<StrPart>, text: String) {
    if !text.is_empty() {
        parts.push(StrPart::Text(text));
    }
}

struct Parser<'a> {
    src: &'a str,
    tokens: Vec<Token>,
    docs: Vec<DocComment>,
    pos: usize,
    /// The nesting level of the expression or body being parsed.
    depth: u32,
    /// The deepest level the tree built so far reaches, within the current
    /// [`start_measure`](Self::start_measure) scope.
    reached: u32,
    /// Set while parsing a `while`/`until` condition or a call's arguments without parentheses,
    /// where a `do` belongs to the loop or that call rather than to the last call parsed.
    no_do: bool,
    /// Set while parsing the middle of `cond ? a : b`, where `a :b` is more likely a missing
    /// space than a call.
    in_ternary: bool,
    diags: Diagnostics,
}

impl<'a> Parser<'a> {
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
            return Ok(self.bump());
        }
        if matches!(self.peek(), TokenKind::Assign | TokenKind::OpAssign(_)) {
            return self.err(
                "E0512",
                "an assignment can't be used as a value in Grindlang; assign on a line of its own",
                self.peek_span(),
            );
        }
        let msg = format!(
            "expected {}, found {}",
            k.describe(),
            self.peek().describe()
        );
        self.err("E0510", msg, self.peek_span())
    }

    fn err<T>(&mut self, code: &'static str, msg: impl Into<String>, span: Span) -> PResult<T> {
        self.diags.push(Diagnostic::error(code, msg, span));
        Err(Aborted)
    }

    /// Whether whitespace separates the current token from the one before it.
    fn spaced_before(&self) -> bool {
        self.pos > 0 && self.tokens[self.pos - 1].span.end < self.peek_span().start
    }

    /// Whether the token after the current one follows it with no whitespace between.
    fn tight_after(&self) -> bool {
        self.tokens
            .get(self.pos + 1)
            .is_some_and(|next| next.span.start == self.peek_span().end)
    }

    fn skip_newlines(&mut self) {
        while self.at(&TokenKind::Newline) {
            self.bump();
        }
    }

    fn skip_seps(&mut self) {
        while matches!(self.peek(), TokenKind::Newline | TokenKind::Semi) {
            self.bump();
        }
    }

    fn text(&self, span: Span) -> &'a str {
        span.slice(self.src)
    }

    // ---- nesting limit (see `crate::parser`) -----------------------------------

    fn enter(&mut self) -> PResult<()> {
        self.depth += 1;
        self.note_level(self.depth)
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn note_level(&mut self, level: u32) -> PResult<()> {
        if level > MAX_NESTING {
            self.diags.push(too_deeply_nested(self.peek_span()));
            return Err(Aborted);
        }
        self.reached = self.reached.max(level);
        Ok(())
    }

    fn start_measure(&mut self) -> u32 {
        std::mem::replace(&mut self.reached, self.depth)
    }

    fn end_measure(&mut self, outer: u32) -> u32 {
        let height = self.reached - self.depth;
        self.reached = self.reached.max(outer);
        height
    }

    // ---- top level -----------------------------------------------------------

    fn parse_program(&mut self) -> PResult<Program> {
        let start = self.peek_span();
        let mut items = Vec::new();
        loop {
            self.skip_seps();
            let item = match self.peek() {
                TokenKind::Eof => break,
                TokenKind::Def => {
                    let docs = self.docs_above();
                    Item::Def(self.parse_def(None, docs)?)
                }
                TokenKind::Private | TokenKind::Public => self.parse_visibility()?,
                TokenKind::End
                | TokenKind::Else
                | TokenKind::Elsif
                | TokenKind::When
                | TokenKind::RBrace => {
                    let msg = format!(
                        "unexpected {} at the top level; it does not close anything",
                        self.peek().describe()
                    );
                    return self.err("E0513", msg, self.peek_span());
                }
                _ => Item::Stmt(self.parse_stmt()?),
            };
            items.push(item);
            self.end_stmt()?;
        }
        Ok(Program {
            items,
            span: start.to(self.peek_span()),
        })
    }

    /// `private` or `public`: alone, it sets the visibility of the `def`s after it; before a
    /// `def`, of that one.
    fn parse_visibility(&mut self) -> PResult<Item> {
        let docs = self.docs_above();
        let tok = self.bump();
        let (visibility, word) = match tok.kind {
            TokenKind::Private => (Visibility::Private, "private"),
            _ => (Visibility::Public, "public"),
        };
        match self.peek() {
            TokenKind::Def => Ok(Item::Def(
                self.parse_def(Some((visibility, tok.span)), docs)?,
            )),
            TokenKind::Newline | TokenKind::Semi | TokenKind::Eof => {
                Ok(Item::Visibility(Spanned::new(visibility, tok.span)))
            }
            _ => {
                let msg = format!(
                    "write `{word} def name …`, or `{word}` on a line of its own to apply it to \
                     the `def`s below it"
                );
                self.err("E0515", msg, self.peek_span())
            }
        }
    }

    /// The comments directly above the current token, with no blank line between them or
    /// after the last, which may annotate the `def` it starts (`SPEC-RUBY.md` §2.2).
    fn docs_above(&self) -> Vec<DocComment> {
        let floor = match self.pos {
            0 => 0,
            p => self.tokens[p - 1].span.end,
        };
        let mut next = self.peek_span().start;
        let mut run = Vec::new();
        for doc in self.docs.iter().rev() {
            if doc.span.start >= next {
                continue;
            }
            if doc.span.start < floor {
                break;
            }
            let gap = &self.src[doc.span.end as usize..next as usize];
            if !gap.trim().is_empty() || gap.matches('\n').count() > 1 {
                break;
            }
            run.push(doc.clone());
            next = doc.span.start;
        }
        run.reverse();
        run
    }

    fn parse_def(
        &mut self,
        visibility: Option<(Visibility, Span)>,
        docs: Vec<DocComment>,
    ) -> PResult<Def> {
        let kw = self.bump();
        let name = self.parse_def_name()?;
        let params = match self.peek() {
            TokenKind::LParen => {
                self.bump();
                self.parse_params(TokenKind::RParen)?
            }
            TokenKind::Newline | TokenKind::Semi => Vec::new(),
            other => {
                let msg = format!(
                    "expected `(` or a line break after `def {}`, found {}",
                    name.node,
                    other.describe()
                );
                return self.err("E0510", msg, self.peek_span());
            }
        };
        let body = self.parse_body()?;
        let end = self.expect(TokenKind::End)?;
        let start = visibility.map_or(kw.span, |(_, span)| span);
        Ok(Def {
            name,
            params,
            body,
            visibility: visibility.map(|(v, _)| v),
            docs,
            span: start.to(end.span),
        })
    }

    fn parse_def_name(&mut self) -> PResult<Ident> {
        let tok = self.bump();
        match tok.kind {
            TokenKind::Name(n) => {
                if let Some(base) = n.strip_suffix(['?', '!']) {
                    let suffix = &n[base.len()..];
                    let suggestion = match suffix {
                        "?" => format!("is_{base}"),
                        _ => base.to_string(),
                    };
                    let msg = format!(
                        "method names ending in `{suffix}` are not supported yet; rename `{n}`, \
                         for example to `{suggestion}`"
                    );
                    return self.err("E0516", msg, tok.span);
                }
                self.check_call_name(&n, tok.span)?;
                Ok(Spanned::new(n, tok.span))
            }
            TokenKind::Const(c) => {
                let msg = format!(
                    "a method name starts with a lowercase letter; `{c}` would be a constant"
                );
                self.err("E0510", msg, tok.span)
            }
            TokenKind::Reserved(word) => self.reject_reserved(word, tok.span),
            other => {
                let msg = format!(
                    "expected a method name after `def`, found {}",
                    other.describe()
                );
                self.err("E0510", msg, tok.span)
            }
        }
    }

    /// Parameter names up to and including `close` (`)` of a `def` or lambda, `|` of a block),
    /// the opening already consumed.
    fn parse_params(&mut self, close: TokenKind) -> PResult<Vec<Ident>> {
        let mut params = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(&close) {
                break;
            }
            params.push(self.parse_param()?);
            self.skip_newlines();
            if close == TokenKind::Pipe && self.at(&TokenKind::Semi) {
                return self.err(
                    "E0521",
                    "block-local variables (`|x; y|`) are not supported in Grindlang",
                    self.peek_span(),
                );
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.skip_newlines();
        self.expect(close)?;
        Ok(params)
    }

    fn parse_param(&mut self) -> PResult<Ident> {
        let tok = self.bump();
        let (code, msg) = match tok.kind {
            TokenKind::Name(n) => {
                if self.at(&TokenKind::Assign) {
                    return self.err(
                        "E0523",
                        "default parameter values are not supported in Grindlang",
                        self.peek_span(),
                    );
                }
                return Ok(Spanned::new(n, tok.span));
            }
            TokenKind::Star | TokenKind::Pow => (
                "E0523",
                format!(
                    "`{}` parameters are not supported in Grindlang: a function takes a fixed \
                     number of arguments",
                    if tok.kind == TokenKind::Star {
                        "*args"
                    } else {
                        "**opts"
                    }
                ),
            ),
            TokenKind::Amp => (
                "E0521",
                "`&block` parameters are not supported in Grindlang; take a lambda as a plain \
                 parameter and call it with `f.call(x)`"
                    .to_string(),
            ),
            TokenKind::Label(n) => (
                "E0523",
                format!(
                    "keyword parameters like `{n}:` are not supported in Grindlang; take plain \
                     parameters"
                ),
            ),
            TokenKind::LParen => (
                "E0523",
                "destructuring parameters like `(a, b)` are not supported in Grindlang".to_string(),
            ),
            other => (
                "E0510",
                format!("expected a parameter name, found {}", other.describe()),
            ),
        };
        self.err(code, msg, tok.span)
    }

    // ---- statements ----------------------------------------------------------

    /// After a statement: a separator, or the end of the enclosing body.
    fn end_stmt(&mut self) -> PResult<()> {
        match self.peek() {
            TokenKind::Newline | TokenKind::Semi => {
                self.bump();
                Ok(())
            }
            TokenKind::Eof
            | TokenKind::End
            | TokenKind::Else
            | TokenKind::Elsif
            | TokenKind::When
            | TokenKind::RBrace => Ok(()),
            TokenKind::Reserved("rescue") => self.err(
                "E0522",
                "the `rescue` modifier is not supported in Grindlang: there are no exceptions",
                self.peek_span(),
            ),
            other => {
                let msg = format!(
                    "expected a line break or `;` after this statement, found {}",
                    other.describe()
                );
                self.err("E0510", msg, self.peek_span())
            }
        }
    }

    fn at_body_end(&self) -> bool {
        matches!(
            self.peek(),
            TokenKind::Eof
                | TokenKind::End
                | TokenKind::Else
                | TokenKind::Elsif
                | TokenKind::When
                | TokenKind::RBrace
        )
    }

    /// Statements up to the `end`, `else`, `elsif`, `when`, or `}` that closes them, which is
    /// left for the caller.
    fn parse_body(&mut self) -> PResult<Body> {
        self.enter()?;
        let saved = std::mem::replace(&mut self.no_do, false);
        let start = self.peek_span().start as usize;
        let mut stmts = Vec::new();
        loop {
            self.skip_seps();
            if self.at_body_end() {
                break;
            }
            stmts.push(self.parse_stmt()?);
            self.end_stmt()?;
        }
        let span = match (stmts.first(), stmts.last()) {
            (Some(first), Some(last)) => first.span.to(last.span),
            _ => Span::new(start, start),
        };
        self.no_do = saved;
        self.leave();
        Ok(Body { stmts, span })
    }

    /// A statement and any modifiers after it (`x += 1 if hit`).
    fn parse_stmt(&mut self) -> PResult<Stmt> {
        let outer = self.start_measure();
        let mut stmt = self.parse_bare_stmt()?;
        let mut height = self.end_measure(outer);
        loop {
            let modifier = match self.peek() {
                TokenKind::If => Modifier::If,
                TokenKind::Unless => Modifier::Unless,
                TokenKind::While => Modifier::While,
                TokenKind::Until => Modifier::Until,
                _ => return Ok(stmt),
            };
            self.bump();
            let outer = self.start_measure();
            let cond = self.parse_expr()?;
            height = height.max(self.end_measure(outer)) + 1;
            self.note_level(self.depth + height)?;
            let span = stmt.span.to(cond.span);
            stmt = Stmt {
                kind: StmtKind::Modified {
                    stmt: Box::new(stmt),
                    modifier,
                    cond,
                },
                span,
            };
        }
    }

    /// Only dispatches: every nested body recurses through here.
    fn parse_bare_stmt(&mut self) -> PResult<Stmt> {
        match self.peek() {
            TokenKind::While | TokenKind::Until => self.parse_while(),
            TokenKind::Loop => self.parse_loop(),
            TokenKind::Break | TokenKind::Next => self.parse_jump(),
            TokenKind::Return => self.parse_return(),
            TokenKind::Def | TokenKind::Private | TokenKind::Public => self.misplaced_top_item(),
            TokenKind::Name(_) if starts_argument(self.peek_at(1)) => self.parse_command(),
            _ => self.parse_expr_stmt(),
        }
    }

    fn misplaced_top_item<T>(&mut self) -> PResult<T> {
        let msg = match self.peek() {
            TokenKind::Def => "a `def` must be at the top level of a script; for a local \
                               function, assign a lambda: `name = ->(x) { … }`"
                .to_string(),
            other => format!(
                "`{}` is only allowed at the top level of a script",
                other.symbol()
            ),
        };
        self.err("E0515", msg, self.peek_span())
    }

    fn parse_while(&mut self) -> PResult<Stmt> {
        let kw = self.bump();
        let until = kw.kind == TokenKind::Until;
        let saved = std::mem::replace(&mut self.no_do, true);
        let cond = self.parse_expr()?;
        self.no_do = saved;
        if !self.eat(&TokenKind::Do) && !matches!(self.peek(), TokenKind::Newline | TokenKind::Semi)
        {
            let msg = format!(
                "expected `do` or a line break after the loop condition, found {}",
                self.peek().describe()
            );
            return self.err("E0510", msg, self.peek_span());
        }
        let body = self.parse_body()?;
        let end = self.expect(TokenKind::End)?;
        Ok(Stmt {
            kind: StmtKind::While { cond, body, until },
            span: kw.span.to(end.span),
        })
    }

    fn parse_loop(&mut self) -> PResult<Stmt> {
        let kw = self.bump();
        let block = match self.peek() {
            TokenKind::Do => self.parse_do_block()?,
            TokenKind::LBrace => self.parse_brace_block()?,
            other => {
                let msg = format!(
                    "expected `do` or `{{` after `loop`, found {}",
                    other.describe()
                );
                return self.err("E0510", msg, self.peek_span());
            }
        };
        if let Some(param) = block.params.first() {
            return self.err("E0510", "`loop` passes its block no parameters", param.span);
        }
        Ok(Stmt {
            kind: StmtKind::Loop(block.body),
            span: kw.span.to(block.span),
        })
    }

    /// Whether the statement ends here, possibly with a modifier.
    fn at_stmt_end(&self) -> bool {
        self.at_body_end()
            || matches!(
                self.peek(),
                TokenKind::Newline
                    | TokenKind::Semi
                    | TokenKind::If
                    | TokenKind::Unless
                    | TokenKind::While
                    | TokenKind::Until
                    | TokenKind::Reserved("rescue")
            )
    }

    /// `break` or `next`.
    fn parse_jump(&mut self) -> PResult<Stmt> {
        let tok = self.bump();
        let kind = match tok.kind {
            TokenKind::Break => StmtKind::Break,
            _ => StmtKind::Next,
        };
        if !self.at_stmt_end() {
            let msg = format!("`{}` takes no value in Grindlang", tok.kind.symbol());
            return self.err("E0517", msg, self.peek_span());
        }
        Ok(Stmt {
            kind,
            span: tok.span,
        })
    }

    fn parse_return(&mut self) -> PResult<Stmt> {
        let kw = self.bump();
        let mut values = Vec::new();
        if !self.at_stmt_end() {
            values = self.parse_arg_list()?;
        }
        let span = values.last().map_or(kw.span, |v| kw.span.to(v.span));
        Ok(Stmt {
            kind: StmtKind::Return(values),
            span,
        })
    }

    /// A call without parentheses at the start of a statement: `say "Hi"`, `on :hit do … end`.
    fn parse_command(&mut self) -> PResult<Stmt> {
        let tok = self.bump();
        let TokenKind::Name(n) = tok.kind else {
            unreachable!("a command starts with a name")
        };
        self.check_call_name(&n, tok.span)?;
        let outer = self.start_measure();
        let saved = std::mem::replace(&mut self.no_do, true);
        let mut args = vec![self.parse_call_arg()?];
        while self.eat(&TokenKind::Comma) {
            args.push(self.parse_call_arg()?);
        }
        self.no_do = saved;
        let block = match self.peek() {
            TokenKind::Do => Some(Box::new(self.parse_do_block()?)),
            TokenKind::LBrace => Some(Box::new(self.parse_brace_block()?)),
            _ => None,
        };
        let height = self.end_measure(outer) + 1;
        self.note_level(self.depth + height)?;
        if let op @ (TokenKind::And | TokenKind::Or) = self.peek() {
            let msg = format!(
                "`{}` after a call without parentheses would apply to the whole call in Ruby; \
                 put the arguments in parentheses: `{n}(…)`",
                op.symbol()
            );
            return self.err("E0528", msg, self.peek_span());
        }
        let end = match (&block, args.last()) {
            (Some(b), _) => b.span,
            (None, Some(Arg::Pos(e) | Arg::Label { value: e, .. })) => e.span,
            (None, None) => tok.span,
        };
        let span = tok.span.to(end);
        let call = Expr {
            kind: ExprKind::Call {
                name: Spanned::new(n, tok.span),
                args,
                block,
            },
            span,
        };
        Ok(Stmt {
            kind: StmtKind::Expr(call),
            span,
        })
    }

    /// A statement starting with an expression: an assignment, or the expression itself.
    fn parse_expr_stmt(&mut self) -> PResult<Stmt> {
        let first = self.parse_expr()?;
        match self.peek().clone() {
            TokenKind::Assign | TokenKind::Comma => self.parse_assign(first),
            TokenKind::OpAssign(op) => {
                self.check_target(&first)?;
                self.bump();
                let value = self.parse_arg()?;
                self.check_after_assignment()?;
                let span = first.span.to(value.span);
                Ok(Stmt {
                    kind: StmtKind::OpAssign {
                        target: first,
                        op,
                        value,
                    },
                    span,
                })
            }
            TokenKind::BitAssign(op) => {
                let msg =
                    format!("`{op}` is not supported in Grindlang: there are no bitwise operators");
                self.err("E0526", msg, self.peek_span())
            }
            _ => {
                let span = first.span;
                Ok(Stmt {
                    kind: StmtKind::Expr(first),
                    span,
                })
            }
        }
    }

    fn parse_assign(&mut self, first: Expr) -> PResult<Stmt> {
        let mut targets = vec![first];
        while self.eat(&TokenKind::Comma) {
            targets.push(self.parse_arg()?);
        }
        for t in &targets {
            self.check_target(t)?;
        }
        self.expect(TokenKind::Assign)?;
        let values = self.parse_arg_list()?;
        self.check_after_assignment()?;
        let span = targets[0].span.to(values[values.len() - 1].span);
        Ok(Stmt {
            kind: StmtKind::Assign { targets, values },
            span,
        })
    }

    fn check_target(&mut self, t: &Expr) -> PResult<()> {
        match t.kind {
            ExprKind::Name(_)
            | ExprKind::Const(_)
            | ExprKind::Ivar(_)
            | ExprKind::Field { .. }
            | ExprKind::Index { .. } => Ok(()),
            _ => self.err(
                "E0512",
                "cannot assign to this expression; assign to a name, a constant, `@name`, \
                 `x.name`, or `x[i]`",
                t.span,
            ),
        }
    }

    fn check_after_assignment(&mut self) -> PResult<()> {
        match self.peek() {
            TokenKind::Assign | TokenKind::OpAssign(_) => self.err(
                "E0512",
                "chained assignment (`a = b = …`) is not supported in Grindlang; assign each on \
                 a line of its own",
                self.peek_span(),
            ),
            op @ (TokenKind::And | TokenKind::Or) => {
                let (word, symbol) = match op {
                    TokenKind::And => ("and", "&&"),
                    _ => ("or", "||"),
                };
                let msg = format!(
                    "`{word}` binds more loosely than `=` in Ruby, so the assignment would happen \
                     before the `{word}`; write `{symbol}`, or put the right side in parentheses"
                );
                self.err("E0528", msg, self.peek_span())
            }
            _ => Ok(()),
        }
    }

    // ---- expressions ---------------------------------------------------------

    /// A full expression, including `not`, `and`, and `or`: a condition, an expression
    /// statement, or what parentheses or an interpolation hold.
    ///
    /// It opens no nesting level of its own: its operands do, in
    /// [`parse_arg_bp`](Self::parse_arg_bp), so a pair of parentheses counts once.
    fn parse_expr(&mut self) -> PResult<Expr> {
        let outer = self.start_measure();
        let mut lhs = self.parse_not()?;
        let mut height = self.end_measure(outer);
        while let op @ (TokenKind::And | TokenKind::Or) = self.peek() {
            let op = match op {
                TokenKind::And => BinOp::And,
                _ => BinOp::Or,
            };
            self.bump();
            let outer = self.start_measure();
            let rhs = self.parse_not()?;
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

    /// `not e`, which binds more loosely than every operator but `and` and `or`.
    fn parse_not(&mut self) -> PResult<Expr> {
        if !self.at(&TokenKind::Not) {
            return self.parse_arg();
        }
        let kw = self.bump();
        self.enter()?;
        let operand = self.parse_not()?;
        self.leave();
        let span = kw.span.to(operand.span);
        Ok(Expr {
            kind: ExprKind::Unary {
                op: UnOp::Not,
                operand: Box::new(operand),
            },
            span,
        })
    }

    /// An `arg`, Ruby's expression without `not`, `and`, or `or`: an operand of `=`, a call
    /// argument, an element, an index.
    fn parse_arg(&mut self) -> PResult<Expr> {
        self.parse_arg_bp(0)
    }

    fn parse_arg_list(&mut self) -> PResult<Vec<Expr>> {
        let mut list = vec![self.parse_arg()?];
        while self.eat(&TokenKind::Comma) {
            list.push(self.parse_arg()?);
        }
        Ok(list)
    }

    /// Pratt parsing: opens the nesting level and parses the first operand, leaving the infix
    /// loop to [`parse_infix`](Self::parse_infix) so its frame isn't on the stack once per level.
    fn parse_arg_bp(&mut self, min_bp: u8) -> PResult<Expr> {
        self.enter()?;
        let outer = self.start_measure();
        let lhs = self.parse_operand()?;
        let height = self.end_measure(outer);
        let e = self.parse_infix(lhs, height, min_bp)?;
        self.leave();
        Ok(e)
    }

    /// Fold infix operators onto `lhs`, which reaches `height` levels below the current depth.
    fn parse_infix(&mut self, mut lhs: Expr, mut height: u32, min_bp: u8) -> PResult<Expr> {
        loop {
            self.check_spaced_operator(&lhs)?;
            if let Some((code, msg)) = rejected_infix(self.peek()) {
                return self.err(code, msg, self.peek_span());
            }
            let Some(op) = Infix::of(self.peek()) else {
                break;
            };
            let (l_bp, r_bp) = op.binding_power();
            if l_bp < min_bp {
                break;
            }
            self.bump();
            let outer = self.start_measure();
            let kind = match op {
                Infix::Ternary => self.parse_ternary_rest(lhs, r_bp)?,
                Infix::Range { exclusive } => {
                    let hi = self.parse_arg_bp(r_bp)?;
                    ExprKind::Range {
                        lo: Box::new(lhs),
                        hi: Box::new(hi),
                        exclusive,
                    }
                }
                Infix::Binary(op) => {
                    let rhs = self.parse_arg_bp(r_bp)?;
                    ExprKind::Binary {
                        op,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    }
                }
            };
            height = height.max(self.end_measure(outer)) + 1;
            self.note_level(self.depth + height)?;
            let span = match &kind {
                ExprKind::Range { lo, hi, .. } => lo.span.to(hi.span),
                ExprKind::Binary { lhs, rhs, .. } => lhs.span.to(rhs.span),
                ExprKind::Ternary { cond, els, .. } => cond.span.to(els.span),
                _ => unreachable!(),
            };
            lhs = Expr { kind, span };
        }
        self.check_juxtaposed(&lhs)?;
        Ok(lhs)
    }

    /// The branches of `cond ? a : b`, the `?` consumed.
    fn parse_ternary_rest(&mut self, cond: Expr, r_bp: u8) -> PResult<ExprKind> {
        let saved = std::mem::replace(&mut self.in_ternary, true);
        let then = self.parse_arg()?;
        self.in_ternary = saved;
        if matches!(self.peek(), TokenKind::Symbol(_)) {
            return self.err(
                "E0510",
                "expected `:` in `cond ? a : b`; put a space after the `:`",
                self.peek_span(),
            );
        }
        self.expect(TokenKind::Colon)?;
        let els = self.parse_arg_bp(r_bp)?;
        Ok(ExprKind::Ternary {
            cond: Box::new(cond),
            then: Box::new(then),
            els: Box::new(els),
        })
    }

    /// `foo -1`, `foo *xs`, `foo &blk`: after a name that could be a call, an operator with a
    /// space before it and none after is an argument in Ruby's reading, so it's rejected rather
    /// than read as an operator.
    fn check_spaced_operator(&mut self, lhs: &Expr) -> PResult<()> {
        if !bare_call(lhs) || !self.spaced_before() || !self.tight_after() {
            return Ok(());
        }
        let callee = self.text(lhs.span);
        let (code, msg) = match self.peek() {
            TokenKind::Minus => (
                "E0527",
                format!(
                    "`{callee} -…` is ambiguous: write `{callee}(-…)` to pass a negative number, \
                     or `{callee} - …` to subtract"
                ),
            ),
            TokenKind::Star | TokenKind::Pow => {
                let op = self.peek().symbol();
                (
                    "E0527",
                    format!(
                        "`{callee} {op}…` is ambiguous: splat arguments are not supported, so for \
                         the operator write `{callee} {op} …`"
                    ),
                )
            }
            TokenKind::Amp => (
                "E0521",
                "passing a block with `&` is not supported in Grindlang".to_string(),
            ),
            _ => return Ok(()),
        };
        self.err(code, msg, self.peek_span())
    }

    /// A name followed by what could be its argument, as in `x = foo 1`: a call without
    /// parentheses, which may only start a statement (`SPEC-RUBY.md` §3.4).
    fn check_juxtaposed(&mut self, e: &Expr) -> PResult<()> {
        if !bare_call(e)
            || !starts_argument(self.peek())
            || matches!(self.peek(), TokenKind::Reserved(_))
        {
            return Ok(());
        }
        let callee = self.text(e.span);
        let msg = if self.in_ternary && matches!(self.peek(), TokenKind::Symbol(_)) {
            let next = self.text(self.peek_span());
            format!(
                "`{callee} {next}` reads as a call without parentheses; in `cond ? a : b`, put a \
                 space after the `:`"
            )
        } else {
            format!(
                "a call without parentheses is only allowed at the start of a statement; write \
                 `{callee}(…)`"
            )
        };
        self.err("E0527", msg, self.peek_span())
    }

    /// A prefix operator's operand, or a primary expression.
    fn parse_operand(&mut self) -> PResult<Expr> {
        let (code, msg) = match self.peek() {
            TokenKind::Minus => return self.parse_negation(),
            TokenKind::Bang => {
                let bang = self.bump();
                let operand = self.parse_arg_bp(NOT_BP)?;
                let span = bang.span.to(operand.span);
                return Ok(Expr {
                    kind: ExprKind::Unary {
                        op: UnOp::Not,
                        operand: Box::new(operand),
                    },
                    span,
                });
            }
            TokenKind::Not => (
                "E0511",
                "`not` can't be used here; write `!`, or put the expression in parentheses",
            ),
            TokenKind::Star | TokenKind::Pow => (
                "E0523",
                "splat arguments (`*xs`, `**opts`) are not supported in Grindlang",
            ),
            TokenKind::Amp => (
                "E0521",
                "passing a block with `&` is not supported in Grindlang",
            ),
            TokenKind::Tilde => ("E0526", "bitwise operators are not supported in Grindlang"),
            _ => return self.parse_primary(),
        };
        self.err(code, msg, self.peek_span())
    }

    /// Unary minus. Written directly before a number it makes a negative literal, as in Ruby:
    /// `-2.abs` is `(-2).abs`, except that `-2 ** 2` is `-(2 ** 2)`.
    fn parse_negation(&mut self) -> PResult<Expr> {
        let minus = self.bump();
        if let TokenKind::Number(n) = *self.peek()
            && minus.span.end == self.peek_span().start
        {
            let num = self.bump();
            if self.at(&TokenKind::Pow) {
                let base = Expr {
                    kind: ExprKind::Number(n),
                    span: num.span,
                };
                let operand = self.parse_infix(base, 0, POW_BP)?;
                return Ok(self.negate(minus.span, operand));
            }
            let literal = Expr {
                kind: ExprKind::Number(-n),
                span: minus.span.to(num.span),
            };
            return self.parse_postfix(literal, 0);
        }
        let operand = self.parse_arg_bp(NEG_BP)?;
        Ok(self.negate(minus.span, operand))
    }

    fn negate(&self, minus: Span, operand: Expr) -> Expr {
        Expr {
            span: minus.to(operand.span),
            kind: ExprKind::Unary {
                op: UnOp::Neg,
                operand: Box::new(operand),
            },
        }
    }

    /// A primary expression and its suffixes: `.name`, `.name(args)`, `[i]`, `(args)`, blocks.
    fn parse_primary(&mut self) -> PResult<Expr> {
        let outer = self.start_measure();
        let atom = self.parse_atom()?;
        let height = self.end_measure(outer);
        self.parse_postfix(atom, height)
    }

    /// Only dispatches, so that nested parentheses recursing through here don't carry the frame
    /// of [`parse_simple_atom`](Self::parse_simple_atom) once per level.
    fn parse_atom(&mut self) -> PResult<Expr> {
        match self.peek() {
            TokenKind::LParen => self.parse_paren(),
            TokenKind::LBracket => self.parse_array(),
            TokenKind::LBrace => self.parse_hash(),
            TokenKind::Arrow => self.parse_lambda(),
            TokenKind::If | TokenKind::Unless => self.parse_if(),
            TokenKind::Case => self.parse_case(),
            TokenKind::StrBegin(_) => self.parse_interpolated(),
            _ => self.parse_simple_atom(),
        }
    }

    fn parse_simple_atom(&mut self) -> PResult<Expr> {
        let tok = self.bump();
        let kind = match tok.kind {
            TokenKind::Nil => ExprKind::Nil,
            TokenKind::True => ExprKind::Bool(true),
            TokenKind::False => ExprKind::Bool(false),
            TokenKind::Number(n) => ExprKind::Number(n),
            TokenKind::Str(s) => {
                let mut parts = Vec::new();
                push_text(&mut parts, s);
                ExprKind::Str(parts)
            }
            TokenKind::Symbol(s) => ExprKind::Symbol(s),
            TokenKind::Name(n) => {
                self.check_call_name(&n, tok.span)?;
                ExprKind::Name(n)
            }
            TokenKind::Const(c) => {
                self.check_const(&c, tok.span)?;
                ExprKind::Const(c)
            }
            TokenKind::Ivar(n) => ExprKind::Ivar(n),
            TokenKind::Reserved(word) => return self.reject_reserved(word, tok.span),
            other => {
                let msg = format!("expected an expression, found {}", other.describe());
                return self.err("E0511", msg, tok.span);
            }
        };
        Ok(Expr {
            kind,
            span: tok.span,
        })
    }

    /// Apply the suffixes following `e`, which reaches `height` levels below the current depth.
    /// Each suffix puts everything parsed so far one level deeper.
    fn parse_postfix(&mut self, mut e: Expr, mut height: u32) -> PResult<Expr> {
        while self.suffix_follows(&e) {
            let outer = self.start_measure();
            e = self.parse_suffix(e)?;
            height = height.max(self.end_measure(outer)) + 1;
            self.note_level(self.depth + height)?;
        }
        Ok(e)
    }

    fn suffix_follows(&self, e: &Expr) -> bool {
        match self.peek() {
            TokenKind::Dot | TokenKind::LBracket | TokenKind::AmpDot | TokenKind::ColonColon => {
                true
            }
            TokenKind::LParen => matches!(e.kind, ExprKind::Name(_)),
            TokenKind::LBrace => can_take_block(e),
            TokenKind::Do => !self.no_do && can_take_block(e),
            _ => false,
        }
    }

    fn parse_suffix(&mut self, e: Expr) -> PResult<Expr> {
        match self.peek() {
            TokenKind::Dot => self.parse_dot_suffix(e),
            TokenKind::LBracket => self.parse_index(e),
            TokenKind::LParen => self.parse_name_call(e),
            TokenKind::LBrace => {
                let block = self.parse_brace_block()?;
                Ok(self.attach_block(e, block))
            }
            TokenKind::Do => {
                let block = self.parse_do_block()?;
                Ok(self.attach_block(e, block))
            }
            TokenKind::AmpDot => self.err(
                "E0526",
                "safe navigation (`&.`) is not supported in Grindlang; test `x.nil?` first",
                self.peek_span(),
            ),
            _ => self.err(
                "E0526",
                "`::` is not supported in Grindlang; write `Math.sqrt(x)`, not `Math::sqrt(x)`",
                self.peek_span(),
            ),
        }
    }

    /// `.name`, `.name(args)`, or `.(args)`.
    fn parse_dot_suffix(&mut self, recv: Expr) -> PResult<Expr> {
        self.bump();
        if self.at(&TokenKind::LParen) {
            let (args, span) = self.parse_paren_args()?;
            return Ok(Expr {
                span: recv.span.to(span),
                kind: ExprKind::CallValue {
                    callee: Box::new(recv),
                    args,
                },
            });
        }
        let tok = self.bump();
        let TokenKind::Name(n) = tok.kind else {
            let msg = format!(
                "expected a method name after `.`, found {}",
                tok.kind.describe()
            );
            return self.err("E0510", msg, tok.span);
        };
        self.check_method_name(&n, tok.span)?;
        let name = Spanned::new(n, tok.span);
        if self.at(&TokenKind::LParen) {
            if self.spaced_before() {
                return self.space_before_paren(&name.node);
            }
            let (args, span) = self.parse_paren_args()?;
            return Ok(Expr {
                span: recv.span.to(span),
                kind: ExprKind::MethodCall {
                    recv: Box::new(recv),
                    name,
                    args,
                    block: None,
                },
            });
        }
        Ok(Expr {
            span: recv.span.to(name.span),
            kind: ExprKind::Field {
                recv: Box::new(recv),
                name,
            },
        })
    }

    fn space_before_paren<T>(&mut self, callee: &str) -> PResult<T> {
        let msg = format!(
            "put no space between `{callee}` and `(`; `{callee} (…)` means something else in Ruby"
        );
        self.err("E0527", msg, self.peek_span())
    }

    fn parse_index(&mut self, base: Expr) -> PResult<Expr> {
        if bare_call(&base) && self.spaced_before() {
            let callee = self.text(base.span);
            let msg = format!(
                "`{callee} [` is ambiguous: write `{callee}[…]` to index, or `{callee}([…])` to \
                 pass an array"
            );
            return self.err("E0527", msg, self.peek_span());
        }
        self.bump();
        let saved = std::mem::replace(&mut self.no_do, false);
        self.skip_newlines();
        let index = self.parse_arg()?;
        if is_negative_literal(&index) {
            let msg = format!(
                "negative indices are not supported in Grindlang; for the last element, write \
                 `{}.last`",
                self.text(base.span)
            );
            return self.err("E0525", msg, index.span);
        }
        self.skip_newlines();
        let close = self.expect(TokenKind::RBracket)?;
        self.no_do = saved;
        Ok(Expr {
            span: base.span.to(close.span),
            kind: ExprKind::Index {
                base: Box::new(base),
                index: Box::new(index),
            },
        })
    }

    /// `name(args)`.
    fn parse_name_call(&mut self, callee: Expr) -> PResult<Expr> {
        let ExprKind::Name(n) = callee.kind else {
            unreachable!("only a name takes parentheses directly")
        };
        if self.spaced_before() {
            return self.space_before_paren(&n);
        }
        let (args, span) = self.parse_paren_args()?;
        Ok(Expr {
            span: callee.span.to(span),
            kind: ExprKind::Call {
                name: Spanned::new(n, callee.span),
                args,
                block: None,
            },
        })
    }

    fn parse_paren_args(&mut self) -> PResult<(Vec<Arg>, Span)> {
        let open = self.bump();
        let saved = std::mem::replace(&mut self.no_do, false);
        let mut args = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RParen) {
                break;
            }
            args.push(self.parse_call_arg()?);
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let close = self.expect(TokenKind::RParen)?;
        self.no_do = saved;
        Ok((args, open.span.to(close.span)))
    }

    fn parse_call_arg(&mut self) -> PResult<Arg> {
        if let TokenKind::Label(n) = self.peek().clone() {
            let tok = self.bump();
            let value = self.parse_arg()?;
            return Ok(Arg::Label {
                name: Spanned::new(n, tok.span),
                value,
            });
        }
        Ok(Arg::Pos(self.parse_arg()?))
    }

    /// Give `e` the block after it: `foo { … }`, `xs.each do |x| … end`, `lambda { |x| … }`.
    fn attach_block(&self, e: Expr, block: Block) -> Expr {
        let span = e.span.to(block.span);
        let block = Box::new(block);
        let kind = match e.kind {
            ExprKind::Name(n) if n == "lambda" => ExprKind::Lambda(block),
            ExprKind::Name(n) => ExprKind::Call {
                name: Spanned::new(n, e.span),
                args: Vec::new(),
                block: Some(block),
            },
            ExprKind::Call { name, args, .. } => ExprKind::Call {
                name,
                args,
                block: Some(block),
            },
            ExprKind::Field { recv, name } => ExprKind::MethodCall {
                recv,
                name,
                args: Vec::new(),
                block: Some(block),
            },
            ExprKind::MethodCall {
                recv, name, args, ..
            } => ExprKind::MethodCall {
                recv,
                name,
                args,
                block: Some(block),
            },
            _ => unreachable!("checked by can_take_block"),
        };
        Expr { kind, span }
    }

    fn parse_brace_block(&mut self) -> PResult<Block> {
        let open = self.bump();
        let params = self.parse_block_params()?;
        if params.is_empty() && matches!(self.peek(), TokenKind::Label(_)) {
            return self.err(
                "E0510",
                "a `{` right after a call opens a block, not a hash; pass a hash in parentheses: \
                 `foo({ key: value })`",
                open.span,
            );
        }
        let body = self.parse_body()?;
        let close = self.expect(TokenKind::RBrace)?;
        Ok(Block {
            params,
            body,
            span: open.span.to(close.span),
        })
    }

    fn parse_do_block(&mut self) -> PResult<Block> {
        let open = self.bump();
        let params = self.parse_block_params()?;
        let body = self.parse_body()?;
        let end = self.expect(TokenKind::End)?;
        Ok(Block {
            params,
            body,
            span: open.span.to(end.span),
        })
    }

    /// `|a, b|` or `||` at the start of a block, if there.
    fn parse_block_params(&mut self) -> PResult<Vec<Ident>> {
        if self.eat(&TokenKind::OrOr) {
            return Ok(Vec::new());
        }
        if self.eat(&TokenKind::Pipe) {
            return self.parse_params(TokenKind::Pipe);
        }
        Ok(Vec::new())
    }

    /// `->(params) { … }` or `-> do … end`.
    fn parse_lambda(&mut self) -> PResult<Expr> {
        let arrow = self.bump();
        let params = if self.eat(&TokenKind::LParen) {
            self.parse_params(TokenKind::RParen)?
        } else {
            Vec::new()
        };
        let (body, end) = match self.peek() {
            TokenKind::LBrace => {
                self.bump();
                let body = self.parse_body()?;
                (body, self.expect(TokenKind::RBrace)?.span)
            }
            TokenKind::Do => {
                self.bump();
                let body = self.parse_body()?;
                (body, self.expect(TokenKind::End)?.span)
            }
            other => {
                let mut msg = format!(
                    "expected `{{` or `do` to start the lambda's body, found {}",
                    other.describe()
                );
                if matches!(other, TokenKind::Name(_)) {
                    msg.push_str("; put its parameters in parentheses: `->(x) { … }`");
                }
                return self.err("E0510", msg, self.peek_span());
            }
        };
        let span = arrow.span.to(end);
        Ok(Expr {
            kind: ExprKind::Lambda(Box::new(Block { params, body, span })),
            span,
        })
    }

    /// A condition and the `then` or line break after it.
    fn parse_cond(&mut self, what: &str) -> PResult<Expr> {
        let cond = self.parse_expr()?;
        if !self.eat(&TokenKind::Then)
            && !matches!(self.peek(), TokenKind::Newline | TokenKind::Semi)
        {
            let msg = format!(
                "expected `then` or a line break after {what}, found {}",
                self.peek().describe()
            );
            return self.err("E0510", msg, self.peek_span());
        }
        Ok(cond)
    }

    fn parse_if(&mut self) -> PResult<Expr> {
        let kw = self.bump();
        let unless = kw.kind == TokenKind::Unless;
        let cond = self.parse_cond("the condition")?;
        let body = self.parse_body()?;
        let mut arms = vec![(cond, body)];
        while self.at(&TokenKind::Elsif) {
            if unless {
                return self.err(
                    "E0510",
                    "`unless` can't have an `elsif`; use `if`",
                    self.peek_span(),
                );
            }
            self.bump();
            let cond = self.parse_cond("the condition")?;
            let body = self.parse_body()?;
            arms.push((cond, body));
        }
        let else_body = if self.eat(&TokenKind::Else) {
            Some(self.parse_body()?)
        } else {
            None
        };
        let end = self.expect(TokenKind::End)?;
        Ok(Expr {
            kind: ExprKind::If(Box::new(If {
                arms,
                else_body,
                unless,
            })),
            span: kw.span.to(end.span),
        })
    }

    fn parse_case(&mut self) -> PResult<Expr> {
        let kw = self.bump();
        let subject = self.parse_expr()?;
        self.skip_seps();
        let mut whens = Vec::new();
        while self.at(&TokenKind::When) {
            let w = self.bump();
            let values = self.parse_arg_list()?;
            let span = w.span.to(values[values.len() - 1].span);
            if !self.eat(&TokenKind::Then)
                && !matches!(self.peek(), TokenKind::Newline | TokenKind::Semi)
            {
                let msg = format!(
                    "expected `then` or a line break after the `when` values, found {}",
                    self.peek().describe()
                );
                return self.err("E0510", msg, self.peek_span());
            }
            let body = self.parse_body()?;
            whens.push(When { values, body, span });
        }
        if whens.is_empty() {
            if let TokenKind::Reserved(word) = *self.peek() {
                return self.reject_reserved(word, self.peek_span());
            }
            let msg = format!(
                "expected `when` after `case {}`, found {}",
                self.text(subject.span),
                self.peek().describe()
            );
            return self.err("E0510", msg, self.peek_span());
        }
        let else_body = if self.eat(&TokenKind::Else) {
            Some(self.parse_body()?)
        } else {
            None
        };
        let end = self.expect(TokenKind::End)?;
        Ok(Expr {
            kind: ExprKind::Case(Box::new(Case {
                subject,
                whens,
                else_body,
            })),
            span: kw.span.to(end.span),
        })
    }

    fn parse_paren(&mut self) -> PResult<Expr> {
        let open = self.bump();
        let saved = std::mem::replace(&mut self.no_do, false);
        self.skip_newlines();
        let inner = self.parse_expr()?;
        self.skip_newlines();
        let close = self.expect(TokenKind::RParen)?;
        self.no_do = saved;
        Ok(Expr {
            kind: ExprKind::Paren(Box::new(inner)),
            span: open.span.to(close.span),
        })
    }

    fn parse_array(&mut self) -> PResult<Expr> {
        let open = self.bump();
        let saved = std::mem::replace(&mut self.no_do, false);
        let mut items = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RBracket) {
                break;
            }
            items.push(self.parse_arg()?);
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let close = self.expect(TokenKind::RBracket)?;
        self.no_do = saved;
        Ok(Expr {
            kind: ExprKind::Array(items),
            span: open.span.to(close.span),
        })
    }

    fn parse_hash(&mut self) -> PResult<Expr> {
        let open = self.bump();
        let saved = std::mem::replace(&mut self.no_do, false);
        let mut pairs = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RBrace) {
                break;
            }
            pairs.push(self.parse_pair()?);
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let close = self.expect(TokenKind::RBrace)?;
        self.no_do = saved;
        Ok(Expr {
            kind: ExprKind::Hash(pairs),
            span: open.span.to(close.span),
        })
    }

    fn parse_pair(&mut self) -> PResult<Pair> {
        if let TokenKind::Label(n) = self.peek().clone() {
            let tok = self.bump();
            let value = self.parse_arg()?;
            return Ok(Pair {
                key: Key::Label(Spanned::new(n, tok.span)),
                value,
            });
        }
        let key = self.parse_arg()?;
        if self.at(&TokenKind::Colon) && matches!(key.kind, ExprKind::Str(_)) {
            return self.err(
                "E0525",
                "quoted symbol keys like `\"key\": value` are not supported in Grindlang; write \
                 `key: value`, or `\"key\" => value`",
                key.span.to(self.peek_span()),
            );
        }
        self.expect(TokenKind::FatArrow)?;
        let value = self.parse_arg()?;
        Ok(Pair {
            key: Key::Expr(key),
            value,
        })
    }

    /// `"text #{expr} text"`.
    fn parse_interpolated(&mut self) -> PResult<Expr> {
        let first = self.bump();
        let TokenKind::StrBegin(text) = first.kind else {
            unreachable!("an interpolated string starts with StrBegin")
        };
        let saved = std::mem::replace(&mut self.no_do, false);
        let mut parts = Vec::new();
        push_text(&mut parts, text);
        let end = loop {
            parts.push(StrPart::Interp(self.parse_expr()?));
            let tok = self.bump();
            match tok.kind {
                TokenKind::StrMid(text) => push_text(&mut parts, text),
                TokenKind::StrEnd(text) => {
                    push_text(&mut parts, text);
                    break tok.span;
                }
                other => {
                    let msg = format!(
                        "expected `}}` to end the interpolation, found {}",
                        other.describe()
                    );
                    return self.err("E0510", msg, tok.span);
                }
            }
        };
        self.no_do = saved;
        Ok(Expr {
            kind: ExprKind::Str(parts),
            span: first.span.to(end),
        })
    }

    // ---- rejections by name --------------------------------------------------

    fn reject_reserved<T>(&mut self, word: &'static str, span: Span) -> PResult<T> {
        let (code, msg) = match word {
            "class" | "module" => (
                "E0520",
                format!(
                    "`{word}` is not supported in Grindlang: a script is one module of functions \
                     and constants"
                ),
            ),
            "self" => (
                "E0520",
                "`self` is not supported in Grindlang: there are no objects".to_string(),
            ),
            "super" => (
                "E0520",
                "`super` is not supported in Grindlang: there are no classes".to_string(),
            ),
            "yield" => (
                "E0521",
                "`yield` is not supported in Grindlang; take a lambda as a parameter and call it \
                 with `f.call(x)`"
                    .to_string(),
            ),
            "begin" => (
                "E0522",
                "`begin` is not supported in Grindlang: there are no exceptions, and a loop that \
                 tests at the end is written `loop do … break unless cond end`"
                    .to_string(),
            ),
            "rescue" | "ensure" | "retry" | "raise" => (
                "E0522",
                format!("`{word}` is not supported in Grindlang: there are no exceptions"),
            ),
            "for" => (
                "E0526",
                "`for … in` loops are not supported in Grindlang; write `xs.each do |x| … end` or \
                 `(a..b).each do |i| … end`"
                    .to_string(),
            ),
            "in" => (
                "E0526",
                "pattern matching (`case … in`) is not supported in Grindlang; use `case … when`"
                    .to_string(),
            ),
            "defined?" => (
                "E0526",
                "`defined?` is not supported in Grindlang: every name is checked when the script \
                 compiles"
                    .to_string(),
            ),
            _ => ("E0526", format!("`{word}` is not supported in Grindlang")),
        };
        self.err(code, msg, span)
    }

    /// Reject a call to a function Ruby uses for objects, reflection, or blocks.
    fn check_call_name(&mut self, name: &str, span: Span) -> PResult<()> {
        let (code, msg) = match name {
            "attr_reader" | "attr_writer" | "attr_accessor" => (
                "E0520",
                format!(
                    "`{name}` is not supported in Grindlang: there are no objects; keep fields in \
                     a hash, or in host memory (`@name`)"
                ),
            ),
            "define_method"
            | "method_missing"
            | "instance_variable_get"
            | "instance_variable_set"
            | "public_send"
            | "__send__" => (
                "E0520",
                format!(
                    "`{name}` is not supported in Grindlang: there are no objects or reflection"
                ),
            ),
            "eval" | "instance_eval" | "instance_exec" | "class_eval" => (
                "E0520",
                format!(
                    "`{name}` is not supported in Grindlang: code can't be evaluated at run time"
                ),
            ),
            "block_given?" => (
                "E0521",
                "`block_given?` is not supported in Grindlang: a `def` takes no block".to_string(),
            ),
            "proc" => (
                "E0521",
                "`proc` is not supported in Grindlang; write a lambda, `->(x) { … }`".to_string(),
            ),
            _ => return Ok(()),
        };
        self.err(code, msg, span)
    }

    /// Reject a method Ruby uses for objects or reflection, after a `.`.
    fn check_method_name(&mut self, name: &str, span: Span) -> PResult<()> {
        match name {
            "new" => self.err(
                "E0520",
                "`.new` is not supported in Grindlang: there are no classes; build a hash, \
                 `{ name: …, hp: … }`",
                span,
            ),
            "instance_eval" | "instance_exec" | "class_eval" => {
                let msg = format!(
                    "`.{name}` is not supported in Grindlang: code can't be evaluated at run time"
                );
                self.err("E0520", msg, span)
            }
            "send"
            | "public_send"
            | "__send__"
            | "instance_variable_get"
            | "instance_variable_set"
            | "define_method" => {
                let msg = format!(
                    "`.{name}` is not supported in Grindlang: there are no objects or reflection"
                );
                self.err("E0520", msg, span)
            }
            _ => Ok(()),
        }
    }

    fn check_const(&mut self, name: &str, span: Span) -> PResult<()> {
        match name {
            "Struct" => self.err(
                "E0520",
                "`Struct` is not supported in Grindlang: there are no classes; build a hash, \
                 `{ name: …, hp: … }`",
                span,
            ),
            "Proc" => self.err(
                "E0521",
                "`Proc` is not supported in Grindlang; write a lambda, `->(x) { … }`",
                span,
            ),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_src(src: &str) -> Result<Program, Diagnostics> {
        crate::ruby::parse(src)
    }

    fn parse_ok(src: &str) -> Program {
        parse_src(src).unwrap_or_else(|d| panic!("parse failed:\n{}", d.render(src)))
    }

    fn err_code(src: &str) -> &'static str {
        parse_src(src).unwrap_err().0[0].code
    }

    /// The first top-level statement.
    fn stmt(src: &str) -> StmtKind {
        match parse_ok(src).items.into_iter().next() {
            Some(Item::Stmt(s)) => s.kind,
            other => panic!("expected a statement, got {other:?}"),
        }
    }

    /// The first top-level statement, which is an expression.
    fn expr(src: &str) -> ExprKind {
        match stmt(src) {
            StmtKind::Expr(e) => e.kind,
            other => panic!("expected an expression statement, got {other:?}"),
        }
    }

    /// The value assigned by the first statement, `x = …`.
    fn value(src: &str) -> ExprKind {
        match stmt(src) {
            StmtKind::Assign { mut values, .. } => values.remove(0).kind,
            other => panic!("expected an assignment, got {other:?}"),
        }
    }

    fn binary(e: &ExprKind) -> (BinOp, &ExprKind, &ExprKind) {
        match e {
            ExprKind::Binary { op, lhs, rhs } => (*op, &lhs.kind, &rhs.kind),
            other => panic!("expected a binary operation, got {other:?}"),
        }
    }

    fn name(e: &ExprKind) -> &str {
        match e {
            ExprKind::Name(n) => n,
            other => panic!("expected a name, got {other:?}"),
        }
    }

    #[test]
    fn not_binds_tighter_than_and_or_which_share_a_level() {
        // `not a and b` is `(not a) and b`.
        let e = expr("not a and b");
        let (op, lhs, _) = binary(&e);
        assert_eq!(op, BinOp::And);
        assert!(matches!(lhs, ExprKind::Unary { op: UnOp::Not, .. }));
        // `a or b and c` is `(a or b) and c`, unlike `||` and `&&`.
        let e = expr("a or b and c");
        let (op, lhs, _) = binary(&e);
        assert_eq!(op, BinOp::And);
        assert_eq!(binary(lhs).0, BinOp::Or);
        let e = expr("a || b && c");
        let (op, _, rhs) = binary(&e);
        assert_eq!(op, BinOp::Or);
        assert_eq!(binary(rhs).0, BinOp::And);
    }

    #[test]
    fn arithmetic_precedence() {
        let e = value("x = 1 + 2 * 3");
        let (op, _, rhs) = binary(&e);
        assert_eq!((op, binary(rhs).0), (BinOp::Add, BinOp::Mul));
        // `**` is right-associative and binds tighter than unary minus.
        let e = value("x = 2 ** 3 ** 2");
        let (_, _, rhs) = binary(&e);
        assert_eq!(binary(rhs).0, BinOp::Pow);
        let ExprKind::Unary {
            op: UnOp::Neg,
            operand,
        } = value("x = -y ** 2")
        else {
            panic!()
        };
        assert_eq!(binary(&operand.kind).0, BinOp::Pow);
        // `!` binds tighter than `==`.
        let e = value("x = !a == b");
        let (op, lhs, _) = binary(&e);
        assert_eq!(op, BinOp::Eq);
        assert!(matches!(lhs, ExprKind::Unary { op: UnOp::Not, .. }));
    }

    #[test]
    fn a_minus_written_against_a_number_makes_a_negative_literal() {
        // `-2.abs` is `(-2).abs`…
        let ExprKind::Field { recv, name } = value("x = -2.abs") else {
            panic!()
        };
        assert_eq!(recv.kind, ExprKind::Number(-2.0));
        assert_eq!(name.node, "abs");
        // …but `-2 ** 2` is `-(2 ** 2)`.
        let ExprKind::Unary {
            op: UnOp::Neg,
            operand,
        } = value("x = -2 ** 2")
        else {
            panic!()
        };
        assert_eq!(binary(&operand.kind).0, BinOp::Pow);
        // `a - 1` and `a-1` subtract.
        assert_eq!(binary(&value("x = a-1")).0, BinOp::Sub);
    }

    #[test]
    fn the_ternary_is_right_associative() {
        let ExprKind::Ternary { els, .. } = value("x = a ? b : c ? d : e") else {
            panic!()
        };
        assert!(matches!(els.kind, ExprKind::Ternary { .. }));
        let ExprKind::Ternary { then, els, .. } = value("x = a ? :yes : :no") else {
            panic!()
        };
        assert_eq!(then.kind, ExprKind::Symbol("yes".into()));
        assert_eq!(els.kind, ExprKind::Symbol("no".into()));
    }

    #[test]
    fn a_statement_may_start_with_a_call_without_parentheses() {
        let ExprKind::Call { name, args, block } = expr("on :hit do |dmg|\n  @hp -= dmg\nend")
        else {
            panic!()
        };
        assert_eq!(name.node, "on");
        assert!(
            matches!(&args[..], [Arg::Pos(Expr { kind: ExprKind::Symbol(s), .. })] if s == "hit")
        );
        let block = block.expect("block");
        assert_eq!(block.params[0].node, "dmg");
        assert!(matches!(
            block.body.stmts[0].kind,
            StmtKind::OpAssign { .. }
        ));
    }

    #[test]
    fn a_call_without_parentheses_takes_named_arguments_and_a_do_block() {
        let ExprKind::Call { args, block, .. } =
            expr("choice \"Buy\", if: -> { @gold >= 5 } do\n  @gold -= 5\nend")
        else {
            panic!()
        };
        assert!(
            matches!(&args[1], Arg::Label { name, value } if name.node == "if"
            && matches!(value.kind, ExprKind::Lambda(_)))
        );
        assert!(block.is_some());
    }

    #[test]
    fn a_do_block_goes_to_the_call_before_it() {
        let ExprKind::MethodCall { name, block, .. } = expr("xs.each do |x|\n  say x\nend") else {
            panic!()
        };
        assert_eq!(name.node, "each");
        assert!(block.is_some());
        // In an assignment, too.
        assert!(matches!(
            value("ys = xs.map do |x| x * 2 end"),
            ExprKind::MethodCall { block: Some(_), .. }
        ));
    }

    #[test]
    fn a_while_conditions_do_is_the_loops() {
        let StmtKind::While { cond, until, .. } = stmt("while xs.any? do\n  xs.pop\nend") else {
            panic!()
        };
        assert!(!until);
        assert!(matches!(cond.kind, ExprKind::Field { .. }));
    }

    #[test]
    fn a_brace_after_a_call_opens_a_block_and_elsewhere_a_hash() {
        assert!(matches!(
            expr("xs.each { |x| say x }"),
            ExprKind::MethodCall { block: Some(_), .. }
        ));
        let ExprKind::Hash(pairs) = value("h = { hp: 10, \"fire\" => 2 }") else {
            panic!()
        };
        assert!(matches!(&pairs[0].key, Key::Label(k) if k.node == "hp"));
        assert!(matches!(&pairs[1].key, Key::Expr(_)));
        // A block's `||` means no parameters.
        assert!(matches!(value("f = lambda { || 1 }"), ExprKind::Lambda(_)));
    }

    #[test]
    fn parses_lambdas() {
        let ExprKind::Lambda(block) = value("f = ->(a, b) { a + b }") else {
            panic!()
        };
        assert_eq!(block.params.len(), 2);
        assert!(matches!(value("f = -> do 1 end"), ExprKind::Lambda(_)));
        let ExprKind::Lambda(block) = value("f = lambda do |x| x end") else {
            panic!()
        };
        assert_eq!(block.params[0].node, "x");
        let ExprKind::CallValue { args, .. } = expr("f.(1)") else {
            panic!()
        };
        assert_eq!(args.len(), 1);
    }

    #[test]
    fn if_unless_and_case_are_expressions() {
        let ExprKind::If(i) = value("x = if c then 1 elsif d then 2 else 3 end") else {
            panic!()
        };
        assert_eq!(i.arms.len(), 2);
        assert!(i.else_body.is_some() && !i.unless);
        let ExprKind::If(i) = expr("unless done\n  go\nend") else {
            panic!()
        };
        assert!(i.unless);
        let ExprKind::Case(c) =
            value("r = case hp\nwhen 0 then :dead\nwhen 1..9, 10 then :low\nelse :ok\nend")
        else {
            panic!()
        };
        assert_eq!(c.whens.len(), 2);
        assert!(matches!(
            c.whens[1].values[0].kind,
            ExprKind::Range {
                exclusive: false,
                ..
            }
        ));
        assert!(c.else_body.is_some());
    }

    #[test]
    fn parses_modifiers_loops_and_jumps() {
        let StmtKind::Modified {
            stmt: inner,
            modifier,
            ..
        } = stmt("x += 1 if hit")
        else {
            panic!()
        };
        assert_eq!(modifier, Modifier::If);
        assert!(matches!(
            inner.kind,
            StmtKind::OpAssign {
                op: AssignOp::Add,
                ..
            }
        ));
        assert!(
            matches!(stmt("loop do\n  break if done\n  next\nend"), StmtKind::Loop(b) if b.stmts.len() == 2)
        );
        assert!(matches!(
            stmt("until done do step end"),
            StmtKind::While { until: true, .. }
        ));
        let StmtKind::Modified { stmt: inner, .. } = stmt("return if x.nil?") else {
            panic!()
        };
        assert_eq!(inner.kind, StmtKind::Return(Vec::new()));
    }

    #[test]
    fn parses_assignments() {
        let StmtKind::Assign { targets, values } = stmt("a, b = b, a") else {
            panic!()
        };
        assert_eq!((targets.len(), values.len()), (2, 2));
        let StmtKind::Assign { targets, .. } = stmt("@hp, mem.gold, xs[0], MAX = 1, 2, 3, 4")
        else {
            panic!()
        };
        assert!(matches!(targets[1].kind, ExprKind::Field { .. }));
        assert!(matches!(targets[2].kind, ExprKind::Index { .. }));
        assert!(matches!(
            stmt("name ||= \"x\""),
            StmtKind::OpAssign {
                op: AssignOp::Or,
                ..
            }
        ));
    }

    #[test]
    fn parses_interpolation() {
        let ExprKind::Str(parts) = value("s = \"hp: #{@hp} of #{max}\"") else {
            panic!()
        };
        assert_eq!(parts.len(), 4);
        assert!(matches!(&parts[1], StrPart::Interp(e) if e.kind == ExprKind::Ivar("hp".into())));
        assert_eq!(value("s = \"\""), ExprKind::Str(Vec::new()));
    }

    #[test]
    fn a_leading_dot_continues_the_line() {
        let ExprKind::Field { recv, name } = value("n = xs\n  .select { |x| x > 0 }\n  .count")
        else {
            panic!()
        };
        assert_eq!(name.node, "count");
        assert!(matches!(recv.kind, ExprKind::MethodCall { .. }));
    }

    #[test]
    fn defs_carry_visibility_and_the_comments_above_them() {
        let program = parse_ok(
            "# not attached: a blank line follows\n\
             \n\
             # @param x number\n\
             # @return number\n\
             private def f(x)\n  x\nend\n\
             private\n\
             def g() end\n\
             def h\n  1\nend",
        );
        let Item::Def(f) = &program.items[0] else {
            panic!()
        };
        assert_eq!(f.visibility, Some(Visibility::Private));
        assert_eq!(f.docs.len(), 2);
        assert_eq!(f.docs[0].text, " @param x number");
        assert!(matches!(
            program.items[1],
            Item::Visibility(Spanned {
                node: Visibility::Private,
                ..
            })
        ));
        let Item::Def(g) = &program.items[2] else {
            panic!()
        };
        assert_eq!(g.visibility, None);
        assert!(g.docs.is_empty());
        let Item::Def(h) = &program.items[3] else {
            panic!()
        };
        assert!(h.params.is_empty());
        assert_eq!(h.body.stmts.len(), 1);
    }

    #[test]
    fn names_stay_names() {
        // Whether `x` is a local or a call is decided later.
        assert_eq!(name(&value("y = x")), "x");
        assert!(matches!(value("y = x()"), ExprKind::Call { .. }));
    }

    // ---- rejections ----

    #[test]
    fn rejects_calls_without_parentheses_outside_a_statements_start() {
        assert_eq!(err_code("x = foo 1"), "E0527");
        assert_eq!(err_code("x = a.b c"), "E0527");
        assert_eq!(err_code("mem.add_item :sword"), "E0527");
        assert_eq!(err_code("foo -1"), "E0527");
        assert_eq!(err_code("foo [1]"), "E0527");
        assert_eq!(err_code("foo (1) + 2"), "E0527");
        assert_eq!(err_code("x = c ? a :b"), "E0527");
    }

    #[test]
    fn rejects_loose_and_or_after_an_assignment_or_command() {
        assert_eq!(err_code("x = a or b"), "E0528");
        assert_eq!(err_code("say a and b"), "E0528");
        assert_eq!(err_code("x = not a"), "E0511");
        parse_ok("x = (a or b)");
        parse_ok("go if a and not b");
    }

    #[test]
    fn rejects_misplaced_items() {
        assert_eq!(err_code("def f\n  def g; end\nend"), "E0515");
        assert_eq!(err_code("if x\n  private\nend"), "E0515");
        assert_eq!(err_code("private :f"), "E0515");
        assert_eq!(err_code("end"), "E0513");
        assert_eq!(err_code("def alive?() end"), "E0516");
        assert_eq!(err_code("loop do\n  break 1\nend"), "E0517");
        assert_eq!(err_code("a = b = 1"), "E0512");
        assert_eq!(err_code("if (x = 1) then end"), "E0512");
        assert_eq!(err_code("f() = 1"), "E0512");
    }

    // ---- nesting limit (E0209) ----

    fn nested(open: &str, inner: &str, close: &str, n: usize) -> String {
        format!("{}{inner}{}", open.repeat(n), close.repeat(n))
    }

    #[test]
    fn rejects_deep_nesting() {
        assert_eq!(
            err_code(&format!("x = {}", nested("(", "1", ")", 300))),
            "E0209"
        );
        assert_eq!(
            err_code(&format!("x = {}", nested("[", "1", "]", 300))),
            "E0209"
        );
        assert_eq!(
            err_code(&nested("if a then\n", "x\n", "end\n", 300)),
            "E0209"
        );
        assert_eq!(err_code(&nested("f = -> { ", "1", " }", 300)), "E0209");
        assert_eq!(
            err_code(&format!("x = 1{}", " + 1".repeat(10_000))),
            "E0209"
        );
        assert_eq!(err_code(&format!("x = y{}", ".a".repeat(10_000))), "E0209");
        assert_eq!(
            err_code(&format!("x = 1{}", " if a".repeat(10_000))),
            "E0209"
        );
        assert_eq!(
            err_code(&format!("x = {}", nested("\"#{", "1", "}\"", 300))),
            "E0209"
        );
    }

    #[test]
    fn accepts_nesting_within_the_limit() {
        parse_ok(&format!("x = {}", nested("(", "1", ")", 190)));
        parse_ok(&format!("x = 1{}", " + 1".repeat(190)));
        // An `if` is an expression as well as a statement, so each one counts twice: once as an
        // expression, once for its branch.
        parse_ok(&nested("if a then\n", "x\n", "end\n", 95));
        assert_eq!(
            err_code(&nested("if a then\n", "x\n", "end\n", 101)),
            "E0209"
        );
        parse_ok(&format!("x = {}", nested("\"#{", "1", "}\"", 90)));
    }
}
