//! Source positions and human-facing diagnostics.
//!
//! Slowshell never aborts on a user mistake. Every layer returns a `Diag` that
//! carries enough context (file, line, column, and a concrete suggestion) for the
//! error overlay to render something a user can act on without reading a log file.

use std::fmt;
use std::sync::Arc;

use crate::color::Color;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
    /// Byte offset into the source file, used for the caret in the error overlay.
    pub offset: u32,
}

impl Pos {
    pub const fn new(line: u32, col: u32, offset: u32) -> Pos {
        Pos { line, col, offset }
    }
}

impl fmt::Display for Pos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.col)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Span {
    /// Display name of the file, usually relative to the config root.
    pub file: Arc<str>,
    pub start: Pos,
    pub end: Pos,
}

impl Span {
    pub fn new(file: impl Into<Arc<str>>, start: Pos, end: Pos) -> Span {
        Span { file: file.into(), start, end }
    }

    pub fn unknown() -> Span {
        Span { file: Arc::from("<unknown>"), start: Pos::default(), end: Pos::default() }
    }

    /// The span covering both `self` and `other`, used to report whole items.
    pub fn merge(&self, other: &Span) -> Span {
        Span { file: self.file.clone(), start: self.start, end: other.end }
    }

    /// `shell.config:42:7`
    pub fn location(&self) -> String {
        if self.file.as_ref() == "<unknown>" {
            return self.file.to_string();
        }
        format!("{}:{}:{}", self.file, self.start.line, self.start.col)
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.location())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Severity {
    /// A hint about how to fix the problem. Never on its own.
    Hint,
    /// Something suspicious that does not stop the component from working.
    Warning,
    /// The component cannot be built. Isolated by the error boundary.
    Error,
    /// A config file could not be parsed at all.
    Fatal,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Hint => "hint",
            Severity::Warning => "warning",
            Severity::Error => "error",
            Severity::Fatal => "fatal",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiagKind {
    Syntax,
    UnknownType,
    UnknownProperty,
    TypeMismatch,
    UnknownIdentifier,
    Runtime,
    Io,
    Platform,
    Plugin,
}

impl DiagKind {
    pub fn title(self) -> &'static str {
        match self {
            DiagKind::Syntax => "Syntax Error",
            DiagKind::UnknownType => "Unknown Element",
            DiagKind::UnknownProperty => "Unknown Property",
            DiagKind::TypeMismatch => "Type Mismatch",
            DiagKind::UnknownIdentifier => "Unknown Identifier",
            DiagKind::Runtime => "Runtime Error",
            DiagKind::Io => "File Error",
            DiagKind::Platform => "Platform Error",
            DiagKind::Plugin => "Plugin Error",
        }
    }
}

/// A single actionable problem.
#[derive(Clone, Debug)]
pub struct Diag {
    pub severity: Severity,
    pub kind: DiagKind,
    pub message: String,
    pub span: Option<Span>,
    /// `Did you mean "foreground"?` style alternatives.
    pub hints: Vec<String>,
    /// Related diagnostics, e.g. the invalid node followed by its source text.
    pub notes: Vec<String>,
}

impl Diag {
    pub fn new(severity: Severity, kind: DiagKind, message: impl Into<String>) -> Diag {
        Diag {
            severity,
            kind,
            message: message.into(),
            span: None,
            hints: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn error(kind: DiagKind, message: impl Into<String>) -> Diag {
        Diag::new(Severity::Error, kind, message)
    }

    pub fn fatal(kind: DiagKind, message: impl Into<String>) -> Diag {
        Diag::new(Severity::Fatal, kind, message)
    }

    pub fn warning(kind: DiagKind, message: impl Into<String>) -> Diag {
        Diag::new(Severity::Warning, kind, message)
    }

    pub fn with_span(mut self, span: Span) -> Diag {
        self.span = Some(span);
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Diag {
        self.hints.push(hint.into());
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Diag {
        self.notes.push(note.into());
        self
    }

    /// Pick the closest matches from `candidates` and attach them as hints.
    ///
    /// This is what turns "Unknown property: fooBar" into an actionable message.
    pub fn suggest_from(mut self, candidates: &[&str], input: &str) -> Diag {
        let ranked = closest(input, candidates, 3);
        if !ranked.is_empty() {
            let list = ranked
                .iter()
                .map(|s| format!("Did you mean \"{s}\"?"))
                .collect::<Vec<_>>()
                .join("\n");
            self.hints.push(list);
        }
        self
    }

    /// Chainable alias for [`Diag::suggest_from`].
    pub fn with_suggestion(self, candidates: &[&str], input: &str) -> Diag {
        self.suggest_from(candidates, input)
    }

    /// A bare `Did you mean "x"?` hint.
    pub fn with_suggestions(mut self, names: &[&str], input: &str) -> Diag {
        self = self.suggest_from(names, input);
        if !names.is_empty() {
            let list = names.join(", ");
            self.hints.push(format!("Known values: {list}"));
        }
        self
    }

    /// Render the full block shown in the debug overlay and `shellctl doctor`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(self.kind.title());
        out.push('\n');
        out.push('\n');
        if let Some(span) = &self.span {
            out.push_str(&span.location());
            out.push('\n');
        }
        out.push('\n');
        out.push_str(&self.message);
        out.push('\n');
        for hint in &self.hints {
            out.push('\n');
            out.push_str(hint);
        }
        for note in &self.notes {
            out.push('\n');
            out.push_str(note);
        }
        out
    }
}

impl fmt::Display for Diag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.span {
            Some(span) => write!(f, "{} [{}]", self.message, span.location()),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for Diag {}

pub type DiagResult<T> = Result<T, Diag>;

/// Levenshtein distance, bounded and early-exiting. Small inputs, called rarely.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Rank `candidates` by similarity to `input`, dropping implausible matches.
pub fn closest<'a>(input: &str, candidates: &[&'a str], limit: usize) -> Vec<&'a str> {
    let input_l = input.to_ascii_lowercase();
    let budget = (input_l.chars().count() / 3).max(2) + 1;
    let mut scored: Vec<(usize, &str)> = candidates
        .iter()
        .map(|c| (edit_distance(&input_l, &c.to_ascii_lowercase()), *c))
        .filter(|(d, c)| *d <= budget || input_l.contains(&c.to_ascii_lowercase()) || c.to_ascii_lowercase().contains(&input_l))
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
    scored.into_iter().take(limit).map(|(_, c)| c).collect()
}

/// A value that failed to convert, with the expected type named for the message.
#[derive(Clone, Debug)]
pub struct TypeError {
    pub expected: &'static str,
    pub got: String,
    pub got_type: &'static str,
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "expected {}, found {} ({})", self.expected, self.got, self.got_type)
    }
}

impl std::error::Error for TypeError {}

pub type ValueResult<T> = Result<T, TypeError>;

/// Helper so `Diag::mismatch` reads well at call sites.
pub fn type_name(v: &crate::value::Value) -> &'static str {
    use crate::value::Value::*;
    match v {
        Null => "null",
        Bool(_) => "bool",
        Int(_) => "int",
        Float(_) => "float",
        Str(_) => "string",
        Color(_) => "color",
        List(_) => "list",
        Map(_) => "map",
        Object(_) => "object",
        Callable(_) => "function",
    }
}

/// Format a color for display inside a diagnostic.
pub fn color_str(c: &Color) -> String {
    c.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_near_misses() {
        let props = ["foreground", "background", "borderRadius", "padding"];
        let got = closest("foregorund", &props, 2);
        assert_eq!(got, vec!["foreground"]);
    }

    #[test]
    fn rejects_unrelated_words() {
        let props = ["foreground", "background"];
        assert!(closest("qqqqzzzz", &props, 2).is_empty());
    }

    #[test]
    fn render_includes_location_and_hint() {
        let d = Diag::error(DiagKind::UnknownProperty, "Unknown property: \"temperaturee\"")
            .with_span(Span::new(
                "widgets/weather.config",
                Pos::new(31, 9, 420),
                Pos::new(31, 20, 431),
            ))
            .suggest_from(&["temperature", "pressure"], "temperaturee");
        let text = d.render();
        assert!(text.contains("widgets/weather.config:31:9"));
        assert!(text.contains("Did you mean"));
    }
}
