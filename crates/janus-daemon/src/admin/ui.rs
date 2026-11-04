//! The UI's static assets on the UI listener (`docs/admin-surface#ui`). The
//! assets themselves are public — they hold no state; everything the UI
//! shows comes from the token-gated `/v1` API beside them.

use std::path::{Path, PathBuf};

use tower_http::services::{ServeDir, ServeFile};

use super::AdminError;

/// The built UI of the source tree this daemon was compiled from. Only a
/// debug build looks there; a release binary has no source tree to rely on.
#[cfg(debug_assertions)]
const SOURCE_TREE_ASSETS: Option<&str> =
    Some(concat!(env!("CARGO_MANIFEST_DIR"), "/../../ui/dist"));
#[cfg(not(debug_assertions))]
const SOURCE_TREE_ASSETS: Option<&str> = None;

/// The directories searched when `ui.assets_dir` is not set, in order: `ui`
/// beside the executable, `../share/janus/ui` relative to it, then (debug
/// builds only) the source tree's `ui/dist`.
fn default_candidates(exe_dir: Option<&Path>, source_tree: Option<&str>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(exe_dir) = exe_dir {
        dirs.push(exe_dir.join("ui"));
        dirs.push(exe_dir.join("../share/janus/ui"));
    }
    dirs.extend(source_tree.map(PathBuf::from));
    dirs
}

fn has_index(dir: &Path) -> Result<(), String> {
    match std::fs::metadata(dir.join("index.html")) {
        Ok(meta) if meta.is_file() => Ok(()),
        Ok(_) => Err("index.html is not a file".into()),
        Err(e) => Err(format!("no index.html ({e})")),
    }
}

fn resolve(
    configured: Option<&Path>,
    exe_dir: Option<&Path>,
    source_tree: Option<&str>,
) -> Result<PathBuf, AdminError> {
    if let Some(dir) = configured {
        return has_index(dir)
            .map(|()| dir.to_path_buf())
            .map_err(|reason| AdminError::UiAssets {
                path: dir.to_path_buf(),
                reason,
            });
    }
    let candidates = default_candidates(exe_dir, source_tree);
    if let Some(dir) = candidates.iter().find(|dir| has_index(dir).is_ok()) {
        return Ok(dir.clone());
    }
    let tried = candidates
        .iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(AdminError::UiAssets {
        path: candidates.first().cloned().unwrap_or_default(),
        reason: format!("no index.html in any default location (tried: {tried}); install the UI beside the executable or set ui.assets_dir"),
    })
}

/// The assets directory to serve: `configured`, else the first default
/// location holding an `index.html`.
pub(crate) fn assets(configured: Option<&Path>) -> Result<PathBuf, AdminError> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    resolve(configured, exe_dir.as_deref(), SOURCE_TREE_ASSETS)
}

/// Files under `dir`; any path that names no file gets `index.html`, so the
/// single-page app's own routes load on a direct visit or a reload.
pub(crate) fn service(dir: &Path) -> ServeDir<ServeFile> {
    ServeDir::new(dir).fallback(ServeFile::new(dir.join("index.html")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ui_dir(root: &Path, rel: &str) -> PathBuf {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("index.html"), "<html></html>").expect("index");
        dir
    }

    #[test]
    fn configured_dir_is_used_alone() {
        let root = tempfile::tempdir().expect("tmp");
        let exe = root.path().join("bin");
        ui_dir(&exe, "ui");
        let empty = root.path().join("empty");
        std::fs::create_dir_all(&empty).expect("mkdir");
        let err = resolve(Some(&empty), Some(&exe), None).expect_err("no fallback");
        assert!(err.to_string().contains("empty"), "{err}");
        let own = ui_dir(root.path(), "own");
        assert_eq!(resolve(Some(&own), Some(&exe), None).expect("ok"), own);
    }

    #[test]
    fn exe_dir_ui_wins_over_share_and_source_tree() {
        let root = tempfile::tempdir().expect("tmp");
        let exe = root.path().join("bin");
        let beside = ui_dir(&exe, "ui");
        ui_dir(&exe, "../share/janus/ui");
        let src = ui_dir(root.path(), "src");
        let got = resolve(None, Some(&exe), src.to_str()).expect("ok");
        assert_eq!(got, beside);
    }

    #[test]
    fn share_dir_then_source_tree_are_fallbacks() {
        let root = tempfile::tempdir().expect("tmp");
        let exe = root.path().join("bin");
        std::fs::create_dir_all(&exe).expect("mkdir");
        let src = ui_dir(root.path(), "src");
        assert_eq!(resolve(None, Some(&exe), src.to_str()).expect("src"), src);
        let share = ui_dir(&exe, "../share/janus/ui");
        let got = resolve(None, Some(&exe), src.to_str()).expect("share");
        assert_eq!(
            got.canonicalize().expect("c"),
            share.canonicalize().expect("c")
        );
    }

    #[test]
    fn failure_names_every_path_tried() {
        let root = tempfile::tempdir().expect("tmp");
        let exe = root.path().join("bin");
        let message = resolve(None, Some(&exe), Some("/nowhere/ui/dist"))
            .expect_err("nothing installed")
            .to_string();
        for path in [
            exe.join("ui").display().to_string(),
            exe.join("../share/janus/ui").display().to_string(),
            "/nowhere/ui/dist".to_owned(),
        ] {
            assert!(message.contains(&path), "{path} missing from {message}");
        }
    }

    #[test]
    fn release_builds_never_look_in_the_source_tree() {
        assert_eq!(SOURCE_TREE_ASSETS.is_some(), cfg!(debug_assertions));
    }
}
