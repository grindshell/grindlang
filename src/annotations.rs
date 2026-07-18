//! EmmyLua `---@` annotation parsing (`SPEC.md` §2.2, §5.6).
//!
//! The lexer captures `---` doc comments as a side-channel ([`crate::lexer::DocComment`]); the
//! parser hands the run of doc comments preceding a `function` to [`parse_doc_block`], which
//! extracts the `@param` / `@return` directives into an [`FnAnnotations`]. The type expression
//! after a directive is parsed by [`parse_type_ann`] into a [`TypeAnn`] shape; the *checker*
//! (`crate::types`) later resolves that shape to a concrete [`crate::types::Type`] and unifies
//! it with the parameter/return inferred from the body.
//!
//! ## Accepted type syntax (v1 — resolves the `SPEC.md` §5.6 open item)
//!
//! ```text
//! type    := primary ( '?' | '[' ']' )*        -- postfix optional / array, left-to-right
//! primary := NAME                              -- number | bool | string | nil
//!          | '{' '[' 'string' ']' ':' type '}' -- map<string, T>
//!          | '{' NAME ':' type (',' NAME ':' type)* [','] '}'  -- record
//! ```
//!
//! `number[]?` is an optional array; `number?[]` is an array of optionals. Unknown directives
//! (`@type`, `@class`, `@field`, …) and non-`@` lines are ignored so they remain valid
//! documentation — only a malformed `@param`/`@return` we *do* consume is an error.

use crate::ast::{FnAnnotations, ParamAnn, Spanned, TypeAnn};
use crate::diagnostics::Diagnostic;
use crate::lexer::DocComment;

/// Parse the `@param` / `@return` directives out of a function's doc-comment run.
pub fn parse_doc_block(docs: &[DocComment]) -> (FnAnnotations, Vec<Diagnostic>) {
    let mut ann = FnAnnotations::default();
    let mut diags = Vec::new();

    for doc in docs {
        // Only `@`-directives are consumed; everything else is free-form documentation.
        let Some(rest) = doc.text.trim_start().strip_prefix('@') else {
            continue;
        };
        let mut it = rest.splitn(2, char::is_whitespace);
        let directive = it.next().unwrap_or("");
        let args = it.next().unwrap_or("").trim();

        match directive {
            "param" => {
                let mut ai = args.splitn(2, char::is_whitespace);
                let name = ai.next().unwrap_or("").trim();
                let type_str = ai.next().unwrap_or("").trim();
                if name.is_empty() || type_str.is_empty() {
                    diags.push(Diagnostic::error(
                        "E0110",
                        "malformed `---@param` (expected `@param <name> <type>`)",
                        doc.span,
                    ));
                    continue;
                }
                match parse_type_ann(type_str) {
                    Ok(ty) => ann.params.push(ParamAnn {
                        name: Spanned::new(name.to_string(), doc.span),
                        ty: Spanned::new(ty, doc.span),
                    }),
                    Err(msg) => diags.push(Diagnostic::error(
                        "E0110",
                        format!("malformed type in `---@param {name}`: {msg}"),
                        doc.span,
                    )),
                }
            }
            "return" => {
                if args.is_empty() {
                    diags.push(Diagnostic::error(
                        "E0110",
                        "malformed `---@return` (expected `@return <type>`)",
                        doc.span,
                    ));
                    continue;
                }
                match parse_type_ann(args) {
                    // v1 has a single return; a second `@return` line is ignored.
                    Ok(ty) if ann.ret.is_none() => ann.ret = Some(Spanned::new(ty, doc.span)),
                    Ok(_) => {}
                    Err(msg) => diags.push(Diagnostic::error(
                        "E0110",
                        format!("malformed type in `---@return`: {msg}"),
                        doc.span,
                    )),
                }
            }
            // `@type`, `@class`, `@field`, etc. are valid EmmyLua we don't consume in v1.
            _ => {}
        }
    }

    (ann, diags)
}

/// Parse a single EmmyLua type expression into a [`TypeAnn`]. Returns a human-readable message
/// on malformed input (the caller attaches a span).
pub fn parse_type_ann(s: &str) -> Result<TypeAnn, String> {
    let toks = tokenize(s)?;
    let mut p = TypeParser { toks: &toks, pos: 0 };
    let ty = p.parse_type()?;
    if p.pos != p.toks.len() {
        return Err(format!("unexpected trailing `{}`", desc(&p.toks[p.pos])));
    }
    Ok(ty)
}

// ---- type-expression tokenizer ----------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Tk {
    Ident(String),
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Colon,
    Comma,
    Question,
}

fn desc(t: &Tk) -> String {
    match t {
        Tk::Ident(n) => n.clone(),
        Tk::LBracket => "[".into(),
        Tk::RBracket => "]".into(),
        Tk::LBrace => "{".into(),
        Tk::RBrace => "}".into(),
        Tk::Colon => ":".into(),
        Tk::Comma => ",".into(),
        Tk::Question => "?".into(),
    }
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}
fn is_ident_continue(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn tokenize(s: &str) -> Result<Vec<Tk>, String> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'[' => {
                out.push(Tk::LBracket);
                i += 1;
            }
            b']' => {
                out.push(Tk::RBracket);
                i += 1;
            }
            b'{' => {
                out.push(Tk::LBrace);
                i += 1;
            }
            b'}' => {
                out.push(Tk::RBrace);
                i += 1;
            }
            b':' => {
                out.push(Tk::Colon);
                i += 1;
            }
            b',' => {
                out.push(Tk::Comma);
                i += 1;
            }
            b'?' => {
                out.push(Tk::Question);
                i += 1;
            }
            _ if is_ident_start(c) => {
                let start = i;
                while i < b.len() && is_ident_continue(b[i]) {
                    i += 1;
                }
                out.push(Tk::Ident(String::from_utf8_lossy(&b[start..i]).into_owned()));
            }
            _ => return Err(format!("unexpected character `{}`", c as char)),
        }
    }
    Ok(out)
}

struct TypeParser<'a> {
    toks: &'a [Tk],
    pos: usize,
}

impl TypeParser<'_> {
    fn peek(&self) -> Option<&Tk> {
        self.toks.get(self.pos)
    }
    fn bump(&mut self) -> Option<&Tk> {
        let t = self.toks.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn parse_type(&mut self) -> Result<TypeAnn, String> {
        let mut ty = self.parse_primary()?;
        loop {
            match self.peek() {
                Some(Tk::Question) => {
                    self.pos += 1;
                    ty = TypeAnn::Optional(Box::new(ty));
                }
                Some(Tk::LBracket) => {
                    self.pos += 1;
                    match self.bump() {
                        Some(Tk::RBracket) => ty = TypeAnn::Array(Box::new(ty)),
                        _ => return Err("expected `]` for an array type `T[]`".into()),
                    }
                }
                _ => break,
            }
        }
        Ok(ty)
    }

    fn parse_primary(&mut self) -> Result<TypeAnn, String> {
        match self.bump() {
            Some(Tk::Ident(n)) => Ok(TypeAnn::Named(n.clone())),
            Some(Tk::LBrace) => self.parse_brace(),
            Some(other) => Err(format!("expected a type, found `{}`", desc(other))),
            None => Err("expected a type".into()),
        }
    }

    /// After `{` — either a map `[string]: T` or a record `k: T, …`.
    fn parse_brace(&mut self) -> Result<TypeAnn, String> {
        if matches!(self.peek(), Some(Tk::LBracket)) {
            self.pos += 1; // [
            match self.bump() {
                Some(Tk::Ident(n)) if n == "string" => {}
                _ => return Err("map key type must be `string` (`{ [string]: T }`)".into()),
            }
            match self.bump() {
                Some(Tk::RBracket) => {}
                _ => return Err("expected `]` in map type".into()),
            }
            match self.bump() {
                Some(Tk::Colon) => {}
                _ => return Err("expected `:` in map type".into()),
            }
            let val = self.parse_type()?;
            match self.bump() {
                Some(Tk::RBrace) => {}
                _ => return Err("expected `}` to close map type".into()),
            }
            return Ok(TypeAnn::Map(Box::new(val)));
        }

        let mut fields = Vec::new();
        loop {
            let key = match self.bump() {
                Some(Tk::Ident(k)) => k.clone(),
                Some(Tk::RBrace) => break, // `{}` or trailing `}` after a comma
                _ => return Err("expected a field name in record type".into()),
            };
            match self.bump() {
                Some(Tk::Colon) => {}
                _ => return Err(format!("expected `:` after record field `{key}`")),
            }
            let val = self.parse_type()?;
            fields.push((key, val));
            match self.bump() {
                Some(Tk::Comma) => continue,
                Some(Tk::RBrace) => break,
                _ => return Err("expected `,` or `}` in record type".into()),
            }
        }
        if fields.is_empty() {
            return Err("empty record type `{}` is not allowed".into());
        }
        Ok(TypeAnn::Record(fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> TypeAnn {
        parse_type_ann(s).unwrap_or_else(|e| panic!("`{s}` should parse: {e}"))
    }

    #[test]
    fn scalars_and_postfix() {
        assert_eq!(t("number"), TypeAnn::Named("number".into()));
        assert_eq!(
            t("string?"),
            TypeAnn::Optional(Box::new(TypeAnn::Named("string".into())))
        );
        assert_eq!(
            t("number[]"),
            TypeAnn::Array(Box::new(TypeAnn::Named("number".into())))
        );
        // Postfix is left-to-right: `number[]?` is an optional array.
        assert_eq!(
            t("number[]?"),
            TypeAnn::Optional(Box::new(TypeAnn::Array(Box::new(TypeAnn::Named(
                "number".into()
            )))))
        );
    }

    #[test]
    fn map_and_record() {
        assert_eq!(
            t("{ [string]: number }"),
            TypeAnn::Map(Box::new(TypeAnn::Named("number".into())))
        );
        assert_eq!(
            t("{ name: string, age: number }"),
            TypeAnn::Record(vec![
                ("name".into(), TypeAnn::Named("string".into())),
                ("age".into(), TypeAnn::Named("number".into())),
            ])
        );
        // Trailing comma and a nested optional field.
        assert_eq!(
            t("{ hp: number, tag: string?, }"),
            TypeAnn::Record(vec![
                ("hp".into(), TypeAnn::Named("number".into())),
                (
                    "tag".into(),
                    TypeAnn::Optional(Box::new(TypeAnn::Named("string".into())))
                ),
            ])
        );
    }

    #[test]
    fn malformed_is_rejected() {
        assert!(parse_type_ann("number[").is_err());
        assert!(parse_type_ann("{ x number }").is_err());
        assert!(parse_type_ann("{}").is_err());
        assert!(parse_type_ann("{ [number]: string }").is_err());
        assert!(parse_type_ann("number number").is_err());
        assert!(parse_type_ann("").is_err());
    }

    #[test]
    fn doc_block_extracts_directives() {
        use crate::diagnostics::Span;
        let sp = Span::new(0, 0);
        let docs = vec![
            DocComment {
                span: sp,
                text: " @param base number".into(),
            },
            DocComment {
                span: sp,
                text: " a free-form description line".into(),
            },
            DocComment {
                span: sp,
                text: " @return number".into(),
            },
        ];
        let (ann, diags) = parse_doc_block(&docs);
        assert!(diags.is_empty());
        assert_eq!(ann.params.len(), 1);
        assert_eq!(ann.params[0].name.node, "base");
        assert_eq!(ann.params[0].ty.node, TypeAnn::Named("number".into()));
        assert_eq!(ann.ret.as_ref().unwrap().node, TypeAnn::Named("number".into()));
    }

    #[test]
    fn malformed_param_directive_reports() {
        use crate::diagnostics::Span;
        let docs = vec![DocComment {
            span: Span::new(0, 0),
            text: " @param x notatype!".into(),
        }];
        let (_, diags) = parse_doc_block(&docs);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "E0110");
    }
}
