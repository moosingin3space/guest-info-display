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
    /// Not started: the primary is holding `current` on its first frame until
    /// its reflections have it loaded too, and `pos0` means nothing yet.
    /// Reflections load the clip, pause on its first frame, and report back.
    pub pending: bool,
}

/// How far a reflection is from being able to start a pending clip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Readiness {
    /// The clip this is about; a report for another clip is stale.
    pub path: String,
    /// Loaded and paused on its first frame.
    pub ready: bool,
    /// Download progress, `0.0..=1.0`, while the file is still arriving.
    pub progress: Option<f32>,
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
    /// Primary: the reflections subscribed right now, by id.
    followers: Vec<String>,
    /// Primary: how many reflections are paired, connected or not.
    paired: usize,
    /// Primary: what each reflection last reported, per node.
    readiness: HashMap<(String, u32), Readiness>,
    /// Reflection: what this device would report, per node.
    local: HashMap<u32, Readiness>,
    /// Reflection: download progress per layout path.
    progress: HashMap<String, f32>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Mutex::new(Registry {
            mode: Mode::Solo,
            cues: HashMap::new(),
            generation: 0,
            outbox: None,
            followers: Vec::new(),
            paired: 0,
            readiness: HashMap::new(),
            local: HashMap::new(),
            progress: HashMap::new(),
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

/// Clips the primary needs this reflection to have soonest, by layout path:
/// what each node is playing (rank 0), then what each plays next (rank 1).
pub fn wanted_soon() -> Vec<(u8, String)> {
    let registry = registry().lock().unwrap();
    let mut out: Vec<(u8, String)> = Vec::new();
    for (_, cue) in registry.cues.values() {
        out.push((0, cue.current.clone()));
        if let Some(next) = &cue.next {
            out.push((1, next.clone()));
        }
    }
    out.sort();
    out
}

/// A new document: its nodes are not the old document's nodes.
pub fn clear() {
    let mut registry = registry().lock().unwrap();
    registry.cues.clear();
    registry.local.clear();
    registry.progress.clear();
}

// ------------------------------------------------- waiting for reflections

/// Primary: the reflections subscribed right now. One that is gone is not
/// waited for, and forgets what it reported.
pub fn set_followers(followers: Vec<String>) {
    log::debug!("video: subscribed reflections now {followers:?}");
    let mut registry = registry().lock().unwrap();
    registry
        .readiness
        .retain(|(follower, _), _| followers.contains(follower));
    registry.followers = followers;
}

/// Primary: how many reflections are paired, so a primary that has only just
/// started can give the ones not yet connected a moment to arrive.
pub fn set_paired(paired: usize) {
    registry().lock().unwrap().paired = paired;
}

/// Primary: paired reflections that are not subscribed.
pub fn absent() -> usize {
    let registry = registry().lock().unwrap();
    registry.paired.saturating_sub(registry.followers.len())
}

/// Primary: a reflection's report on a node.
pub fn set_readiness(follower: String, node: u32, readiness: Readiness) {
    registry()
        .lock()
        .unwrap()
        .readiness
        .insert((follower, node), readiness);
}

/// Primary: how many subscribed reflections are not yet ready to start
/// `path` on `node`, and the slowest download among them if any reported one.
pub fn waiting_on(node: u32, path: &str) -> (usize, Option<f32>) {
    let registry = registry().lock().unwrap();
    let mut waiting = 0;
    let mut slowest: Option<f32> = None;
    for follower in &registry.followers {
        match registry.readiness.get(&(follower.clone(), node)) {
            Some(r) if r.path == path && r.ready => {}
            Some(r) if r.path == path => {
                waiting += 1;
                if let Some(p) = r.progress {
                    slowest = Some(slowest.map_or(p, |s: f32| s.min(p)));
                }
            }
            _ => waiting += 1,
        }
    }
    (waiting, slowest)
}

/// Reflection: what this device would tell the primary about `node`.
pub fn report(node: u32, readiness: Readiness) {
    registry().lock().unwrap().local.insert(node, readiness);
}

/// Reflection: every report to send, for the session to pass on.
pub fn reports() -> Vec<(u32, Readiness)> {
    let registry = registry().lock().unwrap();
    registry
        .local
        .iter()
        .map(|(node, r)| (*node, r.clone()))
        .collect()
}

/// Reflection: how far the download behind `path` has got.
///
/// Also reported straight to any node holding `path` pending: until the file
/// lands there is no player for the node to report through, and the
/// primary's "Waiting for…" caption would have no number to show.
pub fn set_progress(path: &str, progress: f32) {
    let mut registry = registry().lock().unwrap();
    registry.progress.insert(path.to_string(), progress);
    let pending: Vec<u32> = registry
        .cues
        .values()
        .filter(|(_, cue)| cue.pending && cue.current == path)
        .map(|(_, cue)| cue.node)
        .collect();
    for node in pending {
        if registry.local.get(&node).is_some_and(|r| r.ready) {
            continue;
        }
        registry.local.insert(
            node,
            Readiness {
                path: path.to_string(),
                ready: false,
                progress: Some(progress),
            },
        );
    }
}

pub fn progress(path: &str) -> Option<f32> {
    registry().lock().unwrap().progress.get(path).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(path: &str) -> Readiness {
        Readiness {
            path: path.into(),
            ready: true,
            progress: None,
        }
    }

    fn loading(path: &str, progress: f32) -> Readiness {
        Readiness {
            path: path.into(),
            ready: false,
            progress: Some(progress),
        }
    }

    // The registry is process-wide, so one test walks the whole story rather
    // than several racing each other over it.
    #[test]
    fn the_primary_waits_only_for_subscribed_reflections_on_the_right_clip() {
        set_followers(Vec::new());
        assert_eq!(waiting_on(7, "a.mp4"), (0, None), "nobody to wait for");

        set_followers(vec!["den".into(), "hall".into()]);
        assert_eq!(waiting_on(7, "a.mp4"), (2, None), "no reports yet");

        set_readiness("den".into(), 7, loading("a.mp4", 0.4));
        set_readiness("hall".into(), 7, loading("a.mp4", 0.9));
        assert_eq!(
            waiting_on(7, "a.mp4"),
            (2, Some(0.4)),
            "the slowest download"
        );

        set_readiness("hall".into(), 7, ready("a.mp4"));
        assert_eq!(waiting_on(7, "a.mp4"), (1, Some(0.4)));

        // A report about another clip says nothing about this one.
        set_readiness("den".into(), 7, ready("b.mp4"));
        assert_eq!(waiting_on(7, "a.mp4"), (1, None));

        // A reflection that unsubscribes is no longer waited for.
        set_followers(vec!["hall".into()]);
        assert_eq!(waiting_on(7, "a.mp4"), (0, None));
        set_followers(Vec::new());
    }
}
