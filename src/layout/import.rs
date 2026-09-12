//! Importing a layout through the XDG file-chooser portal.
//!
//! The config path is where a layout is *stored*, not how a host is expected to
//! put one there — nobody should have to learn what `~/.var/app/…` means to
//! re-theme their party. This module is the other end: pick a file from
//! anywhere, and the app copies it in.
//!
//! Two properties the flow is built around:
//!
//! - **Validate, then commit.** The chosen document is parsed before anything
//!   on disk is touched. A broken import shows its diagnostics and changes
//!   nothing, so it cannot break a display mid-party.
//! - **Copy, don't reference.** Portal access to the picked path is not durable
//!   across restarts, so the file in the config dir stays the single live,
//!   watched document and the picked file is only a source.
//!
//! The source is a **folder**, not a multi-selection. A layout that references
//! three photos needed all four files ctrl-clicked, and the recovery for
//! forgetting one is a chooser round-trip per missing image — which is a poor
//! trade for the host. Picking the folder grants the portal a wider read than
//! picking files did, but the app's own footprint is *narrower*: it copies only
//! the paths the parsed document actually names, where the old flow copied
//! whatever was selected regardless.
//!
//! The folder is only the *source*. What gets stored is unchanged — `layout.kdl`
//! plus the files beside it in the config directory — so nothing downstream ever
//! learns a folder was involved.

use std::path::{Path, PathBuf};

use ashpd::desktop::ResponseError;
use ashpd::desktop::file_chooser::{FileFilter, SelectedFiles};
use async_channel::Sender as AsyncSender;

use super::assets::image_paths;
use super::{Diagnostic, config_dir, layout_path, load, write_layout};

/// Result of an import attempt, for the settings dialog to report.
#[derive(Debug)]
pub enum Outcome {
    Installed {
        assets: usize,
    },
    /// The user dismissed the chooser. Says nothing, shows nothing.
    Cancelled,
    /// The folder holds no `.kdl` at all.
    NoLayout,
    /// The folder holds more than one `.kdl`. Refused rather than guessed:
    /// picking one silently installs a party the host did not ask for.
    Ambiguous(Vec<String>),
    /// The chosen document does not parse. Nothing on disk changed.
    Rejected(Vec<Diagnostic>),
    /// The portal or the filesystem refused.
    Failed(String),
    /// One missing image was found and copied in under the name the layout
    /// uses for it.
    Located {
        path: String,
    },
}

/// Open the chooser and install whatever comes back.
///
/// Runs on a thread of its own, like [`crate::inhibitor`]: the portal call is
/// async, ashpd has to stay on `async-io` (its `tokio` feature switches zbus
/// process-wide and Freya's own zbus threads panic), and a file dialog is rare
/// and one-shot enough that a thread per invocation is the simplest thing that
/// works. The outcome comes back over `respond`, which a Freya task awaits.
pub fn import(respond: AsyncSender<Outcome>) {
    std::thread::Builder::new()
        .name("layout-import".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = respond.send_blocking(Outcome::Failed(e.to_string()));
                    return;
                }
            };
            let outcome = rt.block_on(pick_and_install());
            let _ = respond.send_blocking(outcome);
        })
        .map(|_| ())
        .unwrap_or_else(|e| log::warn!("layout: cannot start the import thread: {e}"));
}

/// Find the file behind one `image` path the document names but the config
/// directory does not have.
///
/// The document portal grants exactly the files a host selected, so a `.kdl`
/// referencing `ana.jpg` that they forgot to ctrl-click imports fine and then
/// renders a placeholder. This is the way back without re-importing the whole
/// layout.
pub fn locate(target: String, respond: AsyncSender<Outcome>) {
    std::thread::Builder::new()
        .name("layout-locate".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = respond.send_blocking(Outcome::Failed(e.to_string()));
                    return;
                }
            };
            let outcome = rt.block_on(pick_and_place(&target));
            let _ = respond.send_blocking(outcome);
        })
        .map(|_| ())
        .unwrap_or_else(|e| log::warn!("layout: cannot start the locate thread: {e}"));
}

async fn pick_and_place(target: &str) -> Outcome {
    // The builder borrows its title for the life of the request, so it has to
    // outlive the chain rather than be built inside it.
    let title = format!("Locate {target}");
    let request = SelectedFiles::open_file()
        .title(title.as_str())
        .accept_label("Use this file")
        .modal(true)
        .multiple(false)
        .filter(
            FileFilter::new("Images")
                .mimetype("image/png")
                .mimetype("image/jpeg"),
        )
        .send()
        .await;

    let files = match request {
        Ok(request) => match request.response() {
            Ok(files) => files,
            Err(ashpd::Error::Response(ResponseError::Cancelled)) => return Outcome::Cancelled,
            Err(e) => return Outcome::Failed(e.to_string()),
        },
        Err(e) => return Outcome::Failed(e.to_string()),
    };

    let Some(picked) = files.uris().iter().find_map(|u| file_uri_path(u.as_str())) else {
        return Outcome::Cancelled;
    };

    place(&picked, target)
}

/// Copy `picked` in under the name the layout asks for.
///
/// Under the *layout's* name, not the picked file's: the document says
/// `image "ana.jpg"`, so dropping someone's `IMG_1847.jpg` beside it verbatim
/// would fix nothing. The path is already validated to stay inside the config
/// directory, and it may name a subdirectory, so the parents are created.
pub fn place(picked: &Path, target: &str) -> Outcome {
    let destination = config_dir().join(target);
    if let Some(parent) = destination.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return Outcome::Failed(format!("cannot create {}: {e}", parent.display()));
    }
    match std::fs::copy(picked, &destination) {
        Ok(_) => Outcome::Located {
            path: target.to_string(),
        },
        Err(e) => Outcome::Failed(format!("cannot copy to {}: {e}", destination.display())),
    }
}

async fn pick_and_install() -> Outcome {
    // No filters: with `directory(true)` the chooser is picking a folder, and a
    // file filter would only narrow what the host can see inside it.
    let request = SelectedFiles::open_file()
        .title("Import a layout folder")
        .accept_label("Import")
        .modal(true)
        .directory(true)
        .send()
        .await;

    let files = match request {
        Ok(request) => match request.response() {
            Ok(files) => files,
            Err(ashpd::Error::Response(ResponseError::Cancelled)) => return Outcome::Cancelled,
            Err(e) => return Outcome::Failed(e.to_string()),
        },
        Err(e) => return Outcome::Failed(e.to_string()),
    };

    let Some(folder) = files.uris().iter().find_map(|u| file_uri_path(u.as_str())) else {
        return Outcome::Cancelled;
    };
    install(&folder)
}

/// The part worth testing: everything except the dialog.
pub fn install(folder: &Path) -> Outcome {
    let layouts = match kdl_files(folder) {
        Ok(found) => found,
        Err(e) => return Outcome::Failed(format!("cannot read {}: {e}", folder.display())),
    };

    let layout = match layouts.as_slice() {
        [] => return Outcome::NoLayout,
        [one] => one,
        many => {
            return Outcome::Ambiguous(
                many.iter()
                    .filter_map(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .collect(),
            );
        }
    };

    let source = match std::fs::read_to_string(layout) {
        Ok(s) => s,
        Err(e) => return Outcome::Failed(format!("cannot read {}: {e}", layout.display())),
    };

    // Parse before touching disk. A document that cannot render is not one to
    // put in front of guests, and the running display must not change.
    //
    // The parse also decides what gets copied: every path below came out of a
    // node the parser accepted, and it rejects `..` and absolute paths, so
    // joining them onto the picked folder cannot reach outside it.
    let active = match load(&source) {
        Ok(active) => active,
        Err(diagnostics) => return Outcome::Rejected(diagnostics),
    };
    let wanted = image_paths(&active.doc.root);

    if let Err(e) = write_layout(&source) {
        return Outcome::Failed(format!("cannot write {}: {e}", layout_path().display()));
    }

    let mut copied = 0;
    for path in &wanted {
        match copy_asset(&folder.join(path), path) {
            Ok(()) => copied += 1,
            // Not fatal, and not silent: the image renders a placeholder and
            // settings offers Locate… for it.
            Err(e) => log::warn!("layout: cannot copy {path}: {e}"),
        }
    }

    Outcome::Installed { assets: copied }
}

/// The `.kdl` files directly inside `folder`.
///
/// Top level only. A layout folder is a folder with a layout in it, not a
/// project tree, and recursing would turn "which document did I just install?"
/// into a question.
fn kdl_files(folder: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(folder)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("kdl")))
        .collect();
    // Readdir order is arbitrary; sorting makes the ambiguity message stable.
    found.sort();
    Ok(found)
}

/// Copy one asset in under the relative path the document names it by.
///
/// The relative path, not the base name: `image "photos/ana.jpg"` is a legal
/// layout path, and flattening it to `ana.jpg` leaves the renderer looking for
/// a file that is not where it was put.
fn copy_asset(from: &Path, relative: &str) -> std::io::Result<()> {
    let destination = config_dir().join(relative);
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(from, destination)?;
    Ok(())
}

/// `file:///some/path%20with%20spaces` into a path.
///
/// The portal hands back document-portal URIs, and its own `Uri` type is a
/// string wrapper, so the percent-decoding is ours to do. Anything that is not
/// a `file:` URI is skipped — there is nothing useful to do with a remote one.
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // Strip an empty authority ("file:///path"); a non-empty one is a remote
    // host, which we cannot read.
    let encoded = match rest.find('/') {
        Some(0) => rest,
        _ => return None,
    };

    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            match u8::from_str_radix(hex, 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                    continue;
                }
                Err(_) => return None,
            }
        }
        out.push(bytes[i]);
        i += 1;
    }

    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

/// A one-line summary for the settings dialog, plus the detail lines if any.
pub fn describe(outcome: &Outcome) -> (String, Vec<String>) {
    match outcome {
        Outcome::Installed { assets: 0 } => ("Imported layout.kdl".to_string(), Vec::new()),
        Outcome::Installed { assets } => (
            format!("Imported layout.kdl and {assets} image(s)"),
            Vec::new(),
        ),
        Outcome::Cancelled => (String::new(), Vec::new()),
        Outcome::NoLayout => (
            "Nothing imported — that folder has no .kdl file".to_string(),
            Vec::new(),
        ),
        Outcome::Ambiguous(names) => (
            "Nothing imported — that folder has more than one .kdl".to_string(),
            names.clone(),
        ),
        Outcome::Rejected(diagnostics) => (
            "Not imported — that file has errors".to_string(),
            diagnostics.iter().map(Diagnostic::to_string).collect(),
        ),
        Outcome::Failed(e) => ("Import failed".to_string(), vec![e.clone()]),
        Outcome::Located { path } => (format!("Found {path}"), Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// These tests set `XDG_CONFIG_HOME`, which is process-wide, so they cannot
    /// run at the same time as each other.
    static ENV: Mutex<()> = Mutex::new(());

    /// A private config directory, and the lock that makes it private.
    fn sandbox(name: &str) -> (PathBuf, MutexGuard<'static, ()>) {
        let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("gid-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("temp dir");
        // SAFETY: ENV is held for the life of the returned guard, so no other
        // test in this process reads or writes the variable meanwhile.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        (dir, guard)
    }

    /// A folder to import from, distinct from the config directory it installs
    /// into — the real ones are never the same place either.
    fn source_folder(root: &Path) -> PathBuf {
        let folder = root.join("source");
        std::fs::create_dir_all(&folder).expect("source dir");
        folder
    }

    #[test]
    fn decodes_file_uris() {
        assert_eq!(
            file_uri_path("file:///run/user/1000/doc/abc/layout.kdl"),
            Some(PathBuf::from("/run/user/1000/doc/abc/layout.kdl"))
        );
        assert_eq!(
            file_uri_path("file:///home/ana/Ana%27s%20party/layout.kdl"),
            Some(PathBuf::from("/home/ana/Ana's party/layout.kdl"))
        );
        assert_eq!(file_uri_path("https://example.test/layout.kdl"), None);
        assert_eq!(file_uri_path("file://remote/layout.kdl"), None);
    }

    #[test]
    fn a_broken_import_changes_nothing_on_disk() {
        let (root, _env) = sandbox("import");
        let folder = source_folder(&root);

        std::fs::write(folder.join("party.kdl"), "root { clock }").expect("write");
        assert!(matches!(install(&folder), Outcome::Installed { assets: 0 }));
        let installed = std::fs::read_to_string(layout_path()).expect("installed");

        // Half-typed, as an editor would leave it mid-save.
        std::fs::write(folder.join("party.kdl"), "root { clock ").expect("write");
        assert!(matches!(install(&folder), Outcome::Rejected(_)));
        assert_eq!(
            std::fs::read_to_string(layout_path()).expect("still there"),
            installed,
            "a rejected import must leave the installed layout untouched"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn only_the_images_the_document_names_are_copied() {
        let (root, _env) = sandbox("import-assets");
        let folder = source_folder(&root);

        std::fs::create_dir_all(folder.join("photos")).expect("subdir");
        std::fs::write(folder.join("photos/ana.jpg"), b"ana").expect("write");
        std::fs::write(folder.join("banner.png"), b"banner").expect("write");
        std::fs::write(folder.join("holiday.jpg"), b"unrelated").expect("write");
        std::fs::write(
            folder.join("party.kdl"),
            r#"
            root {
                image "photos/ana.jpg"
                image "banner.png"
            }
            "#,
        )
        .expect("write");

        assert!(matches!(install(&folder), Outcome::Installed { assets: 2 }));

        // The subdirectory survives: flattening to a base name would leave the
        // renderer looking somewhere the file is not.
        assert_eq!(
            std::fs::read(config_dir().join("photos/ana.jpg")).expect("copied"),
            b"ana"
        );
        assert_eq!(
            std::fs::read(config_dir().join("banner.png")).expect("copied"),
            b"banner"
        );
        assert!(
            !config_dir().join("holiday.jpg").exists(),
            "a file the document never mentions must stay where it is"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_image_missing_from_the_folder_still_installs_the_layout() {
        let (root, _env) = sandbox("import-partial");
        let folder = source_folder(&root);

        std::fs::write(folder.join("party.kdl"), r#"root { image "gone.png" }"#).expect("write");

        // Installed, with nothing copied — the node renders a placeholder and
        // settings offers Locate… for it.
        assert!(matches!(install(&folder), Outcome::Installed { assets: 0 }));
        assert!(layout_path().exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn two_layouts_in_one_folder_are_refused_rather_than_guessed() {
        let (root, _env) = sandbox("import-ambiguous");
        let folder = source_folder(&root);

        std::fs::write(folder.join("wedding.kdl"), "root { clock }").expect("write");
        std::fs::write(folder.join("party.kdl"), "root { date }").expect("write");

        match install(&folder) {
            Outcome::Ambiguous(names) => {
                assert_eq!(names, vec!["party.kdl", "wedding.kdl"], "sorted, so stable")
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        assert!(
            !layout_path().exists(),
            "an ambiguous folder must install nothing"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn locating_an_asset_stores_it_under_the_name_the_layout_uses() {
        let (root, _env) = sandbox("locate");

        let picked = root.join("IMG_1847.jpg");
        std::fs::write(&picked, b"\xff\xd8\xff\xe0").expect("write");

        // A subdirectory in the target exercises the parent-creation path.
        assert!(matches!(
            place(&picked, "photos/ana.jpg"),
            Outcome::Located { .. }
        ));
        assert_eq!(
            std::fs::read(config_dir().join("photos/ana.jpg")).expect("copied"),
            b"\xff\xd8\xff\xe0",
            "the layout's name wins over the picked file's"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_folder_with_no_layout_is_reported() {
        let (root, _env) = sandbox("import-empty");
        let folder = source_folder(&root);
        std::fs::write(folder.join("photo.jpg"), b"jpg").expect("write");

        assert!(matches!(install(&folder), Outcome::NoLayout));

        std::fs::remove_dir_all(&root).ok();
    }
}
