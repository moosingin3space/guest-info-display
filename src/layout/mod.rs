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
pub mod render;
pub mod schema;

use std::sync::OnceLock;

use schema::{LayoutDoc, Node};

/// The built-in layout, embedded rather than written in Rust so there is only
/// one renderer and the default doubles as a worked example.
const DEFAULT_SOURCE: &str = include_str!("../../assets/default-layout.kdl");

/// An active layout, plus whatever was wrong with the document that produced
/// it. Diagnostics travel with the document because settings shows them next
/// to the file they came from.
#[derive(Debug, Clone)]
pub struct Active {
    pub doc: LayoutDoc,
    pub diagnostics: Vec<Diagnostic>,
}

/// The built-in document, parsed once.
///
/// A failure here means the shipped `assets/default-layout.kdl` is broken,
/// which is a broken build rather than a runtime condition — so it panics, and
/// a test catches it long before it reaches a display.
pub fn default_doc() -> &'static LayoutDoc {
    static DEFAULT: OnceLock<LayoutDoc> = OnceLock::new();
    DEFAULT.get_or_init(|| match parse::parse(DEFAULT_SOURCE) {
        Ok(parsed) => {
            assert!(
                parsed.diagnostics.is_empty(),
                "built-in layout has diagnostics: {:?}",
                parsed.diagnostics
            );
            LayoutDoc {
                theme: parsed.theme,
                root: parsed.root.expect("built-in layout must have a root"),
            }
        }
        Err(diagnostics) => panic!("built-in layout does not parse: {diagnostics:?}"),
    })
}

/// The built-in widget tree, for documents that only set a `theme`.
fn default_root() -> Node {
    default_doc().root.clone()
}

/// Parse a layout document into something renderable.
///
/// A document with no `root` keeps the built-in tree and applies its `theme` to
/// it, so re-colouring is the cheapest useful edit a host can make.
pub fn load(source: &str) -> Result<Active, Vec<Diagnostic>> {
    let parsed = parse::parse(source)?;
    Ok(Active {
        doc: LayoutDoc {
            theme: parsed.theme,
            root: parsed.root.unwrap_or_else(default_root),
        },
        diagnostics: parsed.diagnostics,
    })
}

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
    use super::*;
    use schema::{Direction, Widget};

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

    /// The guard that "refactor the UI into a layout document" did not quietly
    /// change what ships. If this fails, the built-in file drifted from the
    /// arrangement described in the spec.
    #[test]
    fn built_in_layout_matches_the_shipped_ui() {
        let doc = default_doc();

        let children = match &doc.root.widget {
            Widget::Container(c) => &c.children,
            other => panic!("root must be a container, found {other:?}"),
        };
        assert_eq!(children.len(), 3, "header, body, role label");

        // Header: date left, clock right.
        let header = match &children[0].widget {
            Widget::Container(c) => c,
            other => panic!("header must be a container, found {other:?}"),
        };
        assert_eq!(header.direction, Direction::Row);
        assert!(matches!(header.children[0].widget, Widget::Date { .. }));
        assert!(matches!(header.children[1].widget, Widget::Clock { .. }));

        // Body: a flexible player card and a fixed-width Wi-Fi card.
        let body = match &children[1].widget {
            Widget::Container(c) => c,
            other => panic!("body must be a container, found {other:?}"),
        };
        assert_eq!(body.direction, Direction::Row);
        assert_eq!(children[1].style.flex, Some(1.0));
        assert_eq!(body.children.len(), 2);

        let player = match &body.children[0].widget {
            Widget::Container(c) => c,
            other => panic!("player must be a card, found {other:?}"),
        };
        assert!(player.surface, "the player sits on a card");
        assert!(matches!(
            player.children[0].widget,
            Widget::NowPlaying { .. }
        ));
        assert!(matches!(
            player.children[1].widget,
            Widget::UpNext { count: 5, .. }
        ));

        let sidebar = match &body.children[1].widget {
            Widget::Container(c) => c,
            other => panic!("sidebar must be a card, found {other:?}"),
        };
        assert!(sidebar.surface);
        assert!(matches!(sidebar.children[0].widget, Widget::WifiQr { .. }));
        assert!(matches!(
            sidebar.children[1].widget,
            Widget::SettingsButton { .. }
        ));

        assert!(matches!(children[2].widget, Widget::RoleLabel));
    }

    #[test]
    fn theme_only_document_keeps_the_built_in_tree() {
        let active = load("theme { muted-text \"#ffffffa0\" }").expect("valid");
        assert_eq!(active.doc.root, default_doc().root);
        assert_ne!(active.doc.theme, default_doc().theme);
    }
}
