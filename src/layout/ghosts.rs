//! `theme { ghosts }`: ghosts drifting across the background now and then.
//!
//! One canvas covers the root, behind everything else. It sits idle between
//! ghosts and repaints at [`FRAME`] only while one is crossing. Each screen
//! picks its own moments, so the ghosts on a reflection don't match the
//! primary's.

use std::cell::RefCell;
use std::f32::consts::TAU;
use std::rc::Rc;
use std::time::Duration;

use freya::engine::prelude::{
    Paint, PathBuilder, SkBlurStyle, SkColor, SkMaskFilter, SkPath, SkPoint, SkRect,
};
use freya::prelude::*;
use tokio::time::{Instant, sleep, sleep_until};

use super::schema::Ghosts;

const FRAME: Duration = Duration::from_millis(33);

/// One ghost's trip across the screen. Positions are fractions of the
/// canvas, so a resize mid-flight just rescales the path.
#[derive(Clone, Copy)]
struct Flight {
    start: Instant,
    duration: Duration,
    /// Left to right, or the other way.
    rightward: bool,
    /// Height of its path at the start, as a fraction of the canvas height.
    y: f32,
    /// How far the path climbs (negative) or sinks by the far side.
    drift: f32,
    /// Its height, as a fraction of the canvas height.
    size: f32,
    /// Up-and-down bobbing: how far, and how many times across the screen.
    bob: f32,
    bobs: f32,
    phase: f32,
}

impl Flight {
    fn random(start: Instant) -> Self {
        Self {
            start,
            duration: Duration::from_secs_f32(14. + fastrand::f32() * 10.),
            rightward: fastrand::bool(),
            y: 0.15 + fastrand::f32() * 0.7,
            drift: (fastrand::f32() - 0.5) * 0.3,
            size: 0.09 + fastrand::f32() * 0.06,
            bob: 0.02 + fastrand::f32() * 0.03,
            bobs: 2. + fastrand::f32() * 2.,
            phase: fastrand::f32() * TAU,
        }
    }

    /// A smaller ghost tagging along a little behind and beside this one.
    fn companion(&self) -> Self {
        Self {
            start: self.start + Duration::from_millis(900 + fastrand::u64(..900)),
            size: self.size * 0.65,
            y: self.y + if fastrand::bool() { 0.08 } else { -0.08 },
            phase: self.phase + 1.5,
            ..*self
        }
    }

    fn done(&self, now: Instant) -> bool {
        now >= self.start + self.duration
    }
}

#[derive(PartialEq)]
pub struct GhostLayer {
    pub ghosts: Ghosts,
}

impl Component for GhostLayer {
    fn render(&self) -> impl IntoElement {
        let flights = use_hook(|| Rc::new(RefCell::new(Vec::<Flight>::new())));
        // Read by the task on every launch, so a reload that changes `every`
        // takes effect from the next ghost on.
        let every = use_hook(|| Rc::new(RefCell::new(self.ghosts.every)));
        *every.borrow_mut() = self.ghosts.every;

        let mut frames = use_state(|| 0u64);
        use_hook({
            let flights = flights.clone();
            let every = every.clone();
            move || {
                spawn(async move {
                    // The first one comes sooner, so a fresh layout shows
                    // what it does without a long wait.
                    let mut next = Instant::now() + gap(*every.borrow()) / 2;
                    loop {
                        let idle = flights.borrow().is_empty();
                        match idle {
                            true => sleep_until(next).await,
                            false => sleep(FRAME).await,
                        }
                        let now = Instant::now();
                        let mut in_flight = flights.borrow_mut();
                        in_flight.retain(|f| !f.done(now));
                        if now >= next {
                            let flight = Flight::random(now);
                            if fastrand::u8(..4) == 0 {
                                in_flight.push(flight.companion());
                            }
                            in_flight.push(flight);
                            next = now + gap(*every.borrow());
                        }
                        drop(in_flight);
                        // Also once after the last one leaves, so the canvas
                        // is repainted empty.
                        *frames.write() += 1;
                    }
                });
            }
        });

        let ghosts = self.ghosts;
        rect()
            .position(Position::new_absolute().top(0.).left(0.))
            .expanded()
            .child(
                canvas(RenderCallback::new(move |ctx: &mut CanvasContext| {
                    let now = Instant::now();
                    for flight in flights.borrow().iter() {
                        paint(ctx, flight, now, &ghosts);
                    }
                }))
                // A canvas repaints only when it changes, and its callback
                // always compares equal, so a new key per frame forces it.
                .key(*frames.read())
                .expanded(),
            )
    }
}

/// From half to one and a half times `every`.
fn gap(every: f32) -> Duration {
    Duration::from_secs_f32(every * (0.5 + fastrand::f32()))
}

fn paint(ctx: &mut CanvasContext, f: &Flight, now: Instant, ghosts: &Ghosts) {
    let Some(elapsed) = now.checked_duration_since(f.start) else {
        // A companion that hasn't set off yet.
        return;
    };
    let t = (elapsed.as_secs_f32() / f.duration.as_secs_f32()).min(1.);
    let (w, h) = (ctx.size.width, ctx.size.height);
    let size = f.size * h;

    // Start and end just off screen, so ghosts drift in and out of view.
    let across = -size + t * (w + 2. * size);
    let x = if f.rightward { across } else { w - across };
    let wave = t * f.bobs * TAU + f.phase;
    let y = (f.y + f.drift * t) * h + wave.sin() * f.bob * h;
    // Lean into the bob: nose up while rising, down while sinking.
    let lean = -wave.cos() * 8.;

    let canvas = ctx.canvas;
    canvas.save_layer_alpha_f(None, ghosts.opacity);
    canvas.translate((x, y));
    canvas.rotate(if f.rightward { lean } else { -lean }, None);
    canvas.scale((if f.rightward { size } else { -size }, size));

    let c = ghosts.color;
    let body = body(elapsed.as_secs_f32() * 5. + f.phase);
    let mut paint = Paint::default();
    paint.set_anti_alias(true);

    // A soft glow, then the body itself.
    paint.set_color(SkColor::from_argb(110, c.r, c.g, c.b));
    paint.set_mask_filter(SkMaskFilter::blur(SkBlurStyle::Normal, 0.08, true));
    canvas.draw_path(&body, &paint);
    paint.set_mask_filter(None);
    paint.set_color(SkColor::from_argb(c.a, c.r, c.g, c.b));
    canvas.draw_path(&body, &paint);

    // Eyes and a little "oh", looking where it's going.
    paint.set_color(SkColor::from_rgb(10, 14, 36));
    canvas.draw_oval(SkRect::from_xywh(0.02, -0.2, 0.12, 0.17), &paint);
    canvas.draw_oval(SkRect::from_xywh(0.22, -0.2, 0.12, 0.17), &paint);
    canvas.draw_oval(SkRect::from_xywh(0.13, 0.04, 0.1, 0.13), &paint);

    canvas.restore();
}

/// The outline, about one unit tall around the origin, facing +x: a domed
/// head over a skirt that streams out behind and ripples as `ripple` turns.
fn body(ripple: f32) -> SkPath {
    // The usual cubic approximation of a quarter circle.
    const K: f32 = 0.276;
    let mut p = PathBuilder::new();
    p.move_to((-0.5, 0.))
        .cubic_to((-0.5, -K), (-K, -0.5), (0., -0.5))
        .cubic_to((K, -0.5), (0.5, -K), (0.5, 0.))
        // Rounding out over the chest, so the front isn't a flat edge.
        .cubic_to((0.5, 0.16), (0.56, 0.32), (0.46, 0.42));

    // Four scallops from the front of the hem to the tail, which trails a
    // bit higher, as if the ghost were pulling it along.
    const SCALLOPS: usize = 4;
    let (front, tail) = (0.46_f32, -0.75_f32);
    for i in 0..SCALLOPS {
        let a = i as f32 / SCALLOPS as f32;
        let b = (i + 1) as f32 / SCALLOPS as f32;
        let x = |s: f32| front + (tail - front) * s;
        let y = |s: f32| 0.42 - 0.1 * s + 0.04 * (ripple + s * 6.).sin();
        let mid = (a + b) / 2.;
        p.quad_to(
            SkPoint::new(x(mid), y(mid) + 0.14),
            SkPoint::new(x(b), y(b)),
        );
    }
    p.cubic_to((-0.62, 0.2), (-0.5, 0.15), (-0.5, 0.));
    p.close();
    p.detach()
}
