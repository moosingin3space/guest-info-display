//! KDL source into [`schema`], with line-accurate diagnostics.
//!
//! Three tiers of problem, per the spec:
//!
//! - **Fatal** — syntax error, a document with neither `theme` nor `root`, or a
//!   limit breached. Nothing is produced; the caller keeps whatever was already
//!   on screen.
//! - **Node-level** ([`Severity::Error`]) — unknown widget, unknown property,
//!   wrong type. The offending node is skipped, its siblings are not.
//! - **Warning** — unknown `theme` key, a value clamped to its cap.
//!
//! The parser never returns a partially-applied document: either the caller
//! gets a tree it can render, or it gets fatal diagnostics and changes nothing.

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};
use miette::SourceSpan;

use super::schema::{
    Align, Background, Container, Direction, Fit, GradientDirection, GradientStop, Node, Padding,
    Rgba, Sizing, Style, TextAlign, Theme, Typography, Weight, When, Widget,
};
use super::{Diagnostic, Severity, line_col};

/// A layout is a layout, not a payload.
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
/// Bounds render cost.
pub const MAX_NODES: usize = 512;
/// Bounds recursion in both parse and render.
pub const MAX_DEPTH: usize = 32;
/// Beyond this the queue can't be read from across a room anyway.
pub const MAX_UP_NEXT: usize = 25;

/// Widget and container node names, for dispatch and for "did you mean".
const WIDGET_NAMES: &[&str] = &[
    "column",
    "row",
    "card",
    "spacer",
    "clock",
    "date",
    "now-playing",
    "up-next",
    "cover-art",
    "wifi-qr",
    "text",
    "image",
    "role-label",
    "settings-button",
];

#[derive(Debug)]
pub struct Parsed {
    pub theme: Theme,
    /// `None` when the document had no `root` node. The caller substitutes the
    /// built-in default tree, so re-colouring is the cheapest useful edit: a
    /// file containing only a `theme` block is valid and useful.
    pub root: Option<Node>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Parse a layout document.
///
/// `Err` carries fatal diagnostics — the document produced nothing usable.
/// `Ok` may still carry diagnostics for skipped nodes and clamped values.
pub fn parse(source: &str) -> Result<Parsed, Vec<Diagnostic>> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(vec![Diagnostic {
            severity: Severity::Error,
            message: format!(
                "layout is {} bytes, over the {MAX_SOURCE_BYTES} byte limit",
                source.len()
            ),
            line: 1,
            column: 1,
        }]);
    }

    let document: KdlDocument = match source.parse() {
        Ok(d) => d,
        Err(e) => {
            let mut out: Vec<Diagnostic> = e
                .diagnostics
                .iter()
                .map(|d| {
                    let (line, column) = line_col(source, d.span.offset());
                    Diagnostic {
                        severity: Severity::Error,
                        message: d
                            .message
                            .clone()
                            .unwrap_or_else(|| "invalid KDL syntax".to_string()),
                        line,
                        column,
                    }
                })
                .collect();
            if out.is_empty() {
                out.push(Diagnostic {
                    severity: Severity::Error,
                    message: "invalid KDL syntax".to_string(),
                    line: 1,
                    column: 1,
                });
            }
            return Err(out);
        }
    };

    // Limits are checked up front, on the raw document, so the conversion below
    // never has to unwind half-built state.
    let (count, depth) = measure(&document, 1);
    if count > MAX_NODES {
        return Err(vec![fatal(
            source,
            document.span(),
            format!("layout has {count} nodes, over the {MAX_NODES} limit"),
        )]);
    }
    if depth > MAX_DEPTH {
        return Err(vec![fatal(
            source,
            document.span(),
            format!("layout nests {depth} deep, over the {MAX_DEPTH} limit"),
        )]);
    }

    let mut p = Parser {
        source,
        diagnostics: Vec::new(),
    };

    let mut theme = None;
    let mut root = None;
    for node in document.nodes() {
        match node.name().value() {
            "theme" => {
                if theme.is_some() {
                    p.warn(node.span(), "duplicate `theme`, ignoring this one");
                    continue;
                }
                theme = Some(p.theme(node));
            }
            "root" => {
                if root.is_some() {
                    p.warn(node.span(), "duplicate `root`, ignoring this one");
                    continue;
                }
                root = Some(p.root(node));
            }
            other => {
                p.error(
                    node.span(),
                    format!("unknown top-level node `{other}`, expected `theme` or `root`"),
                );
            }
        }
    }

    if theme.is_none() && root.is_none() {
        return Err(vec![fatal(
            source,
            document.span(),
            "layout has neither a `theme` nor a `root` node".to_string(),
        )]);
    }

    Ok(Parsed {
        theme: theme.unwrap_or_default(),
        root,
        diagnostics: p.diagnostics,
    })
}

/// Node count and maximum depth of a raw document.
fn measure(doc: &KdlDocument, depth: usize) -> (usize, usize) {
    let mut count = 0;
    let mut max_depth = depth;
    for node in doc.nodes() {
        count += 1;
        if let Some(children) = node.children() {
            let (c, d) = measure(children, depth + 1);
            count += c;
            max_depth = max_depth.max(d);
        }
    }
    (count, max_depth)
}

fn fatal(source: &str, span: SourceSpan, message: String) -> Diagnostic {
    let (line, column) = line_col(source, span.offset());
    Diagnostic {
        severity: Severity::Error,
        message,
        line,
        column,
    }
}

/// A node's entries, split into positional arguments and named properties,
/// with consumption tracked so anything left over can be reported as unknown.
struct Entries<'a> {
    args: Vec<&'a KdlEntry>,
    props: Vec<(&'a str, &'a KdlEntry)>,
    used: Vec<bool>,
}

impl<'a> Entries<'a> {
    fn new(node: &'a KdlNode) -> Self {
        let mut args = Vec::new();
        let mut props = Vec::new();
        for entry in node.entries() {
            match entry.name() {
                Some(name) => props.push((name.value(), entry)),
                None => args.push(entry),
            }
        }
        let used = vec![false; props.len()];
        Self { args, props, used }
    }

    /// Take a property by name. KDL says the last occurrence wins, so scan from
    /// the end; every occurrence is marked used so duplicates aren't also
    /// reported as unknown.
    fn take(&mut self, name: &str) -> Option<&'a KdlEntry> {
        let mut found = None;
        for (i, (key, entry)) in self.props.iter().enumerate() {
            if *key == name {
                self.used[i] = true;
                found = Some(*entry);
            }
        }
        found
    }

    fn arg(&self, index: usize) -> Option<&'a KdlEntry> {
        self.args.get(index).copied()
    }

    fn leftovers(&self) -> Vec<(&'a str, &'a KdlEntry)> {
        self.props
            .iter()
            .zip(&self.used)
            .filter(|(_, used)| !**used)
            .map(|((name, entry), _)| (*name, *entry))
            .collect()
    }
}

struct Parser<'a> {
    source: &'a str,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Parser<'a> {
    fn push(&mut self, severity: Severity, span: SourceSpan, message: String) {
        let (line, column) = line_col(self.source, span.offset());
        self.diagnostics.push(Diagnostic {
            severity,
            message,
            line,
            column,
        });
    }

    fn error(&mut self, span: SourceSpan, message: impl Into<String>) {
        self.push(Severity::Error, span, message.into());
    }

    fn warn(&mut self, span: SourceSpan, message: impl Into<String>) {
        self.push(Severity::Warning, span, message.into());
    }

    // ---------------------------------------------------------------- values

    fn number(&mut self, entry: &KdlEntry, what: &str) -> Option<f32> {
        match entry.value() {
            KdlValue::Integer(i) => Some(*i as f32),
            KdlValue::Float(f) => Some(*f as f32),
            other => {
                self.error(
                    entry.span(),
                    format!("`{what}` expects a number, found {}", describe(other)),
                );
                None
            }
        }
    }

    fn string(&mut self, entry: &KdlEntry, what: &str) -> Option<String> {
        match entry.value() {
            KdlValue::String(s) => Some(s.clone()),
            other => {
                self.error(
                    entry.span(),
                    format!("`{what}` expects a string, found {}", describe(other)),
                );
                None
            }
        }
    }

    fn boolean(&mut self, entry: &KdlEntry, what: &str) -> Option<bool> {
        match entry.value() {
            KdlValue::Bool(b) => Some(*b),
            other => {
                self.error(
                    entry.span(),
                    format!(
                        "`{what}` expects #true or #false, found {}",
                        describe(other)
                    ),
                );
                None
            }
        }
    }

    fn color(&mut self, entry: &KdlEntry, what: &str) -> Option<Rgba> {
        let raw = self.string(entry, what)?;
        match parse_color(&raw) {
            Some(c) => Some(c),
            None => {
                self.error(
                    entry.span(),
                    format!("`{what}`: `{raw}` is not a #rrggbb or #rrggbbaa color"),
                );
                None
            }
        }
    }

    /// A string property drawn from a fixed vocabulary.
    fn keyword<T: Copy>(
        &mut self,
        entry: &KdlEntry,
        what: &str,
        options: &[(&str, T)],
    ) -> Option<T> {
        let raw = self.string(entry, what)?;
        match options.iter().find(|(name, _)| *name == raw) {
            Some((_, value)) => Some(*value),
            None => {
                let names: Vec<&str> = options.iter().map(|(n, _)| *n).collect();
                self.error(
                    entry.span(),
                    format!(
                        "`{what}`: unknown value `{raw}`, expected one of {}",
                        names.join(", ")
                    ),
                );
                None
            }
        }
    }

    fn sizing(&mut self, entry: &KdlEntry, what: &str) -> Option<Sizing> {
        match entry.value() {
            KdlValue::Integer(i) => Some(Sizing::Px(*i as f32)),
            KdlValue::Float(f) => Some(Sizing::Px(*f as f32)),
            KdlValue::String(s) if s == "fill" => Some(Sizing::Fill),
            KdlValue::String(s) if s == "auto" => Some(Sizing::Auto),
            other => {
                self.error(
                    entry.span(),
                    format!(
                        "`{what}` expects a number, \"fill\" or \"auto\", found {}",
                        describe(other)
                    ),
                );
                None
            }
        }
    }

    /// `padding=24`, `padding="20 32"` (block inline) or
    /// `padding="20 32 8 32"` (top right bottom left), matching CSS order.
    fn padding(&mut self, entry: &KdlEntry) -> Option<Padding> {
        match entry.value() {
            KdlValue::Integer(i) => return Some(Padding::all(*i as f32)),
            KdlValue::Float(f) => return Some(Padding::all(*f as f32)),
            KdlValue::String(_) => {}
            other => {
                self.error(
                    entry.span(),
                    format!(
                        "`padding` expects a number or a string of 1, 2 or 4 numbers, found {}",
                        describe(other)
                    ),
                );
                return None;
            }
        }

        let raw = self.string(entry, "padding")?;
        let parts: Result<Vec<f32>, _> = raw.split_whitespace().map(str::parse::<f32>).collect();
        let parts = match parts {
            Ok(p) => p,
            Err(_) => {
                self.error(
                    entry.span(),
                    format!("`padding`: `{raw}` is not a list of numbers"),
                );
                return None;
            }
        };
        match parts[..] {
            [all] => Some(Padding::all(all)),
            [block, inline] => Some(Padding::block_inline(block, inline)),
            [top, right, bottom, left] => Some(Padding {
                top,
                right,
                bottom,
                left,
            }),
            _ => {
                self.error(
                    entry.span(),
                    format!(
                        "`padding` takes 1, 2 or 4 numbers, found {} in `{raw}`",
                        parts.len()
                    ),
                );
                None
            }
        }
    }

    // ----------------------------------------------------------------- style

    /// Properties every node accepts. Consumed before widget-specific ones so
    /// leftovers can be reported as unknown.
    fn style(&mut self, entries: &mut Entries<'_>) -> Style {
        let mut style = Style::default();

        if let Some(e) = entries.take("width") {
            style.width = self.sizing(e, "width");
        }
        if let Some(e) = entries.take("height") {
            style.height = self.sizing(e, "height");
        }
        if let Some(e) = entries.take("flex") {
            style.flex = self.number(e, "flex");
        }
        if let Some(e) = entries.take("padding") {
            style.padding = self.padding(e);
        }
        if let Some(e) = entries.take("spacing") {
            style.spacing = self.number(e, "spacing");
        }
        if let Some(e) = entries.take("main-align") {
            style.main_align = self.keyword(e, "main-align", ALIGNMENTS);
        }
        if let Some(e) = entries.take("cross-align") {
            style.cross_align = match self.keyword(e, "cross-align", ALIGNMENTS) {
                // Freya's cross axis has no space distribution to do — there is
                // only one item across it. Clamp rather than reject: the intent
                // is obvious and the layout still renders.
                Some(Align::SpaceBetween | Align::SpaceAround) => {
                    self.warn(
                        e.span(),
                        "`cross-align` has no space-* modes, using \"start\"",
                    );
                    Some(Align::Start)
                }
                other => other,
            };
        }
        if let Some(e) = entries.take("when") {
            style.when = self.keyword(e, "when", CONDITIONS).unwrap_or_default();
        }

        style.typography = self.typography(entries);
        style
    }

    fn typography(&mut self, entries: &mut Entries<'_>) -> Typography {
        let mut t = Typography::default();
        if let Some(e) = entries.take("font-size") {
            t.font_size = self.number(e, "font-size");
        }
        if let Some(e) = entries.take("weight") {
            t.weight = self.keyword(e, "weight", WEIGHTS);
        }
        if let Some(e) = entries.take("color") {
            t.color = self.color(e, "color");
        }
        if let Some(e) = entries.take("align") {
            t.align = self.keyword(e, "align", TEXT_ALIGNMENTS);
        }
        if let Some(e) = entries.take("mono") {
            t.mono = self.boolean(e, "mono").unwrap_or(false);
        }
        t
    }

    // ------------------------------------------------------------------ tree

    fn root(&mut self, node: &KdlNode) -> Node {
        let mut entries = Entries::new(node);
        let mut style = self.style(&mut entries);
        self.report_leftovers(&entries);

        // `root` is implicitly full-bleed; an explicit width/height wins.
        style.width.get_or_insert(Sizing::Fill);
        style.height.get_or_insert(Sizing::Fill);

        let children = match node.children() {
            Some(doc) => self.children(doc),
            None => Vec::new(),
        };

        Node {
            style,
            widget: Widget::Container(Container {
                direction: Direction::Column,
                surface: false,
                children,
            }),
        }
    }

    fn children(&mut self, doc: &KdlDocument) -> Vec<Node> {
        doc.nodes().iter().filter_map(|n| self.widget(n)).collect()
    }

    /// One widget node. `None` means it was skipped and a diagnostic recorded —
    /// its siblings still render.
    fn widget(&mut self, node: &KdlNode) -> Option<Node> {
        let name = node.name().value();
        let mut entries = Entries::new(node);
        let style = self.style(&mut entries);

        let widget = match name {
            "column" | "row" | "card" => {
                let direction = if name == "row" {
                    Direction::Row
                } else if let Some(e) = entries.take("direction") {
                    self.keyword(e, "direction", DIRECTIONS)
                        .unwrap_or(Direction::Column)
                } else {
                    Direction::Column
                };
                let children = match node.children() {
                    Some(doc) => self.children(doc),
                    None => Vec::new(),
                };
                Widget::Container(Container {
                    direction,
                    surface: name == "card",
                    children,
                })
            }
            "spacer" => Widget::Spacer,
            "clock" => Widget::Clock {
                format: self
                    .opt_string(&mut entries, "format")
                    .unwrap_or_else(|| "%H:%M:%S".to_string()),
            },
            "date" => Widget::Date {
                format: self
                    .opt_string(&mut entries, "format")
                    .unwrap_or_else(|| "%A, %B %-d".to_string()),
            },
            "now-playing" => Widget::NowPlaying {
                heading: self
                    .opt_string(&mut entries, "heading")
                    .unwrap_or_else(|| "Now Playing".to_string()),
                cover_size: self.opt_number(&mut entries, "cover-size").unwrap_or(220.0),
                show_artist: self.opt_bool(&mut entries, "show-artist").unwrap_or(true),
            },
            "up-next" => {
                let raw = self.opt_number(&mut entries, "count").unwrap_or(5.0);
                let count = raw.max(0.0) as usize;
                let count = if count > MAX_UP_NEXT {
                    self.warn(
                        node.span(),
                        format!("`count` clamped to the {MAX_UP_NEXT} track maximum"),
                    );
                    MAX_UP_NEXT
                } else {
                    count
                };
                Widget::UpNext {
                    heading: self
                        .opt_string(&mut entries, "heading")
                        .unwrap_or_else(|| "Up Next".to_string()),
                    count,
                }
            }
            "cover-art" => Widget::CoverArt {
                size: self.opt_number(&mut entries, "size").unwrap_or(220.0),
            },
            "wifi-qr" => Widget::WifiQr {
                heading: self
                    .opt_string(&mut entries, "heading")
                    .unwrap_or_else(|| "Scan to connect to Wi-Fi".to_string()),
                size: self.opt_number(&mut entries, "size").unwrap_or(220.0),
            },
            "text" => {
                let entry = entries.arg(0)?;
                let content = self.string(entry, "text")?;
                Widget::Text { content }
            }
            "image" => {
                let entry = entries.arg(0)?;
                let path = self.string(entry, "image")?;
                if let Err(why) = validate_asset_path(&path) {
                    self.error(entry.span(), format!("`{path}`: {why}"));
                    return None;
                }
                let fit = entries
                    .take("fit")
                    .and_then(|e| self.keyword(e, "fit", FITS))
                    .unwrap_or(Fit::Cover);
                Widget::Image { path, fit }
            }
            "role-label" => Widget::RoleLabel,
            "settings-button" => Widget::SettingsButton {
                size: self.opt_number(&mut entries, "size").unwrap_or(24.0),
            },
            other => {
                let hint = match nearest_name(other) {
                    Some(suggestion) => format!(" — did you mean `{suggestion}`?"),
                    None => String::new(),
                };
                self.error(node.span(), format!("unknown widget `{other}`{hint}"));
                return None;
            }
        };

        // A leftover property means the author asked for something we don't
        // understand; skipping the node makes that visible rather than
        // silently rendering something they didn't ask for.
        if !entries.leftovers().is_empty() {
            self.report_leftovers(&entries);
            return None;
        }

        Some(Node { style, widget })
    }

    fn report_leftovers(&mut self, entries: &Entries<'_>) {
        for (name, entry) in entries.leftovers() {
            self.error(entry.span(), format!("unknown property `{name}`"));
        }
    }

    fn opt_string(&mut self, entries: &mut Entries<'_>, name: &str) -> Option<String> {
        let entry = entries.take(name)?;
        self.string(entry, name)
    }

    fn opt_number(&mut self, entries: &mut Entries<'_>, name: &str) -> Option<f32> {
        let entry = entries.take(name)?;
        self.number(entry, name)
    }

    fn opt_bool(&mut self, entries: &mut Entries<'_>, name: &str) -> Option<bool> {
        let entry = entries.take(name)?;
        self.boolean(entry, name)
    }

    // ----------------------------------------------------------------- theme

    fn theme(&mut self, node: &KdlNode) -> Theme {
        let mut theme = Theme::default();
        let Some(children) = node.children() else {
            return theme;
        };

        for child in children.nodes() {
            let name = child.name().value();
            let mut entries = Entries::new(child);
            match name {
                "background" => {
                    if let Some(background) = self.background(child, &mut entries) {
                        theme.background = background;
                    }
                }
                "surface" | "surface-border" | "placeholder" | "text" | "muted-text" => {
                    let Some(entry) = entries.arg(0) else {
                        self.warn(child.span(), format!("`{name}` needs a color, ignoring"));
                        continue;
                    };
                    if let Some(color) = self.color(entry, name) {
                        match name {
                            "surface" => theme.surface = color,
                            "surface-border" => theme.surface_border = color,
                            "placeholder" => theme.placeholder = color,
                            "text" => theme.text = color,
                            _ => theme.muted_text = color,
                        }
                    }
                }
                "font-family" | "mono-font-family" => {
                    let Some(entry) = entries.arg(0) else {
                        self.warn(child.span(), format!("`{name}` needs a name, ignoring"));
                        continue;
                    };
                    if let Some(family) = self.string(entry, name) {
                        if name == "font-family" {
                            theme.font_family = Some(family);
                        } else {
                            theme.mono_font_family = Some(family);
                        }
                    }
                }
                other => {
                    // A theme from a newer build should degrade, not blank the
                    // screen, so unknown keys are only a warning.
                    self.warn(
                        child.span(),
                        format!("unknown theme key `{other}`, ignoring"),
                    );
                }
            }
        }

        theme
    }

    fn background(&mut self, node: &KdlNode, entries: &mut Entries<'_>) -> Option<Background> {
        if let Some(entry) = entries.arg(0) {
            return self.color(entry, "background").map(Background::Solid);
        }

        let gradient = node.children().and_then(|doc| doc.get("gradient"));
        let Some(gradient) = gradient else {
            self.warn(
                node.span(),
                "`background` needs a color or a `gradient` block, ignoring",
            );
            return None;
        };

        let mut gradient_entries = Entries::new(gradient);
        let direction = gradient_entries
            .take("to")
            .and_then(|e| self.keyword(e, "to", GRADIENT_DIRECTIONS))
            .unwrap_or(GradientDirection::Bottom);

        let mut stops = Vec::new();
        if let Some(doc) = gradient.children() {
            for stop in doc.nodes() {
                if stop.name().value() != "stop" {
                    self.warn(
                        stop.span(),
                        format!("unknown gradient node `{}`, ignoring", stop.name().value()),
                    );
                    continue;
                }
                let stop_entries = Entries::new(stop);
                let (Some(color_entry), Some(position_entry)) =
                    (stop_entries.arg(0), stop_entries.arg(1))
                else {
                    self.warn(stop.span(), "`stop` needs a color and a position, ignoring");
                    continue;
                };
                let (Some(color), Some(position)) = (
                    self.color(color_entry, "stop"),
                    self.number(position_entry, "stop"),
                ) else {
                    continue;
                };
                stops.push(GradientStop {
                    color,
                    position: position.clamp(0.0, 100.0) as i16,
                });
            }
        }

        if stops.len() < 2 {
            self.warn(
                gradient.span(),
                "`gradient` needs at least two stops, ignoring",
            );
            return None;
        }

        Some(Background::Gradient { direction, stops })
    }
}

const ALIGNMENTS: &[(&str, Align)] = &[
    ("start", Align::Start),
    ("center", Align::Center),
    ("end", Align::End),
    ("space-between", Align::SpaceBetween),
    ("space-around", Align::SpaceAround),
];

const TEXT_ALIGNMENTS: &[(&str, TextAlign)] = &[
    ("start", TextAlign::Start),
    ("center", TextAlign::Center),
    ("end", TextAlign::End),
];

const WEIGHTS: &[(&str, Weight)] = &[
    ("normal", Weight::Normal),
    ("semibold", Weight::SemiBold),
    ("bold", Weight::Bold),
];

const DIRECTIONS: &[(&str, Direction)] = &[("column", Direction::Column), ("row", Direction::Row)];

const FITS: &[(&str, Fit)] = &[("cover", Fit::Cover), ("contain", Fit::Contain)];

const CONDITIONS: &[(&str, When)] = &[
    ("always", When::Always),
    ("playing", When::Playing),
    ("idle", When::Idle),
    ("connected", When::Connected),
    ("disconnected", When::Disconnected),
    ("wifi-configured", When::WifiConfigured),
    ("wifi-unconfigured", When::WifiUnconfigured),
];

const GRADIENT_DIRECTIONS: &[(&str, GradientDirection)] = &[
    ("top", GradientDirection::Top),
    ("bottom", GradientDirection::Bottom),
    ("left", GradientDirection::Left),
    ("right", GradientDirection::Right),
    ("top-left", GradientDirection::TopLeft),
    ("top-right", GradientDirection::TopRight),
    ("bottom-left", GradientDirection::BottomLeft),
    ("bottom-right", GradientDirection::BottomRight),
];

fn describe(value: &KdlValue) -> String {
    match value {
        KdlValue::String(s) => format!("the string \"{s}\""),
        KdlValue::Integer(i) => format!("the number {i}"),
        KdlValue::Float(f) => format!("the number {f}"),
        KdlValue::Bool(b) => format!("#{b}"),
        KdlValue::Null => "#null".to_string(),
    }
}

fn parse_color(raw: &str) -> Option<Rgba> {
    let hex = raw.strip_prefix('#')?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    match hex.len() {
        6 => Some(Rgba::opaque(byte(0)?, byte(2)?, byte(4)?)),
        8 => Some(Rgba::new(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
        _ => None,
    }
}

/// Asset paths are relative to the layout file's directory and must stay inside
/// it. Absolute paths and `..` would let a layout read anything the sandbox can
/// see, and — more practically — would not survive being mirrored to a
/// reflection with a different filesystem.
fn validate_asset_path(path: &str) -> Result<(), &'static str> {
    use std::path::{Component, Path};

    if path.is_empty() {
        return Err("path is empty");
    }
    let path = Path::new(path);
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => return Err("`..` is not allowed in an asset path"),
            Component::RootDir | Component::Prefix(_) => {
                return Err("asset paths must be relative to the layout file");
            }
        }
    }
    Ok(())
}

/// Cheap "did you mean" over the widget names. Bounded edit distance keeps a
/// wild typo from suggesting something unrelated.
fn nearest_name(name: &str) -> Option<&'static str> {
    let max = if name.len() <= 4 { 1 } else { 2 };
    WIDGET_NAMES
        .iter()
        .map(|candidate| (*candidate, edit_distance(name, candidate)))
        .filter(|(_, d)| *d <= max)
        .min_by_key(|(_, d)| *d)
        .map(|(candidate, _)| candidate)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut current = vec![0; b_chars.len() + 1];

    for (i, ac) in a.chars().enumerate() {
        current[0] = i + 1;
        for (j, bc) in b_chars.iter().enumerate() {
            let cost = usize::from(ac != *bc);
            current[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut prev, &mut current);
    }
    prev[b_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(source: &str) -> Parsed {
        match parse(source) {
            Ok(p) => p,
            Err(diags) => panic!("expected a parse, got fatal: {diags:?}"),
        }
    }

    fn children_of(node: &Node) -> &[Node] {
        match &node.widget {
            Widget::Container(c) => &c.children,
            other => panic!("expected a container, found {other:?}"),
        }
    }

    #[test]
    fn parses_a_minimal_tree() {
        let p = ok(r#"
            root {
                row padding="20 32" main-align="space-between" {
                    date
                    clock font-size=30 mono=#true
                }
            }
        "#);
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);

        let root = p.root.expect("root");
        let row = &children_of(&root)[0];
        assert_eq!(row.style.padding, Some(Padding::block_inline(20.0, 32.0)));
        assert_eq!(row.style.main_align, Some(Align::SpaceBetween));

        let kids = children_of(row);
        assert!(matches!(kids[0].widget, Widget::Date { .. }));
        assert!(matches!(kids[1].widget, Widget::Clock { .. }));
        assert_eq!(kids[1].style.typography.font_size, Some(30.0));
        assert!(kids[1].style.typography.mono);
    }

    #[test]
    fn theme_only_document_is_valid() {
        let p = ok(r##"theme { muted-text "#ffffffa0" }"##);
        assert!(p.root.is_none());
        assert_eq!(p.theme.muted_text, Rgba::new(255, 255, 255, 160));
        // Untouched keys keep the built-in values.
        assert_eq!(p.theme.surface, Theme::default().surface);
    }

    #[test]
    fn parses_a_gradient() {
        let p = ok(r##"
            theme {
                background {
                    gradient to="bottom-right" {
                        stop "#3b0a24" 0
                        stop "#7a1f3d" 100
                    }
                }
            }
        "##);
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
        match p.theme.background {
            Background::Gradient { direction, stops } => {
                assert_eq!(direction, GradientDirection::BottomRight);
                assert_eq!(stops.len(), 2);
                assert_eq!(stops[0].color, Rgba::opaque(0x3b, 0x0a, 0x24));
                assert_eq!(stops[1].position, 100);
            }
            other => panic!("expected a gradient, found {other:?}"),
        }
    }

    #[test]
    fn solid_background() {
        let p = ok(r##"theme { background "#101010" }"##);
        assert_eq!(
            p.theme.background,
            Background::Solid(Rgba::opaque(16, 16, 16))
        );
    }

    #[test]
    fn unknown_widget_is_skipped_and_suggests() {
        let p = ok("root {\n  clok\n  date\n}");
        let root = p.root.expect("root");
        // The typo is gone; its sibling survives.
        assert_eq!(children_of(&root).len(), 1);
        assert!(matches!(children_of(&root)[0].widget, Widget::Date { .. }));

        assert_eq!(p.diagnostics.len(), 1);
        let d = &p.diagnostics[0];
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.line, 2, "diagnostic should point at the typo's line");
        assert!(d.message.contains("did you mean `clock`"), "{}", d.message);
    }

    #[test]
    fn unknown_property_skips_the_node() {
        let p = ok("root {\n  clock tick-tock=#true\n}");
        assert!(children_of(&p.root.expect("root")).is_empty());
        assert_eq!(p.diagnostics.len(), 1);
        assert!(p.diagnostics[0].message.contains("unknown property"));
    }

    #[test]
    fn wrong_type_is_reported_with_a_line() {
        let p = ok("root {\n  clock\n  up-next count=\"lots\"\n}");
        let errors: Vec<_> = p
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line, 3);
        assert!(errors[0].message.contains("expects a number"));
    }

    #[test]
    fn up_next_count_is_clamped() {
        let p = ok("root { up-next count=500 }");
        match children_of(&p.root.expect("root"))[0].widget {
            Widget::UpNext { count, .. } => assert_eq!(count, MAX_UP_NEXT),
            ref other => panic!("expected up-next, found {other:?}"),
        }
        assert_eq!(p.diagnostics[0].severity, Severity::Warning);
    }

    #[test]
    fn escaping_asset_paths_are_rejected() {
        let p = ok(r#"root { image "../../etc/passwd" }"#);
        assert!(children_of(&p.root.expect("root")).is_empty());
        assert!(
            p.diagnostics[0].message.contains(".."),
            "{:?}",
            p.diagnostics
        );

        let p = ok(r#"root { image "/etc/passwd" }"#);
        assert!(p.diagnostics[0].message.contains("relative"));
    }

    #[test]
    fn unknown_theme_key_is_only_a_warning() {
        let p = ok(r##"theme { chartreuse "#00ff00" }"##);
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.diagnostics[0].severity, Severity::Warning);
        assert_eq!(p.theme, Theme::default());
    }

    #[test]
    fn syntax_error_is_fatal_with_a_line() {
        let err = parse("root {\n  clock\n").expect_err("unterminated block");
        assert!(!err.is_empty());
    }

    #[test]
    fn empty_document_is_fatal() {
        parse("// nothing here\n").expect_err("no theme and no root");
    }

    #[test]
    fn unknown_top_level_node_is_skipped() {
        let p = ok("root { clock }\nsidebar { }");
        assert_eq!(p.diagnostics.len(), 1);
        assert!(p.diagnostics[0].message.contains("unknown top-level node"));
    }

    #[test]
    fn node_budget_is_fatal() {
        let mut source = String::from("root {\n");
        for _ in 0..MAX_NODES + 1 {
            source.push_str("  clock\n");
        }
        source.push('}');
        let err = parse(&source).expect_err("over the node budget");
        assert!(err[0].message.contains("nodes"), "{:?}", err);
    }

    #[test]
    fn depth_budget_is_fatal() {
        let mut source = String::from("root {\n");
        for _ in 0..MAX_DEPTH + 2 {
            source.push_str("column {\n");
        }
        for _ in 0..MAX_DEPTH + 2 {
            source.push_str("}\n");
        }
        source.push('}');
        let err = parse(&source).expect_err("over the depth budget");
        assert!(err[0].message.contains("deep"), "{:?}", err);
    }

    #[test]
    fn oversized_source_is_fatal() {
        let source = format!("root {{ text \"{}\" }}", "x".repeat(MAX_SOURCE_BYTES));
        parse(&source).expect_err("over the size limit");
    }

    #[test]
    fn colors_accept_both_lengths() {
        assert_eq!(parse_color("#ffffff"), Some(Rgba::opaque(255, 255, 255)));
        assert_eq!(parse_color("#ffffff0f"), Some(Rgba::new(255, 255, 255, 15)));
        assert_eq!(parse_color("#fff"), None);
        assert_eq!(parse_color("ffffff"), None);
        assert_eq!(parse_color("#gggggg"), None);
    }

    #[test]
    fn cross_align_space_modes_are_clamped() {
        let p = ok(r#"root { column cross-align="space-between" }"#);
        let root = p.root.expect("root");
        let column = &children_of(&root)[0];
        assert_eq!(column.style.cross_align, Some(Align::Start));
        assert_eq!(p.diagnostics[0].severity, Severity::Warning);
    }
}
