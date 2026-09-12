//! Noticing that `layout.kdl` changed.
//!
//! Polling rather than inotify, for two reasons. Editors write-and-rename, so a
//! watch on the file itself goes deaf after the first save and the host sees
//! their second edit silently ignored — the worst possible failure for a
//! feature whose whole promise is edit-and-see. And a 2s tick on an appliance
//! costs nothing. It also needs no new dependency.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use freya::prelude::State;

use super::layout_path;
use crate::Model;
use crate::persistence::Role;

/// Slow enough to be free, fast enough that a host tabbing back to the screen
/// after a save sees the change already applied.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Modification time and length. Together these catch every edit that matters;
/// a rewrite that preserves both to the nanosecond is not a case worth code.
type Stamp = (SystemTime, u64);

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

pub enum Change {
    /// New contents, read cleanly.
    Updated(String),
    /// The file went away; fall back to the built-in layout.
    Removed,
    /// It changed but could not be read — a permission change, or a torn read
    /// of something that is not valid UTF-8.
    Unreadable(std::io::Error),
}

pub struct Watcher {
    path: PathBuf,
    seen: Option<Stamp>,
}

impl Watcher {
    /// Seeded from the file as it is now, so the first poll reports only what
    /// changed *after* startup read it.
    pub fn new(path: PathBuf) -> Self {
        let seen = stamp(&path);
        Self { path, seen }
    }

    pub fn poll(&mut self) -> Option<Change> {
        let current = stamp(&self.path);
        if current == self.seen {
            return None;
        }
        self.seen = current;

        match current {
            None => Some(Change::Removed),
            Some(_) => match std::fs::read_to_string(&self.path) {
                Ok(source) => Some(Change::Updated(source)),
                Err(e) => Some(Change::Unreadable(e)),
            },
        }
    }
}

/// Watch the layout file for the lifetime of the app, applying every change.
///
/// Reflections take their layout from their primary, so changes are ignored
/// while the role is reflection. The check is per tick rather than at startup
/// so a live role switch is picked up without restarting the task — and the
/// poll still runs, so a file edited while in reflection role is already
/// stamped and won't re-fire on the switch back.
pub async fn run(model: State<Model>) {
    let mut watcher = Watcher::new(layout_path());

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;

        let Some(change) = watcher.poll() else {
            continue;
        };

        if !matches!(model.peek().role, Role::Primary) {
            continue;
        }

        match change {
            Change::Updated(source) => {
                log::info!("layout: {} changed, reloading", watcher.path.display());
                crate::apply_layout_source(model, &source);
            }
            Change::Removed => {
                log::info!(
                    "layout: {} is gone, falling back to the built-in layout",
                    watcher.path.display()
                );
                crate::use_built_in_layout(model);
            }
            Change::Unreadable(e) => {
                // Keep whatever is on screen: an unreadable file is no reason to
                // blank a display in front of guests.
                log::warn!("layout: {} is unreadable: {e}", watcher.path.display());
            }
        }
    }
}
