//! APK detection (spec §5.1/§5.9): `.apk` extension, ZIP magic, and an
//! `AndroidManifest.xml` entry in the central directory; and the app's
//! package name and version from that (binary XML) manifest.

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
    Ok(find_entry(f, MANIFEST)?.is_some())
}

const MANIFEST: &str = "AndroidManifest.xml";

/// A file in a ZIP archive: where its local header is and how it is stored.
struct Entry {
    method: u16,
    compressed: u64,
    size: u64,
    local_offset: u64,
}

fn find_entry<R: Read + Seek>(f: &mut R, wanted: &str) -> std::io::Result<Option<Entry>> {
    f.seek(SeekFrom::Start(0))?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)?;
    if &magic != b"PK\x03\x04" {
        return Ok(None);
    }
    // End of central directory: 22 bytes + up to 64 KiB of comment.
    let len = f.seek(SeekFrom::End(0))?;
    let tail_len = len.min(22 + 65_535);
    f.seek(SeekFrom::Start(len - tail_len))?;
    let mut tail = vec![0u8; tail_len as usize];
    f.read_exact(&mut tail)?;
    let Some(eocd) = (0..tail.len().saturating_sub(21)).rev().find(|&i| tail[i..].starts_with(b"PK\x05\x06")) else {
        return Ok(None);
    };
    let cd_size = le32(&tail, eocd + 12) as u64;
    let cd_off = le32(&tail, eocd + 16) as u64;
    // ZIP64 archives store 0xFFFFFFFF here; APKs that large are not a thing.
    if cd_off + cd_size > len || cd_size > 64 << 20 {
        return Ok(None);
    }
    f.seek(SeekFrom::Start(cd_off))?;
    let mut cd = vec![0u8; cd_size as usize];
    f.read_exact(&mut cd)?;
    let mut i = 0;
    while i + 46 <= cd.len() && cd[i..].starts_with(b"PK\x01\x02") {
        let (n, m, k) = (le16(&cd, i + 28) as usize, le16(&cd, i + 30) as usize, le16(&cd, i + 32) as usize);
        if cd.get(i + 46..i + 46 + n) == Some(wanted.as_bytes()) {
            return Ok(Some(Entry {
                method: le16(&cd, i + 10),
                compressed: le32(&cd, i + 20) as u64,
                size: le32(&cd, i + 24) as u64,
                local_offset: le32(&cd, i + 42) as u64,
            }));
        }
        i += 46 + n + m + k;
    }
    Ok(None)
}

fn le16(b: &[u8], i: usize) -> u16 {
    b.get(i..i + 2).map_or(0, |s| u16::from_le_bytes(s.try_into().unwrap()))
}

fn le32(b: &[u8], i: usize) -> u32 {
    b.get(i..i + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}

/// Reads and (if needed) inflates one entry.
fn read_entry<R: Read + Seek>(f: &mut R, e: &Entry) -> Result<Vec<u8>, String> {
    if e.size > 32 << 20 || e.compressed > 32 << 20 {
        return Err("the manifest is implausibly large".into());
    }
    let io = |err: std::io::Error| err.to_string();
    f.seek(SeekFrom::Start(e.local_offset)).map_err(io)?;
    let mut h = [0u8; 30];
    f.read_exact(&mut h).map_err(io)?;
    if &h[..4] != b"PK\x03\x04" {
        return Err("broken ZIP local header".into());
    }
    let skip = le16(&h, 26) as i64 + le16(&h, 28) as i64;
    f.seek(SeekFrom::Current(skip)).map_err(io)?;
    let mut data = vec![0u8; e.compressed as usize];
    f.read_exact(&mut data).map_err(io)?;
    match e.method {
        0 => Ok(data),
        8 => miniz_oxide::inflate::decompress_to_vec_with_limit(&data, 32 << 20).map_err(|e| format!("inflate: {e:?}")),
        m => Err(format!("unsupported ZIP compression {m}")),
    }
}

/// What the manifest says about an APK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApkInfo {
    pub package: String,
    pub version_name: Option<String>,
    pub version_code: Option<u64>,
}

/// The package name and version of an APK file.
pub fn info(path: &Path) -> Result<ApkInfo, String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let entry = find_entry(&mut f, MANIFEST).map_err(|e| e.to_string())?.ok_or("not an APK (no AndroidManifest.xml)")?;
    parse_manifest(&read_entry(&mut f, &entry)?)
}

// Binary XML (frameworks/base/libs/androidfw/include/androidfw/ResourceTypes.h).
const RES_XML_TYPE: u16 = 0x0003;
const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_XML_RESOURCE_MAP_TYPE: u16 = 0x0180;
const RES_XML_START_ELEMENT_TYPE: u16 = 0x0102;
const ATTR_VERSION_CODE: u32 = 0x0101_021b;
const ATTR_VERSION_NAME: u32 = 0x0101_021c;
const TYPE_STRING: u8 = 0x03;
const TYPE_INT_DEC: u8 = 0x10;
const TYPE_INT_HEX: u8 = 0x11;

/// Reads `<manifest package=… android:versionName=… android:versionCode=…>`.
pub fn parse_manifest(xml: &[u8]) -> Result<ApkInfo, String> {
    if le16(xml, 0) != RES_XML_TYPE {
        return Err("the manifest is not binary XML".into());
    }
    let mut strings = Vec::new();
    let mut res_ids: Vec<u32> = Vec::new();
    let mut at = le16(xml, 2) as usize;
    while at + 8 <= xml.len() {
        let (kind, header, size) = (le16(xml, at), le16(xml, at + 2) as usize, le32(xml, at + 4) as usize);
        if size < 8 || at + size > xml.len() {
            break;
        }
        let chunk = &xml[at..at + size];
        match kind {
            RES_STRING_POOL_TYPE => strings = string_pool(chunk)?,
            RES_XML_RESOURCE_MAP_TYPE => res_ids = (header..size).step_by(4).map(|i| le32(chunk, i)).collect(),
            RES_XML_START_ELEMENT_TYPE => {
                let name = le32(chunk, header + 4) as usize;
                if strings.get(name).map(String::as_str) != Some("manifest") {
                    return Err("the manifest's first element is not <manifest>".into());
                }
                return manifest_attrs(chunk, header, &strings, &res_ids);
            }
            _ => {}
        }
        at += size;
    }
    Err("no <manifest> element".into())
}

fn manifest_attrs(chunk: &[u8], header: usize, strings: &[String], res_ids: &[u32]) -> Result<ApkInfo, String> {
    let ext = header;
    let (start, size, count) = (le16(chunk, ext + 8) as usize, le16(chunk, ext + 10) as usize, le16(chunk, ext + 12) as usize);
    let mut info = ApkInfo { package: String::new(), version_name: None, version_code: None };
    for n in 0..count {
        let a = ext + start + n * size.max(20);
        let name_idx = le32(chunk, a + 4) as usize;
        let raw = le32(chunk, a + 8) as usize;
        let data_type = chunk.get(a + 15).copied().unwrap_or(0);
        let data = le32(chunk, a + 16);
        let name = strings.get(name_idx).map(String::as_str).unwrap_or("");
        let id = res_ids.get(name_idx).copied();
        let text = || strings.get(raw).cloned().or_else(|| (data_type == TYPE_STRING).then(|| strings.get(data as usize).cloned()).flatten());
        if name == "package" {
            info.package = text().unwrap_or_default();
        } else if id == Some(ATTR_VERSION_NAME) || name == "versionName" {
            info.version_name = text();
        } else if (id == Some(ATTR_VERSION_CODE) || name == "versionCode") && matches!(data_type, TYPE_INT_DEC | TYPE_INT_HEX) {
            info.version_code = Some(data as u64);
        }
    }
    if info.package.is_empty() {
        return Err("the manifest names no package".into());
    }
    Ok(info)
}

fn string_pool(chunk: &[u8]) -> Result<Vec<String>, String> {
    let count = le32(chunk, 8) as usize;
    let utf8 = le32(chunk, 16) & (1 << 8) != 0;
    let strings_start = le32(chunk, 20) as usize;
    let header = le16(chunk, 2) as usize;
    if count > 1 << 20 {
        return Err("bad string pool".into());
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = strings_start + le32(chunk, header + i * 4) as usize;
        out.push(if utf8 { utf8_string(chunk, off) } else { utf16_string(chunk, off) }.unwrap_or_default());
    }
    Ok(out)
}

/// Lengths are 1 byte, or 2 with the high bit set (u16 units for UTF-16).
fn utf8_string(b: &[u8], mut at: usize) -> Option<String> {
    let len8 = |at: &mut usize| -> Option<usize> {
        let first = *b.get(*at)? as usize;
        *at += 1;
        if first & 0x80 != 0 {
            let second = *b.get(*at)? as usize;
            *at += 1;
            Some(((first & 0x7f) << 8) | second)
        } else {
            Some(first)
        }
    };
    let _chars = len8(&mut at)?;
    let bytes = len8(&mut at)?;
    Some(String::from_utf8_lossy(b.get(at..at + bytes)?).into_owned())
}

fn utf16_string(b: &[u8], at: usize) -> Option<String> {
    let first = le16(b, at) as usize;
    let (len, start) = if first & 0x8000 != 0 { (((first & 0x7fff) << 16) | le16(b, at + 2) as usize, at + 4) } else { (first, at + 2) };
    let units: Vec<u16> = (0..len).map(|i| le16(b, start + i * 2)).collect();
    (start + len * 2 <= b.len()).then(|| String::from_utf16_lossy(&units))
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

    /// A binary-XML manifest with `package`, `versionName` (string) and
    /// `versionCode` (int), UTF-8 or UTF-16 string pool, as aapt writes it.
    fn manifest(utf8: bool) -> Vec<u8> {
        let names = ["versionCode", "versionName", "package", "manifest", "com.example.game", "1.4.2"];
        let mut pool_strings = Vec::new();
        let mut offsets = Vec::new();
        for n in names {
            offsets.push(pool_strings.len() as u32);
            if utf8 {
                pool_strings.extend([n.len() as u8, n.len() as u8]);
                pool_strings.extend(n.as_bytes());
                pool_strings.push(0);
            } else {
                pool_strings.extend((n.len() as u16).to_le_bytes());
                for u in n.encode_utf16() {
                    pool_strings.extend(u.to_le_bytes());
                }
                pool_strings.extend([0, 0]);
            }
        }
        while pool_strings.len() % 4 != 0 {
            pool_strings.push(0);
        }
        let header = 28usize;
        let strings_start = header + offsets.len() * 4;
        let mut pool = Vec::new();
        pool.extend(1u16.to_le_bytes());
        pool.extend((header as u16).to_le_bytes());
        pool.extend(((strings_start + pool_strings.len()) as u32).to_le_bytes());
        pool.extend((names.len() as u32).to_le_bytes());
        pool.extend(0u32.to_le_bytes());
        pool.extend((if utf8 { 1u32 << 8 } else { 0 }).to_le_bytes());
        pool.extend((strings_start as u32).to_le_bytes());
        pool.extend(0u32.to_le_bytes());
        for o in offsets {
            pool.extend(o.to_le_bytes());
        }
        pool.extend(pool_strings);
        let mut map = Vec::new();
        map.extend(0x0180u16.to_le_bytes());
        map.extend(8u16.to_le_bytes());
        map.extend(16u32.to_le_bytes());
        map.extend(0x0101_021bu32.to_le_bytes());
        map.extend(0x0101_021cu32.to_le_bytes());
        let attr = |name: u32, raw: u32, ty: u8, data: u32| {
            let mut a = Vec::new();
            a.extend(u32::MAX.to_le_bytes());
            a.extend(name.to_le_bytes());
            a.extend(raw.to_le_bytes());
            a.extend(8u16.to_le_bytes());
            a.push(0);
            a.push(ty);
            a.extend(data.to_le_bytes());
            a
        };
        let attrs = [attr(0, u32::MAX, 0x10, 42), attr(1, 5, 0x03, 5), attr(2, 4, 0x03, 4)];
        let mut el = Vec::new();
        el.extend(0x0102u16.to_le_bytes());
        el.extend(16u16.to_le_bytes());
        el.extend(((16 + 20 + 60) as u32).to_le_bytes());
        el.extend(1u32.to_le_bytes());
        el.extend(u32::MAX.to_le_bytes());
        el.extend(u32::MAX.to_le_bytes());
        el.extend(3u32.to_le_bytes());
        el.extend(20u16.to_le_bytes());
        el.extend(20u16.to_le_bytes());
        el.extend(3u16.to_le_bytes());
        el.extend([0u8; 6]);
        for a in attrs {
            el.extend(a);
        }
        let mut xml = Vec::new();
        xml.extend(3u16.to_le_bytes());
        xml.extend(8u16.to_le_bytes());
        xml.extend(((8 + pool.len() + map.len() + el.len()) as u32).to_le_bytes());
        xml.extend(pool);
        xml.extend(map);
        xml.extend(el);
        xml
    }

    #[test]
    fn reads_package_and_version() {
        for utf8 in [true, false] {
            let info = parse_manifest(&manifest(utf8)).unwrap();
            assert_eq!(info.package, "com.example.game", "utf8={utf8}");
            assert_eq!(info.version_name.as_deref(), Some("1.4.2"));
            assert_eq!(info.version_code, Some(42));
        }
        assert!(parse_manifest(b"<?xml version=\"1.0\"?>").is_err());
    }

    #[test]
    fn extension() {
        assert!(has_apk_ext(Path::new("/x/Foo.APK")));
        assert!(!has_apk_ext(Path::new("/x/foo.apks")));
    }
}
