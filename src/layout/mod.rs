//! Configurable layouts: the UI tree, palette and fonts come from a KDL
//! document rather than from hard-coded Rust.
//!
//! The pipeline is deliberately three separate pieces:
//!
//! - [`schema`] — plain data, no Freya types, no I/O.
//! - [`parse`] — KDL source into [`schema`], collecting [`Diagnostic`]s.
//! - `render` — [`schema`] into Freya elements, given a `Model`.
//!
//! The governing rule for everything below is **render what you can, report
//! what you can't**: a party display with one mistyped widget shows the rest of
//! the party's layout, not a stack trace.

pub mod parse;
pub mod schema;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The offending node was skipped. Its siblings still render.
    Error,
    /// The value was clamped or ignored; something still rendered there.
    Warning,
}

/// One problem with a layout document, located in the source so the settings
/// dialog can say "line 14" rather than "somewhere".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub line: usize,
    pub column: usize,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

/// Resolve a byte offset into 1-based line and column.
///
/// `kdl` spans come from `miette`, whose offsets are byte offsets into the
/// source we handed it. Offsets are clamped and walked back to a character
/// boundary so a diagnostic can never panic on multi-byte input — a layout
/// full of emoji headings is entirely plausible.
pub(crate) fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let mut offset = offset.min(source.len());
    while offset > 0 && !source.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &source[..offset];
    let line = before.bytes().filter(|b| *b == b'\n').count() + 1;
    let column = match before.rfind('\n') {
        Some(nl) => before[nl + 1..].chars().count() + 1,
        None => before.chars().count() + 1,
    };
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::line_col;

    #[test]
    fn line_col_is_one_based() {
        let src = "alpha\nbeta\ngamma";
        assert_eq!(line_col(src, 0), (1, 1));
        assert_eq!(line_col(src, 6), (2, 1));
        assert_eq!(line_col(src, 8), (2, 3));
    }

    #[test]
    fn line_col_survives_multibyte_and_overrun() {
        let src = "héllo\nwörld";
        // Mid-character offsets walk back rather than panicking.
        assert_eq!(line_col(src, 2), (1, 2));
        assert_eq!(line_col(src, 9999), (2, 6));
    }
}
