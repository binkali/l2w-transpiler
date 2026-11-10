//! Diagnostics: source locations, error/warning messages and their rendering.
//!
//! Every stage of the transpiler reports problems as [`Diagnostic`] values
//! carrying an optional [`Span`] (1-based line and column). Rendering shows
//! the offending source line with a caret marker, similar to `rustc`.

use std::fmt;

/// A location in the source file. `line` and `col` are 1-based, `col` and
/// `len` are counted in characters (not bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub line: u32,
    pub col: u32,
    pub len: u32,
}

impl Span {
    pub const fn new(line: u32, col: u32, len: u32) -> Self {
        Span { line, col, len }
    }

    /// Returns a span covering `self` up to and including `other`.
    /// Spans across multiple lines are truncated to the first line.
    pub fn to(self, other: Span) -> Span {
        if other.line == self.line && other.col + other.len >= self.col {
            Span::new(self.line, self.col, other.col + other.len - self.col)
        } else {
            self
        }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.col)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Option<Span>,
    pub help: Vec<String>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            severity: Severity::Error,
            message: message.into(),
            span: Some(span),
            help: Vec::new(),
        }
    }

    pub fn warning(message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            message: message.into(),
            span: Some(span),
            help: Vec::new(),
        }
    }

    /// An error not tied to a source location (e.g. I/O, missing tools).
    pub fn global_error(message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Error,
            message: message.into(),
            span: None,
            help: Vec::new(),
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help.push(help.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// Renders the diagnostic against its source file.
    pub fn render(&self, file: &SourceFile) -> String {
        let mut out = format!("{}: {}\n", self.severity, self.message);
        if let Some(span) = self.span {
            out.push_str(&format!("  --> {}:{}:{}\n", file.name, span.line, span.col));
            if let Some(text) = file.line(span.line) {
                let gutter = span.line.to_string().len();
                let pad = " ".repeat(gutter);
                let text = text.replace('\t', " ");
                out.push_str(&format!(" {pad} |\n"));
                out.push_str(&format!(" {} | {}\n", span.line, text));
                let caret_pad = " ".repeat(span.col.saturating_sub(1) as usize);
                let carets = "^".repeat(span.len.max(1) as usize);
                out.push_str(&format!(" {pad} | {caret_pad}{carets}\n"));
            }
        }
        for help in &self.help {
            out.push_str(&format!("  = help: {help}\n"));
        }
        out
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.span {
            Some(span) => write!(
                f,
                "{}:{}: {}: {}",
                span.line, span.col, self.severity, self.message
            ),
            None => write!(f, "{}: {}", self.severity, self.message),
        }
    }
}

/// A loaded source file, used to render diagnostics.
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub name: String,
    pub text: String,
}

impl SourceFile {
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        SourceFile {
            name: name.into(),
            text: text.into(),
        }
    }

    /// Returns the given 1-based line without its line terminator.
    pub fn line(&self, line: u32) -> Option<&str> {
        self.text
            .lines()
            .nth(line.checked_sub(1)? as usize)
            .map(|l| l.trim_end_matches('\r'))
    }
}

/// Returns `true` if any diagnostic in the list is an error.
pub fn has_errors(diags: &[Diagnostic]) -> bool {
    diags.iter().any(Diagnostic::is_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_caret_under_span() {
        let file = SourceFile::new("t.s", "mov rax, 1\n  foo rbx\n");
        let d = Diagnostic::error("unknown thing", Span::new(2, 3, 3)).with_help("try bar");
        let r = d.render(&file);
        assert!(r.contains("--> t.s:2:3"), "{r}");
        assert!(r.contains(" 2 |   foo rbx"), "{r}");
        assert!(r.contains("   |   ^^^"), "{r}");
        assert!(r.contains("= help: try bar"), "{r}");
    }

    #[test]
    fn span_join() {
        let a = Span::new(1, 5, 3);
        let b = Span::new(1, 12, 2);
        assert_eq!(a.to(b), Span::new(1, 5, 9));
        assert_eq!(a.to(Span::new(2, 1, 1)), a);
    }
}
