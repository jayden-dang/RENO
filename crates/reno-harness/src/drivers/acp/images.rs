//! Inline attachments as ACP image content blocks.

use serde_json::{Value, json};

/// Inline attachments as native image blocks, best-effort like the Claude
/// driver: the path refs already ride the prompt text, so an unreadable,
/// oversized or non-inlinable file (SVG, HEIC, TIFF, ...) must not fail the turn.
pub(crate) async fn load_images(paths: &[String]) -> Value {
    load_images_capped(paths, 20 * 1024 * 1024, u64::MAX).await
}

/// [`load_images`] under a cap per file and a cap on all inlined files
/// together, in bytes. A file over either rides as its path ref only.
async fn load_images_capped(paths: &[String], each: u64, total: u64) -> Value {
    use base64::Engine;
    let mut images = vec![];
    let mut inlined = 0u64;
    for path in paths {
        match tokio::fs::metadata(path).await {
            Ok(meta) if meta.len() <= each && meta.len() <= total - inlined => {}
            Ok(_) => {
                tracing::debug!(%path, "attachment over inline cap; path ref only");
                continue;
            }
            Err(error) => {
                tracing::warn!(%path, %error, "attachment unreadable; path ref only");
                continue;
            }
        }
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(%path, %error, "attachment unreadable; path ref only");
                continue;
            }
        };
        let mime = if bytes.starts_with(b"\x89PNG") {
            "image/png"
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            "image/jpeg"
        } else if bytes.starts_with(b"GIF8") {
            "image/gif"
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            "image/webp"
        } else {
            tracing::debug!(%path, "attachment is not an inlinable image; path ref only");
            continue;
        };
        // The file may have grown since its metadata was read.
        if bytes.len() as u64 > each || bytes.len() as u64 > total - inlined {
            tracing::debug!(%path, "attachment over inline cap; path ref only");
            continue;
        }
        inlined += bytes.len() as u64;
        images.push(json!({"type":"image","data":base64::engine::general_purpose::STANDARD.encode(bytes),"mimeType":mime}));
    }
    Value::Array(images)
}

#[cfg(test)]
mod tests {
    use super::{load_images, load_images_capped};

    #[tokio::test]
    async fn unsupported_or_missing_attachments_do_not_fail_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nrest").unwrap();
        let svg = dir.path().join("b.svg");
        std::fs::write(&svg, b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>").unwrap();
        let missing = dir.path().join("gone.jpg");
        let images = load_images(&[
            svg.display().to_string(),
            missing.display().to_string(),
            png.display().to_string(),
        ])
        .await;
        let images = images.as_array().unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0]["mimeType"], "image/png");
    }

    #[tokio::test]
    async fn files_over_either_cap_ride_as_path_refs_only() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let png = |name: &str, len: usize| {
            let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
            bytes.resize(len, b'.');
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path.display().to_string()
        };
        // 10 bytes each, 15 together: `big` is over its own cap, `third`
        // over what is left of the total, `small` still fits.
        let paths = [
            png("first", 8),
            png("big", 12),
            png("third", 8),
            png("small", 4),
        ];
        let images = load_images_capped(&paths, 10, 15).await;
        let lengths: Vec<_> = images
            .as_array()
            .unwrap()
            .iter()
            .map(|image| {
                base64::engine::general_purpose::STANDARD
                    .decode(image["data"].as_str().unwrap())
                    .unwrap()
                    .len()
            })
            .collect();
        assert_eq!(lengths, [8, 4]);
    }
}
