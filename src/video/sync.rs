//! Keeping a reflection's `video` nodes on the primary's clip and frame.
//!
//! The primary is in charge: it runs each playlist and announces, as a
//! [`Cue`], which clip a node is playing and the instant — on the primary's
//! clock, see [`super::clock`] — that clip's first frame was on screen. It cues
//! again once it knows the clip after, so a reflection can queue that clip for
//! a gapless cut of its own.
//!
//! A reflection's players poll [`latest`] and line themselves up with each new
//! cue. A cue for a clip a reflection does not have yet is passed over: it
//! keeps playing what it has and locks on at the first cue it can follow.
//!
//! Plain globals rather than state threaded through the UI: cues arrive on
//! the network thread and are read on GStreamer's, and neither has any business
//! with the component tree.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cue {
    /// Which `video` node, by its document-order id.
    pub node: u32,
    /// The layout path of the clip playing.
    pub current: String,
    /// Primary-clock time, in nanoseconds, at which position 0 of `current`
    /// was (or will be) on screen.
    pub pos0: u64,
    /// The clip queued after `current`, once the primary has picked it.
    pub next: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Plays on its own; nobody to follow or lead. Before the role is known.
    Solo,
    /// The primary: publishes cues.
    Lead,
    /// A reflection: follows cues.
    Follow,
}

struct Registry {
    mode: Mode,
    /// Newest cue per node, with a counter that moves on every arrival so a
    /// player can tell a re-sent cue from one it has already acted on.
    cues: HashMap<u32, (u64, Cue)>,
    generation: u64,
    outbox: Option<async_channel::Sender<Cue>>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Mutex::new(Registry {
            mode: Mode::Solo,
            cues: HashMap::new(),
            generation: 0,
            outbox: None,
        })
    })
}

pub fn mode() -> Mode {
    registry().lock().unwrap().mode
}

/// Switch roles. Leading resets the clock to this device's own; following
/// forgets it until the primary has been measured.
pub fn set_mode(mode: Mode) {
    let mut registry = registry().lock().unwrap();
    if registry.mode == mode {
        return;
    }
    registry.mode = mode;
    registry.cues.clear();
    drop(registry);
    match mode {
        Mode::Lead => super::clock::lead(),
        Mode::Follow => super::clock::follow(),
        Mode::Solo => {}
    }
}

/// Where a primary's cues go: the UI forwards them to the reflections.
pub fn set_outbox(outbox: async_channel::Sender<Cue>) {
    registry().lock().unwrap().outbox = Some(outbox);
}

/// A primary's player announcing what it plays.
pub fn publish(cue: Cue) {
    let registry = registry().lock().unwrap();
    if registry.mode != Mode::Lead {
        return;
    }
    log::debug!(
        "video: cue node {} {} pos0 {:.3}s next {:?} (now {:.3}s)",
        cue.node,
        cue.current,
        cue.pos0 as f64 / 1e9,
        cue.next,
        super::clock::now() as f64 / 1e9
    );
    if let Some(outbox) = &registry.outbox {
        let _ = outbox.try_send(cue);
    }
}

/// A cue from the primary, for this reflection's players to pick up.
pub fn receive(cue: Cue) {
    log::debug!(
        "video: received cue node {} {} pos0 {:.3}s next {:?} (now {:.3}s)",
        cue.node,
        cue.current,
        cue.pos0 as f64 / 1e9,
        cue.next,
        super::clock::now() as f64 / 1e9
    );
    let mut registry = registry().lock().unwrap();
    registry.generation += 1;
    let generation = registry.generation;
    registry.cues.insert(cue.node, (generation, cue));
}

/// The newest cue for `node`, if any, with its arrival counter.
pub fn latest(node: u32) -> Option<(u64, Cue)> {
    registry().lock().unwrap().cues.get(&node).cloned()
}

/// A new document: its nodes are not the old document's nodes.
pub fn clear() {
    registry().lock().unwrap().cues.clear();
}
