//! The UI's static assets on the UI listener (`docs/admin-surface#ui`). The
//! assets themselves are public — they hold no state; everything the UI
//! shows comes from the token-gated `/v1` API beside them.

use std::path::{Path, PathBuf};

use tower_http::services::{ServeDir, ServeFile};

use super::AdminError;

/// The built UI of the source tree this daemon was compiled from, used when
/// `ui.assets_dir` is not set.
const BUILT_IN_ASSETS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ui/dist");

/// The assets directory to serve: `configured`, else [`BUILT_IN_ASSETS`].
/// It must hold the UI's `index.html`.
pub(crate) fn assets(configured: Option<&Path>) -> Result<PathBuf, AdminError> {
    let dir = configured.map_or_else(|| PathBuf::from(BUILT_IN_ASSETS), Path::to_path_buf);
    let index = dir.join("index.html");
    match std::fs::metadata(&index) {
        Ok(meta) if meta.is_file() => Ok(dir),
        Ok(_) => Err(AdminError::UiAssets {
            path: dir,
            reason: "index.html is not a file".into(),
        }),
        Err(e) => Err(AdminError::UiAssets {
            path: dir,
            reason: format!("no index.html ({e}); build the UI (ui/dist) or set ui.assets_dir"),
        }),
    }
}

/// Files under `dir`; any path that names no file gets `index.html`, so the
/// single-page app's own routes load on a direct visit or a reload.
pub(crate) fn service(dir: &Path) -> ServeDir<ServeFile> {
    ServeDir::new(dir).fallback(ServeFile::new(dir.join("index.html")))
}
