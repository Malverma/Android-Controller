//! What is on a phone's screen, as a list of elements: from the helper's
//! accessibility service (fast, any time) or `uiautomator dump` (ADB only,
//! slower). Both give the same JSON node shape:
//!
//! `{"text", "desc", "id", "cls", "pkg", "b": [left, top, right, bottom],
//!   "click", "long", "scroll", "edit", "check": null|bool, "sel", "on"}`
//!
//! Coordinates are the phone's screen pixels in its current orientation.

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Node {
    pub text: String,
    pub desc: String,
    pub id: String,
    pub cls: String,
    pub pkg: String,
    pub bounds: [i32; 4],
    pub click: bool,
    pub long: bool,
    pub scroll: bool,
    pub edit: bool,
    pub check: Option<bool>,
    pub selected: bool,
    pub enabled: bool,
}

impl Node {
    pub fn center(&self) -> (i32, i32) {
        ((self.bounds[0] + self.bounds[2]) / 2, (self.bounds[1] + self.bounds[3]) / 2)
    }

    fn size(&self) -> (i32, i32) {
        (self.bounds[2] - self.bounds[0], self.bounds[3] - self.bounds[1])
    }

    /// Worth showing: says something or does something.
    fn interesting(&self) -> bool {
        let (w, h) = self.size();
        w > 0 && h > 0 && (!self.text.is_empty() || !self.desc.is_empty() || self.click || self.long || self.scroll || self.edit || self.check.is_some())
    }

    pub fn from_json(v: &Value) -> Option<Node> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let b = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
        let arr = v.get("b")?.as_array()?;
        let mut bounds = [0; 4];
        for (i, x) in arr.iter().take(4).enumerate() {
            bounds[i] = x.as_i64()? as i32;
        }
        Some(Node {
            text: s("text"),
            desc: s("desc"),
            id: s("id"),
            cls: s("cls"),
            pkg: s("pkg"),
            bounds,
            click: b("click"),
            long: b("long"),
            scroll: b("scroll"),
            edit: b("edit"),
            check: v.get("check").and_then(Value::as_bool),
            selected: b("sel"),
            enabled: v.get("on").and_then(Value::as_bool).unwrap_or(true),
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "text": self.text, "desc": self.desc, "id": self.id, "cls": self.cls, "pkg": self.pkg,
            "b": self.bounds, "click": self.click, "long": self.long, "scroll": self.scroll, "edit": self.edit,
            "check": self.check, "sel": self.selected, "on": self.enabled,
        })
    }

    /// `Button "Play" id=btn_play @540,1820 300x120 click`
    pub fn line(&self) -> String {
        let cls = self.cls.rsplit('.').next().unwrap_or_default();
        let mut s = cls.to_owned();
        if !self.text.is_empty() {
            s.push_str(&format!(" \"{}\"", clip(&self.text, 80)));
        }
        if !self.desc.is_empty() && self.desc != self.text {
            s.push_str(&format!(" desc=\"{}\"", clip(&self.desc, 60)));
        }
        if !self.id.is_empty() {
            s.push_str(&format!(" id={}", short_id(&self.id)));
        }
        let (x, y) = self.center();
        let (w, h) = self.size();
        s.push_str(&format!(" @{x},{y} {w}x{h}"));
        for (on, flag) in [(self.click, "click"), (self.long, "long-press"), (self.scroll, "scroll"), (self.edit, "edit"), (self.selected, "selected")] {
            if on {
                s.push(' ');
                s.push_str(flag);
            }
        }
        match self.check {
            Some(true) => s.push_str(" checked"),
            Some(false) => s.push_str(" unchecked"),
            None => {}
        }
        if !self.enabled {
            s.push_str(" disabled");
        }
        s
    }
}

fn clip(s: &str, n: usize) -> String {
    let one_line = s.replace('\n', "⏎");
    if one_line.chars().count() <= n { one_line } else { format!("{}…", one_line.chars().take(n).collect::<String>()) }
}

/// `com.example:id/btn_play` → `btn_play`.
pub fn short_id(id: &str) -> &str {
    id.split_once(":id/").map_or(id, |(_, s)| s)
}

/// What to look for: any of text, resource id, content description.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    pub text: Option<String>,
    pub id: Option<String>,
    pub desc: Option<String>,
    /// Which match (0 = first) when several fit.
    pub index: usize,
}

impl Query {
    pub fn from_params(p: &Value) -> Option<Query> {
        let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_owned).filter(|s| !s.is_empty());
        let q = Query { text: s("text"), id: s("id"), desc: s("desc"), index: p.get("index").and_then(Value::as_u64).unwrap_or(0) as usize };
        (q.text.is_some() || q.id.is_some() || q.desc.is_some()).then_some(q)
    }

    pub fn to_json(&self) -> Value {
        json!({ "text": self.text, "id": self.id, "desc": self.desc, "index": self.index })
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(t) = &self.text {
            parts.push(format!("text \"{t}\""));
        }
        if let Some(i) = &self.id {
            parts.push(format!("id {i}"));
        }
        if let Some(d) = &self.desc {
            parts.push(format!("description \"{d}\""));
        }
        parts.join(" and ")
    }
}

/// Matches, best first: exact (ignoring case) before contains, the closest
/// in length among those; clickable before not; then top to bottom. A text
/// query also matches the description (icon buttons have no text).
pub fn find<'a>(nodes: &'a [Node], q: &Query) -> Vec<&'a Node> {
    let norm = |s: &str| s.trim().to_lowercase();
    // (inexact, length difference) of `want` in `have`, or no match.
    let fit = |want: &str, have: &str| -> Option<(bool, usize)> {
        let (w, h) = (norm(want), norm(have));
        if h == w {
            Some((false, 0))
        } else if !w.is_empty() && h.contains(&w) {
            Some((true, h.len() - w.len()))
        } else {
            None
        }
    };
    let score = |n: &Node| -> Option<(bool, usize, bool)> {
        let (w, h) = n.size();
        if w <= 0 || h <= 0 {
            return None;
        }
        if let Some(id) = &q.id
            && n.id != *id
            && short_id(&n.id) != id.as_str()
        {
            return None;
        }
        let mut best = (false, 0);
        if let Some(t) = &q.text {
            let by_desc = if q.desc.is_none() { fit(t, &n.desc) } else { None };
            best = fit(t, &n.text).into_iter().chain(by_desc).min()?;
        }
        if let Some(d) = &q.desc {
            let f = fit(d, &n.desc)?;
            best = best.max(f);
        }
        Some((best.0, best.1, !n.click))
    };
    let mut hits: Vec<((bool, usize, bool), &Node)> = nodes.iter().filter_map(|n| score(n).map(|s| (s, n))).collect();
    hits.sort_by_key(|(s, n)| (*s, n.bounds[1], n.bounds[0]));
    hits.into_iter().map(|(_, n)| n).collect()
}

/// The screen for Claude: one element per line, the interesting ones only.
pub fn render(nodes: &[Node], all: bool) -> String {
    let mut out = String::new();
    let mut shown = 0;
    for n in nodes.iter().filter(|n| all || n.interesting()) {
        out.push_str(&n.line());
        out.push('\n');
        shown += 1;
        if shown >= 400 {
            out.push_str("… (more elements; ask with all=false or narrow it down)\n");
            break;
        }
    }
    if shown == 0 {
        out.push_str("(no readable elements: a game or video draws its own pixels; use a screenshot or the game add-on)\n");
    }
    out
}

/// Parses `uiautomator dump` XML (only the attributes above are kept).
pub fn parse_uiautomator(xml: &str) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<node ") {
        rest = &rest[start + 6..];
        let end = tag_end(rest);
        let tag = &rest[..end];
        let attr = |name: &str| attribute(tag, name).unwrap_or_default();
        let flag = |name: &str| attribute(tag, name).as_deref() == Some("true");
        let bounds = parse_bounds(&attr("bounds")).unwrap_or_default();
        nodes.push(Node {
            text: attr("text"),
            desc: attr("content-desc"),
            id: attr("resource-id"),
            cls: attr("class"),
            pkg: attr("package"),
            bounds,
            click: flag("clickable"),
            long: flag("long-clickable"),
            scroll: flag("scrollable"),
            edit: attr("class").contains("EditText"),
            check: flag("checkable").then(|| flag("checked")),
            selected: flag("selected"),
            enabled: attribute(tag, "enabled").as_deref() != Some("false"),
        });
        rest = &rest[end..];
    }
    nodes
}

/// End of a start tag: the first `>` outside quotes.
fn tag_end(s: &str) -> usize {
    let mut quoted = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '>' if !quoted => return i,
            _ => {}
        }
    }
    s.len()
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let mut search = tag;
    loop {
        let at = search.find(name)?;
        let before = search[..at].chars().last();
        let after = &search[at + name.len()..];
        if before.is_none_or(char::is_whitespace) && after.starts_with("=\"") {
            let v = &after[2..];
            return Some(unescape(&v[..v.find('"')?]));
        }
        search = &search[at + name.len()..];
    }
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.find(';') else { break };
        let ent = &rest[1..semi];
        let c = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `[0,63][1080,210]` → `[0, 63, 1080, 210]`.
fn parse_bounds(s: &str) -> Option<[i32; 4]> {
    let v: Vec<i32> = s.split(|c: char| !(c.is_ascii_digit() || c == '-')).filter(|x| !x.is_empty()).filter_map(|x| x.parse().ok()).collect();
    (v.len() == 4).then(|| [v[0], v[1], v[2], v[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation="0"><node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.example.game" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[0,0][1080,2400]"><node index="0" text="Play &amp; win" resource-id="com.example.game:id/btn_play" class="android.widget.Button" package="com.example.game" content-desc="" checkable="false" checked="false" clickable="true" enabled="true" focusable="true" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[390,1760][690,1880]" /><node index="1" text="" resource-id="com.example.game:id/settings" class="android.widget.ImageButton" package="com.example.game" content-desc="Settings" checkable="false" checked="false" clickable="true" enabled="true" focusable="true" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[960,80][1060,180]" /><node index="2" text="Sound" resource-id="" class="android.widget.Switch" package="com.example.game" content-desc="" checkable="true" checked="true" clickable="true" enabled="false" focusable="true" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[40,300][1040,400]" /><node index="3" text="Player name" resource-id="com.example.game:id/name" class="android.widget.EditText" package="com.example.game" content-desc="" checkable="false" checked="false" clickable="true" enabled="true" focusable="true" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[40,420][1040,520]" /></node></hierarchy>"#;

    #[test]
    fn parses_a_dump() {
        let nodes = parse_uiautomator(DUMP);
        assert_eq!(nodes.len(), 5);
        let play = &nodes[1];
        assert_eq!((play.text.as_str(), short_id(&play.id), play.center()), ("Play & win", "btn_play", (540, 1820)));
        assert!(play.click);
        assert_eq!(nodes[3].check, Some(true));
        assert!(!nodes[3].enabled);
        assert!(nodes[4].edit);
        let text = render(&nodes, false);
        assert_eq!(text.lines().count(), 4, "{text}");
        assert!(text.contains("Button \"Play & win\" id=btn_play @540,1820 300x120 click\n"), "{text}");
        assert!(text.contains("ImageButton desc=\"Settings\" id=settings @1010,130 100x100 click"));
        assert!(text.contains("Switch \"Sound\" @540,350 1000x100 click checked disabled"));
        // JSON round trip (the helper's shape).
        assert_eq!(Node::from_json(&play.to_json()).as_ref(), Some(play));
    }

    #[test]
    fn finds_by_text_id_or_description() {
        let nodes = parse_uiautomator(DUMP);
        let q = |t: Option<&str>, id: Option<&str>, d: Option<&str>| Query {
            text: t.map(Into::into),
            id: id.map(Into::into),
            desc: d.map(Into::into),
            index: 0,
        };
        assert_eq!(find(&nodes, &q(Some("play & WIN"), None, None))[0].center(), (540, 1820), "exact, any case");
        assert_eq!(find(&nodes, &q(Some("play"), None, None))[0].center(), (540, 1820), "contains");
        assert_eq!(find(&nodes, &q(Some("settings"), None, None))[0].center(), (1010, 130), "text finds an icon's description");
        assert_eq!(find(&nodes, &q(None, Some("btn_play"), None))[0].center(), (540, 1820));
        assert_eq!(find(&nodes, &q(None, Some("com.example.game:id/name"), None))[0].center(), (540, 470));
        assert!(find(&nodes, &q(Some("quit"), None, None)).is_empty());
        assert_eq!(Query::from_params(&json!({ "text": "Play", "index": 1 })).unwrap().index, 1);
        assert_eq!(Query::from_params(&json!({ "x": 1 })), None);
    }
}
