//! The image bytes behind a document's `image` nodes.
//!
//! Both roles look assets up by the same key — the path the `image` node names
//! — but they fill the store from opposite ends. A primary reads the files out
//! of the config directory next to `layout.kdl`. A reflection never touches its
//! own disk for these, any more than it reads its own `layout.kdl`: the bytes
//! arrive over the wire from the primary that owns the document.
//!
//! That asymmetry is why [`Assets`] tracks *wanted* paths separately from the
//! bytes it holds. A path with no bytes is a different thing on each side — a
//! file the host forgot to import, or a file still in flight — and
//! [`Assets::missing`] is what settings turns into a **Locate…** action.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

use bytes::Bytes;

use super::schema::{Node, Widget};

/// Per-asset ceiling.
///
/// These cross the wire to every reflection in a single message and are held
/// in memory decoded on top of that, so the cap is about not wedging a party
/// on someone's 200 MB raw photo. Generous for anything a 1280-wide panel can
/// actually show.
pub const MAX_ASSET_BYTES: usize = 16 * 1024 * 1024;

/// One image, with a cheap fingerprint of its content.
///
/// The fingerprint exists because Freya's `ImageSource` hashes only the id it
/// is handed, never the bytes behind it, so its decoded-image cache is keyed on
/// whatever we pass. Key that on the path alone and a host who replaces
/// `couple.jpg` with a different photo keeps seeing the old one for the life of
/// the process. Cover art never hit this — a Spotify URL changes with the
/// artwork — but a layout asset's path is precisely what stays the same.
///
/// Hashed once here rather than per frame: these run to megabytes.
#[derive(Debug, Clone)]
pub struct Asset {
    pub bytes: Bytes,
    /// Include in the render key so replaced content is re-decoded.
    pub fingerprint: u64,
}

impl Asset {
    fn new(bytes: Bytes) -> Self {
        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        Self {
            fingerprint: hasher.finish(),
            bytes,
        }
    }
}

/// Encoded image bytes for the active document, keyed by layout-relative path.
#[derive(Debug, Clone, Default)]
pub struct Assets {
    /// Paths named by `image` nodes, in document order, deduplicated. Kept
    /// even when the bytes are absent so the UI can name what is missing.
    wanted: Vec<String>,
    bytes: HashMap<String, Asset>,
    /// Why a wanted path produced no bytes. Only a primary fills this in — a
    /// reflection's absent asset is "not here yet", not an error.
    errors: HashMap<String, String>,
}

impl Assets {
    /// A reflection's store: it knows what the document asks for, and waits
    /// for the primary to send it.
    pub fn expecting(root: &Node) -> Self {
        Self {
            wanted: image_paths(root),
            bytes: HashMap::new(),
            errors: HashMap::new(),
        }
    }

    /// A primary's store: read every `image` path out of `dir`.
    ///
    /// A path that cannot be read is not fatal and never has been — the node
    /// renders its placeholder and the reason shows up in settings. One
    /// mistyped filename does not cost a host the rest of their layout.
    pub fn from_disk(root: &Node, dir: &Path) -> Self {
        let wanted = image_paths(root);
        let mut bytes = HashMap::new();
        let mut errors = HashMap::new();

        for path in &wanted {
            match read_asset(&dir.join(path)) {
                Ok(data) => {
                    bytes.insert(path.clone(), Asset::new(data));
                }
                Err(why) => {
                    log::warn!("layout: {path}: {why}");
                    errors.insert(path.clone(), why);
                }
            }
        }

        Self {
            wanted,
            bytes,
            errors,
        }
    }

    pub fn get(&self, path: &str) -> Option<&Asset> {
        self.bytes.get(path)
    }

    /// Every asset we hold, for the primary to broadcast.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Asset)> {
        self.bytes.iter()
    }

    /// Bytes that arrived from the primary.
    pub fn insert(&mut self, path: String, data: Bytes) {
        self.errors.remove(&path);
        self.bytes.insert(path, Asset::new(data));
    }

    /// Wanted paths we have no bytes for, each with a reason a host can act
    /// on. Document order, so the list is stable between renders.
    pub fn missing(&self) -> Vec<(&str, &str)> {
        self.wanted
            .iter()
            .filter(|p| !self.bytes.contains_key(*p))
            .map(|p| {
                let why = self
                    .errors
                    .get(p)
                    .map(String::as_str)
                    .unwrap_or("not sent by the primary yet");
                (p.as_str(), why)
            })
            .collect()
    }
}

/// Read one asset, refusing anything too big to put on the wire.
fn read_asset(path: &Path) -> Result<Bytes, String> {
    let meta = std::fs::metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "file not found".to_string(),
        _ => e.to_string(),
    })?;
    if !meta.is_file() {
        return Err("not a file".to_string());
    }
    if meta.len() as usize > MAX_ASSET_BYTES {
        return Err(format!(
            "{} MB is over the {} MB limit",
            meta.len() / (1024 * 1024),
            MAX_ASSET_BYTES / (1024 * 1024)
        ));
    }
    std::fs::read(path)
        .map(Bytes::from)
        .map_err(|e| e.to_string())
}

/// Every distinct path named by an `image` node, in document order.
///
/// `when` conditions are ignored on purpose: a photo shown only while idle
/// still has to be on the display before the music stops.
pub fn image_paths(root: &Node) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

fn walk(node: &Node, out: &mut Vec<String>) {
    match &node.widget {
        Widget::Image { path, .. } => {
            if !out.iter().any(|p| p == path) {
                out.push(path.clone());
            }
        }
        Widget::Container(c) => {
            for child in &c.children {
                walk(child, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::load;

    fn root_of(source: &str) -> Node {
        load(source).expect("valid").doc.root
    }

    #[test]
    fn image_paths_are_collected_in_document_order_without_duplicates() {
        let root = root_of(
            r#"
            root {
                image "a.png"
                row {
                    image "b.jpg"
                    image "a.png"
                }
                text "not an image"
            }
            "#,
        );
        assert_eq!(image_paths(&root), vec!["a.png", "b.jpg"]);
    }

    #[test]
    fn a_document_with_no_images_wants_nothing() {
        let root = root_of("root { clock }");
        assert!(image_paths(&root).is_empty());
        assert!(Assets::expecting(&root).missing().is_empty());
    }

    #[test]
    fn a_reflection_reports_what_it_is_still_waiting_for() {
        let root = root_of(r#"root { image "ana.jpg" }"#);
        let mut assets = Assets::expecting(&root);
        assert_eq!(
            assets.missing(),
            vec![("ana.jpg", "not sent by the primary yet")]
        );

        assets.insert("ana.jpg".to_string(), Bytes::from_static(b"\x89PNG"));
        assert!(assets.missing().is_empty());
        assert!(assets.get("ana.jpg").is_some());
    }

    /// The regression behind the stale-photo bug: same path, new bytes, and
    /// the render key has to move or Freya serves the old decoded image.
    #[test]
    fn replacing_an_asset_changes_its_fingerprint() {
        let root = root_of(r#"root { image "ana.jpg" }"#);
        let mut assets = Assets::expecting(&root);

        assets.insert("ana.jpg".to_string(), Bytes::from_static(b"first"));
        let before = assets.get("ana.jpg").expect("stored").fingerprint;

        assets.insert("ana.jpg".to_string(), Bytes::from_static(b"second"));
        let after = assets.get("ana.jpg").expect("stored").fingerprint;

        assert_ne!(before, after, "the path is the same; the content is not");
    }

    #[test]
    fn a_missing_file_is_reported_rather_than_fatal() {
        let dir = std::env::temp_dir().join(format!("gid-assets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("there.png"), b"\x89PNG\r\n").expect("write");

        let root = root_of(
            r#"
            root {
                image "there.png"
                image "gone.png"
            }
            "#,
        );
        let assets = Assets::from_disk(&root, &dir);

        assert!(assets.get("there.png").is_some(), "the file that is there");
        assert_eq!(assets.missing(), vec![("gone.png", "file not found")]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_oversized_asset_is_refused_with_its_size() {
        let dir = std::env::temp_dir().join(format!("gid-assets-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let big = dir.join("huge.png");
        std::fs::write(&big, vec![0u8; MAX_ASSET_BYTES + 1]).expect("write");

        let err = read_asset(&big).expect_err("over the cap");
        assert!(err.contains("limit"), "{err}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
