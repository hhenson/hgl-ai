//! Server-rendered pages: escaping, the page frame and small pieces. Every value from
//! the hub or a form passes through [`esc`]; nothing else builds HTML.

use std::fmt::Write as _;

/// Escape text for HTML element content and quoted attribute values.
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

/// `20261127T041100Z` → `2026-11-27 04:11 UTC`; other text as it is.
pub fn when(stamp: &str) -> String {
    hgl_ai_proto::parse_stamp(stamp)
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| stamp.to_string())
}

/// A `<time>` element for a compact stamp.
pub fn time(stamp: &str) -> String {
    format!(
        "<time datetime=\"{}\">{}</time>",
        esc(stamp),
        esc(&when(stamp))
    )
}

/// Text that keeps its line breaks (Markdown is shown as written).
pub fn text_block(s: &str) -> String {
    format!("<div class=\"text\">{}</div>", esc(s.trim_end()))
}

pub struct Nav<'a> {
    pub user: &'a str,
    pub csrf: &'a str,
    pub open_questions: usize,
    pub current: &'a str,
}

/// The whole page around `body`. `nav` is absent on the sign-in pages.
pub fn page(title: &str, nav: Option<&Nav>, banner: Option<&str>, body: &str) -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light dark\">\
         <title>{} · hgl-ai console</title>\
         <link rel=\"stylesheet\" href=\"/assets/style.css\">\
         <script src=\"/assets/app.js\" defer></script></head><body>\
         <a class=\"skip\" href=\"#main\">Skip to content</a><header>",
        esc(title)
    );
    s.push_str("<p class=\"brand\">hgl-ai console</p>");
    if let Some(n) = nav {
        s.push_str("<nav aria-label=\"Main\"><ul>");
        for (href, label) in [
            ("/", "Status".to_string()),
            (
                "/questions",
                format!("Questions ({} open)", n.open_questions),
            ),
            ("/conversations", "Conversations".to_string()),
            ("/telemetry", "Telemetry".to_string()),
        ] {
            let cur = if href == n.current {
                " aria-current=\"page\""
            } else {
                ""
            };
            let _ = write!(s, "<li><a href=\"{href}\"{cur}>{}</a></li>", esc(&label));
        }
        let _ = write!(
            s,
            "</ul></nav><form class=\"signout\" method=\"post\" action=\"/auth/signout\">\
             <input type=\"hidden\" name=\"csrf\" value=\"{}\">\
             <span class=\"who\">{}</span> <button type=\"submit\">Sign out</button></form>",
            esc(n.csrf),
            esc(n.user)
        );
    }
    s.push_str("</header><main id=\"main\">");
    if let Some(b) = banner {
        let _ = write!(s, "<p class=\"banner\" role=\"alert\">{}</p>", esc(b));
    }
    let _ = writeln!(s, "<h1>{}</h1>{body}</main></body></html>", esc(title));
    s
}

/// A hidden CSRF field.
pub fn csrf(token: &str) -> String {
    format!(
        "<input type=\"hidden\" name=\"csrf\" value=\"{}\">",
        esc(token)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_and_times() {
        assert_eq!(
            esc("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
        assert_eq!(when("20261127T041100Z"), "2026-11-27 04:11 UTC");
        assert_eq!(when("soon"), "soon");
        assert_eq!(
            when("aaa\u{e9}105T090000Z"),
            "aaa\u{e9}105T090000Z",
            "never sliced"
        );
        let p = page("T<x>", None, Some("down"), "<p>b</p>");
        assert!(p.contains("<title>T&lt;x&gt; · hgl-ai console</title>"));
        assert!(p.contains("role=\"alert\">down</p>"));
        assert!(p.starts_with("<!doctype html>\n<html lang=\"en\">"));
    }
}
