//! The telemetry page: served from the binary, self-only; the API that hands a signed-in
//! page its hub credential; and the hub's WebSocket listener, which takes the console's
//! pages only (that part against a real `nats-server`, skipped without one).

mod common;

use std::sync::Arc;

use common::*;
use hgl_ai_console::App;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; \
                   connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; \
                   form-action 'self'; object-src 'none'";

fn header<'a>(r: &'a reqwest::Response, k: &str) -> &'a str {
    r.headers()
        .get(k)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// A console whose `[console] viewer_credential` is `viewer` (if given).
async fn console(viewer: Option<&std::path::Path>) -> (Providers, Harness, String) {
    let (pem, public) = apple_key();
    let providers = Providers::start(public).await;
    let mut h = Harness::new(&providers, &pem);
    h.cfg.console.as_mut().unwrap().viewer_credential = viewer.map(|p| p.display().to_string());
    let app = Arc::new(App::new(h.cfg.clone()).unwrap());
    let base = serve(app).await;
    (providers, h, base)
}

/// The attribute values `attr="..."` in `html`.
fn attrs<'a>(html: &'a str, attr: &str) -> Vec<&'a str> {
    let pat = format!("{attr}=\"");
    html.match_indices(&pat)
        .map(|(i, _)| {
            let rest = &html[i + pat.len()..];
            &rest[..rest.find('"').unwrap()]
        })
        .collect()
}

#[tokio::test]
async fn the_page_and_its_files_are_the_consoles_own() {
    let dir = tempfile::tempdir().unwrap();
    let cred = dir.path().join("viewer.cred");
    let mut reg = hgl_ai_install::users::Registry::default();
    let text = reg
        .issue("viewer", hgl_ai_install::users::Role::Viewer, false)
        .unwrap();
    write_secret(&cred, &text);
    let (_p, _h, base) = console(Some(&cred)).await;

    // Signed out: the page sends the browser to sign in; the API says 401.
    let mut anon = Browser::new(&base);
    let r = anon.get("/telemetry").await;
    assert_eq!(r.status(), 303);
    assert_eq!(header(&r, "location"), "/signin");
    for p in ["/api/session", "/api/viewer"] {
        let r = anon.get(p).await;
        assert_eq!(r.status(), 401, "{p}");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["error"], "not signed in");
    }

    let (mut b, _key) = signed_in(&base).await;
    let page = b.get("/telemetry").await;
    assert_eq!(page.status(), 200);
    assert_eq!(header(&page, "content-security-policy"), CSP);
    assert_eq!(header(&page, "content-type"), "text/html; charset=utf-8");
    assert_eq!(header(&page, "cache-control"), "no-store");
    let html = page.text().await.unwrap();
    // Only files of its own: no inline script or style, no other origin.
    assert!(
        !html.contains("<style") && !html.contains(" style="),
        "{html}"
    );
    assert!(
        !html.contains("http://") && !html.contains("https://"),
        "{html}"
    );
    for s in html.split("<script").skip(1) {
        assert!(s.trim_start().contains("src=\"/app/"), "inline script: {s}");
    }
    let mut files: Vec<String> = attrs(&html, "src")
        .into_iter()
        .chain(attrs(&html, "href"))
        .map(String::from)
        .collect();
    assert_eq!(files.len(), 2, "{files:?}");
    let mut css_urls = Vec::new();
    while let Some(f) = files.pop() {
        assert!(f.starts_with("/app/assets/"), "{f}");
        let r = b.get(&f).await;
        assert_eq!(r.status(), 200, "{f}");
        let want = match f.rsplit('.').next().unwrap() {
            "js" => "text/javascript; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "svg" => "image/svg+xml",
            "woff2" => "font/woff2",
            e => panic!("{f}: {e}"),
        };
        assert_eq!(header(&r, "content-type"), want, "{f}");
        assert_eq!(header(&r, "content-security-policy"), CSP);
        assert_eq!(header(&r, "x-content-type-options"), "nosniff");
        assert_eq!(
            header(&r, "cache-control"),
            "public, max-age=31536000, immutable"
        );
        if f.ends_with(".css") {
            let css = r.text().await.unwrap();
            assert!(!css.contains("data:"), "{f}: a data: URL the CSP refuses");
            for u in css.split("url(").skip(1) {
                let u = u[..u.find(')').unwrap()].trim_matches(['\'', '"']);
                assert!(!u.contains(':') && !u.starts_with('/'), "{u}");
                css_urls.push(format!("/app/assets/{u}"));
            }
        }
    }
    // What the stylesheet refers to is served beside it (the grid's icon font).
    assert!(
        css_urls.iter().any(|u| u.ends_with(".woff2")),
        "{css_urls:?}"
    );
    for u in &css_urls {
        assert_eq!(b.get(u).await.status(), 200, "{u}");
    }
    for bad in [
        "/app/index.html",
        "/app/assets/nothing.js",
        "/app/..%2Fsrc%2Flib.rs",
    ] {
        assert_eq!(b.get(bad).await.status(), 404, "{bad}");
    }

    // Who is signed in, for the header and the sign-out form.
    let r = b.get("/api/session").await;
    assert_eq!(header(&r, "cache-control"), "no-store");
    let s: Value = r.json().await.unwrap();
    assert_eq!(s["user"], "This Dude", "the handle, never the email");
    assert!(!s.to_string().contains('@'), "{s}");
    assert_eq!(s["csrf"].as_str().unwrap().len(), 64, "{s}");
    assert!(s["open_questions"].is_null(), "no hub, no count");
    assert_eq!(s["stale_after_secs"], 3 * 86_400, "[limits] stale_after");
    assert_eq!(s["hosts_ttl_secs"], 120);
    assert_eq!(s["github_owner"], "hhenson", "[github] owner");

    // The viewer credential, for this signed-in page only, never cached.
    let r = b.get("/api/viewer").await;
    assert_eq!(r.status(), 200);
    assert_eq!(header(&r, "cache-control"), "no-store");
    let v: Value = r.json().await.unwrap();
    let hgl_ai_bus::Credential::Nkey { seed, .. } =
        hgl_ai_bus::parse_credential(&text).unwrap().unwrap()
    else {
        unreachable!()
    };
    assert_eq!(v["user"], "viewer");
    assert_eq!(v["seed"], seed.as_str());
    assert_eq!(v["path"], "/nats");

    // Without one configured, the page is told why.
    let (_p2, _h2, base2) = console(None).await;
    let (mut b2, _k2) = signed_in(&base2).await;
    let r = b2.get("/api/viewer").await;
    assert_eq!(r.status(), 503);
    let v: Value = r.json().await.unwrap();
    assert!(
        v["error"].as_str().unwrap().contains("viewer_credential"),
        "{v}"
    );
}

/// A WebSocket handshake to the hub's listener from a page of `origin`: the status line.
async fn handshake(port: u16, origin: &str) -> String {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let req = format!(
        "GET /nats HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\nOrigin: {origin}\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 1024];
    let n = s.read(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf[..n]).to_string();
    text.lines().next().unwrap_or("").to_string()
}

#[tokio::test]
async fn the_hub_takes_the_consoles_pages_and_the_viewer_credential_works() {
    let Some(hub) = start_hub_with(Some(ORIGIN)) else {
        return;
    };
    let ws = hub.ws_port.unwrap();
    assert!(
        handshake(ws, ORIGIN).await.contains(" 101 "),
        "the console's own page"
    );
    let other = handshake(ws, "https://elsewhere.example.com").await;
    assert!(other.contains(" 403 "), "another site's page: {other}");

    // The credential the API hands out is the hub's viewer.
    let (_p, _h, base) = console(Some(&hub.cred_files["viewer"])).await;
    let (mut b, _key) = signed_in(&base).await;
    let v: Value = b.get("/api/viewer").await.json().await.unwrap();
    let cred = hgl_ai_bus::Credential::Nkey {
        seed: v["seed"].as_str().unwrap().to_string(),
        user: None,
        // The viewer's role gives the bus its inbox prefix, its only subscriptions.
        role: Some("viewer".into()),
    };
    let admin = hgl_ai_bus::Bus::connect_with(&hub.url, Some(hub.creds["hub-admin"].clone()))
        .await
        .unwrap();
    admin.provision().await.unwrap();
    admin.state_put("mode", "normal").await.unwrap();
    let viewer = hgl_ai_bus::Bus::connect_with(&hub.url, Some(cred))
        .await
        .unwrap();
    assert_eq!(
        viewer.state_get("mode").await.unwrap().unwrap().value,
        "normal"
    );
    assert!(
        viewer.state_put("mode", "drain").await.is_err(),
        "read-only"
    );
}
