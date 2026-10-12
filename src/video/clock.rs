//! The clock every screen's video pipelines run on: the primary's.
//!
//! A primary's pipelines use a plain monotonic clock. A reflection keeps a
//! clock of the same kind *calibrated* to read the primary's time: it measures
//! the offset Snapcast-style — send our time, get the primary's back, assume
//! the network delay was symmetric — and filters the noise by keeping the
//! offsets from the quickest round trips and taking their median.
//!
//! Small corrections recalibrate the clock in place. A large one — the first
//! sync after startup, or a primary that rebooted — would make a running
//! pipeline's clock leap by hours, so it starts a fresh clock instead; players
//! pick that up the next time they lock on, and keep the old one until then.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use gstreamer as gst;
use gstreamer::prelude::*;

/// Offsets this far from the current calibration get a fresh clock rather than
/// a jump in place.
const RESET_THRESHOLD_NS: i64 = 50_000_000;
/// Samples remembered; at one a second, the last minute or so.
const WINDOW: usize = 64;
/// Samples needed before the estimate is trusted at all.
pub const MIN_SAMPLES: usize = 8;

struct State {
    clock: gst::Clock,
    /// Offset the current clock is calibrated with; `None` while uncalibrated.
    offset: Option<i64>,
    samples: VecDeque<Sample>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub rtt: u64,
    pub offset: i64,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(State {
            clock: new_clock(),
            offset: None,
            samples: VecDeque::new(),
        })
    })
}

/// A private monotonic clock — never the process-wide system clock, whose
/// calibration other pipelines share.
fn new_clock() -> gst::Clock {
    gst::glib::Object::builder::<gst::SystemClock>()
        .property("clock-type", gst::ClockType::Monotonic)
        .build()
        .upcast()
}

/// The clock to run a synchronised pipeline on, as of now.
pub fn current() -> gst::Clock {
    state().lock().unwrap().clock.clone()
}

/// `current()`'s time now, in nanoseconds.
pub fn now() -> u64 {
    current().time().nseconds()
}

/// Whether `current()` reads the primary's time yet. Always true on a primary.
pub fn synced() -> bool {
    let state = state().lock().unwrap();
    state.offset.is_some()
}

/// This device's raw monotonic time, for timing a sync round trip.
pub fn local_now() -> u64 {
    state().lock().unwrap().clock.internal_time().nseconds()
}

/// Back to an uncalibrated clock: this device is the primary, so its own time
/// is the reference. Marked synced, since there is nothing to sync to.
pub fn lead() {
    let mut state = state().lock().unwrap();
    if state.offset == Some(0) && state.samples.is_empty() {
        return;
    }
    state.clock = new_clock();
    state.offset = Some(0);
    state.samples.clear();
}

/// Forget any calibration: this device follows a primary and has not measured
/// it yet.
pub fn follow() {
    let mut state = state().lock().unwrap();
    state.clock = new_clock();
    state.offset = None;
    state.samples.clear();
}

/// One round trip: sent at local `t0`, the primary read `remote`, the reply
/// landed at local `t2`.
pub fn add_sample(t0: u64, remote: u64, t2: u64) {
    let Some(rtt) = t2.checked_sub(t0) else {
        return;
    };
    let midpoint = t0 as i128 + rtt as i128 / 2;
    let offset = (remote as i128 - midpoint) as i64;

    let mut state = state().lock().unwrap();
    state.samples.push_back(Sample { rtt, offset });
    while state.samples.len() > WINDOW {
        state.samples.pop_front();
    }
    if state.samples.len() < MIN_SAMPLES {
        return;
    }
    let samples: Vec<Sample> = state.samples.iter().copied().collect();
    let Some(estimate) = estimate(&samples) else {
        return;
    };

    match state.offset {
        Some(current) if (estimate - current).abs() < RESET_THRESHOLD_NS => {}
        previous => {
            if previous.is_some() {
                log::info!(
                    "video: primary's clock moved by {:.1} ms, starting a fresh clock",
                    (estimate - previous.unwrap_or(0)) as f64 / 1e6
                );
            } else {
                log::info!("video: locked onto the primary's clock");
            }
            state.clock = new_clock();
        }
    }
    let internal = state.clock.internal_time();
    let external = gst::ClockTime::from_nseconds((internal.nseconds() as i64 + estimate) as u64);
    state.clock.set_calibration(internal, external, 1, 1);
    state.offset = Some(estimate);
}

/// The offset to trust: the median over the quickest quarter of round trips.
///
/// A slow round trip is slow because one direction queued somewhere, which
/// breaks the symmetric-delay assumption the offset rests on; the fastest ones
/// are the ones where both directions were close to the wire's own latency.
pub fn estimate(samples: &[Sample]) -> Option<i64> {
    if samples.is_empty() {
        return None;
    }
    let mut by_rtt = samples.to_vec();
    by_rtt.sort_by_key(|s| s.rtt);
    let keep = (by_rtt.len() / 4).max(1);
    let mut offsets: Vec<i64> = by_rtt[..keep].iter().map(|s| s.offset).collect();
    offsets.sort_unstable();
    Some(offsets[offsets.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(rtt: u64, offset: i64) -> Sample {
        Sample { rtt, offset }
    }

    #[test]
    fn slow_round_trips_do_not_skew_the_estimate() {
        // True offset 1 s; the slow samples are lopsided by queueing.
        let mut samples: Vec<Sample> = (0..6)
            .map(|i| s(800_000 + i, 1_000_000_000 + i as i64))
            .collect();
        samples.extend((0..18).map(|i| s(40_000_000 + i, 1_015_000_000 + i as i64 * 1000)));
        let estimate = estimate(&samples).expect("samples");
        assert!(
            (estimate - 1_000_000_000).abs() < 10,
            "estimate {estimate} should come from the fast round trips"
        );
    }

    #[test]
    fn the_median_ignores_a_single_outlier_among_fast_samples() {
        // 20 samples: the quickest quarter is the five below.
        let mut samples = vec![s(1, 100), s(1, 101), s(1, 99), s(1, 5_000_000), s(1, 100)];
        samples.extend((0..15).map(|_| s(1_000_000, 0)));
        assert_eq!(estimate(&samples), Some(100));
    }

    #[test]
    fn nothing_to_estimate_from_nothing() {
        assert_eq!(estimate(&[]), None);
    }
}
