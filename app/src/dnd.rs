//! Drag-and-drop onto a tile (spec §5.1/§5.2): what was dropped, what the
//! hover hint says, and whether APKs get installed or copied.

use std::path::{Path, PathBuf};

use ac_transfer::apk::has_apk_ext;
use gtk::{gdk, glib};
use gtk::prelude::*;

/// The types a tile accepts: file lists from file managers and portals, and
/// plain text holding `file://` URIs (some browsers' download bars).
pub fn drop_types() -> [glib::Type; 2] {
    [gdk::FileList::static_type(), glib::GString::static_type()]
}

/// Local paths in a dropped value, or the reason it can't be used.
pub fn paths_from_value(value: &glib::Value) -> Result<Vec<PathBuf>, String> {
    if let Ok(list) = value.get::<gdk::FileList>() {
        let files = list.files();
        let paths: Vec<_> = files.iter().filter_map(|f| f.path()).collect();
        if paths.len() < files.len() {
            return Err("E_DROP_UNSUPPORTED_URI: only local files can be sent".into());
        }
        return Ok(paths);
    }
    if let Ok(text) = value.get::<String>() {
        return parse_uri_text(&text);
    }
    Err("Nothing to send in that drop".into())
}

/// `text/uri-list`-style text: one URI per line, `#` comments allowed.
pub fn parse_uri_text(text: &str) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        if !line.starts_with("file:") {
            return Err(format!("E_DROP_UNSUPPORTED_URI: can't send {line}"));
        }
        match glib::filename_from_uri(line) {
            Ok((p, _)) => out.push(p),
            Err(_) => return Err(format!("E_DROP_UNSUPPORTED_URI: can't send {line}")),
        }
    }
    if out.is_empty() { Err("Nothing to send in that drop".into()) } else { Ok(out) }
}

/// What a drop does with APKs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApkChoice {
    Install,
    Push,
    Ask,
}

/// `transfer.apk_drop_action` with the alternate modifier flipping install ⇄ push.
pub fn apk_choice(action: &str, alternate: bool) -> ApkChoice {
    match (action, alternate) {
        ("push", false) | ("install", true) => ApkChoice::Push,
        ("push", true) => ApkChoice::Install,
        ("ask", _) => ApkChoice::Ask,
        _ => ApkChoice::Install,
    }
}

/// `transfer.apk_alternate_modifier` as a GDK modifier.
pub fn modifier(name: &str) -> gdk::ModifierType {
    match name {
        "ctrl" => gdk::ModifierType::CONTROL_MASK,
        "alt" => gdk::ModifierType::ALT_MASK,
        _ => gdk::ModifierType::SHIFT_MASK,
    }
}

pub fn modifier_label(name: &str) -> &'static str {
    match name {
        "ctrl" => "Ctrl",
        "alt" => "Alt",
        _ => "Shift",
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// Hover text, e.g. "Drop to send 3 files → /sdcard/Download" or
/// "Drop to install Foo.apk (Shift: copy as file)".
pub fn hint(paths: &[PathBuf], target_dir: &str, choice: ApkChoice, modifier: &str, phones: usize) -> String {
    let (apks, others): (Vec<&PathBuf>, Vec<&PathBuf>) = paths.iter().partition(|p| has_apk_ext(p));
    let to = if phones > 1 { format!(" → {phones} phones") } else { String::new() };
    let describe = |items: &[&PathBuf]| match items {
        [one] if one.is_dir() => format!("folder {}", file_name(one)),
        [one] => file_name(one),
        many => format!("{} items", many.len()),
    };
    if apks.is_empty() || choice == ApkChoice::Push {
        let all: Vec<_> = paths.iter().collect();
        let alt = if apks.is_empty() { String::new() } else { format!("\n({}: install instead)", modifier_label(modifier)) };
        return format!("Drop to copy {} → {target_dir}{to}{alt}", describe(&all));
    }
    let install = match apks.as_slice() {
        [one] => format!("install {}", file_name(one)),
        many => format!("install {} apps", many.len()),
    };
    let rest = if others.is_empty() { String::new() } else { format!(" and copy {}", describe(&others)) };
    let alt = match choice {
        ApkChoice::Ask => String::new(),
        _ => format!("\n({}: copy as file)", modifier_label(modifier)),
    };
    format!("Drop to {install}{rest}{to}{alt}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apk_action_resolution() {
        assert_eq!(apk_choice("install", false), ApkChoice::Install);
        assert_eq!(apk_choice("install", true), ApkChoice::Push);
        assert_eq!(apk_choice("push", false), ApkChoice::Push);
        assert_eq!(apk_choice("push", true), ApkChoice::Install);
        assert_eq!(apk_choice("ask", true), ApkChoice::Ask);
    }

    #[test]
    fn uri_text() {
        assert_eq!(
            parse_uri_text("# comment\r\nfile:///home/u/a%20b.jpg\r\nfile:///tmp/c\n").unwrap(),
            vec![PathBuf::from("/home/u/a b.jpg"), PathBuf::from("/tmp/c")]
        );
        assert!(parse_uri_text("https://example.com/x.jpg").unwrap_err().starts_with("E_DROP_UNSUPPORTED_URI"));
        assert!(parse_uri_text("\n").is_err());
    }

    #[test]
    fn hints() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(hint(&[p("/x/a.jpg")], "/sdcard/Download", ApkChoice::Install, "shift", 1), "Drop to copy a.jpg → /sdcard/Download");
        assert_eq!(
            hint(&[p("/x/a.jpg"), p("/x/b.jpg")], "/sdcard/Download", ApkChoice::Install, "shift", 3),
            "Drop to copy 2 items → /sdcard/Download → 3 phones"
        );
        assert_eq!(
            hint(&[p("/x/Foo.apk")], "/sdcard/Download", ApkChoice::Install, "shift", 1),
            "Drop to install Foo.apk\n(Shift: copy as file)"
        );
        assert_eq!(
            hint(&[p("/x/Foo.apk")], "/sdcard/Download", ApkChoice::Push, "ctrl", 1),
            "Drop to copy Foo.apk → /sdcard/Download\n(Ctrl: install instead)"
        );
        assert_eq!(
            hint(&[p("/x/Foo.apk"), p("/x/Bar.apk"), p("/x/n.txt")], "/d", ApkChoice::Ask, "shift", 1),
            "Drop to install 2 apps and copy n.txt"
        );
    }
}
