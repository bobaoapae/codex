//! FORK: input images for the `chatgpt_web` provider.
//!
//! Codex hands images to the model as `data:` URLs. ChatGPT only takes files
//! through the composer's upload input, so each image is materialized once
//! under `CODEX_HOME/chatgpt_web/attachments/` with a name derived from its
//! content — the same image sent twice gets the same file, and the driver's
//! upload dedupe (name + size) then skips it on the second turn.

use base64::Engine;
use sha1::Digest;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use tracing::warn;

/// ChatGPT's composer caps attachments per message; the newest images win.
pub(crate) const MAX_IMAGES_PER_MESSAGE: usize = 10;

/// Attachments older than this are removed when a turn starts.
pub(crate) const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Directory name under `CODEX_HOME`.
const ATTACHMENTS_DIR: &str = "chatgpt_web/attachments";

/// Where materialized images live.
#[derive(Debug, Clone)]
pub(crate) struct ImageStore {
    dir: PathBuf,
}

/// One materialized image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredImage {
    pub(crate) path: PathBuf,
    /// File name, which is also what the transcript placeholder says.
    pub(crate) name: String,
}

impl ImageStore {
    pub(crate) fn new(codex_home: &Path) -> Self {
        Self {
            dir: codex_home.join(ATTACHMENTS_DIR),
        }
    }

    #[cfg(test)]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Removes attachments not touched for `STALE_AFTER`. Best effort.
    pub(crate) fn cleanup_stale(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let path = entry.path();
            let stale = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > STALE_AFTER);
            if stale && let Err(err) = std::fs::remove_file(&path) {
                warn!(
                    "chatgpt_web: could not remove stale attachment {}: {err}",
                    path.display()
                );
            }
        }
    }

    /// Writes the image behind a `data:` URL (if not already there) and
    /// returns its file. `None` when the URL is not a decodable data URL.
    pub(crate) fn materialize(&self, data_url: &str) -> Option<StoredImage> {
        let (mime, bytes) = parse_data_url(data_url)?;
        let name = file_name_for(&mime, &bytes);
        let path = self.dir.join(&name);
        if !path.exists() {
            if let Err(err) = std::fs::create_dir_all(&self.dir) {
                warn!(
                    "chatgpt_web: could not create {}: {err}",
                    self.dir.display()
                );
                return None;
            }
            // Written under a temp name so a concurrent turn never sees a
            // half-written file with the final name.
            let temp = self.dir.join(format!(".{name}.{}.tmp", std::process::id()));
            if let Err(err) = std::fs::write(&temp, &bytes) {
                warn!("chatgpt_web: could not write {}: {err}", temp.display());
                return None;
            }
            if let Err(err) = std::fs::rename(&temp, &path) {
                let _ = std::fs::remove_file(&temp);
                if !path.exists() {
                    warn!("chatgpt_web: could not place {}: {err}", path.display());
                    return None;
                }
            }
        }
        Some(StoredImage { path, name })
    }
}

/// Splits `data:<mime>;base64,<payload>` into its MIME type and bytes.
pub(crate) fn parse_data_url(url: &str) -> Option<(String, Vec<u8>)> {
    let rest = url.strip_prefix("data:")?;
    let (header, payload) = rest.split_once(',')?;
    let mut parts = header.split(';');
    let mime = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
    let is_base64 = parts.any(|part| part.trim().eq_ignore_ascii_case("base64"));
    if !is_base64 {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    let mime = if mime.is_empty() {
        "image/png".to_string()
    } else {
        mime
    };
    Some((mime, bytes))
}

/// Deterministic name: `codex-img-<12 hex of the content hash>.<ext>`.
pub(crate) fn file_name_for(mime: &str, bytes: &[u8]) -> String {
    let digest = sha1::Sha1::digest(bytes);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("codex-img-{}.{}", &hex[..12], extension_for(mime))
}

fn extension_for(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/svg+xml" => "svg",
        _ => "png",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    #[test]
    fn a_data_url_decodes_to_its_mime_and_bytes() {
        let (mime, bytes) =
            parse_data_url(&format!("data:image/png;base64,{PNG_1X1}")).expect("decodes");
        assert_eq!(mime, "image/png");
        assert_eq!(&bytes[..4], b"\x89PNG");
    }

    #[test]
    fn non_base64_and_foreign_urls_are_rejected() {
        assert!(parse_data_url("https://example.com/a.png").is_none());
        assert!(parse_data_url("data:text/plain,hello").is_none());
    }

    #[test]
    fn the_same_bytes_get_the_same_name_and_one_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ImageStore::new(temp.path());
        let url = format!("data:image/jpeg;base64,{PNG_1X1}");

        let first = store.materialize(&url).expect("stored");
        let second = store.materialize(&url).expect("stored");

        assert_eq!(first, second);
        assert!(first.name.starts_with("codex-img-"));
        assert!(first.name.ends_with(".jpg"));
        assert_eq!(std::fs::read_dir(store.dir()).unwrap().count(), 1);
    }

    #[test]
    fn stale_files_are_removed_and_fresh_ones_kept() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ImageStore::new(temp.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        let old = store.dir().join("codex-img-old.png");
        let fresh = store.dir().join("codex-img-new.png");
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(&fresh, b"y").unwrap();
        let long_ago = SystemTime::now() - STALE_AFTER - Duration::from_secs(60);
        let file = std::fs::File::options().write(true).open(&old).unwrap();
        file.set_modified(long_ago).unwrap();
        drop(file);

        store.cleanup_stale();

        assert!(!old.exists());
        assert!(fresh.exists());
    }
}
