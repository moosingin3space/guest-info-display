//! [`super::schema`] into Freya elements.
//!
//! A pure function of `(document, model, clock)` — no state of its own, so a
//! hot reload is nothing more than a new tree in the model and a re-render.
//!
//! Two conventions worth knowing before reading:
//!
//! - `flex` resolves along the *parent's* main axis, so the parent's direction
//!   is threaded through the recursion. In a row it sets width, in a column
//!   height, which is what an author means by "take the remaining space".
//! - Each widget supplies its own defaults for the shared box and text
//!   properties, and anything the document states wins. Values are merged
//!   before the element is built rather than set twice on the builder.

use std::path::PathBuf;

use chrono::{DateTime, Local};
use freya::prelude::*;

use super::schema::{
    Align, Background, Container, Direction, Fit, GradientDirection, LayoutDoc, Node, Padding,
    Rgba, Sizing, Style, TextAlign as SchemaAlign, Theme, Video, Weight, When, Widget,
};
use crate::Model;
use crate::persistence::Role;
use crate::qr_code;
use crate::video;

static SETTINGS_ICON: &[u8] = include_bytes!("../../assets/settings.svg");

struct Ctx<'a> {
    model: &'a Model,
    theme: &'a Theme,
    now: DateTime<Local>,
    settings_open: State<bool>,
    /// A reflection that has lost its primary. Mirrored-data widgets render
    /// their unavailable state; local ones carry on as normal, which is the
    /// honest picture — the clock and the Wi-Fi code are still right.
    disconnected: bool,
}

/// Render a whole document. Returns the root rect so the caller can hang
/// chrome — popups, banners — off it; chrome is never part of the layout tree.
pub fn render(
    doc: &LayoutDoc,
    model: &Model,
    now: DateTime<Local>,
    settings_open: State<bool>,
) -> Rect {
    let ctx = Ctx {
        model,
        theme: &doc.theme,
        now,
        settings_open,
        disconnected: matches!(model.role, Role::Reflection) && !model.connected,
    };

    let root = apply_box(
        with_background(rect().color(rgba(ctx.theme.text)), &ctx.theme.background),
        &doc.root.style,
        Direction::Column,
        BoxDefaults {
            width: Some(Sizing::Fill),
            height: Some(Sizing::Fill),
            ..BoxDefaults::default()
        },
    );

    let (direction, children) = unwrap_container(&doc.root);
    let root = container_children(root, direction, children, &ctx);

    // A layout that omits `settings-button` would otherwise lock a host out of
    // their own display — no way back to the Wi-Fi form, the role switch or
    // pairing. `Ctrl+,` is the other way in, but many of these screens have no
    // keyboard, so drop a faded gear in the corner. Globally positioned, so it
    // sits over the layout rather than disturbing it.
    if has_settings_button(&doc.root, &ctx) {
        return root;
    }
    root.child(
        rect()
            .position(Position::new_global().bottom(16.).right(16.))
            .opacity(0.5)
            .child(settings_button(24., &ctx)),
    )
}

/// Whether a settings button will actually be drawn — `when` conditions
/// included, since a gear inside a branch that is currently hidden is no use
/// to the person standing in front of the screen.
fn has_settings_button(n: &Node, ctx: &Ctx<'_>) -> bool {
    if !visible(n.style.when, ctx) {
        return false;
    }
    match &n.widget {
        Widget::SettingsButton { .. } => true,
        Widget::Container(c) => c
            .children
            .iter()
            .any(|child| has_settings_button(child, ctx)),
        _ => false,
    }
}

/// A container node's direction and children. Only containers are ever passed
/// here — `root` always is one, and [`node`] dispatches leaves elsewhere.
fn unwrap_container(n: &Node) -> (Direction, &[Node]) {
    match &n.widget {
        Widget::Container(c) => (c.direction, c.children.as_slice()),
        _ => (Direction::Column, &[]),
    }
}

// ---------------------------------------------------------------------- tree

/// One node. `None` means its `when` condition excluded it, along with its
/// whole subtree.
fn node(n: &Node, parent: Direction, ctx: &Ctx<'_>) -> Option<Element> {
    if !visible(n.style.when, ctx) {
        return None;
    }

    let element = match &n.widget {
        Widget::Container(c) => container(c, &n.style, parent, ctx).into(),
        Widget::Spacer => apply_box(rect(), &n.style, parent, BoxDefaults::default()).into(),
        Widget::Clock { format } => clock(format, &n.style, parent, ctx).into(),
        Widget::Date { format } => date(format, &n.style, parent, ctx).into(),
        Widget::NowPlaying {
            heading,
            cover_size,
            show_artist,
        } => now_playing(heading, *cover_size, *show_artist, &n.style, parent, ctx).into(),
        Widget::UpNext { heading, count } => up_next(heading, *count, &n.style, parent, ctx).into(),
        Widget::NowPlayingStrip {
            direction,
            cover_size,
            show_next,
            next_label,
        } => now_playing_strip(
            *direction,
            *cover_size,
            *show_next,
            next_label,
            &n.style,
            parent,
            ctx,
        )
        .into(),
        Widget::CoverArt { size } => cover_art(*size, &n.style, parent, ctx).into(),
        Widget::WifiQr { heading, size } => wifi_qr(heading, *size, &n.style, parent, ctx).into(),
        Widget::Qr {
            data,
            heading,
            caption,
            size,
        } => qr(
            data,
            heading.as_deref(),
            caption.as_deref(),
            *size,
            &n.style,
            parent,
            ctx,
        )
        .into(),
        Widget::Text { content } => {
            styled_label(&n.style, ctx, 20.0, Weight::Normal, ctx.theme.text)
                .text(content.clone())
                .into()
        }
        Widget::Image { path, fit } => image(path, *fit, &n.style, parent, ctx).into(),
        Widget::Video(v) => video_node(v, &n.style, parent, ctx).into(),
        Widget::RoleLabel => role_label(&n.style, parent, ctx).into(),
        Widget::SettingsButton { size } => settings_button(*size, ctx).into(),
    };

    Some(element)
}

fn visible(when: When, ctx: &Ctx<'_>) -> bool {
    let m = ctx.model;
    match when {
        When::Always => true,
        When::Playing => m.is_playing,
        When::Idle => !m.is_playing,
        When::Connected => !ctx.disconnected,
        When::Disconnected => ctx.disconnected,
        When::WifiConfigured => m.wifi_creds.is_some(),
        When::WifiUnconfigured => m.wifi_creds.is_none(),
    }
}

fn container(c: &Container, style: &Style, parent: Direction, ctx: &Ctx<'_>) -> Rect {
    let mut r = rect();
    if c.direction == Direction::Row {
        r = r.horizontal();
    }
    if c.surface {
        r = r
            .corner_radius(16.)
            .background(rgba(ctx.theme.surface))
            .border(Border::new().fill(rgba(ctx.theme.surface_border)).width(1.));
    }
    let r = apply_box(r, style, parent, BoxDefaults::default());
    container_children(r, c.direction, &c.children, ctx)
}

fn container_children(mut r: Rect, direction: Direction, children: &[Node], ctx: &Ctx<'_>) -> Rect {
    let elements: Vec<Element> = children
        .iter()
        .filter_map(|child| node(child, direction, ctx))
        .collect();

    // Freya only distributes leftover space when the container opts into flex
    // content, so mirror the document: if any child asked for a share, its
    // parent has to be a flex container.
    if children.iter().any(|c| c.style.flex.is_some()) {
        r = r.content(Content::flex());
    }
    r.children(elements)
}

// ---------------------------------------------------------------------- leaves

fn clock(format: &str, style: &Style, _parent: Direction, ctx: &Ctx<'_>) -> Label {
    styled_label(style, ctx, 30.0, Weight::Normal, ctx.theme.text)
        .text(ctx.now.format(format).to_string())
}

fn date(format: &str, style: &Style, _parent: Direction, ctx: &Ctx<'_>) -> Label {
    styled_label(style, ctx, 24.0, Weight::Normal, ctx.theme.text)
        .text(ctx.now.format(format).to_string())
}

fn now_playing(
    heading: &str,
    cover_size: f32,
    show_artist: bool,
    style: &Style,
    parent: Direction,
    ctx: &Ctx<'_>,
) -> Rect {
    let outer = apply_box(
        rect().overflow(Overflow::Clip),
        style,
        parent,
        BoxDefaults {
            spacing: Some(16.),
            ..BoxDefaults::default()
        },
    );

    if ctx.disconnected {
        return outer.center().child(unavailable(ctx));
    }

    let (name, artist) = match &ctx.model.current_track {
        Some(t) => (t.name.clone(), t.artists.clone()),
        None => ("Nothing playing".to_string(), String::new()),
    };

    let mut details = rect().width(Size::flex(1.)).spacing(8.).child(
        styled_label(style, ctx, 24.0, Weight::Bold, ctx.theme.text)
            .width(Size::fill())
            .max_lines(1)
            .text_overflow(TextOverflow::Ellipsis)
            .text(name),
    );
    if show_artist {
        details = details.child(
            label()
                .width(Size::fill())
                .max_lines(1)
                .text_overflow(TextOverflow::Ellipsis)
                .font_size(20.)
                .color(rgba(ctx.theme.muted_text))
                .text(artist),
        );
    }

    outer
        .child(
            label()
                .color(rgba(ctx.theme.muted_text))
                .font_size(30.)
                .font_weight(FontWeight::SEMI_BOLD)
                .text(heading.to_string()),
        )
        .child(
            rect()
                .horizontal()
                .width(Size::fill())
                .content(Content::flex())
                .overflow(Overflow::Clip)
                .spacing(20.)
                .cross_align(Alignment::Center)
                .child(cover_box(cover_size, ctx))
                .child(details),
        )
}

/// The compact track line. In a row the current track takes the space the
/// next one leaves; in a column everything stacks, centred, for a sidebar.
fn now_playing_strip(
    direction: Direction,
    cover_size: f32,
    show_next: bool,
    next_label: &str,
    style: &Style,
    parent: Direction,
    ctx: &Ctx<'_>,
) -> Rect {
    let row = direction == Direction::Row;
    let mut outer = rect().overflow(Overflow::Clip);
    if row {
        outer = outer.horizontal().content(Content::flex());
    }
    let outer = apply_box(
        outer,
        style,
        parent,
        BoxDefaults {
            spacing: Some(16.),
            cross_align: Some(Align::Center),
            ..BoxDefaults::default()
        },
    );

    if ctx.disconnected {
        return outer.child(unavailable(ctx));
    }

    // A column has room for neither shrinking nor wrapping, so it centres.
    let align = if row {
        TextAlign::Start
    } else {
        TextAlign::Center
    };
    // Along a row each block takes a share; down a column, the full width.
    let block_width = if row { Size::flex(1.) } else { Size::fill() };
    let line = |l: Label| {
        l.width(Size::fill())
            .max_lines(1)
            .text_overflow(TextOverflow::Ellipsis)
            .text_align(align)
    };

    let (name, artist) = match &ctx.model.current_track {
        Some(t) => (t.name.clone(), t.artists.clone()),
        None => ("Nothing playing".to_string(), String::new()),
    };
    let current = rect()
        .width(block_width.clone())
        .spacing(4.)
        .child(line(styled_label(style, ctx, 18.0, Weight::Bold, ctx.theme.text)).text(name))
        .child(line(label().font_size(15.).color(rgba(ctx.theme.muted_text))).text(artist));

    let mut outer = outer.child(cover_box(cover_size, ctx)).child(current);

    let next = ctx.model.queue.first().filter(|t| t.is_resolved());
    if show_next && let Some(next) = next {
        outer = outer.child(
            rect()
                .width(block_width)
                .spacing(4.)
                .child(
                    line(
                        label()
                            .font_size(13.)
                            .font_weight(FontWeight::SEMI_BOLD)
                            .color(rgba(ctx.theme.muted_text)),
                    )
                    .text(next_label.to_string()),
                )
                .child(
                    line(label().font_size(15.).color(rgba(ctx.theme.muted_text)))
                        .text(format!("{} — {}", next.name, next.artists)),
                ),
        );
    }
    outer
}

fn cover_art(size: f32, style: &Style, parent: Direction, ctx: &Ctx<'_>) -> Rect {
    let outer = apply_box(rect(), style, parent, BoxDefaults::default());
    if ctx.disconnected {
        return outer.center().child(unavailable(ctx));
    }
    outer.child(cover_box(size, ctx))
}

/// The square artwork slot, or its placeholder.
fn cover_box(size: f32, ctx: &Ctx<'_>) -> Rect {
    let cover = ctx
        .model
        .current_track
        .as_ref()
        .and_then(|t| t.cover_url.as_ref())
        .and_then(|url| {
            ctx.model
                .covers
                .get(url)
                .map(|bytes| (url.clone(), bytes.clone()))
        });

    let boxed = rect()
        .width(Size::px(size))
        .height(Size::px(size))
        .corner_radius(12.)
        .overflow(Overflow::Clip);

    match cover {
        Some(source) => boxed.child(
            ImageViewer::new(source)
                .expanded()
                .aspect_ratio(AspectRatio::Max)
                .image_cover(ImageCover::Center),
        ),
        // Below this the words wrap into a jumble, so a small slot gets a note.
        None if size < 80. => boxed
            .center()
            .background(rgba(ctx.theme.placeholder))
            .child(
                label()
                    .color(rgba(ctx.theme.muted_text))
                    .font_size(size * 0.45)
                    .text("♪"),
            ),
        None => boxed
            .center()
            .background(rgba(ctx.theme.placeholder))
            .child(label().color(rgba(ctx.theme.muted_text)).text("Cover Art")),
    }
}

fn up_next(heading: &str, count: usize, style: &Style, parent: Direction, ctx: &Ctx<'_>) -> Rect {
    let outer = apply_box(
        rect().overflow(Overflow::Clip),
        style,
        parent,
        BoxDefaults {
            spacing: Some(16.),
            ..BoxDefaults::default()
        },
    );

    let queue = &ctx.model.queue;
    let items: Element = if ctx.disconnected || queue.is_empty() || count == 0 {
        label()
            .font_size(14.)
            .color(rgba(ctx.theme.muted_text))
            .text("—")
            .into()
    } else {
        rect()
            .width(Size::fill())
            .spacing(12.)
            .children(queue.iter().take(count).map(|t| {
                if t.is_resolved() {
                    queue_item(t.name.clone(), t.artists.clone(), ctx)
                } else {
                    queue_item("--".to_string(), String::new(), ctx)
                }
                .into()
            }))
            .into()
    };

    outer
        .child(
            label()
                .color(rgba(ctx.theme.muted_text))
                .font_size(14.)
                .font_weight(FontWeight::SEMI_BOLD)
                .text(heading.to_string()),
        )
        .child(items)
}

fn queue_item(title: String, artist: String, ctx: &Ctx<'_>) -> Rect {
    rect()
        .width(Size::fill())
        .spacing(2.)
        .child(
            label()
                .width(Size::fill())
                .max_lines(1)
                .text_overflow(TextOverflow::Ellipsis)
                .text(title),
        )
        .child(
            label()
                .width(Size::fill())
                .max_lines(1)
                .text_overflow(TextOverflow::Ellipsis)
                .font_size(14.)
                .color(rgba(ctx.theme.muted_text))
                .text(artist),
        )
}

fn wifi_qr(heading: &str, size: f32, style: &Style, parent: Direction, ctx: &Ctx<'_>) -> Rect {
    let outer = apply_box(
        rect(),
        style,
        parent,
        BoxDefaults {
            spacing: Some(16.),
            cross_align: Some(Align::Center),
            ..BoxDefaults::default()
        },
    );

    let boxed = rect()
        .width(Size::px(size))
        .height(Size::px(size))
        .corner_radius(12.)
        .overflow(Overflow::Clip);

    // A reflection's credentials are its primary's, mirrored like the layout.
    let boxed = match &ctx.model.wifi_creds {
        Some(creds) => boxed.child(qr_code::wifi_qr_element(creds)),
        None => boxed
            .center()
            .background(rgba(ctx.theme.placeholder))
            .child(
                label()
                    .color(rgba(ctx.theme.muted_text))
                    .text("Not configured"),
            ),
    };

    outer
        .child(
            styled_label(style, ctx, 20.0, Weight::SemiBold, ctx.theme.text)
                .text(heading.to_string()),
        )
        .child(boxed)
}

/// A `qr` node: a code for any text, with the same box as `wifi-qr` and
/// optional words above and below.
fn qr(
    data: &str,
    heading: Option<&str>,
    caption: Option<&str>,
    size: f32,
    style: &Style,
    parent: Direction,
    ctx: &Ctx<'_>,
) -> Rect {
    let outer = apply_box(
        rect(),
        style,
        parent,
        BoxDefaults {
            spacing: Some(16.),
            cross_align: Some(Align::Center),
            ..BoxDefaults::default()
        },
    );

    let boxed = rect()
        .width(Size::px(size))
        .height(Size::px(size))
        .corner_radius(12.)
        .overflow(Overflow::Clip)
        .child(qr_code::qr_element(data));

    outer
        .maybe_child(heading.map(|heading| {
            styled_label(style, ctx, 20.0, Weight::SemiBold, ctx.theme.text)
                .text(heading.to_string())
        }))
        .child(boxed)
        .maybe_child(caption.map(|caption| {
            label()
                .color(rgba(ctx.theme.muted_text))
                .font_size(14.)
                .text(caption.to_string())
        }))
}

/// An `image` node: the host's own picture, or a box naming the file that is
/// not there.
///
/// The placeholder is deliberately not an error state. On a reflection it is
/// the normal first few seconds after a cold boot — the layout is persisted
/// but the bytes are not, so they arrive on the next subscribe — and on a
/// primary it is a filename the host can fix from settings.
fn image(path: &str, fit: Fit, style: &Style, parent: Direction, ctx: &Ctx<'_>) -> Rect {
    // Clip matters for `cover`: the image is scaled up until it fills the box,
    // and the overflow has to go somewhere.
    let base = rect().corner_radius(12.).overflow(Overflow::Clip);

    let Some(asset) = ctx.model.assets.get(path) else {
        return apply_box(
            base.background(rgba(ctx.theme.placeholder)),
            style,
            parent,
            BoxDefaults {
                main_align: Some(Align::Center),
                cross_align: Some(Align::Center),
                ..BoxDefaults::default()
            },
        )
        .child(
            label()
                .color(rgba(ctx.theme.muted_text))
                .text(path.to_string()),
        );
    };

    // `Max` scales until the box is covered and crops; `Min` scales until the
    // whole image fits. Centring only bites under `Max`, where there is
    // overflow to distribute, but setting it unconditionally is harmless and
    // saves a branch.
    let aspect = match fit {
        Fit::Cover => AspectRatio::Max,
        Fit::Contain => AspectRatio::Min,
    };

    // The key carries the content fingerprint, not just the path: Freya hashes
    // only what we hand it, so a replaced file at an unchanged path would
    // otherwise keep rendering the image it decoded the first time.
    apply_box(base, style, parent, BoxDefaults::default()).child(
        ImageViewer::new(((path, asset.fingerprint), asset.bytes.clone()))
            .expanded()
            .aspect_ratio(aspect)
            .image_cover(ImageCover::Center),
    )
}

/// A `video` node: its playlist playing, or a box naming the file it waits on.
///
/// Clips that are missing are passed over rather than failing the playlist,
/// so one forgotten file costs a host that clip, not the screen, and joins the
/// rotation once it turns up. The box only shows a name when *none* of the
/// clips are here.
fn video_node(v: &Video, style: &Style, parent: Direction, ctx: &Ctx<'_>) -> Rect {
    let base = rect().corner_radius(12.).overflow(Overflow::Clip);
    let files: Vec<Option<PathBuf>> = v
        .items
        .iter()
        .map(|path| ctx.model.assets.video(path).map(|p| p.to_path_buf()))
        .collect();

    if files.iter().all(Option::is_none) {
        let placeholder = apply_box(
            base.background(rgba(ctx.theme.placeholder)),
            style,
            parent,
            BoxDefaults {
                main_align: Some(Align::Center),
                cross_align: Some(Align::Center),
                spacing: Some(12.),
                ..BoxDefaults::default()
            },
        );
        // On a primary a missing clip is the host's to fix, so name it, as an
        // image does. On a reflection it is simply still on its way.
        if !matches!(ctx.model.role, Role::Reflection) {
            return placeholder.child(
                label()
                    .color(rgba(ctx.theme.muted_text))
                    .text(v.items[0].clone()),
            );
        }
        let receiving = v.items.iter().find_map(|p| ctx.model.assets.progress(p));
        return match receiving {
            Some(done) => placeholder
                .child(
                    label()
                        .color(rgba(ctx.theme.muted_text))
                        .text(format!("Receiving video… {:.0}%", done * 100.0)),
                )
                .child(
                    rect()
                        .width(Size::px(240.))
                        .height(Size::px(6.))
                        .corner_radius(3.)
                        .background(rgba(ctx.theme.surface_border))
                        .child(
                            rect()
                                .width(Size::percent(done * 100.))
                                .height(Size::fill())
                                .corner_radius(3.)
                                .background(rgba(ctx.theme.muted_text)),
                        ),
                ),
            None => placeholder.child(
                label()
                    .color(rgba(ctx.theme.muted_text))
                    .text("Waiting for video from the primary…"),
            ),
        };
    }

    // Black behind the frame, so `contain` letterboxes like a screen would.
    apply_box(
        base.background((0, 0, 0)),
        style,
        parent,
        BoxDefaults::default(),
    )
    .child(video::VideoView {
        node: v.clone(),
        files,
        inhibitor: ctx.model.inhibitor.clone(),
        muted_text: rgba(ctx.theme.muted_text),
        reflection: matches!(ctx.model.role, Role::Reflection),
    })
}

fn role_label(style: &Style, parent: Direction, ctx: &Ctx<'_>) -> Rect {
    let text = format!(
        "{} · {}",
        match ctx.model.role {
            Role::Primary => "Primary",
            Role::Reflection => "Reflection",
        },
        ctx.model.endpoint_id.fmt_short(),
    );

    // Wrapped so the 16px it used to carry as a bottom margin survives; the
    // schema has padding but no margin, and on a full-width centered label
    // the two are indistinguishable.
    apply_box(
        rect(),
        style,
        parent,
        BoxDefaults {
            width: Some(Sizing::Fill),
            padding: Some(Padding {
                top: 0.,
                right: 0.,
                bottom: 16.,
                left: 0.,
            }),
            ..BoxDefaults::default()
        },
    )
    .child(
        styled_label(style, ctx, 14.0, Weight::Normal, ctx.theme.muted_text)
            .width(Size::fill())
            .text_align(TextAlign::Center)
            .text(text),
    )
}

fn settings_button(size: f32, ctx: &Ctx<'_>) -> TooltipContainer {
    let settings_open = ctx.settings_open;
    TooltipContainer::new(Tooltip::new("Settings")).child(
        Button::new()
            .flat()
            .on_press(move |_| {
                let mut settings_open = settings_open;
                settings_open.set(true);
            })
            .child(
                // Without an explicit color the viewer waits to inherit one
                // before rasterizing, and the flat button draws nothing.
                SvgViewer::new(("settings-icon", SETTINGS_ICON))
                    .color(Color::WHITE)
                    .width(Size::px(size))
                    .height(Size::px(size)),
            ),
    )
}

fn unavailable(ctx: &Ctx<'_>) -> Label {
    label()
        .color(rgba(ctx.theme.muted_text))
        .font_size(24.)
        .font_weight(FontWeight::SEMI_BOLD)
        .text("Primary unavailable")
}

// ----------------------------------------------------------------- style glue

#[derive(Default, Clone, Copy)]
struct BoxDefaults {
    width: Option<Sizing>,
    height: Option<Sizing>,
    spacing: Option<f32>,
    padding: Option<Padding>,
    main_align: Option<Align>,
    cross_align: Option<Align>,
}

fn apply_box(mut r: Rect, style: &Style, parent: Direction, d: BoxDefaults) -> Rect {
    let mut width = style.width.or(d.width);
    let mut height = style.height.or(d.height);

    // A flex share is claimed along the parent's main axis, and beats an
    // explicit size on that axis.
    if let Some(f) = style.flex {
        match parent {
            Direction::Row => {
                r = r.width(Size::flex(f));
                width = None;
            }
            Direction::Column => {
                r = r.height(Size::flex(f));
                height = None;
            }
        }
    }

    if let Some(w) = width {
        r = r.width(size(w));
    }
    if let Some(h) = height {
        r = r.height(size(h));
    }
    if let Some(p) = style.padding.or(d.padding) {
        r = r.padding((p.top, p.right, p.bottom, p.left));
    }
    if let Some(s) = style.spacing.or(d.spacing) {
        r = r.spacing(s);
    }
    if let Some(a) = style.main_align.or(d.main_align) {
        r = r.main_align(alignment(a));
    }
    if let Some(a) = style.cross_align.or(d.cross_align) {
        r = r.cross_align(alignment(a));
    }
    r
}

fn styled_label(
    style: &Style,
    ctx: &Ctx<'_>,
    default_size: f32,
    default_weight: Weight,
    default_color: Rgba,
) -> Label {
    let t = &style.typography;
    let mut l = label()
        .font_size(t.font_size.unwrap_or(default_size))
        .font_weight(font_weight(t.weight.unwrap_or(default_weight)))
        .color(rgba(t.color.unwrap_or(default_color)));

    let family = if t.mono {
        ctx.theme.mono_font_family.clone()
    } else {
        ctx.theme.font_family.clone()
    };
    if let Some(family) = family {
        l = l.font_family(family);
    }
    if let Some(align) = t.align {
        l = l.text_align(text_align(align));
    }
    l
}

fn size(s: Sizing) -> Size {
    match s {
        Sizing::Auto => Size::auto(),
        Sizing::Fill => Size::fill(),
        Sizing::Px(v) => Size::px(v),
    }
}

fn alignment(a: Align) -> Alignment {
    match a {
        Align::Start => Alignment::Start,
        Align::Center => Alignment::Center,
        Align::End => Alignment::End,
        Align::SpaceBetween => Alignment::SpaceBetween,
        Align::SpaceAround => Alignment::SpaceAround,
    }
}

fn font_weight(w: Weight) -> FontWeight {
    match w {
        Weight::Normal => FontWeight::NORMAL,
        Weight::SemiBold => FontWeight::SEMI_BOLD,
        Weight::Bold => FontWeight::BOLD,
    }
}

fn text_align(a: SchemaAlign) -> TextAlign {
    match a {
        SchemaAlign::Start => TextAlign::Start,
        SchemaAlign::Center => TextAlign::Center,
        SchemaAlign::End => TextAlign::End,
    }
}

fn rgba(c: Rgba) -> (u8, u8, u8, u8) {
    (c.r, c.g, c.b, c.a)
}

fn with_background(r: Rect, b: &Background) -> Rect {
    match b {
        Background::Solid(c) => r.background(rgba(*c)),
        Background::Gradient { direction, stops } => {
            let mut g = LinearGradient::new().angle(gradient_angle(*direction));
            for stop in stops {
                g = g.stop((rgba(stop.color), stop.position as f32));
            }
            r.background(g)
        }
    }
}

/// The document speaks CSS directions because that is what an author will have
/// seen before. Freya's angles are CSS's turned half way round: its 0° is
/// top-to-bottom, where CSS calls that 180deg, and its 90° runs to the left.
fn gradient_angle(d: GradientDirection) -> f32 {
    match d {
        GradientDirection::Bottom => 0.,
        GradientDirection::BottomLeft => 45.,
        GradientDirection::Left => 90.,
        GradientDirection::TopLeft => 135.,
        GradientDirection::Top => 180.,
        GradientDirection::TopRight => 225.,
        GradientDirection::Right => 270.,
        GradientDirection::BottomRight => 315.,
    }
}
