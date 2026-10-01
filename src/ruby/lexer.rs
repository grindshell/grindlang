//! Lexer for the Ruby surface (`SPEC-RUBY.md` §2): source text to a flat token stream with spans.
//!
//! Unlike the Lua lexer, this one needs to know a little about where it is:
//!
//! * **Line breaks are tokens** ([`TokenKind::Newline`]), since a line break ends a statement.
//!   None is emitted where the line plainly continues: after an operator, a `,`, or an opening
//!   bracket, after a `\`, and before a line that starts with `.` (§2.1).
//! * **Operand or operator.** Whether the previous token ends an operand decides what `/`, `%`,
//!   `?`, `<<`, and `:` start: in an operand position they would begin a regex, a `%` literal, a
//!   character literal, a heredoc, or a symbol. Ruby also reads `foo /x/` (a space before the
//!   `/`, none after) as passing a regex; those forms are rejected, never guessed at.
//! * **Interpolation.** A double-quoted string with `#{…}` in it becomes [`TokenKind::StrBegin`],
//!   the tokens of each interpolated expression, [`TokenKind::StrMid`] between them, and
//!   [`TokenKind::StrEnd`].
//!
//! Ruby literals Grindlang does not support are rejected here, where they can be recognized
//! (`E0524`–`E0526`); keywords and operators it does not support are tokenized and rejected by
//! the parser in context. Lexing collects every diagnostic rather than stopping at the first.

use crate::diagnostics::{Diagnostic, Diagnostics, Span};
use crate::lexer::DocComment;

use super::ast::AssignOp;

/// A lexical token: a [`TokenKind`] plus its source [`Span`].
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    // Values
    Number(f64),
    /// A string with no interpolation.
    Str(String),
    /// An interpolated string's text up to its first `#{`.
    StrBegin(String),
    /// The text between one interpolation's `}` and the next `#{`.
    StrMid(String),
    /// The text after the last interpolation's `}`, up to the closing quote.
    StrEnd(String),
    /// `:name`, holding `name`.
    Symbol(String),
    /// A name starting with a lowercase letter or `_`, which may end in `?` or `!`.
    Name(String),
    /// A name starting with an uppercase letter.
    Const(String),
    /// `@name`, holding `name`.
    Ivar(String),
    /// `name:` with the colon right after the name, as in `{ hp: 10 }` or `say "Hi", if: c`.
    Label(String),

    // Keywords
    And,
    Break,
    Case,
    Def,
    Do,
    Else,
    Elsif,
    End,
    False,
    If,
    Loop,
    Next,
    Nil,
    Not,
    Or,
    Private,
    Public,
    Return,
    Then,
    True,
    Unless,
    Until,
    When,
    While,
    /// A Ruby keyword that Grindlang reserves and rejects (`class`, `yield`, `begin`, …).
    Reserved(&'static str),

    // Operators and punctuation
    Plus,
    Minus,
    Star,
    /// `**`
    Pow,
    Slash,
    Percent,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Bang,
    Assign,
    /// `+=`, `-=`, `*=`, `/=`, `%=`, `**=`, `||=`, `&&=`.
    OpAssign(AssignOp),
    DotDot,
    DotDotDot,
    Question,
    Colon,
    /// `->`
    Arrow,
    Dot,
    /// `=>`
    FatArrow,
    /// `<<`
    Shl,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Pipe,
    Comma,
    Semi,
    /// A line break that ends a statement.
    Newline,

    // Ruby operators Grindlang rejects, tokenized so the parser can say so where they appear.
    Amp,
    Caret,
    Tilde,
    /// `>>`
    Shr,
    /// `<=>`
    Spaceship,
    /// `===`
    EqEqEq,
    /// `=~`
    Match,
    /// `!~`
    NotMatch,
    /// `&.`
    AmpDot,
    /// `::`
    ColonColon,
    /// `<<=`, `>>=`, `&=`, `|=`, `^=`.
    BitAssign(&'static str),

    Eof,
}

impl TokenKind {
    /// Human-readable name for diagnostics (`` `end` ``, `a string`, `` `hp:` ``, …).
    pub fn describe(&self) -> String {
        use TokenKind::*;
        match self {
            Number(_) => "a number".to_string(),
            Str(_) | StrBegin(_) => "a string".to_string(),
            StrMid(_) | StrEnd(_) => "`}`".to_string(),
            Symbol(s) => format!("`:{s}`"),
            Name(n) => format!("`{n}`"),
            Const(n) => format!("constant `{n}`"),
            Ivar(n) => format!("`@{n}`"),
            Label(n) => format!("`{n}:`"),
            Reserved(w) | BitAssign(w) => format!("`{w}`"),
            OpAssign(op) => format!("`{}`", op.symbol()),
            Newline => "a line break".to_string(),
            Eof => "end of input".to_string(),
            other => format!("`{}`", other.symbol()),
        }
    }

    /// The spelling of a keyword or punctuation token (empty for the others).
    pub fn symbol(&self) -> &'static str {
        use TokenKind::*;
        match self {
            And => "and",
            Break => "break",
            Case => "case",
            Def => "def",
            Do => "do",
            Else => "else",
            Elsif => "elsif",
            End => "end",
            False => "false",
            If => "if",
            Loop => "loop",
            Next => "next",
            Nil => "nil",
            Not => "not",
            Or => "or",
            Private => "private",
            Public => "public",
            Return => "return",
            Then => "then",
            True => "true",
            Unless => "unless",
            Until => "until",
            When => "when",
            While => "while",
            Plus => "+",
            Minus => "-",
            Star => "*",
            Pow => "**",
            Slash => "/",
            Percent => "%",
            EqEq => "==",
            NotEq => "!=",
            Lt => "<",
            Le => "<=",
            Gt => ">",
            Ge => ">=",
            AndAnd => "&&",
            OrOr => "||",
            Bang => "!",
            Assign => "=",
            DotDot => "..",
            DotDotDot => "...",
            Question => "?",
            Colon => ":",
            Arrow => "->",
            Dot => ".",
            FatArrow => "=>",
            Shl => "<<",
            LParen => "(",
            RParen => ")",
            LBracket => "[",
            RBracket => "]",
            LBrace => "{",
            RBrace => "}",
            Pipe => "|",
            Comma => ",",
            Semi => ";",
            Amp => "&",
            Caret => "^",
            Tilde => "~",
            Shr => ">>",
            Spaceship => "<=>",
            EqEqEq => "===",
            Match => "=~",
            NotMatch => "!~",
            AmpDot => "&.",
            ColonColon => "::",
            _ => "",
        }
    }
}

fn keyword(ident: &str) -> Option<TokenKind> {
    use TokenKind::*;
    Some(match ident {
        "and" => And,
        "break" => Break,
        "case" => Case,
        "def" => Def,
        "do" => Do,
        "else" => Else,
        "elsif" => Elsif,
        "end" => End,
        "false" => False,
        "if" => If,
        "loop" => Loop,
        "next" => Next,
        "nil" => Nil,
        "not" => Not,
        "or" => Or,
        "private" => Private,
        "public" => Public,
        "return" => Return,
        "then" => Then,
        "true" => True,
        "unless" => Unless,
        "until" => Until,
        "when" => When,
        "while" => While,
        _ => return None,
    })
}

/// Ruby keywords (and `raise`) that Grindlang reserves, rejecting them wherever they appear
/// (`SPEC-RUBY.md` §2.4).
const RESERVED: &[&str] = &[
    "class",
    "module",
    "self",
    "super",
    "yield",
    "begin",
    "rescue",
    "ensure",
    "retry",
    "redo",
    "raise",
    "alias",
    "undef",
    "defined?",
    "for",
    "in",
    "BEGIN",
    "END",
    "__method__",
    "__FILE__",
    "__LINE__",
    "__ENCODING__",
    "__END__",
];

/// Whether `word` is a keyword of the Ruby surface or a word it reserves.
pub(super) fn is_reserved(word: &str) -> bool {
    keyword(word).is_some() || RESERVED.contains(&word)
}

/// Lex `src` into a token stream ending in [`TokenKind::Eof`], plus every `#` comment, which the
/// parser searches for the annotations above a `def`.
///
/// Returns every diagnostic collected if any lexical errors occurred.
pub fn lex(src: &str) -> Result<(Vec<Token>, Vec<DocComment>), Diagnostics> {
    let mut lexer = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        tokens: Vec::new(),
        docs: Vec::new(),
        interps: Vec::new(),
        diags: Diagnostics::new(),
    };
    lexer.run();
    if lexer.diags.has_errors() {
        Err(lexer.diags)
    } else {
        Ok((lexer.tokens, lexer.docs))
    }
}

/// A `#{…}` whose expression is being lexed.
struct Interp {
    /// Where its string starts, for an unterminated-string error.
    string_start: usize,
    /// How many `{` opened inside it are still open; its own `}` comes at zero.
    braces: u32,
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    tokens: Vec<Token>,
    docs: Vec<DocComment>,
    /// The interpolations being lexed, innermost last.
    interps: Vec<Interp>,
    diags: Diagnostics,
}

impl Lexer<'_> {
    fn peek_at(&self, n: usize) -> Option<u8> {
        self.bytes.get(self.pos + n).copied()
    }

    fn eat_while(&mut self, pred: impl Fn(u8) -> bool) {
        while self.peek_at(0).is_some_and(&pred) {
            self.pos += 1;
        }
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.tokens.push(Token {
            kind,
            span: Span::new(start, self.pos),
        });
    }

    fn error(&mut self, code: &'static str, msg: impl Into<String>, start: usize, end: usize) {
        self.diags
            .push(Diagnostic::error(code, msg, Span::new(start, end)));
    }

    /// Report an unsupported literal spanning `start..self.pos` and stand a string in for it, so
    /// that what follows is lexed as if an operand came before it.
    fn reject_literal(&mut self, code: &'static str, msg: impl Into<String>, start: usize) {
        self.error(code, msg, start, self.pos);
        self.push(TokenKind::Str(String::new()), start);
    }

    fn last(&self) -> Option<&TokenKind> {
        self.tokens.last().map(|t| &t.kind)
    }

    /// Whether an operand, rather than an operator, comes next: nothing before ends one.
    fn operand_expected(&self) -> bool {
        !self.last().is_some_and(ends_operand)
    }

    fn space_before(&self, at: usize) -> bool {
        at > 0 && matches!(self.bytes[at - 1], b' ' | b'\t' | b'\n' | b'\r')
    }

    /// Ruby's reading of `foo /x/`, `foo %w[a]`, and `foo <<~EOS`: after a name, an operator
    /// `len` bytes long with a space before it and none after starts a literal argument.
    fn literal_after_name(&self, start: usize, len: usize) -> bool {
        matches!(self.last(), Some(TokenKind::Name(_)))
            && self.space_before(start)
            && self
                .bytes
                .get(start + len)
                .is_some_and(|&b| !b.is_ascii_whitespace() && b != b'=')
    }

    fn at_line_start(&self, at: usize) -> bool {
        at == 0 || self.bytes[at - 1] == b'\n'
    }

    fn line_end(&self, from: usize) -> usize {
        self.src[from..]
            .find('\n')
            .map_or(self.src.len(), |i| from + i)
    }

    fn run(&mut self) {
        while let Some(c) = self.peek_at(0) {
            let start = self.pos;
            match c {
                b' ' | b'\t' | b'\r' | 0x0b | 0x0c => self.pos += 1,
                b'\n' => {
                    self.pos += 1;
                    self.newline(start);
                }
                b'\\' if self.src[start + 1..].starts_with('\n') => self.pos += 2,
                b'\\' if self.src[start + 1..].starts_with("\r\n") => self.pos += 3,
                b'#' => self.comment(start),
                b'=' if self.at_line_start(start) && self.src[start..].starts_with("=begin") => {
                    self.block_comment(start)
                }
                b'"' => {
                    self.pos += 1;
                    self.string_body(start, start, true);
                }
                b'\'' => self.single_quoted(start),
                b'@' => self.ivar(start),
                b'$' => self.global(start),
                b'`' => self.backtick(start),
                c if is_name_start(c) => self.name(start),
                c if c.is_ascii_digit() => self.number(start),
                _ => self.punct(start),
            }
        }
        if let Some(open) = self.interps.last() {
            let string_start = open.string_start;
            self.error(
                "E0501",
                "unterminated string literal: a `#{` in it is never closed",
                string_start,
                self.pos,
            );
        }
        self.tokens.push(Token {
            kind: TokenKind::Eof,
            span: Span::new(self.pos, self.pos),
        });
    }

    /// A line break at `at`: a [`TokenKind::Newline`] unless the statement plainly continues.
    fn newline(&mut self, at: usize) {
        if !self.interps.is_empty() {
            return;
        }
        match self.last() {
            None | Some(TokenKind::Newline | TokenKind::Semi) => return,
            Some(k) if continues_line(k) => return,
            _ => {}
        }
        if self.next_line_starts_with_dot() {
            return;
        }
        self.tokens.push(Token {
            kind: TokenKind::Newline,
            span: Span::new(at, at + 1),
        });
    }

    /// Whether the next line that isn't blank or a comment starts with `.` (and not `..`): a
    /// method chain continued from the line before.
    fn next_line_starts_with_dot(&self) -> bool {
        let mut i = self.pos;
        loop {
            match self.bytes.get(i) {
                Some(b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c) => i += 1,
                Some(b'#') => i = self.line_end(i),
                Some(b'.') => return self.bytes.get(i + 1) != Some(&b'.'),
                _ => return false,
            }
        }
    }

    fn comment(&mut self, start: usize) {
        let end = self.line_end(start);
        let text = self.src[start + 1..end].trim_end().to_string();
        self.docs.push(DocComment {
            span: Span::new(start, end),
            text,
        });
        self.pos = end;
    }

    /// `=begin` … `=end`: reported, then skipped through the `=end` line.
    fn block_comment(&mut self, start: usize) {
        self.error(
            "E0526",
            "`=begin`/`=end` comments are not supported in Grindlang; start each line with `#`",
            start,
            start + "=begin".len(),
        );
        let mut line = self.line_end(start);
        while line < self.src.len() {
            let next = line + 1;
            line = self.line_end(next);
            if self.src[next..].starts_with("=end") {
                break;
            }
        }
        self.pos = line;
    }

    fn name(&mut self, start: usize) {
        self.eat_while(is_name_continue);
        let lower = !self.bytes[start].is_ascii_uppercase();
        // A method name may end in `?` or `!` (`nil?`, `sort!`), but not in the `!` of `!=`.
        if lower && matches!(self.peek_at(0), Some(b'?' | b'!')) && self.peek_at(1) != Some(b'=') {
            self.pos += 1;
        }
        let text = &self.src[start..self.pos];
        // `name:` where a hash key or named argument can begin is a label, even for a keyword
        // (`if: cond`).
        if self.peek_at(0) == Some(b':')
            && self.peek_at(1) != Some(b':')
            && matches!(
                self.last(),
                Some(TokenKind::LBrace | TokenKind::LParen | TokenKind::Comma | TokenKind::Name(_))
            )
        {
            let label = TokenKind::Label(text.to_string());
            self.pos += 1;
            self.push(label, start);
            return;
        }
        if text == "__END__" && self.at_line_start(start) && self.line_end(start) == self.pos {
            self.error(
                "E0526",
                "`__END__` is not supported in Grindlang",
                start,
                self.pos,
            );
            self.pos = self.src.len();
            return;
        }
        let kind = if matches!(self.last(), Some(TokenKind::Dot)) {
            // After a `.`, even a keyword names a method: `x.class`, `3.then`.
            if lower {
                TokenKind::Name(text.to_string())
            } else {
                TokenKind::Const(text.to_string())
            }
        } else if let Some(k) = keyword(text) {
            k
        } else if let Some(&word) = RESERVED.iter().find(|&&w| w == text) {
            TokenKind::Reserved(word)
        } else if lower {
            TokenKind::Name(text.to_string())
        } else {
            TokenKind::Const(text.to_string())
        };
        self.push(kind, start);
    }

    fn number(&mut self, start: usize) {
        let second = self.peek_at(1);
        if self.bytes[start] == b'0' && matches!(second, Some(b'x' | b'X')) {
            self.pos += 2;
            self.eat_while(|c| c.is_ascii_hexdigit() || c == b'_');
            let digits = &self.src[start + 2..self.pos];
            let value = (!digits.is_empty()
                && underscores_between(digits, |c| c.is_ascii_hexdigit()))
            .then(|| {
                digits
                    .bytes()
                    .filter(|&c| c != b'_')
                    .fold(0.0, |acc, c| acc * 16.0 + hex_val(c) as f64)
            });
            return self.finish_number(start, value);
        }
        if self.bytes[start] == b'0'
            && matches!(
                second,
                Some(b'b' | b'B' | b'o' | b'O' | b'd' | b'D' | b'_' | b'0'..=b'9')
            )
        {
            self.eat_while(is_name_continue);
            let text = &self.src[start..self.pos];
            let base = match second {
                Some(b'b' | b'B') => "binary",
                Some(b'd' | b'D') => "`0d`",
                _ => "octal",
            };
            let msg = format!(
                "{base} literals like `{text}` are not supported in Grindlang; write the number \
                 in decimal, or in hex (`0x…`)"
            );
            return self.reject_number(msg, start);
        }
        self.eat_while(|c| c.is_ascii_digit() || c == b'_');
        if self.peek_at(0) == Some(b'.') && self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
            self.eat_while(|c| c.is_ascii_digit() || c == b'_');
        }
        if matches!(self.peek_at(0), Some(b'e' | b'E')) {
            let sign = usize::from(matches!(self.peek_at(1), Some(b'+' | b'-')));
            if self.peek_at(1 + sign).is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1 + sign;
                self.eat_while(|c| c.is_ascii_digit() || c == b'_');
            }
        }
        let text = &self.src[start..self.pos];
        let value = underscores_between(text, |c| c.is_ascii_digit())
            .then(|| text.replace('_', "").parse::<f64>().ok())
            .flatten();
        self.finish_number(start, value);
    }

    /// Push the number lexed from `start`, unless a letter follows it: `3r` and `2i` are Ruby's
    /// rationals and imaginaries, and anything else is malformed.
    fn finish_number(&mut self, start: usize, value: Option<f64>) {
        if self.peek_at(0).is_some_and(is_name_start) {
            let suffix = self.pos;
            self.eat_while(is_name_continue);
            let text = &self.src[start..self.pos];
            let (code, msg) = if matches!(&self.src[suffix..self.pos], "r" | "i" | "ri") {
                let msg = format!(
                    "rational and imaginary literals like `{text}` are not supported in \
                     Grindlang: every number is an `f64`"
                );
                ("E0525", msg)
            } else {
                ("E0503", format!("invalid number literal `{text}`"))
            };
            self.error(code, msg, start, self.pos);
            self.push(TokenKind::Number(0.0), start);
            return;
        }
        match value {
            Some(v) => self.push(TokenKind::Number(v), start),
            None => {
                let text = &self.src[start..self.pos];
                let msg = format!("invalid number literal `{text}`");
                self.error("E0503", msg, start, self.pos);
                self.push(TokenKind::Number(0.0), start);
            }
        }
    }

    fn reject_number(&mut self, msg: String, start: usize) {
        self.error("E0525", msg, start, self.pos);
        self.push(TokenKind::Number(0.0), start);
    }

    /// Lex a double-quoted string's text from the cursor to its closing quote or next `#{`.
    /// `first` is whether the text starts the string (at `token_start`, its opening quote)
    /// rather than resuming after an interpolation's `}` (at `token_start`).
    fn string_body(&mut self, token_start: usize, string_start: usize, first: bool) {
        let mut buf = Vec::new();
        loop {
            match self.peek_at(0) {
                None => {
                    self.error(
                        "E0501",
                        "unterminated string literal",
                        string_start,
                        self.pos,
                    );
                    break;
                }
                Some(b'"') => {
                    self.pos += 1;
                    break;
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.escape(&mut buf);
                }
                Some(b'#') if self.peek_at(1) == Some(b'{') => {
                    self.pos += 2;
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let kind = if first {
                        TokenKind::StrBegin(text)
                    } else {
                        TokenKind::StrMid(text)
                    };
                    self.push(kind, token_start);
                    self.interps.push(Interp {
                        string_start,
                        braces: 0,
                    });
                    return;
                }
                Some(b'#')
                    if matches!(self.peek_at(1), Some(b'@' | b'$'))
                        && self
                            .peek_at(2)
                            .is_some_and(|c| is_name_start(c) || c == b'@') =>
                {
                    // Ruby interpolates `"#@hp"` as if it were `"#{@hp}"`.
                    let at = self.pos;
                    self.pos += 2;
                    self.eat_while(|c| is_name_continue(c) || c == b'@');
                    let var = &self.src[at + 1..self.pos];
                    let msg = format!(
                        "`#{var}` in a string is Ruby's shorthand for `#{{{var}}}`, which is not \
                         supported in Grindlang; write `#{{{var}}}`"
                    );
                    self.error("E0525", msg, at, self.pos);
                }
                Some(c) => {
                    buf.push(c);
                    self.pos += 1;
                }
            }
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        let kind = if first {
            TokenKind::Str(text)
        } else {
            TokenKind::StrEnd(text)
        };
        self.push(kind, token_start);
    }

    /// A double-quoted string's escape, the `\` already consumed (`SPEC-RUBY.md` §2.6).
    fn escape(&mut self, buf: &mut Vec<u8>) {
        let at = self.pos - 1;
        let Some(c) = self.peek_at(0) else {
            return; // the unterminated string is reported by the caller
        };
        self.pos += 1;
        match c {
            b'n' => buf.push(b'\n'),
            b't' => buf.push(b'\t'),
            b'r' => buf.push(b'\r'),
            b's' => buf.push(b' '),
            b'e' => buf.push(0x1b),
            b'a' => buf.push(0x07),
            b'b' => buf.push(0x08),
            b'f' => buf.push(0x0c),
            b'v' => buf.push(0x0b),
            b'\\' | b'"' | b'\'' | b'#' => buf.push(c),
            // A backslash at the end of a line joins it to the next.
            b'\n' => {}
            b'0'..=b'7' => {
                // Up to three octal digits, as in Ruby: `\0`, `\033`.
                let mut val = u32::from(c - b'0');
                for _ in 0..2 {
                    match self.peek_at(0) {
                        Some(d @ b'0'..=b'7') => {
                            val = val * 8 + u32::from(d - b'0');
                            self.pos += 1;
                        }
                        _ => break,
                    }
                }
                buf.push(val as u8);
            }
            b'x' if self.peek_at(0).is_some_and(|d| d.is_ascii_hexdigit()) => {
                let mut val = 0;
                for _ in 0..2 {
                    match self.peek_at(0) {
                        Some(d) if d.is_ascii_hexdigit() => {
                            val = val * 16 + hex_val(d);
                            self.pos += 1;
                        }
                        _ => break,
                    }
                }
                buf.push(val as u8);
            }
            b'u' => match self.unicode_escape() {
                Some(ch) => buf.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes()),
                None => self.error(
                    "E0502",
                    "invalid unicode escape; write `\\u{1F600}` or `\\u00e9`",
                    at,
                    self.pos,
                ),
            },
            _ => {
                // Step over the whole character, which may take several bytes.
                let ch = self.src[self.pos - 1..].chars().next().unwrap_or('?');
                self.pos += ch.len_utf8() - 1;
                let msg = format!("invalid escape sequence `\\{ch}`; write `\\\\` for a backslash");
                self.error("E0502", msg, at, self.pos);
            }
        }
    }

    /// The rest of `\u{X…}` or `\uXXXX`, the `\u` already consumed.
    fn unicode_escape(&mut self) -> Option<char> {
        let braced = self.peek_at(0) == Some(b'{');
        let digits_start = self.pos + usize::from(braced);
        let digits_end = if braced {
            digits_start + self.src[digits_start..].find('}')?
        } else {
            digits_start + 4
        };
        let digits = self.src.get(digits_start..digits_end)?;
        let valid =
            (1..=6).contains(&digits.len()) && digits.bytes().all(|d| d.is_ascii_hexdigit());
        if !valid || (!braced && digits.len() != 4) {
            return None;
        }
        self.pos = digits_end + usize::from(braced);
        char::from_u32(u32::from_str_radix(digits, 16).ok()?)
    }

    /// `'…'`: raw text, with only `\\` and `\'` escaped.
    fn single_quoted(&mut self, start: usize) {
        self.pos += 1;
        let mut buf = Vec::new();
        loop {
            match self.peek_at(0) {
                None => {
                    self.error("E0501", "unterminated string literal", start, self.pos);
                    break;
                }
                Some(b'\'') => {
                    self.pos += 1;
                    break;
                }
                Some(b'\\') if matches!(self.peek_at(1), Some(b'\\' | b'\'')) => {
                    buf.push(self.bytes[self.pos + 1]);
                    self.pos += 2;
                }
                Some(c) => {
                    buf.push(c);
                    self.pos += 1;
                }
            }
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        self.push(TokenKind::Str(text), start);
    }

    fn ivar(&mut self, start: usize) {
        if self.peek_at(1) == Some(b'@') {
            self.pos += 2;
            self.eat_while(is_name_continue);
            let text = &self.src[start..self.pos];
            let msg = format!(
                "class variables like `{text}` are not supported in Grindlang; use a constant, or \
                 host memory (`@name`)"
            );
            return self.reject_literal("E0524", msg, start);
        }
        if !self.peek_at(1).is_some_and(is_name_start) {
            self.pos += 1;
            return self.error("E0500", "unexpected character `@`", start, self.pos);
        }
        self.pos += 1;
        self.eat_while(is_name_continue);
        let name = self.src[start + 1..self.pos].to_string();
        self.push(TokenKind::Ivar(name), start);
    }

    fn global(&mut self, start: usize) {
        self.pos += 1;
        if self.peek_at(0).is_some_and(is_name_continue) {
            self.eat_while(is_name_continue);
        } else if self.peek_at(0).is_some_and(|c| c.is_ascii_punctuation()) {
            self.pos += 1;
        }
        let text = &self.src[start..self.pos];
        let msg = format!(
            "global variables like `{text}` are not supported in Grindlang; use a constant, or \
             host memory (`@name`)"
        );
        self.reject_literal("E0524", msg, start);
    }

    fn backtick(&mut self, start: usize) {
        self.pos += 1;
        let end = self.line_end(self.pos);
        self.pos = self.src[self.pos..end]
            .find('`')
            .map_or(end, |i| self.pos + i + 1);
        self.reject_literal(
            "E0525",
            "backtick commands are not supported in Grindlang",
            start,
        );
    }

    fn punct(&mut self, start: usize) {
        use TokenKind::*;
        let (c, c1, c2) = (self.bytes[start], self.peek_at(1), self.peek_at(2));
        let (kind, len) = match (c, c1, c2) {
            (b'+', Some(b'='), _) => (OpAssign(AssignOp::Add), 2),
            (b'+', ..) => (Plus, 1),
            (b'-', Some(b'='), _) => (OpAssign(AssignOp::Sub), 2),
            (b'-', Some(b'>'), _) => (Arrow, 2),
            (b'-', ..) => (Minus, 1),
            (b'*', Some(b'*'), Some(b'=')) => (OpAssign(AssignOp::Pow), 3),
            (b'*', Some(b'*'), _) => (Pow, 2),
            (b'*', Some(b'='), _) => (OpAssign(AssignOp::Mul), 2),
            (b'*', ..) => (Star, 1),
            (b'/', ..) if self.operand_expected() || self.literal_after_name(start, 1) => {
                return self.regex(start);
            }
            (b'/', Some(b'='), _) => (OpAssign(AssignOp::Div), 2),
            (b'/', ..) => (Slash, 1),
            (b'%', ..)
                if (self.operand_expected() || self.literal_after_name(start, 1))
                    && self.percent_literal_follows() =>
            {
                return self.percent_literal(start);
            }
            (b'%', Some(b'='), _) => (OpAssign(AssignOp::Mod), 2),
            (b'%', ..) => (Percent, 1),
            (b'=', Some(b'='), Some(b'=')) => (EqEqEq, 3),
            (b'=', Some(b'='), _) => (EqEq, 2),
            (b'=', Some(b'~'), _) => (Match, 2),
            (b'=', Some(b'>'), _) => (FatArrow, 2),
            (b'=', ..) => (Assign, 1),
            (b'!', Some(b'='), _) => (NotEq, 2),
            (b'!', Some(b'~'), _) => (NotMatch, 2),
            (b'!', ..) => (Bang, 1),
            (b'<', Some(b'='), Some(b'>')) => (Spaceship, 3),
            (b'<', Some(b'<'), Some(b'=')) => (BitAssign("<<="), 3),
            (b'<', Some(b'<'), _) if self.heredoc_follows(start) => return self.heredoc(start),
            (b'<', Some(b'<'), _) => (Shl, 2),
            (b'<', Some(b'='), _) => (Le, 2),
            (b'<', ..) => (Lt, 1),
            (b'>', Some(b'>'), Some(b'=')) => (BitAssign(">>="), 3),
            (b'>', Some(b'>'), _) => (Shr, 2),
            (b'>', Some(b'='), _) => (Ge, 2),
            (b'>', ..) => (Gt, 1),
            (b'&', Some(b'&'), Some(b'=')) => (OpAssign(AssignOp::And), 3),
            (b'&', Some(b'&'), _) => (AndAnd, 2),
            (b'&', Some(b'.'), _) => (AmpDot, 2),
            (b'&', Some(b'='), _) => (BitAssign("&="), 2),
            (b'&', ..) => (Amp, 1),
            (b'|', Some(b'|'), Some(b'=')) => (OpAssign(AssignOp::Or), 3),
            (b'|', Some(b'|'), _) => (OrOr, 2),
            (b'|', Some(b'='), _) => (BitAssign("|="), 2),
            (b'|', ..) => (Pipe, 1),
            (b'^', Some(b'='), _) => (BitAssign("^="), 2),
            (b'^', ..) => (Caret, 1),
            (b'~', ..) => (Tilde, 1),
            (b'.', Some(b'.'), Some(b'.')) => (DotDotDot, 3),
            (b'.', Some(b'.'), _) => (DotDot, 2),
            (b'.', Some(d), _) if d.is_ascii_digit() && self.operand_expected() => {
                self.pos += 1;
                self.eat_while(|c| c.is_ascii_digit() || c == b'_');
                let text = &self.src[start..self.pos];
                let msg = format!("invalid number literal `{text}`; write `0{text}`");
                self.error("E0503", msg, start, self.pos);
                return self.push(Number(0.0), start);
            }
            (b'.', ..) => (Dot, 1),
            (b':', Some(b':'), _) => (ColonColon, 2),
            (b':', Some(n), _)
                if (is_name_start(n) || n == b'"' || n == b'\'')
                    && (self.operand_expected() || self.space_before(start)) =>
            {
                return self.symbol(start);
            }
            (b':', ..) => (Colon, 1),
            (b'?', Some(n), _) if self.operand_expected() && !n.is_ascii_whitespace() => {
                return self.char_literal(start);
            }
            (b'?', ..) => (Question, 1),
            (b'(', ..) => (LParen, 1),
            (b')', ..) => (RParen, 1),
            (b'[', ..) => (LBracket, 1),
            (b']', ..) => (RBracket, 1),
            (b'{', ..) => {
                if let Some(open) = self.interps.last_mut() {
                    open.braces += 1;
                }
                (LBrace, 1)
            }
            (b'}', ..) => {
                if let Some(open) = self.interps.last_mut() {
                    if open.braces == 0 {
                        // The end of a `#{…}`: the string resumes.
                        let string_start = open.string_start;
                        self.interps.pop();
                        self.pos += 1;
                        return self.string_body(start, string_start, false);
                    }
                    open.braces -= 1;
                }
                (RBrace, 1)
            }
            (b',', ..) => (Comma, 1),
            (b';', ..) => (Semi, 1),
            _ => {
                let ch = self.src[start..].chars().next().unwrap_or('?');
                self.pos += ch.len_utf8();
                let msg = format!("unexpected character `{ch}`");
                return self.error("E0500", msg, start, self.pos);
            }
        };
        self.pos += len;
        self.push(kind, start);
    }

    /// `:name`, or a quoted symbol `:"…"`, which is rejected.
    fn symbol(&mut self, start: usize) {
        self.pos += 1;
        if let Some(quote @ (b'"' | b'\'')) = self.peek_at(0) {
            let end = self.line_end(self.pos);
            self.pos = self.src[self.pos + 1..end]
                .find(quote as char)
                .map_or(end, |i| self.pos + 1 + i + 1);
            return self.reject_literal(
                "E0525",
                "quoted symbols like `:\"name\"` are not supported in Grindlang; write `:name`, or \
                 a string",
                start,
            );
        }
        self.eat_while(is_name_continue);
        if matches!(self.peek_at(0), Some(b'?' | b'!')) && self.peek_at(1) != Some(b'=') {
            self.pos += 1;
        }
        let name = self.src[start + 1..self.pos].to_string();
        self.push(TokenKind::Symbol(name), start);
    }

    /// `?a`, Ruby's character literal.
    fn char_literal(&mut self, start: usize) {
        self.pos += 1;
        if self.peek_at(0) == Some(b'\\') {
            self.pos += 1;
        }
        let ch = self.src[self.pos..]
            .chars()
            .next()
            .map_or(0, char::len_utf8);
        self.pos += ch;
        let text = &self.src[start + 1..self.pos];
        let msg = format!(
            "character literals like `?{text}` are not supported in Grindlang; write a string, \
             `'{text}'`"
        );
        self.reject_literal("E0525", msg, start);
    }

    /// `/…/`, a regular expression, or `foo /x` read as one.
    fn regex(&mut self, start: usize) {
        let after_name = !self.operand_expected();
        self.pos += 1;
        let end = self.line_end(self.pos);
        let mut i = self.pos;
        while i < end && self.bytes[i] != b'/' {
            i += if self.bytes[i] == b'\\' { 2 } else { 1 };
        }
        self.pos = (i + 1).min(end);
        self.eat_while(|c| c.is_ascii_alphabetic());
        let mut msg = String::from("regular expressions are not supported in Grindlang");
        if after_name {
            msg.push_str(" (to divide, put spaces on both sides of the `/`)");
        }
        self.reject_literal("E0525", msg, start);
    }

    /// Whether the `%` at the cursor starts a `%w[…]`-style literal.
    fn percent_literal_follows(&self) -> bool {
        let delimiter = |c: Option<u8>| c.is_some_and(|c| c.is_ascii_punctuation());
        match self.peek_at(1) {
            Some(b'w' | b'W' | b'i' | b'I' | b'q' | b'Q' | b'r' | b's' | b'x') => {
                delimiter(self.peek_at(2))
            }
            c => delimiter(c) && c != Some(b'='),
        }
    }

    fn percent_literal(&mut self, start: usize) {
        let after_name = !self.operand_expected();
        self.pos += 1;
        let kind = self.peek_at(0).filter(u8::is_ascii_alphabetic);
        if kind.is_some() {
            self.pos += 1;
        }
        let open = self.peek_at(0).unwrap_or(b'(');
        let close = match open {
            b'(' => b')',
            b'[' => b']',
            b'{' => b'}',
            b'<' => b'>',
            c => c,
        };
        self.pos += 1;
        let mut nesting = 0;
        while let Some(c) = self.peek_at(0) {
            self.pos += 1;
            if c == b'\\' {
                self.pos += 1;
            } else if c == close && nesting == 0 {
                break;
            } else if c == close {
                nesting -= 1;
            } else if c == open {
                nesting += 1;
            }
        }
        self.pos = self.pos.min(self.src.len());
        let what = match kind {
            Some(b'w' | b'W') => {
                "`%w[…]` arrays are not supported in Grindlang; write `[\"a\", \"b\"]`"
            }
            Some(b'i' | b'I') => "`%i[…]` arrays are not supported in Grindlang; write `[:a, :b]`",
            Some(b'r') => "regular expressions are not supported in Grindlang",
            Some(b's') => "`%s(…)` symbols are not supported in Grindlang; write `:name`",
            Some(b'x') => "`%x(…)` commands are not supported in Grindlang",
            Some(b'Q') => "`%Q(…)` strings are not supported in Grindlang; write a quoted string",
            _ => "`%q(…)` strings are not supported in Grindlang; write a quoted string",
        };
        let mut msg = String::from(what);
        if after_name {
            msg.push_str(" (for a remainder, put spaces on both sides of the `%`)");
        }
        self.reject_literal("E0525", msg, start);
    }

    /// Whether the `<<` at `start` begins a heredoc (`<<~EOS`, `<<-EOS`, `<<EOS`, `<<"EOS"`)
    /// rather than an append.
    fn heredoc_follows(&self, start: usize) -> bool {
        let mut i = start + 2;
        if matches!(self.bytes.get(i), Some(b'~' | b'-')) {
            i += 1;
        }
        let begins = self
            .bytes
            .get(i)
            .is_some_and(|&c| is_name_start(c) || c == b'"' || c == b'\'');
        begins && (self.operand_expected() || self.literal_after_name(start, 2))
    }

    /// A heredoc, reported and then skipped through its terminator line.
    fn heredoc(&mut self, start: usize) {
        let after_name = !self.operand_expected();
        self.pos += 2;
        if matches!(self.peek_at(0), Some(b'~' | b'-')) {
            self.pos += 1;
        }
        let quoted = matches!(self.peek_at(0), Some(b'"' | b'\''));
        if quoted {
            self.pos += 1;
        }
        let ident_start = self.pos;
        self.eat_while(is_name_continue);
        let ident = self.src[ident_start..self.pos].to_string();
        if quoted {
            self.pos += 1;
        }
        let mut msg = String::from(
            "heredocs are not supported in Grindlang; write a string, with `\\n` for line breaks",
        );
        if after_name {
            msg.push_str(" (to append, put spaces on both sides of the `<<`)");
        }
        self.error("E0525", msg, start, self.pos);
        let mut line = self.line_end(self.pos);
        while line < self.src.len() {
            let next = line + 1;
            line = self.line_end(next);
            if self.src[next..line].trim() == ident {
                break;
            }
        }
        self.pos = line;
        self.push(TokenKind::Str(String::new()), start);
    }
}

/// Whether a token ends an operand, so that an operator can follow it.
fn ends_operand(kind: &TokenKind) -> bool {
    use TokenKind::*;
    matches!(
        kind,
        Number(_)
            | Str(_)
            | StrEnd(_)
            | Symbol(_)
            | Name(_)
            | Const(_)
            | Ivar(_)
            | Reserved(_)
            | RParen
            | RBracket
            | RBrace
            | End
            | Nil
            | True
            | False
    )
}

/// Whether a line ending in this token continues onto the next (`SPEC-RUBY.md` §2.1).
fn continues_line(kind: &TokenKind) -> bool {
    use TokenKind::*;
    matches!(
        kind,
        Plus | Minus
            | Star
            | Pow
            | Slash
            | Percent
            | EqEq
            | NotEq
            | Lt
            | Le
            | Gt
            | Ge
            | AndAnd
            | OrOr
            | Bang
            | Assign
            | OpAssign(_)
            | DotDot
            | DotDotDot
            | Question
            | Colon
            | Arrow
            | Dot
            | FatArrow
            | Shl
            | LParen
            | LBracket
            | LBrace
            | Pipe
            | Comma
            | And
            | Or
            | Not
            | Label(_)
            | Amp
            | Caret
            | Tilde
            | Shr
            | Spaceship
            | EqEqEq
            | Match
            | NotMatch
            | AmpDot
            | ColonColon
            | BitAssign(_)
    )
}

fn is_name_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic()
}

fn is_name_continue(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

fn hex_val(c: u8) -> u32 {
    (c as char).to_digit(16).unwrap_or(0)
}

/// Whether every `_` in a number's digits sits between two digits, as Ruby requires (`1_000`).
fn underscores_between(text: &str, digit: impl Fn(u8) -> bool) -> bool {
    let b = text.as_bytes();
    (0..b.len())
        .all(|i| b[i] != b'_' || (i > 0 && i + 1 < b.len() && digit(b[i - 1]) && digit(b[i + 1])))
}

#[cfg(test)]
mod tests {
    use super::TokenKind::*;
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        lex(src)
            .unwrap_or_else(|d| panic!("lex failed: {d}"))
            .0
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| *k != Eof)
            .collect()
    }

    fn name(n: &str) -> TokenKind {
        Name(n.into())
    }

    fn err_code(src: &str) -> &'static str {
        lex(src).unwrap_err().0[0].code
    }

    #[test]
    fn lexes_names_constants_fields_and_keywords() {
        assert_eq!(
            kinds("def f(x) @hp end MAX"),
            vec![
                Def,
                name("f"),
                LParen,
                name("x"),
                RParen,
                Ivar("hp".into()),
                End,
                Const("MAX".into())
            ]
        );
    }

    #[test]
    fn a_method_name_may_end_in_a_question_or_exclamation_mark() {
        assert_eq!(
            kinds("x.nil? a!=b"),
            vec![name("x"), Dot, name("nil?"), name("a"), NotEq, name("b")]
        );
        assert_eq!(kinds("defined?"), vec![Reserved("defined?")]);
    }

    #[test]
    fn a_keyword_after_a_dot_is_a_method_name() {
        assert_eq!(kinds("x.end"), vec![name("x"), Dot, name("end")]);
    }

    #[test]
    fn lexes_labels_where_a_key_can_begin() {
        assert_eq!(
            kinds("{ hp: 1 }"),
            vec![LBrace, Label("hp".into()), Number(1.0), RBrace]
        );
        assert_eq!(
            kinds("choice \"x\", if: c"),
            vec![
                name("choice"),
                Str("x".into()),
                Comma,
                Label("if".into()),
                name("c")
            ]
        );
        // After `?`, it's the ternary's colon.
        assert_eq!(
            kinds("c ? a: b"),
            vec![name("c"), Question, name("a"), Colon, name("b")]
        );
    }

    #[test]
    fn lexes_symbols_and_ternary_colons() {
        assert_eq!(kinds("on :hit"), vec![name("on"), Symbol("hit".into())]);
        assert_eq!(
            kinds("c ? :a : :b"),
            vec![
                name("c"),
                Question,
                Symbol("a".into()),
                Colon,
                Symbol("b".into())
            ]
        );
        assert_eq!(
            kinds("x ? 1 :-1"),
            vec![name("x"), Question, Number(1.0), Colon, Minus, Number(1.0)]
        );
    }

    #[test]
    fn lexes_numbers() {
        assert_eq!(kinds("1_000"), vec![Number(1000.0)]);
        assert_eq!(kinds("2.5e-4"), vec![Number(2.5e-4)]);
        assert_eq!(kinds("0xFF"), vec![Number(255.0)]);
        // `3.times`: the `.` belongs to a method call.
        assert_eq!(kinds("3.times"), vec![Number(3.0), Dot, name("times")]);
        assert_eq!(kinds("1..5"), vec![Number(1.0), DotDot, Number(5.0)]);
        assert_eq!(err_code("1__0"), "E0503");
        assert_eq!(err_code("3x"), "E0503");
    }

    #[test]
    fn lexes_strings_and_escapes() {
        assert_eq!(kinds(r#""a\tb\e""#), vec![Str("a\tb\u{1b}".into())]);
        assert_eq!(kinds(r#""\101é\u{1F600}""#), vec![Str("Aé😀".into())]);
        // Single quotes keep backslashes, except before `\` and `'`.
        assert_eq!(kinds(r"'a\nb\'c'"), vec![Str("a\\nb'c".into())]);
        assert_eq!(err_code(r#""\q""#), "E0502");
        assert_eq!(err_code("\"open"), "E0501");
    }

    #[test]
    fn lexes_interpolation() {
        assert_eq!(
            kinds(r##""hp: #{hp}!""##),
            vec![StrBegin("hp: ".into()), name("hp"), StrEnd("!".into())]
        );
        // Braces and strings inside an interpolation nest.
        assert_eq!(
            kinds(r##""#{ {a: "#{b}"} }""##),
            vec![
                StrBegin(String::new()),
                LBrace,
                Label("a".into()),
                StrBegin(String::new()),
                name("b"),
                StrEnd(String::new()),
                RBrace,
                StrEnd(String::new()),
            ]
        );
        assert_eq!(
            kinds(r##""#{a} and #{b}""##),
            vec![
                StrBegin(String::new()),
                name("a"),
                StrMid(" and ".into()),
                name("b"),
                StrEnd(String::new()),
            ]
        );
        assert_eq!(err_code("\"#{a"), "E0501");
    }

    #[test]
    fn a_line_break_ends_a_statement_unless_the_line_continues() {
        assert_eq!(kinds("a\nb"), vec![name("a"), Newline, name("b")]);
        // Blank lines and comments give one line break.
        assert_eq!(kinds("a\n\n# c\n\nb"), vec![name("a"), Newline, name("b")]);
        // After an operator, a comma, or an opening bracket.
        assert_eq!(kinds("a +\nb"), vec![name("a"), Plus, name("b")]);
        assert_eq!(
            kinds("f(a,\nb)"),
            vec![name("f"), LParen, name("a"), Comma, name("b"), RParen]
        );
        // Before a line starting with `.`, and after a `\`.
        assert_eq!(
            kinds("xs\n  # note\n  .first"),
            vec![name("xs"), Dot, name("first")]
        );
        assert_eq!(kinds("a \\\n+ b"), vec![name("a"), Plus, name("b")]);
        // Not before a range.
        assert_eq!(kinds("a\n..b"), vec![name("a"), Newline, DotDot, name("b")]);
    }

    #[test]
    fn captures_comments_as_docs() {
        let (_, docs) = lex("# @param x number\ndef f(x) end").unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].text, " @param x number");
    }

    #[test]
    fn operand_position_decides_what_an_operator_starts() {
        assert_eq!(kinds("a / b"), vec![name("a"), Slash, name("b")]);
        assert_eq!(kinds("a/b"), vec![name("a"), Slash, name("b")]);
        assert_eq!(kinds("a % b"), vec![name("a"), Percent, name("b")]);
        assert_eq!(kinds("xs << x"), vec![name("xs"), Shl, name("x")]);
        assert_eq!(
            kinds("x /= 2"),
            vec![name("x"), OpAssign(AssignOp::Div), Number(2.0)]
        );
        assert_eq!(err_code("x = /ab/"), "E0525");
        assert_eq!(err_code("say /ab/"), "E0525");
        assert_eq!(err_code("x = %w[a b]"), "E0525");
        assert_eq!(err_code("x = <<~EOS\n  text\nEOS\n"), "E0525");
        assert_eq!(err_code("x = ?a"), "E0525");
    }

    #[test]
    fn rejects_unsupported_literals_and_variables() {
        for (src, code) in [
            ("$x", "E0524"),
            ("@@x", "E0524"),
            ("0b101", "E0525"),
            ("0o17", "E0525"),
            ("017", "E0525"),
            ("3r", "E0525"),
            ("2i", "E0525"),
            (":\"a b\"", "E0525"),
            ("`ls`", "E0525"),
            ("\"#@hp\"", "E0525"),
            ("=begin\nx\n=end", "E0526"),
            ("x\n__END__\ny", "E0526"),
            ("é", "E0500"),
        ] {
            assert_eq!(err_code(src), code, "{src}");
        }
    }

    #[test]
    fn collects_every_error() {
        let d = lex("$a\n$b").unwrap_err();
        assert_eq!(d.0.len(), 2);
    }
}
