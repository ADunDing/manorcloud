//! WebDAV 的 XML（webdav.rs 使用）：讀取用戶端要的屬性（PROPFIND、PROPPATCH），寫出回應（207 Multi-Status、LOCK）。
//!
//! 讀取用 quick-xml：它不處理 DTD、不展開外部實體，惡意的 XML 不能讀取伺服器上的檔案。
//! 寫出時，所有來自外部的文字（檔名、用戶端給的屬性名稱與命名空間）都經過跳脫或檢查。

use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

pub const DAV: &str = "DAV:";

/// 一個請求最多列出幾個屬性
const MAX_PROPS: usize = 200;

/// 一個屬性的名稱：命名空間與名稱
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropName {
    pub ns: String,
    pub local: String,
}

impl PropName {
    pub fn dav(local: &str) -> Self {
        PropName {
            ns: DAV.to_string(),
            local: local.to_string(),
        }
    }
}

/// PROPFIND 要的是什麼
#[derive(Debug, PartialEq, Eq)]
pub enum PropFind {
    /// 所有屬性（沒有內容時也是）
    All,
    /// 只要屬性的名稱
    Names,
    /// 指定的屬性
    Props(Vec<PropName>),
}

#[derive(Debug, PartialEq, Eq)]
pub struct InvalidXml;

/// 讀取 PROPFIND 的內容
pub fn propfind(body: &[u8]) -> Result<PropFind, InvalidXml> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(PropFind::All);
    }
    let scan = scan(body)?;
    Ok(if scan.allprop {
        PropFind::All
    } else if scan.propname {
        PropFind::Names
    } else if scan.prop {
        PropFind::Props(scan.props)
    } else {
        PropFind::All
    })
}

/// PROPPATCH 要修改的屬性（`set` 與 `remove` 底下的都算）
pub fn proppatch(body: &[u8]) -> Result<Vec<PropName>, InvalidXml> {
    let scan = scan(body)?;
    if !scan.root {
        return Err(InvalidXml);
    }
    Ok(scan.props)
}

/// 走過整份 XML：`DAV:prop` 底下每一個元素的名稱，以及出現了哪些關鍵的元素
#[derive(Debug, Default)]
struct Scan {
    props: Vec<PropName>,
    root: bool,
    prop: bool,
    allprop: bool,
    propname: bool,
    /// 目前所在的層數
    depth: usize,
    /// 正在讀的 DAV:prop 從哪一層開始
    prop_depth: Option<usize>,
}

impl Scan {
    fn element(&mut self, ns: &str, local: &str, empty: bool) -> Result<(), InvalidXml> {
        self.root = true;
        if self.prop_depth.is_some_and(|d| self.depth == d + 1) {
            if self.props.len() >= MAX_PROPS || !is_safe_name(local) {
                return Err(InvalidXml);
            }
            self.props.push(PropName {
                ns: ns.to_string(),
                local: local.to_string(),
            });
        }
        if ns == DAV {
            match local {
                "allprop" => self.allprop = true,
                "propname" => self.propname = true,
                "prop" if self.prop_depth.is_none() => {
                    self.prop = true;
                    if !empty {
                        self.prop_depth = Some(self.depth);
                    }
                }
                _ => {}
            }
        }
        if !empty {
            self.depth += 1;
        }
        Ok(())
    }

    fn end(&mut self) {
        self.depth = self.depth.saturating_sub(1);
        if self.prop_depth == Some(self.depth) {
            self.prop_depth = None;
        }
    }
}

fn scan(body: &[u8]) -> Result<Scan, InvalidXml> {
    let text = std::str::from_utf8(body).map_err(|_| InvalidXml)?;
    let mut reader = NsReader::from_str(text);
    let mut scan = Scan::default();
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(|_| InvalidXml)?;
        let ns = match ns {
            ResolveResult::Bound(ns) => ns.as_ref().to_string(),
            ResolveResult::Unbound => String::new(),
            ResolveResult::Unknown(_) => return Err(InvalidXml),
        };
        match event {
            Event::Start(e) => scan.element(&ns, e.local_name().as_ref(), false)?,
            Event::Empty(e) => scan.element(&ns, e.local_name().as_ref(), true)?,
            Event::End(_) => scan.end(),
            // 還有沒結束的元素：內容不完整
            Event::Eof if scan.depth > 0 => return Err(InvalidXml),
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(scan)
}

/// 用戶端給的屬性名稱會原樣寫回回應中：只接受一般的名稱，不能夾帶其他標記
fn is_safe_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// XML 的文字與屬性值跳脫
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// 一個屬性元素：DAV: 的用 `D:` 前綴，其他的在元素上宣告自己的命名空間。
/// `inner` 是已經組好的 XML（文字要先跳脫）
pub fn element(name: &PropName, inner: &str) -> String {
    let (open, close) = if name.ns == DAV {
        (format!("D:{}", name.local), format!("D:{}", name.local))
    } else {
        (
            format!("{} xmlns=\"{}\"", name.local, escape(&name.ns)),
            name.local.clone(),
        )
    };
    if inner.is_empty() {
        format!("<{open}/>")
    } else {
        format!("<{open}>{inner}</{close}>")
    }
}

/// 207 Multi-Status 的內容
pub struct MultiStatus(String);

impl MultiStatus {
    pub fn new() -> Self {
        MultiStatus(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\">"
                .to_string(),
        )
    }

    /// 一個項目：找到的屬性（名稱與內容）與沒有的屬性
    pub fn response(&mut self, href: &str, found: &[(PropName, String)], missing: &[PropName]) {
        self.0.push_str("<D:response><D:href>");
        self.0.push_str(&escape(href));
        self.0.push_str("</D:href>");
        let found: Vec<String> = found.iter().map(|(n, v)| element(n, v)).collect();
        let missing: Vec<String> = missing.iter().map(|n| element(n, "")).collect();
        self.propstat(&found, "200 OK");
        self.propstat(&missing, "404 Not Found");
        self.0.push_str("</D:response>");
    }

    fn propstat(&mut self, props: &[String], status: &str) {
        if props.is_empty() {
            return;
        }
        self.0.push_str("<D:propstat><D:prop>");
        for prop in props {
            self.0.push_str(prop);
        }
        self.0.push_str("</D:prop><D:status>HTTP/1.1 ");
        self.0.push_str(status);
        self.0.push_str("</D:status></D:propstat>");
    }

    pub fn finish(mut self) -> String {
        self.0.push_str("</D:multistatus>\n");
        self.0
    }
}

/// LOCK 的回應
pub fn lock_discovery(token: &str, href: &str, depth: &str, timeout_seconds: u64) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:prop xmlns:D=\"DAV:\"><D:lockdiscovery>{}</D:lockdiscovery></D:prop>\n",
        active_lock(token, href, depth, timeout_seconds)
    )
}

fn active_lock(token: &str, href: &str, depth: &str, timeout_seconds: u64) -> String {
    format!(
        "<D:activelock><D:locktype><D:write/></D:locktype><D:lockscope><D:exclusive/></D:lockscope>\
         <D:depth>{depth}</D:depth><D:timeout>Second-{timeout_seconds}</D:timeout>\
         <D:locktoken><D:href>{}</D:href></D:locktoken>\
         <D:lockroot><D:href>{}</D:href></D:lockroot></D:activelock>",
        escape(token),
        escape(href)
    )
}

/// supportedlock 屬性的內容：只支援獨占的寫入鎖
pub const SUPPORTED_LOCK: &str = "<D:lockentry><D:lockscope><D:exclusive/></D:lockscope>\
     <D:locktype><D:write/></D:locktype></D:lockentry>";

#[cfg(test)]
mod tests {
    use super::*;

    fn names(props: &[PropName]) -> Vec<(&str, &str)> {
        props
            .iter()
            .map(|p| (p.ns.as_str(), p.local.as_str()))
            .collect()
    }

    #[test]
    fn propfind_bodies() {
        assert_eq!(propfind(b""), Ok(PropFind::All));
        assert_eq!(propfind(b"  \n"), Ok(PropFind::All));
        assert_eq!(
            propfind(br#"<?xml version="1.0"?><propfind xmlns="DAV:"><allprop/></propfind>"#),
            Ok(PropFind::All)
        );
        assert_eq!(
            propfind(br#"<D:propfind xmlns:D="DAV:"><D:propname/></D:propfind>"#),
            Ok(PropFind::Names)
        );
        let body = br#"<?xml version="1.0" encoding="utf-8"?>
            <D:propfind xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:">
              <D:prop>
                <D:getcontentlength/>
                <D:resourcetype></D:resourcetype>
                <Z:Win32FileAttributes/>
                <quota xmlns="http://example.com/ns"><nested/></quota>
              </D:prop>
            </D:propfind>"#;
        let Ok(PropFind::Props(props)) = propfind(body) else {
            panic!("expected a list of properties");
        };
        assert_eq!(
            names(&props),
            [
                ("DAV:", "getcontentlength"),
                ("DAV:", "resourcetype"),
                ("urn:schemas-microsoft-com:", "Win32FileAttributes"),
                ("http://example.com/ns", "quota"),
            ]
        );
    }

    #[test]
    fn proppatch_bodies() {
        let body = br#"<?xml version="1.0" encoding="utf-8" ?>
            <D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:">
              <D:set><D:prop>
                <Z:Win32CreationTime>Mon, 23 Nov 2026 12:00:00 GMT</Z:Win32CreationTime>
                <Z:Win32FileAttributes>00000020</Z:Win32FileAttributes>
              </D:prop></D:set>
              <D:remove><D:prop><Z:Old/></D:prop></D:remove>
            </D:propertyupdate>"#;
        assert_eq!(
            names(&proppatch(body).unwrap()),
            [
                ("urn:schemas-microsoft-com:", "Win32CreationTime"),
                ("urn:schemas-microsoft-com:", "Win32FileAttributes"),
                ("urn:schemas-microsoft-com:", "Old"),
            ]
        );
        assert_eq!(proppatch(b""), Err(InvalidXml));
    }

    #[test]
    fn bad_xml_is_refused() {
        for bad in [
            &b"<D:propfind xmlns:D=\"DAV:\"><D:prop>"[..],
            b"<x:propfind><x:prop/></x:propfind>",
            b"\xff\xfe",
            b"<propfind xmlns=\"DAV:\"></prop>",
        ] {
            assert_eq!(
                propfind(bad),
                Err(InvalidXml),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn external_entities_are_never_expanded() {
        let body = br#"<?xml version="1.0"?>
            <!DOCTYPE propfind [<!ENTITY xxe SYSTEM "file:///etc/passwd">]>
            <propfind xmlns="DAV:"><prop><displayname>&xxe;</displayname></prop></propfind>"#;
        // 不論是拒絕還是忽略實體，都不會讀到檔案的內容
        if let Ok(PropFind::Props(props)) = propfind(body) {
            assert_eq!(names(&props), [("DAV:", "displayname")]);
        }
    }

    #[test]
    fn output_is_escaped() {
        let mut out = MultiStatus::new();
        out.response(
            "/dav/a&b",
            &[(PropName::dav("displayname"), escape("<a & b>"))],
            &[PropName {
                ns: "urn:x\"y".to_string(),
                local: "z".to_string(),
            }],
        );
        let text = out.finish();
        assert!(text.contains("<D:href>/dav/a&amp;b</D:href>"));
        assert!(text.contains("<D:displayname>&lt;a &amp; b&gt;</D:displayname>"));
        assert!(text.contains("<z xmlns=\"urn:x&quot;y\"/>"));
        assert!(text.contains("404 Not Found"));
        // 寫出的內容自己也讀得回來
        assert!(scan(text.as_bytes()).is_ok());
    }
}
