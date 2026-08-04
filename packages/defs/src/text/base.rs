use derive_more::{Display, Error};

/// A byte range in the source text (half-open: `start..end`).
/// Byte offsets, not char offsets — `&source[start..end]` reproduces the text.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// A value annotated with its source span.
#[derive(Copy, Clone, Debug)]
pub struct Spanned<T> {
    pub span: Span,
    pub value: T,
}

impl<T: PartialEq> PartialEq for Spanned<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

/// Converts byte offsets to (line, column, line-text) for diagnostic rendering.
pub struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(source: &str) -> Self {
        let mut line_starts = vec![0];
        for (i, c) in source.char_indices() {
            if c == '\n' {
                line_starts.push(i + 1);
            }
        }
        Self { line_starts }
    }

    pub fn lookup(&self, pos: usize) -> (usize, usize) {
        let line = self
            .line_starts
            .binary_search(&pos)
            .unwrap_or_else(|i| i.saturating_sub(1));
        let col = pos - self.line_starts[line] + 1;
        (line + 1, col)
    }

    pub fn line_text<'a>(&self, source: &'a str, pos: usize) -> &'a str {
        let line = self
            .line_starts
            .binary_search(&pos)
            .unwrap_or_else(|i| i.saturating_sub(1));
        let start = self.line_starts[line];
        let end = source[start..]
            .find('\n')
            .map_or(source.len(), |off| start + off);
        &source[start..end]
    }
}

/// The construct a parse error happened *inside*, rendered as the diagnostic's
/// secondary label ("in enum `ESoundNames`").
///
/// One context, not a stack: the innermost production to see the error wins,
/// because it is the first to run on the way out. That is the frame a reader
/// needs — the enclosing enum, definition, or tagged block — and the file name
/// supplies the rest.
#[derive(Clone, Debug)]
pub struct ParseContext {
    /// What kind of construct: "definition", "enum", "namespace", "#ifdef",
    /// "tagged block".
    pub what: &'static str,
    /// Its name, when it has one (`UI_TABLE`, `ESoundNames`).
    pub name: Option<String>,
    /// The name if present, else the opening keyword — never the whole header
    /// line, so the label underlines what identifies the construct.
    pub span: Span,
}

impl ParseContext {
    pub fn new(what: &'static str, name: Option<String>, span: Span) -> Self {
        Self { what, name, span }
    }

    /// The secondary-label text: "in enum `ESoundNames`" / "in this enum".
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) => format!("in {} `{name}`", self.what),
            None => format!("in this {}", self.what),
        }
    }
}

#[derive(Debug, Display, Error)]
#[display("{inner}")]
pub struct ParseError<InnerError> {
    /// The source range the error is *about* — normally the offending token, so
    /// the rendered caret covers it rather than pointing at a single byte.
    pub span: Span,
    pub context: Option<ParseContext>,
    pub inner: InnerError,
}

impl<T> ParseError<T> {
    pub(crate) fn new(span: Span, inner: T) -> Self {
        Self {
            span,
            context: None,
            inner,
        }
    }

    /// Attach the enclosing construct. Innermost wins: an outer production
    /// never overwrites context an inner one already set.
    pub(crate) fn within(mut self, context: &ParseContext) -> Self {
        if self.context.is_none() {
            self.context = Some(context.clone());
        }
        self
    }
}
