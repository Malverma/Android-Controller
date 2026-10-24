//! Drop expansion, segment math and remote naming.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One file to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub local: PathBuf,
    /// Remote directory relative to the target directory ("" for the target
    /// itself), `/`-separated.
    pub rel_dir: String,
    pub size: u64,
}

/// Expands dropped paths into files. Folders keep their structure under
/// `<target>/<folder name>/`; symlinks are not followed, hidden files are
/// included. Returns the files and the unreadable paths.
pub fn expand(paths: &[PathBuf], recursive: bool) -> (Vec<Item>, Vec<PathBuf>) {
    let mut items = Vec::new();
    let mut errors = Vec::new();
    for p in paths {
        match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if recursive {
                    walk(p, &name, &mut items, &mut errors);
                } else {
                    errors.push(p.clone());
                }
            }
            // A dropped symlink to a file is what the user pointed at: follow it.
            Ok(_) => match std::fs::metadata(p) {
                Ok(m) if m.is_file() => items.push(Item { local: p.clone(), rel_dir: String::new(), size: m.len() }),
                _ => errors.push(p.clone()),
            },
            Err(_) => errors.push(p.clone()),
        }
    }
    (items, errors)
}

fn walk(dir: &Path, rel: &str, items: &mut Vec<Item>, errors: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        errors.push(dir.to_owned());
        return;
    };
    let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let Ok(ft) = e.file_type() else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        if ft.is_dir() {
            walk(&e.path(), &format!("{rel}/{name}"), items, errors);
        } else if ft.is_file() {
            match e.metadata() {
                Ok(m) => items.push(Item { local: e.path(), rel_dir: rel.to_owned(), size: m.len() }),
                Err(_) => errors.push(e.path()),
            }
        }
    }
}

/// Joins remote path pieces with single `/`s, skipping empty ones.
pub fn join(base: &str, parts: &[&str]) -> String {
    let mut out = base.trim_end_matches('/').to_owned();
    for p in parts {
        let p = p.trim_matches('/');
        if !p.is_empty() {
            out.push('/');
            out.push_str(p);
        }
    }
    if out.is_empty() { "/".into() } else { out }
}

/// `(start, len)` of each segment still to send, from `committed` up to
/// `size`. A zero-byte file is one empty segment.
pub fn segments(size: u64, committed: u64, segment: u64) -> Vec<(u64, u64)> {
    assert!(segment > 0);
    if size == 0 {
        return vec![(0, 0)];
    }
    let mut out = Vec::new();
    let mut at = committed.min(size);
    while at < size {
        let len = segment.min(size - at);
        out.push((at, len));
        at += len;
    }
    out
}

/// Where to resume when the device holds a `.part` of `part_size` bytes:
/// the last whole segment boundary. A complete part needs nothing more; one
/// longer than the file is stale and starts over.
pub fn part_size_resume(part_size: u64, segment: u64, size: u64) -> u64 {
    match part_size {
        p if p > size => 0,
        p if p == size => size,
        p => p - p % segment,
    }
}

/// `"name.ext"` → `("name", ".ext")`. Dotfiles and names without a dot
/// have no extension; `.tar.gz` style double extensions keep only the last.
pub fn split_name(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// First of `name`, `name (1).ext`, `name (2).ext`, … not in `taken`.
pub fn free_name(name: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(name) {
        return name.to_owned();
    }
    let (stem, ext) = split_name(name);
    (1..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|c| !taken.contains(c))
        .expect("unbounded")
}

/// True for files the gallery or music apps index (spec §5.8).
pub fn is_media(name: &str) -> bool {
    const EXT: &[&str] = &[
        "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "bmp", "dng", "avif", "mp4", "mkv", "webm", "mov",
        "3gp", "avi", "m4v", "mp3", "m4a", "aac", "flac", "ogg", "opus", "wav", "amr", "mid", "midi",
    ];
    let (_, ext) = split_name(name);
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    EXT.contains(&ext.as_str())
}

/// Percent-encodes a path for a `file://` URI (keeps `/` and unreserved bytes).
pub fn url_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    #[test]
    fn segment_plan() {
        let seg = 32 * MIB;
        assert_eq!(segments(0, 0, seg), vec![(0, 0)]);
        assert_eq!(segments(1, 0, seg), vec![(0, 1)]);
        assert_eq!(segments(seg - 1, 0, seg), vec![(0, seg - 1)]);
        assert_eq!(segments(seg, 0, seg), vec![(0, seg)]);
        assert_eq!(segments(seg + 1, 0, seg), vec![(0, seg), (seg, 1)]);
        let big = 5 * 1024 * MIB;
        let plan = segments(big, 0, seg);
        assert_eq!(plan.len(), 160);
        assert_eq!(plan.iter().map(|s| s.1).sum::<u64>(), big);
        assert!(plan.windows(2).all(|w| w[0].0 + w[0].1 == w[1].0), "contiguous");
        assert_eq!(segments(big, 3 * seg, seg)[0], (3 * seg, seg), "resume");
        assert!(segments(seg, seg, seg).is_empty(), "nothing left");
    }

    #[test]
    fn resume_point() {
        let seg = 32 * MIB;
        assert_eq!(part_size_resume(0, seg, 100 * MIB), 0);
        assert_eq!(part_size_resume(seg + 5, seg, 100 * MIB), seg, "torn segment dropped");
        assert_eq!(part_size_resume(2 * seg, seg, 100 * MIB), 2 * seg);
        assert_eq!(part_size_resume(3 * seg, seg, 3 * seg), 3 * seg);
        assert_eq!(part_size_resume(seg + 7, seg, seg + 7), seg + 7, "complete, not yet renamed");
        assert_eq!(part_size_resume(4 * seg, seg, 3 * seg), 0, "stale part");
    }

    #[test]
    fn conflict_names() {
        let taken: HashSet<String> = ["a.jpg", "a (1).jpg", ".bashrc", "notes"].map(String::from).into();
        assert_eq!(free_name("b.jpg", &taken), "b.jpg");
        assert_eq!(free_name("a.jpg", &taken), "a (2).jpg");
        assert_eq!(free_name(".bashrc", &taken), ".bashrc (1)");
        assert_eq!(free_name("notes", &taken), "notes (1)");
        assert_eq!(split_name("x.tar.gz"), ("x.tar", ".gz"));
    }

    #[test]
    fn joins_remote_paths() {
        assert_eq!(join("/sdcard/Download/", &["", "Pics", "a.jpg"]), "/sdcard/Download/Pics/a.jpg");
        assert_eq!(join("/sdcard/Download", &["Pics/2024"]), "/sdcard/Download/Pics/2024");
        assert_eq!(join("/", &[]), "/");
    }

    #[test]
    fn media_and_uris() {
        assert!(is_media("IMG_1.JPG") && is_media("clip.mp4") && !is_media("doc.pdf") && !is_media("jpg"));
        assert_eq!(url_encode_path("/sdcard/Download/a b#1.jpg"), "/sdcard/Download/a%20b%231.jpg");
        assert_eq!(url_encode_path("/é"), "/%C3%A9");
    }

    #[test]
    fn expands_folders() {
        let dir = std::env::temp_dir().join(format!("ac-transfer-expand-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Pics/sub")).unwrap();
        std::fs::write(dir.join("Pics/a.jpg"), b"aa").unwrap();
        std::fs::write(dir.join("Pics/.hidden"), b"h").unwrap();
        std::fs::write(dir.join("Pics/sub/b.png"), b"bbb").unwrap();
        std::fs::write(dir.join("top.txt"), b"t").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("Pics"), dir.join("Pics/sub/loop")).unwrap();

        let (items, errors) = expand(&[dir.join("Pics"), dir.join("top.txt"), dir.join("missing")], true);
        let got: Vec<_> = items.iter().map(|i| (i.rel_dir.as_str(), i.local.file_name().unwrap().to_str().unwrap(), i.size)).collect();
        assert_eq!(got, vec![("Pics", ".hidden", 1), ("Pics", "a.jpg", 2), ("Pics/sub", "b.png", 3), ("", "top.txt", 1)]);
        assert_eq!(errors, vec![dir.join("missing")]);
        let (_, errors) = expand(&[dir.join("Pics")], false);
        assert_eq!(errors.len(), 1, "folders refused when not recursive");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
