//! Files attached to a draft: the list behind the composer's chips.
//!
//! Paste, drop and the paperclip picker all end in [`Draft::add_paths`], so one set of rules
//! decides what is accepted: a regular file that exists, within the daemon's size cap, not
//! already attached, and not past the daemon's count cap. The daemon checks again when the task
//! arrives (the files may change in between); these checks only keep a chip from promising what
//! the daemon will refuse.
//!
//! A pasted image has no file, so [`save_image`] writes it to a temporary file first.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use reno_proto::{ATTACHMENT_MAX_BYTES, ATTACHMENTS_MAX_COUNT, Attachment};

/// One attached file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Item {
    pub(crate) path: PathBuf,
    /// What the chip shows.
    pub(crate) name: String,
    pub(crate) mime: Option<String>,
    pub(crate) size: u64,
}

impl Item {
    pub(crate) fn size_label(&self) -> String {
        size_label(self.size)
    }

    fn attachment(&self) -> Attachment {
        Attachment {
            path: self.path.clone(),
            name: self.name.clone(),
            mime: self.mime.clone(),
        }
    }
}

/// The wire form of some files.
pub(crate) fn wire(items: &[Item]) -> Vec<Attachment> {
    items.iter().map(Item::attachment).collect()
}

/// "812 B", "1.4 KB", "3.2 MB".
pub(crate) fn size_label(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    }
}

/// The media type of a file by its extension, for the common kinds. `None` for the rest: the
/// daemon and the executor do not need it.
pub(crate) fn mime_for(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "toml" | "yaml" | "yml" | "rs" | "py" | "js" | "ts" | "go" | "c" | "h" | "cpp" | "java"
        | "swift" | "sh" => "text/plain",
        _ => return None,
    })
}

/// The files of one draft.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Draft {
    items: Vec<Item>,
}

impl Draft {
    pub(crate) fn items(&self) -> &[Item] {
        &self.items
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Attaches each readable file in `paths`. Returns one sentence for each that was not taken,
    /// for a toast; the others are attached whatever the rest did.
    pub(crate) fn add_paths(&mut self, paths: &[PathBuf]) -> Vec<String> {
        let mut refused = Vec::new();
        for path in paths {
            if let Err(why) = self.add(path) {
                refused.push(why);
            }
        }
        refused
    }

    fn add(&mut self, path: &Path) -> Result<(), String> {
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let meta = std::fs::metadata(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("{name} does not exist.")
            } else {
                format!("{name} cannot be read: {e}.")
            }
        })?;
        if meta.is_dir() {
            return Err(format!("{name} is a folder; attach files."));
        }
        if !meta.is_file() {
            return Err(format!("{name} is not a regular file."));
        }
        if meta.len() > ATTACHMENT_MAX_BYTES {
            return Err(format!(
                "{name} is {}; the limit is {}.",
                size_label(meta.len()),
                size_label(ATTACHMENT_MAX_BYTES)
            ));
        }
        // Opening is the readable check.
        std::fs::File::open(path).map_err(|e| format!("{name} cannot be read: {e}."))?;
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if self.items.iter().any(|i| i.path == path) {
            return Err(format!("{name} is already attached."));
        }
        if self.items.len() >= ATTACHMENTS_MAX_COUNT {
            return Err(format!(
                "At most {ATTACHMENTS_MAX_COUNT} files can be attached; {name} was left out."
            ));
        }
        self.items.push(Item {
            mime: mime_for(&path).map(str::to_owned),
            path,
            name,
            size: meta.len(),
        });
        Ok(())
    }

    pub(crate) fn remove(&mut self, index: usize) -> Option<Item> {
        (index < self.items.len()).then(|| self.items.remove(index))
    }

    /// Empties the list and returns what was in it.
    pub(crate) fn take(&mut self) -> Vec<Item> {
        std::mem::take(&mut self.items)
    }

    pub(crate) fn restore(&mut self, items: Vec<Item>) {
        self.items = items;
    }
}

/// Writes pasted image bytes to a new file under `dir` and returns its path. The name carries
/// the time, so two pastes do not meet.
pub(crate) fn save_image(dir: &Path, extension: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let mut path = dir.join(format!("pasted-image-{stamp}.{extension}"));
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("pasted-image-{stamp}-{n}.{extension}"));
        n += 1;
    }
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Where pasted images go.
pub(crate) fn paste_dir() -> PathBuf {
    std::env::temp_dir().join("reno-attachments")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn files_become_chips_with_a_name_a_size_and_a_type() {
        let dir = tempfile::tempdir().unwrap();
        let shot = file(dir.path(), "shot.PNG", &[0; 2048]);
        let notes = file(dir.path(), "notes.xyz", b"hi");
        let mut draft = Draft::default();
        assert!(draft.add_paths(&[shot, notes]).is_empty());
        let items = draft.items();
        assert_eq!(items[0].name, "shot.PNG");
        assert_eq!(items[0].mime.as_deref(), Some("image/png"));
        assert_eq!(items[0].size_label(), "2.0 KB");
        assert_eq!(items[1].mime, None);
        assert_eq!(items[1].size_label(), "2 B");
        let wire = wire(draft.items());
        assert_eq!(wire[0].name, "shot.PNG");
        assert_eq!(wire[1].mime, None);
    }

    #[test]
    fn what_cannot_be_attached_is_said_and_the_rest_still_is() {
        let dir = tempfile::tempdir().unwrap();
        let good = file(dir.path(), "a.txt", b"x");
        let mut draft = Draft::default();
        let refused = draft.add_paths(&[
            dir.path().join("missing.txt"),
            dir.path().to_path_buf(),
            good.clone(),
            good,
        ]);
        assert_eq!(draft.items().len(), 1);
        assert_eq!(refused.len(), 3);
        assert_eq!(refused[0], "missing.txt does not exist.");
        assert!(refused[1].ends_with("is a folder; attach files."));
        assert_eq!(refused[2], "a.txt is already attached.");
    }

    #[test]
    fn the_count_cap_leaves_the_extra_files_out() {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = (0..ATTACHMENTS_MAX_COUNT + 2)
            .map(|i| file(dir.path(), &format!("f{i}.txt"), b"x"))
            .collect();
        let mut draft = Draft::default();
        let refused = draft.add_paths(&paths);
        assert_eq!(draft.items().len(), ATTACHMENTS_MAX_COUNT);
        assert_eq!(refused.len(), 2);
        assert!(refused[0].starts_with("At most 10 files"));
    }

    #[test]
    fn a_file_over_the_size_cap_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.bin");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(ATTACHMENT_MAX_BYTES + 2 * 1024 * 1024)
            .unwrap();
        let mut draft = Draft::default();
        let refused = draft.add_paths(&[big]);
        assert!(draft.is_empty());
        assert_eq!(refused, ["big.bin is 27.0 MB; the limit is 25.0 MB."]);
    }

    #[test]
    fn removing_and_taking_and_restoring() {
        let dir = tempfile::tempdir().unwrap();
        let mut draft = Draft::default();
        draft.add_paths(&[
            file(dir.path(), "a.txt", b"x"),
            file(dir.path(), "b.txt", b"y"),
        ]);
        assert_eq!(draft.remove(0).unwrap().name, "a.txt");
        assert!(draft.remove(5).is_none());
        let taken = draft.take();
        assert!(draft.is_empty());
        draft.restore(taken);
        assert_eq!(draft.items()[0].name, "b.txt");
    }

    #[test]
    fn a_pasted_image_is_written_to_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = save_image(dir.path(), "png", b"one").unwrap();
        let b = save_image(dir.path(), "png", b"two").unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), b"one");
        let mut draft = Draft::default();
        assert!(draft.add_paths(&[a, b]).is_empty());
        assert_eq!(draft.items()[0].mime.as_deref(), Some("image/png"));
    }

    #[test]
    fn size_labels() {
        assert_eq!(size_label(0), "0 B");
        assert_eq!(size_label(1023), "1023 B");
        assert_eq!(size_label(1536), "1.5 KB");
        assert_eq!(size_label(3 * 1024 * 1024 + 200 * 1024), "3.2 MB");
    }
}
