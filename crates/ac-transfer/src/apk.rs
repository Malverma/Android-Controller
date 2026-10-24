//! APK detection (spec §5.1/§5.9): `.apk` extension, ZIP magic, and an
//! `AndroidManifest.xml` entry in the central directory.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Extension check only (cheap, for hover feedback).
pub fn has_apk_ext(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("apk"))
}

/// Full check: extension, `PK\x03\x04`, and a manifest entry.
pub fn is_apk(path: &Path) -> bool {
    has_apk_ext(path) && std::fs::File::open(path).is_ok_and(|mut f| zip_has_manifest(&mut f).unwrap_or(false))
}

fn zip_has_manifest<R: Read + Seek>(f: &mut R) -> std::io::Result<bool> {
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)?;
    if &magic != b"PK\x03\x04" {
        return Ok(false);
    }
    // End of central directory: 22 bytes + up to 64 KiB of comment.
    let len = f.seek(SeekFrom::End(0))?;
    let tail_len = len.min(22 + 65_535);
    f.seek(SeekFrom::Start(len - tail_len))?;
    let mut tail = vec![0u8; tail_len as usize];
    f.read_exact(&mut tail)?;
    let Some(eocd) = (0..tail.len().saturating_sub(21)).rev().find(|&i| tail[i..].starts_with(b"PK\x05\x06")) else {
        return Ok(false);
    };
    let le32 = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as u64;
    let le16 = |b: &[u8], i: usize| u16::from_le_bytes(b[i..i + 2].try_into().unwrap()) as usize;
    let cd_size = le32(&tail, eocd + 12);
    let cd_off = le32(&tail, eocd + 16);
    // ZIP64 archives store 0xFFFFFFFF here; APKs that large are not a thing.
    if cd_off + cd_size > len || cd_size > 64 << 20 {
        return Ok(false);
    }
    f.seek(SeekFrom::Start(cd_off))?;
    let mut cd = vec![0u8; cd_size as usize];
    f.read_exact(&mut cd)?;
    let mut i = 0;
    while i + 46 <= cd.len() && cd[i..].starts_with(b"PK\x01\x02") {
        let (n, m, k) = (le16(&cd, i + 28), le16(&cd, i + 30), le16(&cd, i + 32));
        if cd.get(i + 46..i + 46 + n) == Some(b"AndroidManifest.xml") {
            return Ok(true);
        }
        i += 46 + n + m + k;
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A minimal stored-only ZIP with the given entry names.
    fn zip(names: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for name in names {
            let off = out.len() as u32;
            out.extend(b"PK\x03\x04");
            out.extend([0u8; 22]);
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(name.as_bytes());
            cd.extend(b"PK\x01\x02");
            cd.extend([0u8; 24]);
            cd.extend((name.len() as u16).to_le_bytes());
            cd.extend([0u8; 12]);
            cd.extend(off.to_le_bytes());
            cd.extend(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        out.extend(&cd);
        out.extend(b"PK\x05\x06");
        out.extend([0u8; 4]);
        out.extend((names.len() as u16).to_le_bytes());
        out.extend((names.len() as u16).to_le_bytes());
        out.extend((cd.len() as u32).to_le_bytes());
        out.extend(cd_off.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    #[test]
    fn finds_the_manifest() {
        assert!(zip_has_manifest(&mut Cursor::new(zip(&["classes.dex", "AndroidManifest.xml"]))).unwrap());
        assert!(!zip_has_manifest(&mut Cursor::new(zip(&["word/document.xml"]))).unwrap());
        assert!(!zip_has_manifest(&mut Cursor::new(b"\x89PNG\r\n\x1a\n........................".to_vec())).unwrap());
    }

    #[test]
    fn extension() {
        assert!(has_apk_ext(Path::new("/x/Foo.APK")));
        assert!(!has_apk_ext(Path::new("/x/foo.apks")));
    }
}
