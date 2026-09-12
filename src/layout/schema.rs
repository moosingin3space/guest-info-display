//! Typed representation of a layout document.
//!
//! Deliberately free of Freya types. `parse` produces this from KDL source and
//! `render` turns it into elements; keeping the middle layer plain data means
//! the parser and its diagnostics are testable without a window, a GPU or a
//! running `Model`.
//!
//! Every dimension here is a *design pixel* at [`crate::DESIGN_WIDTH`] (1280).
//! `use_ui_zoom` scales the whole tree on larger panels, so neither this module
//! nor the layout author ever thinks about scale factor.

/// Straight RGBA, matching the `(u8, u8, u8, u8)` tuples the hard-coded UI
/// used for its palette constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn opaque(r: u8, g: u8, b: u8) -> Self {
        Self::new(r, g, b, 255)
    }
}

/// How a node is sized along one axis. `flex` lives on [`Style`] separately
/// because Freya treats a flex share and a concrete size as different things.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sizing {
    /// Shrink to the content.
    Auto,
    /// Fill the parent along this axis.
    Fill,
    /// A fixed number of design pixels.
    Px(f32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Start,
    Center,
    End,
    SpaceBetween,
    SpaceAround,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Padding {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Padding {
    pub const fn all(v: f32) -> Self {
        Self {
            top: v,
            right: v,
            bottom: v,
            left: v,
        }
    }

    /// The `"block inline"` two-value form, matching CSS shorthand order.
    pub const fn block_inline(block: f32, inline: f32) -> Self {
        Self {
            top: block,
            right: inline,
            bottom: block,
            left: inline,
        }
    }
}

/// Visibility condition. A node whose condition is false is skipped along with
/// its whole subtree — this is what lets one file say "photo while idle, player
/// while playing" without inventing widget types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum When {
    #[default]
    Always,
    Playing,
    Idle,
    Connected,
    Disconnected,
    WifiConfigured,
    WifiUnconfigured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    Normal,
    SemiBold,
    Bold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Start,
    Center,
    End,
}

/// Shared text properties. Accepted on every widget that draws text, not only
/// the ones whose catalog entry mentions them — `mono` is documented on
/// `clock` because that is where it matters, but a `text` node may use it too.
///
/// Widgets that draw more than one line (`now-playing`, `up-next`) apply these
/// to their primary line and derive the secondary line from the theme's muted
/// color, exactly as the hard-coded UI did.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Typography {
    pub font_size: Option<f32>,
    pub weight: Option<Weight>,
    pub color: Option<Rgba>,
    pub align: Option<TextAlign>,
    /// Use the theme's monospace family rather than the body family.
    pub mono: bool,
}

/// Properties every node accepts, container or leaf.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Style {
    pub width: Option<Sizing>,
    pub height: Option<Sizing>,
    pub flex: Option<f32>,
    pub padding: Option<Padding>,
    pub spacing: Option<f32>,
    pub main_align: Option<Align>,
    pub cross_align: Option<Align>,
    pub when: When,
    pub typography: Typography,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Column,
    Row,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    Cover,
    Contain,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Container {
    pub direction: Direction,
    /// `card`: draw the translucent rounded surface behind the children.
    pub surface: bool,
    pub children: Vec<Node>,
}

/// One widget from the catalog, with its own properties already defaulted.
#[derive(Debug, Clone, PartialEq)]
pub enum Widget {
    Container(Container),
    Spacer,
    Clock {
        format: String,
    },
    Date {
        format: String,
    },
    NowPlaying {
        heading: String,
        cover_size: f32,
        show_artist: bool,
    },
    UpNext {
        heading: String,
        count: usize,
    },
    CoverArt {
        size: f32,
    },
    WifiQr {
        heading: String,
        size: f32,
    },
    Text {
        content: String,
    },
    Image {
        /// Path relative to the layout file's directory. Validated at parse
        /// time to stay inside it.
        path: String,
        fit: Fit,
    },
    RoleLabel,
    SettingsButton {
        size: f32,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub style: Style,
    pub widget: Widget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GradientStop {
    pub color: Rgba,
    /// Percentage along the gradient, 0–100.
    pub position: i16,
}

/// CSS-style direction names. The file speaks CSS because that is what an
/// author will have seen before; [`crate::layout::render`] converts to Freya's
/// angle convention, which runs the other way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GradientDirection {
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Background {
    Solid(Rgba),
    Gradient {
        direction: GradientDirection,
        stops: Vec<GradientStop>,
    },
}

/// Palette and fonts. Every field has a default matching the hard-coded UI, so
/// a `theme` node only has to state what it changes.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub background: Background,
    pub surface: Rgba,
    pub surface_border: Rgba,
    pub placeholder: Rgba,
    pub text: Rgba,
    pub muted_text: Rgba,
    pub font_family: Option<String>,
    pub mono_font_family: Option<String>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: Background::Gradient {
                direction: GradientDirection::Bottom,
                stops: vec![
                    GradientStop {
                        color: Rgba::opaque(10, 16, 51),
                        position: 0,
                    },
                    GradientStop {
                        color: Rgba::opaque(59, 29, 110),
                        position: 100,
                    },
                ],
            },
            surface: Rgba::new(255, 255, 255, 15),
            surface_border: Rgba::new(255, 255, 255, 31),
            placeholder: Rgba::new(255, 255, 255, 20),
            text: Rgba::opaque(255, 255, 255),
            muted_text: Rgba::new(255, 255, 255, 140),
            font_family: None,
            mono_font_family: Some("Adwaita Mono".to_string()),
        }
    }
}

/// A parsed, validated layout. `root` is always a container — an absent `root`
/// node means "theme only", and the caller supplies the default tree.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutDoc {
    pub theme: Theme,
    pub root: Node,
}

/// The implicit shape of `root`: a full-bleed column.
pub fn root_node(children: Vec<Node>) -> Node {
    Node {
        style: Style {
            width: Some(Sizing::Fill),
            height: Some(Sizing::Fill),
            ..Style::default()
        },
        widget: Widget::Container(Container {
            direction: Direction::Column,
            surface: false,
            children,
        }),
    }
}
