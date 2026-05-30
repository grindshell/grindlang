//! Spans, diagnostics, and the crate's error type.
//!
//! Every phase of the pipeline (lexer, parser, resolver, checker, …) reports problems
//! as [`Diagnostic`]s carrying a [`Span`] into the original source. Fallible entry points
//! return [`Diagnostics`] (a batch of one or more diagnostics) as their error type.

use std::fmt;

/// A half-open byte range `[start, end)` into the original source string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span {
            start: start as u32,
            end: end as u32,
        }
    }

    /// A zero-width span at the start of input, for synthesized nodes.
    pub fn dummy() -> Self {
        Span { start: 0, end: 0 }
    }

    /// The smallest span covering both `self` and `other`.
    pub fn to(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    pub fn len(self) -> usize {
        (self.end - self.start) as usize
    }

    pub fn is_empty(self) -> bool {
        self.end == self.start
    }

    /// Slice the source text this span refers to.
    pub fn slice(self, src: &str) -> &str {
        &src[self.start as usize..self.end as usize]
    }
}

/// Severity of a [`Diagnostic`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Error => f.write_str("error"),
            Severity::Warning => f.write_str("warning"),
        }
    }
}

/// A single problem found in the source, pointing at a [`Span`].
///
/// `code` is a short stable identifier (e.g. `"E0001"`) for tests and tooling to match on
/// without depending on the human-readable `message`.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub span: Span,
}

impl Diagnostic {
    pub fn error(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            severity: Severity::Error,
            code,
            message: message.into(),
            span,
        }
    }

    pub fn warning(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            code,
            message: message.into(),
            span,
        }
    }

    /// Render this diagnostic against the original source, resolving the span to a
    /// 1-based line/column and showing the offending line with a caret underline.
    pub fn render(&self, src: &str) -> String {
        let (line, col, line_text) = locate(src, self.span.start);
        let caret_pad = " ".repeat(col.saturating_sub(1));
        let caret_len = self
            .span
            .len()
            .max(1)
            .min(line_text.len().saturating_sub(col - 1).max(1));
        let carets = "^".repeat(caret_len.max(1));
        format!(
            "{sev}[{code}]: {msg}\n  --> {line}:{col}\n   |\n{line:>3}| {line_text}\n   | {pad}{carets}",
            sev = self.severity,
            code = self.code,
            msg = self.message,
            line = line,
            col = col,
            line_text = line_text,
            pad = caret_pad,
            carets = carets,
        )
    }
}

/// Resolve a byte offset to a 1-based `(line, column)` and the text of that line.
fn locate(src: &str, offset: u32) -> (usize, usize, &str) {
    let offset = (offset as usize).min(src.len());
    let mut line_start = 0usize;
    let mut line = 1usize;
    for (i, b) in src.bytes().enumerate() {
        if i >= offset {
            break;
        }
        if b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    let line_end = src[line_start..]
        .find('\n')
        .map(|p| line_start + p)
        .unwrap_or(src.len());
    let col = offset - line_start + 1;
    (line, col, &src[line_start..line_end])
}

/// A batch of diagnostics; the error type returned by fallible pipeline stages.
///
/// An `Err(Diagnostics)` always contains at least one [`Severity::Error`].
#[derive(Clone, Debug)]
pub struct Diagnostics(pub Vec<Diagnostic>);

impl Diagnostics {
    pub fn new() -> Self {
        Diagnostics(Vec::new())
    }

    pub fn push(&mut self, d: Diagnostic) {
        self.0.push(d);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn has_errors(&self) -> bool {
        self.0.iter().any(|d| d.severity == Severity::Error)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Diagnostic> {
        self.0.iter()
    }

    /// Render every diagnostic against the source, one per stanza.
    pub fn render(&self, src: &str) -> String {
        self.0
            .iter()
            .map(|d| d.render(src))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

impl Default for Diagnostics {
    fn default() -> Self {
        Diagnostics::new()
    }
}

impl From<Diagnostic> for Diagnostics {
    fn from(d: Diagnostic) -> Self {
        Diagnostics(vec![d])
    }
}

impl fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, d) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(
                f,
                "{}[{}]: {} @ {}..{}",
                d.severity, d.code, d.message, d.span.start, d.span.end
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostics {}
