//! Configurable layouts: the UI tree, palette and fonts come from a KDL
//! document rather than from hard-coded Rust.
//!
//! The pipeline is deliberately three separate pieces:
//!
//! - [`schema`] — plain data, no Freya types, no I/O.
//! - [`parse`] — KDL source into [`schema`], collecting [`Diagnostic`]s.
//! - `render` — [`schema`] into Freya elements, given a `Model`.
//!
//! [`assets`] sits alongside them, holding the image bytes an `image` node
//! names: read from disk on a primary, received over the wire on a reflection.
//!
//! The governing rule for everything below is **render what you can, report
//! what you can't**: a party display with one mistyped widget shows the rest of
//! the party's layout, not a stack trace.

pub mod assets;
pub mod parse;
pub mod render;
pub mod schema;
pub mod watch;

use std::path::PathBuf;
use std::sync::OnceLock;

use schema::{LayoutDoc, Node};

/// Same application id the database uses, under the *config* directory rather
/// than the data one. The database is app state; the layout is user-authored
/// input, and mixing them makes "delete my settings" ambiguous.
const APP_DIR: &str = "xyz.mooshq.GuestInfoDisplay";
const LAYOUT_FILENAME: &str = "layout.kdl";

/// `$XDG_CONFIG_HOME/xyz.mooshq.GuestInfoDisplay`, falling back to
/// `~/.config/…`. Inside the Flatpak sandbox this is
/// `~/.var/app/xyz.mooshq.GuestInfoDisplay/config`, which the app already owns
/// — no `--filesystem=` grant is needed to read or write here.
pub fn config_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").expect("HOME must be set")).join(".config")
        });
    base.join(APP_DIR)
}

pub fn layout_path() -> PathBuf {
    config_dir().join(LAYOUT_FILENAME)
}

/// The built-in layout's source, for "Copy default to config".
pub fn default_source() -> &'static str {
    DEFAULT_SOURCE
}

/// Write the layout file, creating the config directory if needed.
///
/// Temp file plus `rename(2)`, never a truncate-and-write: the mtime poller
/// runs every 2s and must never observe a half-written document. The temp file
/// is a sibling so the rename stays within one filesystem.
pub fn write_layout(source: &str) -> std::io::Result<()> {
    let path = layout_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("kdl.tmp");
    std::fs::write(&tmp, source)?;
    std::fs::rename(&tmp, &path)
}

/// The built-in layout, embedded rather than written in Rust so there is only
/// one renderer and the default doubles as a worked example.
const DEFAULT_SOURCE: &str = include_str!("../../assets/default-layout.kdl");

/// Where the layout on screen came from. Surfaced in settings so a host can
/// tell "my file is loaded" from "my file is broken and you're seeing the
/// default".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `assets/default-layout.kdl`, compiled in.
    BuiltIn,
    /// The host's `layout.kdl`.
    File,
    /// Pushed over the wire by the paired primary. A reflection has no say in
    /// its own layout — deliberately, so there is one place to change how the
    /// party looks.
    Mirrored,
}

/// An active layout, plus whatever was wrong with the document that produced
/// it. Diagnostics travel with the document because settings shows them next
/// to the file they came from.
#[derive(Debug, Clone)]
pub struct Active {
    pub doc: LayoutDoc,
    pub diagnostics: Vec<Diagnostic>,
    pub source: Source,
    /// The KDL this was parsed from. Kept because a primary mirrors it
    /// verbatim to its reflections and a reflection persists it across
    /// reboots.
    pub text: String,
}

/// The layout half of the model: what is rendering, and why it might not be
/// what is on disk.
#[derive(Debug, Clone)]
pub struct LayoutState {
    pub active: Active,
    /// Fatal diagnostics from the most recent failed load. The active document
    /// is whatever was good last; these say why it is not the file on disk.
    pub load_error: Vec<Diagnostic>,
}

impl LayoutState {
    /// The layout to start with.
    ///
    /// A primary reads `layout.kdl`; a missing file is the normal state on a
    /// fresh install, not an error, so it produces no diagnostics. A reflection
    /// takes whatever its primary sent last — persisted, so a reboot before the
    /// primary comes up still shows the party's colours rather than flashing
    /// the built-in default — and ignores any local file entirely.
    pub fn startup(primary: bool, mirrored: Option<String>) -> Self {
        let mut state = Self {
            active: built_in(),
            load_error: Vec::new(),
        };
        let path = layout_path();

        if !primary {
            if path.exists() {
                log::info!(
                    "layout: ignoring {} — reflections take their layout from their primary",
                    path.display()
                );
            }
            match mirrored {
                Some(source) => state.apply(&source, Source::Mirrored),
                None => log::info!("layout: nothing mirrored yet, using the built-in layout"),
            }
            return state;
        }

        match std::fs::read_to_string(&path) {
            Ok(source) => state.apply(&source, Source::File),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("layout: no {}, using the built-in layout", path.display());
            }
            Err(e) => {
                log::warn!("layout: cannot read {}: {e}", path.display());
            }
        }
        state
    }

    /// Parse `source` and, if it is usable, put it on screen.
    ///
    /// A fatal document changes nothing that is rendering — the running display
    /// keeps the last good layout and only the error record moves. That is the
    /// whole safety argument for hot reload: a half-typed file in an editor
    /// cannot produce a half-broken screen.
    pub fn apply(&mut self, source: &str, from: Source) {
        match load(source) {
            Ok(mut active) => {
                active.source = from;
                active.text = source.to_string();
                for d in &active.diagnostics {
                    log::warn!("layout: {d}");
                }
                self.active = active;
                self.load_error.clear();
            }
            Err(diagnostics) => {
                for d in &diagnostics {
                    log::warn!("layout: {d}");
                }
                self.load_error = diagnostics;
            }
        }
    }

    pub fn use_built_in(&mut self) {
        self.active = built_in();
        self.load_error.clear();
    }

    /// Re-read the layout file now, for hosts who would rather not wait out
    /// the poll interval. A file that has gone away falls back to the built-in
    /// layout, same as the watcher would do.
    pub fn reload(&mut self) {
        let path = layout_path();
        match std::fs::read_to_string(&path) {
            Ok(source) => self.apply(&source, Source::File),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.use_built_in(),
            Err(e) => {
                log::warn!("layout: cannot read {}: {e}", path.display());
                self.load_error = vec![Diagnostic {
                    severity: Severity::Error,
                    message: format!("cannot read {}: {e}", path.display()),
                    line: 1,
                    column: 1,
                }];
            }
        }
    }

    /// Every problem worth showing a host, worst first.
    pub fn problems(&self) -> impl Iterator<Item = &Diagnostic> {
        self.load_error.iter().chain(&self.active.diagnostics)
    }
}

impl Default for LayoutState {
    fn default() -> Self {
        Self {
            active: built_in(),
            load_error: Vec::new(),
        }
    }
}

fn built_in() -> Active {
    Active {
        doc: default_doc().clone(),
        diagnostics: Vec::new(),
        source: Source::BuiltIn,
        text: DEFAULT_SOURCE.to_string(),
    }
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
        source: Source::File,
        text: source.to_string(),
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
    fn a_reflection_ignores_its_own_layout_file() {
        // Whatever is on disk, a reflection starts from what was mirrored.
        let state = LayoutState::startup(false, Some("root { clock }".to_string()));
        assert_eq!(state.active.source, Source::Mirrored);
        assert_ne!(state.active.doc.root, default_doc().root);

        // And with nothing mirrored yet, the built-in, not the local file.
        let state = LayoutState::startup(false, None);
        assert_eq!(state.active.source, Source::BuiltIn);
    }

    #[test]
    fn active_text_round_trips_for_mirroring() {
        let source = "root { clock format=\"%H:%M\" }";
        let mut state = LayoutState::default();
        state.apply(source, Source::File);
        assert_eq!(
            state.active.text, source,
            "the primary mirrors this verbatim"
        );
    }

    #[test]
    fn a_fatal_document_leaves_the_running_layout_alone() {
        let mut state = LayoutState::default();
        state.apply("root { text \"party\" }", Source::File);
        let good = state.active.doc.clone();
        assert_eq!(state.active.source, Source::File);

        // Half-typed, as an editor would leave it mid-save.
        state.apply("root { text ", Source::File);
        assert_eq!(state.active.doc, good, "the screen must not change");
        assert!(!state.load_error.is_empty(), "but the failure is recorded");

        // And a good document clears the error.
        state.apply("root { clock }", Source::File);
        assert!(state.load_error.is_empty());
    }

    #[test]
    fn theme_only_document_keeps_the_built_in_tree() {
        let active = load("theme { muted-text \"#ffffffa0\" }").expect("valid");
        assert_eq!(active.doc.root, default_doc().root);
        assert_ne!(active.doc.theme, default_doc().theme);
    }
}
