//! Getting a layout's video files from the primary onto its reflections.
//!
//! Images ride the subscription stream as bytes, but a night of clips runs to
//! gigabytes, so videos go over iroh-blobs instead: content-addressed,
//! verified as it streams, and resumable when a reflection drops off the
//! network half-way through a file.
//!
//! - The **primary** hashes each file in place ([`Library`]) — referenced,
//!   never copied — and serves the blobs to its paired reflections only
//!   ([`serve_to_trusted`]).
//! - A **reflection** keeps a cache of `<hash>.<ext>` files ([`Cache`]). Each
//!   layout prunes it to the hashes that layout names and downloads whatever
//!   is missing, handing each finished file to the UI.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_channel::Sender as AsyncSender;
use iroh::{Endpoint, EndpointId};
use iroh_blobs::api::TempTag;
use iroh_blobs::api::blobs::{AddPathOptions, ExportMode, ExportOptions, ImportMode};
use iroh_blobs::api::downloader::DownloadProgressItem;
use iroh_blobs::provider::events::{
    AbortReason, ConnectMode, EventMask, EventSender, ProviderMessage,
};
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::{BlobFormat, Hash};
use serde::{Deserialize, Serialize};

use super::{LayoutUpdate, TrustState};

/// One `video` file of a layout, as the wire names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoRef {
    /// The layout-relative path the `video` node names.
    pub path: String,
    pub hash: Hash,
    /// File size in bytes, so a reflection can say how far along it is.
    pub size: u64,
}

/// `$XDG_DATA_HOME/xyz.mooshq.GuestInfoDisplay/<name>`.
fn data_path(name: &str) -> PathBuf {
    crate::persistence::data_dir().join(name)
}

// ------------------------------------------------------------------ primary

/// The primary's blob store, plus a memo of what each file hashed to.
pub struct Library {
    store: FsStore,
    /// Keyed by file, invalidated by size or mtime. Hashing reads the whole
    /// file, and the layout is re-published on every edit.
    known: HashMap<PathBuf, Known>,
}

struct Known {
    len: u64,
    modified: Option<SystemTime>,
    hash: Hash,
    /// Keeps the blob from being collected while this layout names it.
    _tag: TempTag,
}

impl Library {
    pub async fn open() -> Option<Self> {
        let dir = data_path("blobs");
        match FsStore::load(&dir).await {
            Ok(store) => Some(Self {
                store,
                known: HashMap::new(),
            }),
            Err(e) => {
                log::warn!(
                    "multi_screen: cannot open {}: {e}; videos will not mirror",
                    dir.display()
                );
                None
            }
        }
    }

    pub fn store(&self) -> &FsStore {
        &self.store
    }

    /// Hash every file, in order. A file that cannot be read is left out:
    /// the reflection shows its placeholder, same as the primary does.
    pub async fn hash_all(&mut self, videos: &[(String, PathBuf)]) -> Vec<VideoRef> {
        let mut refs = Vec::with_capacity(videos.len());
        let mut keep = HashSet::new();
        for (path, file) in videos {
            match self.hash(file).await {
                Ok(hash) => {
                    keep.insert(file.clone());
                    refs.push(VideoRef {
                        path: path.clone(),
                        hash,
                        size: self.known.get(file).map_or(0, |k| k.len),
                    });
                }
                Err(e) => log::warn!("multi_screen: cannot hash {}: {e}", file.display()),
            }
        }
        // Forget files this layout no longer names, releasing their tags.
        self.known.retain(|file, _| keep.contains(file));
        refs
    }

    async fn hash(&mut self, file: &Path) -> Result<Hash, String> {
        let meta = std::fs::metadata(file).map_err(|e| e.to_string())?;
        let modified = meta.modified().ok();
        if let Some(known) = self.known.get(file)
            && known.len == meta.len()
            && known.modified == modified
        {
            return Ok(known.hash);
        }

        let started = std::time::Instant::now();
        let tag = self
            .store
            .blobs()
            .add_path_with_opts(AddPathOptions {
                path: file.to_path_buf(),
                format: BlobFormat::Raw,
                // Reference in place: the file already lives in the config
                // directory, and copying gigabytes to hash them is waste.
                mode: ImportMode::TryReference,
            })
            .temp_tag()
            .await
            .map_err(|e| e.to_string())?;
        let hash = tag.hash();
        log::info!(
            "multi_screen: hashed {} in {:.1}s",
            file.display(),
            started.elapsed().as_secs_f32()
        );
        self.known.insert(
            file.to_path_buf(),
            Known {
                len: meta.len(),
                modified,
                hash,
                _tag: tag,
            },
        );
        Ok(hash)
    }
}

/// Provider events that admit a connection only from a paired reflection.
///
/// The blobs protocol will otherwise hand any hash to anyone who asks, and
/// the host's party clips are not for the whole LAN.
pub fn serve_to_trusted(trusted: TrustState) -> EventSender {
    let mask = EventMask {
        connected: ConnectMode::Intercept,
        ..EventMask::DEFAULT
    };
    let (events, mut rx) = EventSender::channel(16, mask);
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let ProviderMessage::ClientConnected(msg) = msg {
                let allowed = msg
                    .inner
                    .endpoint_id
                    .is_some_and(|id| trusted.lock().map(|t| t.contains(&id)).unwrap_or(false));
                if !allowed {
                    log::warn!(
                        "multi_screen: refusing blob connection from {:?}",
                        msg.inner.endpoint_id.map(|id| id.fmt_short().to_string())
                    );
                }
                let result = if allowed {
                    Ok(())
                } else {
                    Err(AbortReason::Permission)
                };
                msg.tx.send(result).await.ok();
            }
        }
    });
    events
}

// --------------------------------------------------------------- reflection

/// A reflection's video files, named by content so a clip that two layouts
/// share is fetched once.
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new() -> Self {
        Self {
            dir: data_path("videos"),
        }
    }

    fn file_for(&self, video: &VideoRef) -> PathBuf {
        let ext = Path::new(&video.path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("video");
        self.dir.join(format!("{}.{ext}", video.hash.to_hex()))
    }

    /// Partial downloads, one store per hash, so an interrupted transfer
    /// resumes where it stopped.
    fn incoming_for(&self, hash: &Hash) -> PathBuf {
        self.dir.join(".incoming").join(hash.to_hex())
    }

    /// Make the cache hold exactly what `videos` names: drop the rest, hand
    /// over what is already here, then fetch what is not, one at a time and
    /// soonest-needed first.
    ///
    /// "Soonest" is the primary's say: a clip a node is playing comes before
    /// one it plays next, which comes before the rest in layout order. It is
    /// asked again before every fetch, and while one runs: if the primary
    /// moves on to a clip that ranks higher than the one downloading, that
    /// download stops — its data stays, to resume — and the urgent one starts.
    pub async fn sync(
        &self,
        endpoint: &Endpoint,
        primary: EndpointId,
        videos: &[VideoRef],
        layouts_tx: &AsyncSender<LayoutUpdate>,
    ) {
        if let Err(e) = std::fs::create_dir_all(self.dir.join(".incoming")) {
            log::warn!("multi_screen: cannot create {}: {e}", self.dir.display());
            return;
        }
        self.prune(videos);

        // One entry per file to fetch, with every layout path it serves.
        let mut missing: Vec<(PathBuf, Vec<&VideoRef>)> = Vec::new();
        for video in videos {
            let file = self.file_for(video);
            if file.is_file() {
                if !ready(layouts_tx, video, file).await {
                    return;
                }
            } else if let Some((_, refs)) = missing.iter_mut().find(|(f, _)| *f == file) {
                refs.push(video);
            } else {
                missing.push((file, vec![video]));
            }
        }
        if missing.is_empty() {
            return;
        }

        // The cues follow the layout down the stream; give them a moment to
        // land so the first pick is informed rather than layout order.
        if crate::video::sync::wanted_soon().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        while !missing.is_empty() {
            let pick = most_urgent(&missing);
            let (file, refs) = &missing[pick];
            let video = refs[0];
            let outcome = self
                .download(endpoint, primary, video, refs, file, layouts_tx, || {
                    // Pre-empt for anything that now outranks this file — by
                    // both ranks as they stand *now*: the primary moving on is
                    // exactly what demotes the file downloading.
                    let best = most_urgent(&missing);
                    best != pick && urgency(&missing[best].1) < urgency(&missing[pick].1)
                })
                .await;
            match outcome {
                Fetch::Done => {
                    let (file, refs) = missing.remove(pick);
                    for video in refs {
                        if !ready(layouts_tx, video, file.clone()).await {
                            return;
                        }
                    }
                }
                Fetch::Preempted => {
                    log::info!(
                        "multi_screen: pausing {} for a clip the primary needs sooner",
                        video.path
                    );
                }
                Fetch::Failed(e) => {
                    log::warn!("multi_screen: cannot fetch {}: {e}", video.path);
                    missing.remove(pick);
                }
                Fetch::Gone => return,
            }
        }
    }

    /// Fetch one file, reporting progress for every path it serves, until it
    /// is done or `preempt` says something more urgent has come up.
    #[allow(clippy::too_many_arguments)]
    async fn download(
        &self,
        endpoint: &Endpoint,
        primary: EndpointId,
        video: &VideoRef,
        refs: &[&VideoRef],
        target: &Path,
        layouts_tx: &AsyncSender<LayoutUpdate>,
        preempt: impl Fn() -> bool,
    ) -> Fetch {
        let incoming = self.incoming_for(&video.hash);
        let store = match FsStore::load(&incoming).await {
            Ok(store) => store,
            Err(e) => return Fetch::Failed(format!("cannot open {}: {e}", incoming.display())),
        };
        let started = std::time::Instant::now();
        log::info!("multi_screen: fetching {} from the primary", video.path);

        // Say it has started, so the screen shows a download rather than
        // "waiting" until the first progress report.
        for r in refs {
            let update = LayoutUpdate::VideoProgress {
                path: r.path.clone(),
                received: 0,
                total: r.size,
            };
            if layouts_tx.send(update).await.is_err() {
                let _ = store.shutdown().await;
                return Fetch::Gone;
            }
        }

        let transfer = async {
            let mut stream = store
                .downloader(endpoint)
                .download(video.hash, vec![primary])
                .stream()
                .await
                .map_err(|e| e.to_string())?;
            let mut last_report = std::time::Instant::now() - PROGRESS_INTERVAL;
            while let Some(item) = futures_util::StreamExt::next(&mut stream).await {
                match item {
                    // Counts what a resumed download already had, too.
                    DownloadProgressItem::Progress(received)
                        if last_report.elapsed() >= PROGRESS_INTERVAL =>
                    {
                        last_report = std::time::Instant::now();
                        for r in refs {
                            let update = LayoutUpdate::VideoProgress {
                                path: r.path.clone(),
                                received,
                                total: r.size,
                            };
                            if layouts_tx.send(update).await.is_err() {
                                return Err(GONE.to_string());
                            }
                        }
                    }
                    DownloadProgressItem::Error(e) => return Err(e.to_string()),
                    DownloadProgressItem::DownloadError => {
                        return Err("the primary could not send it".to_string());
                    }
                    _ => {}
                }
            }
            Ok(())
        };
        let watch = async {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                if preempt() {
                    return;
                }
            }
        };

        let fetched = tokio::select! {
            result = transfer => Some(result),
            () = watch => None,
        };
        let outcome = match fetched {
            None => Fetch::Preempted,
            Some(Err(e)) if e == GONE => Fetch::Gone,
            Some(Err(e)) => Fetch::Failed(e),
            // Move rather than copy: the store's copy is about to be thrown
            // away with the rest of the incoming directory.
            Some(Ok(())) => match store
                .blobs()
                .export_with_opts(ExportOptions {
                    hash: video.hash,
                    mode: ExportMode::TryReference,
                    target: target.to_path_buf(),
                })
                .await
            {
                Ok(_) => Fetch::Done,
                Err(e) => Fetch::Failed(e.to_string()),
            },
        };

        // Shut down cleanly whatever happened, so a paused download's data is
        // there to resume from.
        let _ = store.shutdown().await;
        drop(store);
        if matches!(outcome, Fetch::Done) {
            let _ = std::fs::remove_dir_all(&incoming);
            log::info!(
                "multi_screen: fetched {} in {:.1}s",
                video.path,
                started.elapsed().as_secs_f32()
            );
        }
        outcome
    }

    /// Delete cached files and partial downloads the layout no longer names.
    fn prune(&self, videos: &[VideoRef]) {
        let files: HashSet<PathBuf> = videos.iter().map(|v| self.file_for(v)).collect();
        let incoming: HashSet<PathBuf> =
            videos.iter().map(|v| self.incoming_for(&v.hash)).collect();
        let remove = |dir: &Path, keep: &HashSet<PathBuf>, is_dir: bool| {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for path in entries.filter_map(Result::ok).map(|e| e.path()) {
                if keep.contains(&path) || path.is_dir() != is_dir {
                    continue;
                }
                let result = if is_dir {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                match result {
                    Ok(()) => log::info!("multi_screen: dropped cached {}", path.display()),
                    Err(e) => log::warn!("multi_screen: cannot remove {}: {e}", path.display()),
                }
            }
        };
        remove(&self.dir, &files, false);
        remove(&self.dir.join(".incoming"), &incoming, true);
    }
}

/// How often a download reports progress to the UI.
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
/// The UI has gone away; nobody is waiting for anything.
const GONE: &str = "the UI has gone away";

enum Fetch {
    Done,
    /// Stopped for something more urgent; its data stays, to resume.
    Preempted,
    Failed(String),
    Gone,
}

/// How soon the primary needs a file: 0 if a node is playing one of its paths,
/// 1 if one plays it next, 2 otherwise.
fn urgency(refs: &[&VideoRef]) -> u8 {
    let soon = crate::video::sync::wanted_soon();
    refs.iter()
        .filter_map(|r| {
            soon.iter()
                .find(|(_, p)| *p == r.path)
                .map(|(rank, _)| *rank)
        })
        .min()
        .unwrap_or(2)
}

/// The missing file to fetch next: most urgent, then earliest in the layout.
fn most_urgent(missing: &[(PathBuf, Vec<&VideoRef>)]) -> usize {
    (0..missing.len())
        .min_by_key(|&i| urgency(&missing[i].1))
        .unwrap_or(0)
}

/// Tell the UI a file is playable. `false` once the UI has gone away.
async fn ready(layouts_tx: &AsyncSender<LayoutUpdate>, video: &VideoRef, file: PathBuf) -> bool {
    layouts_tx
        .send(LayoutUpdate::Video {
            path: video.path.clone(),
            file,
        })
        .await
        .is_ok()
}
