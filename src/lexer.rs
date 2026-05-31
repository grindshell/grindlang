//! Lexer: turns Grindlang source into a flat token stream with spans.
//!
//! The token grammar is Lua-compatible (see `SPEC.md` §2). The lexer is intentionally
//! permissive — it recognizes the full Lua token set including constructs the language
//! does not support (e.g. `repeat`, `...`). Rejection of unsupported constructs happens
//! in the parser, where a span-pointing diagnostic can be produced in context.
//!
//! Lexing collects multiple diagnostics where possible (e.g. several bad characters)
//! rather than bailing on the first.

use crate::diagnostics::{Diagnostic, Diagnostics, Span};

/// A lexical token: a [`TokenKind`] plus its source [`Span`].
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    // Literals
    Number(f64),
    Str(String),
    Name(String),

    // Keywords
    And,
    Break,
    Do,
    Else,
    Elseif,
    End,
    False,
    For,
    Function,
    If,
    In,
    Local,
    Nil,
    Not,
    Or,
    Repeat,
    Return,
    Then,
    True,
    Until,
    While,

    // Symbols / operators
    Plus,        // +
    Minus,       // -
    Star,        // *
    Slash,       // /
    DoubleSlash, // //
    Percent,     // %
    Caret,       // ^
    Hash,        // #
    Eq,          // ==
    Ne,          // ~=
    Le,          // <=
    Ge,          // >=
    Lt,          // <
    Gt,          // >
    Assign,      // =
    LParen,      // (
    RParen,      // )
    LBrace,      // {
    RBrace,      // }
    LBracket,    // [
    RBracket,    // ]
    Semi,        // ;
    Colon,       // :
    DoubleColon, // :: (label syntax; rejected by parser)
    Comma,       // ,
    Dot,         // .
    DotDot,      // ..
    Ellipsis,    // ... (varargs; rejected by parser)

    Eof,
}

impl TokenKind {
    /// Human-readable name for diagnostics (`"'end'"`, `"'='"`, `"a number"`, …).
    pub fn describe(&self) -> String {
        use TokenKind::*;
        match self {
            Number(_) => "a number".to_string(),
            Str(_) => "a string".to_string(),
            Name(n) => format!("identifier `{n}`"),
            Eof => "end of input".to_string(),
            other => format!("`{}`", other.symbol()),
        }
    }

    /// The canonical spelling for keyword/symbol tokens (empty for value tokens).
    fn symbol(&self) -> &'static str {
        use TokenKind::*;
        match self {
            And => "and",
            Break => "break",
            Do => "do",
            Else => "else",
            Elseif => "elseif",
            End => "end",
            False => "false",
            For => "for",
            Function => "function",
            If => "if",
            In => "in",
            Local => "local",
            Nil => "nil",
            Not => "not",
            Or => "or",
            Repeat => "repeat",
            Return => "return",
            Then => "then",
            True => "true",
            Until => "until",
            While => "while",
            Plus => "+",
            Minus => "-",
            Star => "*",
            Slash => "/",
            DoubleSlash => "//",
            Percent => "%",
            Caret => "^",
            Hash => "#",
            Eq => "==",
            Ne => "~=",
            Le => "<=",
            Ge => ">=",
            Lt => "<",
            Gt => ">",
            Assign => "=",
            LParen => "(",
            RParen => ")",
            LBrace => "{",
            RBrace => "}",
            LBracket => "[",
            RBracket => "]",
            Semi => ";",
            Colon => ":",
            DoubleColon => "::",
            Comma => ",",
            Dot => ".",
            DotDot => "..",
            Ellipsis => "...",
            _ => "",
        }
    }
}

fn keyword(ident: &str) -> Option<TokenKind> {
    use TokenKind::*;
    Some(match ident {
        "and" => And,
        "break" => Break,
        "do" => Do,
        "else" => Else,
        "elseif" => Elseif,
        "end" => End,
        "false" => False,
        "for" => For,
        "function" => Function,
        "if" => If,
        "in" => In,
        "local" => Local,
        "nil" => Nil,
        "not" => Not,
        "or" => Or,
        "repeat" => Repeat,
        "return" => Return,
        "then" => Then,
        "true" => True,
        "until" => Until,
        "while" => While,
        _ => return None,
    })
}

/// Lex `src` into a token stream terminated by [`TokenKind::Eof`].
///
/// Returns `Err` with every diagnostic collected if any lexical errors occurred.
pub fn lex(src: &str) -> Result<Vec<Token>, Diagnostics> {
    let mut lexer = Lexer::new(src);
    lexer.run();
    if lexer.diags.has_errors() {
        Err(lexer.diags)
    } else {
        Ok(lexer.tokens)
    }
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    tokens: Vec<Token>,
    diags: Diagnostics,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Lexer {
            src: src.as_bytes(),
            pos: 0,
            tokens: Vec::new(),
            diags: Diagnostics::new(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<u8> {
        self.src.get(self.pos + n).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.tokens.push(Token {
            kind,
            span: Span::new(start, self.pos),
        });
    }

    fn run(&mut self) {
        while let Some(c) = self.peek() {
            let start = self.pos;
            match c {
                b' ' | b'\t' | b'\r' | b'\n' | 0x0c | 0x0b => {
                    self.pos += 1;
                }
                b'-' if self.peek_at(1) == Some(b'-') => {
                    self.lex_comment();
                }
                c if is_name_start(c) => self.lex_name(start),
                c if c.is_ascii_digit() => self.lex_number(start),
                b'.' if self.peek_at(1).map(|d| d.is_ascii_digit()).unwrap_or(false) => {
                    self.lex_number(start)
                }
                b'"' | b'\'' => self.lex_quoted_string(start, c),
                b'[' if matches!(self.peek_at(1), Some(b'[') | Some(b'=')) => {
                    if let Some(level) = self.long_bracket_level() {
                        self.lex_long_string(start, level);
                    } else {
                        // Just a '['.
                        self.pos += 1;
                        self.push(TokenKind::LBracket, start);
                    }
                }
                _ => self.lex_symbol(start),
            }
        }
        self.tokens.push(Token {
            kind: TokenKind::Eof,
            span: Span::new(self.pos, self.pos),
        });
    }

    fn lex_comment(&mut self) {
        // Consume the leading `--`.
        self.pos += 2;
        // Long comment? `--[[ ... ]]` or `--[==[ ... ]==]`. `long_bracket_level` returns
        // `None` (consuming nothing) when the cursor isn't at a long-bracket open, so a
        // plain `--` line comment falls through below.
        if let Some(level) = self.long_bracket_level() {
            let start = self.pos;
            if self.consume_long_bracket_body(level).is_none() {
                self.diags.push(Diagnostic::error(
                    "E0003",
                    "unterminated long comment",
                    Span::new(start, self.pos),
                ));
            }
            return;
        }
        // Line comment: to end of line.
        while let Some(c) = self.peek() {
            if c == b'\n' {
                break;
            }
            self.pos += 1;
        }
    }

    fn lex_name(&mut self, start: usize) {
        while let Some(c) = self.peek() {
            if is_name_continue(c) {
                self.pos += 1;
            } else {
                break;
            }
        }
        let ident = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or("");
        let kind = keyword(ident).unwrap_or_else(|| TokenKind::Name(ident.to_string()));
        self.push(kind, start);
    }

    fn lex_number(&mut self, start: usize) {
        let is_hex =
            self.peek() == Some(b'0') && matches!(self.peek_at(1), Some(b'x') | Some(b'X'));
        if is_hex {
            self.pos += 2; // consume 0x
            self.consume_while(|c| c.is_ascii_hexdigit() || c == b'.');
            if matches!(self.peek(), Some(b'p') | Some(b'P')) {
                self.pos += 1;
                if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                    self.pos += 1;
                }
                self.consume_while(|c| c.is_ascii_digit());
            }
        } else {
            self.consume_while(|c| c.is_ascii_digit() || c == b'.');
            if matches!(self.peek(), Some(b'e') | Some(b'E')) {
                self.pos += 1;
                if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                    self.pos += 1;
                }
                self.consume_while(|c| c.is_ascii_digit());
            }
        }
        let text = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or("");
        match parse_number(text) {
            Some(v) => self.push(TokenKind::Number(v), start),
            None => {
                self.diags.push(Diagnostic::error(
                    "E0004",
                    format!("invalid number literal `{text}`"),
                    Span::new(start, self.pos),
                ));
                // Emit a placeholder so the parser can continue structurally.
                self.push(TokenKind::Number(f64::NAN), start);
            }
        }
    }

    fn lex_quoted_string(&mut self, start: usize, quote: u8) {
        self.pos += 1; // opening quote
        let mut buf = Vec::new();
        loop {
            match self.peek() {
                None | Some(b'\n') => {
                    self.diags.push(Diagnostic::error(
                        "E0005",
                        "unterminated string literal",
                        Span::new(start, self.pos),
                    ));
                    break;
                }
                Some(c) if c == quote => {
                    self.pos += 1;
                    break;
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.lex_escape(start, &mut buf);
                }
                Some(c) => {
                    buf.push(c);
                    self.pos += 1;
                }
            }
        }
        let s = String::from_utf8_lossy(&buf).into_owned();
        self.push(TokenKind::Str(s), start);
    }

    fn lex_escape(&mut self, str_start: usize, buf: &mut Vec<u8>) {
        match self.bump() {
            Some(b'n') => buf.push(b'\n'),
            Some(b't') => buf.push(b'\t'),
            Some(b'r') => buf.push(b'\r'),
            Some(b'a') => buf.push(0x07),
            Some(b'b') => buf.push(0x08),
            Some(b'f') => buf.push(0x0c),
            Some(b'v') => buf.push(0x0b),
            Some(b'\\') => buf.push(b'\\'),
            Some(b'"') => buf.push(b'"'),
            Some(b'\'') => buf.push(b'\''),
            Some(b'\n') => buf.push(b'\n'),
            Some(b'0'..=b'9') => {
                // Up to three decimal digits, value already partly consumed.
                self.pos -= 1;
                let mut val: u32 = 0;
                let mut n = 0;
                while n < 3 {
                    match self.peek() {
                        Some(d) if d.is_ascii_digit() => {
                            val = val * 10 + (d - b'0') as u32;
                            self.pos += 1;
                            n += 1;
                        }
                        _ => break,
                    }
                }
                buf.push(val as u8);
            }
            Some(b'x') => {
                let mut val: u32 = 0;
                let mut n = 0;
                while n < 2 {
                    match self.peek() {
                        Some(d) if d.is_ascii_hexdigit() => {
                            val = val * 16 + hex_val(d);
                            self.pos += 1;
                            n += 1;
                        }
                        _ => break,
                    }
                }
                buf.push(val as u8);
            }
            Some(b'u') => {
                // \u{NNNN}
                if self.peek() == Some(b'{') {
                    self.pos += 1;
                    let mut val: u32 = 0;
                    while let Some(d) = self.peek() {
                        if d == b'}' {
                            self.pos += 1;
                            break;
                        } else if d.is_ascii_hexdigit() {
                            val = val * 16 + hex_val(d);
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                    if let Some(ch) = char::from_u32(val) {
                        let mut tmp = [0u8; 4];
                        buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                    }
                }
            }
            other => {
                let span = Span::new(self.pos.saturating_sub(1), self.pos);
                let shown = other.map(|c| c as char).unwrap_or('?');
                self.diags.push(Diagnostic::error(
                    "E0006",
                    format!("invalid escape sequence `\\{shown}`"),
                    span,
                ));
                let _ = str_start;
            }
        }
    }

    /// If the cursor is at the opening of a long bracket (`[`, then `=`*, then `[`),
    /// consume it and return its level (number of `=`). Otherwise consume nothing.
    fn long_bracket_level(&mut self) -> Option<usize> {
        let save = self.pos;
        if self.peek() != Some(b'[') {
            return None;
        }
        self.pos += 1;
        let mut level = 0;
        while self.peek() == Some(b'=') {
            level += 1;
            self.pos += 1;
        }
        if self.peek() == Some(b'[') {
            self.pos += 1;
            Some(level)
        } else {
            self.pos = save;
            None
        }
    }

    fn lex_long_string(&mut self, start: usize, level: usize) {
        let body_start = self.pos;
        match self.consume_long_bracket_body(level) {
            Some(body_end) => {
                let mut bytes = &self.src[body_start..body_end];
                // A leading newline immediately after the opening bracket is dropped.
                if bytes.first() == Some(&b'\n') {
                    bytes = &bytes[1..];
                } else if bytes.starts_with(b"\r\n") {
                    bytes = &bytes[2..];
                }
                let s = String::from_utf8_lossy(bytes).into_owned();
                self.push(TokenKind::Str(s), start);
            }
            None => {
                self.diags.push(Diagnostic::error(
                    "E0007",
                    "unterminated long string",
                    Span::new(start, self.pos),
                ));
                self.push(TokenKind::Str(String::new()), start);
            }
        }
    }

    /// Consume up to and including the closing `]=*]` of a long bracket at `level`.
    /// Returns the byte offset of the start of the closing bracket (i.e. body end),
    /// or `None` if EOF was reached first. Assumes the opening bracket is consumed.
    fn consume_long_bracket_body(&mut self, level: usize) -> Option<usize> {
        loop {
            match self.peek() {
                None => return None,
                Some(b']') => {
                    let body_end = self.pos;
                    let save = self.pos;
                    self.pos += 1;
                    let mut eqs = 0;
                    while self.peek() == Some(b'=') {
                        eqs += 1;
                        self.pos += 1;
                    }
                    if eqs == level && self.peek() == Some(b']') {
                        self.pos += 1;
                        return Some(body_end);
                    } else {
                        // Not a real close; backtrack one past the ']' and keep scanning.
                        self.pos = save + 1;
                    }
                }
                Some(_) => {
                    self.pos += 1;
                }
            }
        }
    }

    fn lex_symbol(&mut self, start: usize) {
        let c = self.peek().unwrap();
        let two = self.peek_at(1);
        use TokenKind::*;
        let (kind, len) = match (c, two) {
            (b'=', Some(b'=')) => (Eq, 2),
            (b'~', Some(b'=')) => (Ne, 2),
            (b'<', Some(b'=')) => (Le, 2),
            (b'>', Some(b'=')) => (Ge, 2),
            (b'/', Some(b'/')) => (DoubleSlash, 2),
            (b':', Some(b':')) => (DoubleColon, 2),
            (b'.', Some(b'.')) => {
                if self.peek_at(2) == Some(b'.') {
                    (Ellipsis, 3)
                } else {
                    (DotDot, 2)
                }
            }
            (b'+', _) => (Plus, 1),
            (b'-', _) => (Minus, 1),
            (b'*', _) => (Star, 1),
            (b'/', _) => (Slash, 1),
            (b'%', _) => (Percent, 1),
            (b'^', _) => (Caret, 1),
            (b'#', _) => (Hash, 1),
            (b'<', _) => (Lt, 1),
            (b'>', _) => (Gt, 1),
            (b'=', _) => (Assign, 1),
            (b'(', _) => (LParen, 1),
            (b')', _) => (RParen, 1),
            (b'{', _) => (LBrace, 1),
            (b'}', _) => (RBrace, 1),
            (b'[', _) => (LBracket, 1),
            (b']', _) => (RBracket, 1),
            (b';', _) => (Semi, 1),
            (b':', _) => (Colon, 1),
            (b',', _) => (Comma, 1),
            (b'.', _) => (Dot, 1),
            _ => {
                // Unknown byte: report and skip one.
                self.pos += 1;
                let shown = c as char;
                self.diags.push(Diagnostic::error(
                    "E0001",
                    format!("unexpected character `{shown}`"),
                    Span::new(start, self.pos),
                ));
                return;
            }
        };
        self.pos += len;
        self.push(kind, start);
    }

    fn consume_while(&mut self, pred: impl Fn(u8) -> bool) {
        while let Some(c) = self.peek() {
            if pred(c) {
                self.pos += 1;
            } else {
                break;
            }
        }
    }
}

fn is_name_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic()
}

fn is_name_continue(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

fn hex_val(c: u8) -> u32 {
    match c {
        b'0'..=b'9' => (c - b'0') as u32,
        b'a'..=b'f' => (c - b'a' + 10) as u32,
        b'A'..=b'F' => (c - b'A' + 10) as u32,
        _ => 0,
    }
}

/// Parse a Grindlang numeric literal to `f64`. Supports decimal ints/floats with
/// exponents and hexadecimal ints/floats (`0x1p4`). Returns `None` on malformed input.
pub fn parse_number(text: &str) -> Option<f64> {
    let lower = text;
    if let Some(hex) = lower
        .strip_prefix("0x")
        .or_else(|| lower.strip_prefix("0X"))
    {
        return parse_hex_number(hex);
    }
    // Reject a bare "." or trailing junk; Rust's parser accepts "3." and ".5".
    if text == "." {
        return None;
    }
    text.parse::<f64>().ok()
}

fn parse_hex_number(hex: &str) -> Option<f64> {
    if hex.is_empty() {
        return None;
    }
    // Split off binary exponent (p/P).
    let (mantissa, exp) = match hex.split_once(['p', 'P']) {
        Some((m, e)) => {
            let e: i32 = e.parse().ok()?;
            (m, e)
        }
        None => (hex, 0),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    let mut value = 0.0f64;
    for c in int_part.bytes() {
        if !c.is_ascii_hexdigit() {
            return None;
        }
        value = value * 16.0 + hex_val(c) as f64;
    }
    let mut scale = 1.0f64 / 16.0;
    for c in frac_part.bytes() {
        if !c.is_ascii_hexdigit() {
            return None;
        }
        value += hex_val(c) as f64 * scale;
        scale /= 16.0;
    }
    Some(scale_pow2(value, exp))
}

/// Multiply `value` by `2^exp`, exactly.
///
/// Doubling or halving an `f64` only shifts its exponent field, so each step is exact and
/// IEEE-mandated (no rounding until the result over/underflows). We deliberately avoid
/// `f64::powi`/`powf`, which are documented as *not* guaranteed to be correctly rounded:
/// `2f64.powi(4)` need not equal exactly `16.0`. That latent dependence is invisible under
/// native libm but surfaces under Miri, which intentionally perturbs imprecise math
/// intrinsics by up to 1 ULP — there `0x1p4` would otherwise lex to `16.000000000000007`.
///
/// The step count is capped at the `f64` exponent span (~2098): beyond it the result has
/// already saturated to ±0 or ±∞, so further steps cannot change it (and the cap keeps a
/// pathologically large exponent from spinning).
fn scale_pow2(mut value: f64, exp: i32) -> f64 {
    let factor = if exp >= 0 { 2.0 } else { 0.5 };
    for _ in 0..exp.unsigned_abs().min(2100) {
        value *= factor;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        lex(src)
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| *k != TokenKind::Eof)
            .collect()
    }

    #[test]
    fn lexes_keywords_and_names() {
        use TokenKind::*;
        assert_eq!(
            kinds("function foo if x then"),
            vec![Function, Name("foo".into()), If, Name("x".into()), Then]
        );
    }

    #[test]
    fn lexes_operators() {
        use TokenKind::*;
        assert_eq!(
            kinds("a // b .. c == d ~= e <= f"),
            vec![
                Name("a".into()),
                DoubleSlash,
                Name("b".into()),
                DotDot,
                Name("c".into()),
                Eq,
                Name("d".into()),
                Ne,
                Name("e".into()),
                Le,
                Name("f".into()),
            ]
        );
    }

    #[test]
    fn lexes_numbers() {
        assert_eq!(kinds("3"), vec![TokenKind::Number(3.0)]);
        assert_eq!(kinds("3.5"), vec![TokenKind::Number(3.5)]);
        assert_eq!(kinds(".5"), vec![TokenKind::Number(0.5)]);
        assert_eq!(kinds("1e3"), vec![TokenKind::Number(1000.0)]);
        assert_eq!(kinds("0xFF"), vec![TokenKind::Number(255.0)]);
    }

    #[test]
    fn lexes_hex_floats() {
        // Binary-exponent scaling is exact (no `powi` rounding), so these hold on every
        // platform and under Miri — `0x1p4` used to lex to 16.000000000000007 under Miri.
        assert_eq!(kinds("0x1p4"), vec![TokenKind::Number(16.0)]);
        assert_eq!(kinds("0x1.8p1"), vec![TokenKind::Number(3.0)]); // 1.5 * 2
        assert_eq!(kinds("0x1p-1"), vec![TokenKind::Number(0.5)]);
    }

    #[test]
    fn lexes_strings_and_escapes() {
        assert_eq!(kinds(r#""hi\n""#), vec![TokenKind::Str("hi\n".into())]);
        assert_eq!(kinds("'a\\tb'"), vec![TokenKind::Str("a\tb".into())]);
    }

    #[test]
    fn lexes_long_strings_and_comments() {
        assert_eq!(
            kinds("[[raw\ntext]]"),
            vec![TokenKind::Str("raw\ntext".into())]
        );
        assert_eq!(kinds("[==[a]]b]==]"), vec![TokenKind::Str("a]]b".into())]);
        // Comments produce no tokens.
        assert_eq!(
            kinds("-- line\n--[[ block ]] 1"),
            vec![TokenKind::Number(1.0)]
        );
    }

    #[test]
    fn reports_unterminated_string() {
        let err = lex("\"oops").unwrap_err();
        assert!(err.0.iter().any(|d| d.code == "E0005"));
    }

    #[test]
    fn reports_unknown_char() {
        let err = lex("a $ b").unwrap_err();
        assert!(err.0.iter().any(|d| d.code == "E0001"));
    }
}
