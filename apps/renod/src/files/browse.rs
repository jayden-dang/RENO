//! Reading a tree: `fs.list`, `fs.read` and `fs.read_image`. Blocking file system calls; the
//! service runs each on a blocking thread.

use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

use reno_proto::{
    FS_IMAGE_CHUNK_BYTES, FS_IMAGE_MAX_BYTES, FS_READ_MAX_BYTES, FsEntry, FsEntryKind,
    FsImageChunk, FsReadResult, LineEnding, encode_chunk, image_hash,
};

use super::error::FsError;
use super::sandbox::{GIT_DIR, Root, join};

/// How much of a file is looked at to tell text from binary, as git does.
const SNIFF_BYTES: usize = 8 * 1024;

/// One page of a directory.
#[derive(Debug)]
pub(crate) struct Page {
    pub(crate) entries: Vec<FsEntry>,
    pub(crate) next_cursor: Option<String>,
}

/// The order of a listing: directories, then everything else; each by name, case-folded first.
type SortKey = (u8, String, String);

fn sort_key(kind: FsEntryKind, name: &str) -> SortKey {
    let rank = u8::from(kind != FsEntryKind::Dir);
    (rank, name.to_lowercase(), name.to_owned())
}

fn cursor_of(entry: &FsEntry) -> String {
    format!(
        "{}:{}",
        u8::from(entry.kind != FsEntryKind::Dir),
        entry.name
    )
}

fn parse_cursor(cursor: &str) -> Result<SortKey, FsError> {
    let (rank, name) = cursor.split_once(':').ok_or(FsError::BadCursor)?;
    let rank: u8 = rank.parse().map_err(|_| FsError::BadCursor)?;
    if rank > 1 || name.is_empty() {
        return Err(FsError::BadCursor);
    }
    Ok((rank, name.to_lowercase(), name.to_owned()))
}

fn millis(time: std::io::Result<std::time::SystemTime>) -> Option<u64> {
    let since = time.ok()?.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(since.as_millis()).ok()
}

/// The entries of `dir` after `cursor`, at most `limit`. `.git` is never listed.
pub(crate) fn list(
    root: &Root,
    dir: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Page, FsError> {
    let target = root.resolve(dir)?;
    if !target.real.is_dir() {
        return Err(FsError::NotADirectory(dir.to_owned()));
    }
    let after = cursor.map(parse_cursor).transpose()?;
    let reader = std::fs::read_dir(&target.real)
        .map_err(|error| FsError::io(format!("listing {dir:?}"), &error))?;
    let mut entries = Vec::new();
    for item in reader {
        let item = item.map_err(|error| FsError::io(format!("listing {dir:?}"), &error))?;
        let Ok(name) = item.file_name().into_string() else {
            continue;
        };
        if name == GIT_DIR {
            continue;
        }
        // The entry itself, links not followed. One that vanished since the listing began is
        // skipped.
        let Ok(meta) = std::fs::symlink_metadata(item.path()) else {
            continue;
        };
        let kind = if meta.is_dir() {
            FsEntryKind::Dir
        } else if meta.is_symlink() {
            FsEntryKind::Symlink
        } else {
            FsEntryKind::File
        };
        entries.push(FsEntry {
            path: join(&target.rel, &name),
            size: (kind != FsEntryKind::Dir).then_some(meta.len()),
            mtime_ms: millis(meta.modified()),
            name,
            kind,
        });
    }
    entries.sort_by_cached_key(|e| sort_key(e.kind, &e.name));
    if let Some(after) = &after {
        entries.retain(|e| &sort_key(e.kind, &e.name) > after);
    }
    let more = entries.len() > limit;
    entries.truncate(limit);
    let next_cursor = more.then(|| entries.last().map(cursor_of)).flatten();
    Ok(Page {
        entries,
        next_cursor,
    })
}

/// The file at `path` as text, or the fact that it is binary.
pub(crate) fn read(root: &Root, path: &str) -> Result<FsReadResult, FsError> {
    let target = root.resolve(path)?;
    let mut file = std::fs::File::open(&target.real)
        .map_err(|error| FsError::io(format!("opening {path:?}"), &error))?;
    let meta = file
        .metadata()
        .map_err(|error| FsError::io(format!("reading {path:?}"), &error))?;
    if !meta.is_file() {
        return Err(FsError::NotAFile(path.to_owned()));
    }
    let size = meta.len();
    let binary_result = |size| FsReadResult {
        path: target.rel.clone(),
        text: None,
        truncated: false,
        encoding: "binary".to_owned(),
        line_ending: LineEnding::None,
        size,
        binary: true,
        in_place: false,
    };

    let mut head = Vec::with_capacity(SNIFF_BYTES);
    file.by_ref()
        .take(SNIFF_BYTES as u64)
        .read_to_end(&mut head)
        .map_err(|error| FsError::io(format!("reading {path:?}"), &error))?;
    if head.contains(&0) {
        return Ok(binary_result(size));
    }
    // One more byte than the cap, to learn that the file is longer.
    let room = (FS_READ_MAX_BYTES + 1).saturating_sub(head.len());
    let mut body = head;
    file.take(room as u64)
        .read_to_end(&mut body)
        .map_err(|error| FsError::io(format!("reading {path:?}"), &error))?;
    let truncated = body.len() > FS_READ_MAX_BYTES;
    body.truncate(FS_READ_MAX_BYTES);

    let (text, encoding) = match std::str::from_utf8(&body) {
        Ok(text) => (text.to_owned(), "utf-8"),
        // The cut fell inside a character: drop its start.
        Err(error) if truncated && error.error_len().is_none() => {
            let valid = body.get(..error.valid_up_to()).unwrap_or_default();
            (String::from_utf8_lossy(valid).into_owned(), "utf-8")
        }
        Err(_) => (String::from_utf8_lossy(&body).into_owned(), "utf-8-lossy"),
    };
    Ok(FsReadResult {
        path: target.rel,
        line_ending: line_ending(&text),
        text: Some(text),
        truncated,
        encoding: encoding.to_owned(),
        size,
        binary: false,
        in_place: false,
    })
}

fn line_ending(text: &str) -> LineEnding {
    let lf = text.bytes().filter(|b| *b == b'\n').count();
    let crlf = text.matches("\r\n").count();
    match (lf - crlf, crlf) {
        (0, 0) => LineEnding::None,
        (_, 0) => LineEnding::Lf,
        (0, _) => LineEnding::Crlf,
        _ => LineEnding::Mixed,
    }
}

/// The media type of an image the viewer shows, by extension.
pub(crate) fn image_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        _ => return None,
    })
}

/// The chunk of the image at `path` that starts at `offset`.
pub(crate) fn read_image(root: &Root, path: &str, offset: u64) -> Result<FsImageChunk, FsError> {
    let target = root.resolve(path)?;
    let mime =
        image_mime(Path::new(&target.rel)).ok_or_else(|| FsError::NotAnImage(path.to_owned()))?;
    let file = std::fs::File::open(&target.real)
        .map_err(|error| FsError::io(format!("opening {path:?}"), &error))?;
    let meta = file
        .metadata()
        .map_err(|error| FsError::io(format!("reading {path:?}"), &error))?;
    if !meta.is_file() {
        return Err(FsError::NotAFile(path.to_owned()));
    }
    if meta.len() > FS_IMAGE_MAX_BYTES {
        return Err(FsError::TooLarge {
            path: path.to_owned(),
            size: meta.len(),
            max: FS_IMAGE_MAX_BYTES,
        });
    }
    // Bounded by the check above, but the file may grow while it is read.
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    file.take(FS_IMAGE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| FsError::io(format!("reading {path:?}"), &error))?;
    let size = bytes.len() as u64;
    if size > FS_IMAGE_MAX_BYTES {
        return Err(FsError::TooLarge {
            path: path.to_owned(),
            size,
            max: FS_IMAGE_MAX_BYTES,
        });
    }
    let start = usize::try_from(offset.min(size)).unwrap_or(bytes.len());
    let end = (start + FS_IMAGE_CHUNK_BYTES).min(bytes.len());
    Ok(FsImageChunk {
        path: target.rel,
        mime: mime.to_owned(),
        size,
        offset: start as u64,
        data: encode_chunk(bytes.get(start..end).unwrap_or_default()),
        eof: end >= bytes.len(),
        hash: image_hash(&bytes),
        in_place: false,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::os::unix::fs::symlink;

    use super::*;

    fn root() -> (tempfile::TempDir, Root) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("w");
        std::fs::create_dir_all(path.join("src")).unwrap();
        std::fs::create_dir_all(path.join("docs")).unwrap();
        std::fs::create_dir_all(path.join(".git")).unwrap();
        let root = Root::open(&path).unwrap();
        (dir, root)
    }

    fn put(root: &Root, rel: &str, bytes: &[u8]) {
        std::fs::write(root.path().join(rel), bytes).unwrap();
    }

    fn names(page: &Page) -> Vec<&str> {
        page.entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn a_listing_has_directories_first_each_by_name_and_hides_git() {
        let (_dir, root) = root();
        put(&root, "b.txt", b"bb");
        put(&root, "A.txt", b"a");
        put(&root, "Zed.txt", b"z");
        symlink("b.txt", root.path().join("link")).unwrap();
        let page = list(&root, "", None, 500).unwrap();
        assert_eq!(
            names(&page),
            ["docs", "src", "A.txt", "b.txt", "link", "Zed.txt"]
        );
        let link = page.entries.iter().find(|e| e.name == "link").unwrap();
        assert_eq!(link.kind, FsEntryKind::Symlink);
        let b = page.entries.iter().find(|e| e.name == "b.txt").unwrap();
        assert_eq!(
            (b.kind, b.size, b.path.as_str()),
            (FsEntryKind::File, Some(2), "b.txt")
        );
        assert!(b.mtime_ms.is_some());
        let docs = &page.entries[0];
        assert_eq!((docs.kind, docs.size), (FsEntryKind::Dir, None));
        assert_eq!(page.next_cursor, None);
    }

    #[test]
    fn pages_follow_the_cursor_and_cover_every_entry_once() {
        let (_dir, root) = root();
        for n in 0..7 {
            put(&root, &format!("f{n}.txt"), b"x");
        }
        let mut seen = Vec::new();
        let mut cursor = None;
        let mut pages = 0;
        loop {
            let page = list(&root, "", cursor.as_deref(), 3).unwrap();
            assert!(page.entries.len() <= 3);
            seen.extend(page.entries.iter().map(|e| e.name.clone()));
            pages += 1;
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        // 2 directories and 7 files: 9 entries, three per page.
        assert_eq!(pages, 3);
        assert_eq!(seen.len(), 9);
        assert_eq!(&seen[..2], ["docs", "src"]);
        let mut sorted = seen.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), 9);
        assert!(matches!(
            list(&root, "", Some("garbage"), 3),
            Err(FsError::BadCursor)
        ));
    }

    #[test]
    fn listing_a_file_or_a_forbidden_directory_is_refused() {
        let (_dir, root) = root();
        put(&root, "a.txt", b"x");
        assert!(matches!(
            list(&root, "a.txt", None, 5),
            Err(FsError::NotADirectory(_))
        ));
        assert!(matches!(
            list(&root, ".git", None, 5),
            Err(FsError::Rejected { .. })
        ));
        assert!(matches!(
            list(&root, "nope", None, 5),
            Err(FsError::NotFound(_))
        ));
    }

    #[test]
    fn text_comes_back_with_its_line_ending_and_a_binary_file_has_no_content() {
        let (_dir, root) = root();
        put(&root, "unix.txt", b"a\nb\n");
        put(&root, "dos.txt", b"a\r\nb\r\n");
        put(&root, "mixed.txt", b"a\r\nb\n");
        put(&root, "one.txt", b"abc");
        put(&root, "bin.dat", b"PK\0\0data");
        let unix = read(&root, "unix.txt").unwrap();
        assert_eq!(unix.text.as_deref(), Some("a\nb\n"));
        assert_eq!(
            (unix.line_ending, unix.size, unix.binary),
            (LineEnding::Lf, 4, false)
        );
        assert_eq!(unix.encoding, "utf-8");
        assert_eq!(
            read(&root, "dos.txt").unwrap().line_ending,
            LineEnding::Crlf
        );
        assert_eq!(
            read(&root, "mixed.txt").unwrap().line_ending,
            LineEnding::Mixed
        );
        assert_eq!(
            read(&root, "one.txt").unwrap().line_ending,
            LineEnding::None
        );
        let bin = read(&root, "bin.dat").unwrap();
        assert!(bin.binary);
        assert_eq!((bin.text, bin.size), (None, 8));
        assert!(matches!(read(&root, "src"), Err(FsError::NotAFile(_))));
    }

    #[test]
    fn a_large_text_is_cut_at_a_character_and_says_so() {
        let (_dir, root) = root();
        // A 3-byte character straddles the cap.
        let mut text = "a".repeat(FS_READ_MAX_BYTES - 1);
        text.push('€');
        text.push_str("tail");
        put(&root, "big.txt", text.as_bytes());
        let got = read(&root, "big.txt").unwrap();
        assert!(got.truncated);
        assert_eq!(got.size as usize, text.len());
        let cut = got.text.unwrap();
        assert_eq!(cut.len(), FS_READ_MAX_BYTES - 1);
        assert_eq!(got.encoding, "utf-8");
        // Exactly at the cap is not truncated.
        put(&root, "fit.txt", "b".repeat(FS_READ_MAX_BYTES).as_bytes());
        let fit = read(&root, "fit.txt").unwrap();
        assert!(!fit.truncated);
        assert_eq!(fit.text.unwrap().len(), FS_READ_MAX_BYTES);
    }

    #[test]
    fn invalid_utf8_is_replaced_and_marked_lossy() {
        let (_dir, root) = root();
        put(&root, "latin.txt", b"caf\xe9\n");
        let got = read(&root, "latin.txt").unwrap();
        assert_eq!(got.encoding, "utf-8-lossy");
        assert_eq!(got.text.as_deref(), Some("caf\u{fffd}\n"));
    }

    #[test]
    fn an_image_comes_in_chunks_with_one_hash_and_a_big_or_other_file_is_refused() {
        let (_dir, root) = root();
        let bytes: Vec<u8> = (0..FS_IMAGE_CHUNK_BYTES * 2 + 10)
            .map(|n| (n % 251) as u8)
            .collect();
        put(&root, "pic.PNG", &bytes);
        let mut got = Vec::new();
        let mut offset = 0;
        let mut hashes = std::collections::HashSet::new();
        loop {
            let chunk = read_image(&root, "pic.PNG", offset).unwrap();
            assert_eq!(
                (chunk.mime.as_str(), chunk.size),
                ("image/png", bytes.len() as u64)
            );
            assert_eq!(chunk.offset, offset);
            hashes.insert(chunk.hash.clone());
            let part = chunk.bytes().unwrap();
            assert!(part.len() <= FS_IMAGE_CHUNK_BYTES);
            offset += part.len() as u64;
            got.extend(part);
            if chunk.eof {
                break;
            }
        }
        assert_eq!(got, bytes);
        assert_eq!(hashes, [image_hash(&bytes)].into());

        put(&root, "logo.svg", b"<svg/>");
        assert_eq!(
            read_image(&root, "logo.svg", 0).unwrap().mime,
            "image/svg+xml"
        );
        put(&root, "notes.txt", b"x");
        assert!(matches!(
            read_image(&root, "notes.txt", 0),
            Err(FsError::NotAnImage(_))
        ));
        let huge = vec![0u8; usize::try_from(FS_IMAGE_MAX_BYTES).unwrap() + 1];
        put(&root, "huge.png", &huge);
        assert!(matches!(
            read_image(&root, "huge.png", 0),
            Err(FsError::TooLarge { .. })
        ));
        // Past the end is an empty last chunk, not an error.
        let past = read_image(&root, "pic.PNG", u64::MAX).unwrap();
        assert!(past.eof);
        assert_eq!(past.bytes().unwrap(), Vec::<u8>::new());
    }
}
