//! The telemetry page: the vendored grid served self-only, and the live feed's row diffs
//! against a real `nats-server` (that part skipped without one).

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use hgl_ai_bus::Bus;
use hgl_ai_console::{AG_GRID_FILES, AG_GRID_VERSION, App, ag_grid_path};
use hgl_ai_proto::build::{BuilderRecord, Platform};
use hgl_ai_proto::usage::{UsageBucket, UsageRecord};
use hgl_ai_proto::{Family, HostRecord, Role, Runtime, Seat, SeatRecord, TreeRecord};
use serde_json::{Value, json};

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; \
                   connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; \
                   form-action 'self'; object-src 'none'";

async fn console() -> (Providers, Harness, String) {
    let (pem, public) = apple_key();
    let providers = Providers::start(public).await;
    let h = Harness::new(&providers, &pem);
    let app = Arc::new(App::new(h.cfg.clone()).unwrap());
    let base = serve(app).await;
    (providers, h, base)
}

fn header<'a>(r: &'a reqwest::Response, k: &str) -> &'a str {
    r.headers()
        .get(k)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

#[tokio::test]
async fn the_grid_is_served_from_the_console_only() {
    let (_p, _h, base) = console().await;
    let (mut b, _key) = signed_in(&base).await;
    let page = b.get("/telemetry").await;
    assert_eq!(header(&page, "content-security-policy"), CSP);
    let page = page.text().await.unwrap();
    for f in [
        "ag-grid.min.css",
        "ag-theme-quartz.min.css",
        "ag-grid-community.min.noStyle.js",
    ] {
        assert!(page.contains(&ag_grid_path(f)), "{f}");
    }
    // Only the console's own scripts; no inline script or style for the CSP to refuse.
    assert_eq!(page.matches("<script").count(), 2, "{page}");
    assert_eq!(page.matches("<script src=\"/assets/").count(), 2);
    assert!(!page.contains(" style=") && !page.contains("<style"));
    assert!(!page.contains("https://"), "no CDN");
    // The tabs, and a plain table per grid for the page without the script.
    for t in [
        "hosts", "tasks", "prs", "queue", "backlog", "landings", "fleet", "builds", "usage",
    ] {
        assert!(
            page.contains(&format!("<a href=\"#{t}\" id=\"tab-{t}\"")),
            "{t}"
        );
        assert!(page.contains(&format!("<section class=\"panel\" id=\"{t}\"")));
    }
    assert!(page.contains("<table data-grid=\"runs\""));

    // Every vendored file, with its type; an immutable cache, as the version is the path.
    let mut urls = Vec::new();
    for (name, body) in AG_GRID_FILES {
        let r = b.get(&ag_grid_path(name)).await;
        assert_eq!(r.status(), 200, "{name}");
        let want = match name.rsplit('.').next().unwrap() {
            "js" => "text/javascript; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "svg" => "image/svg+xml",
            "woff2" => "font/woff2",
            e => panic!("{e}"),
        };
        assert_eq!(header(&r, "content-type"), want, "{name}");
        assert_eq!(header(&r, "content-security-policy"), CSP);
        assert_eq!(header(&r, "x-content-type-options"), "nosniff");
        assert_eq!(
            header(&r, "cache-control"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(&r.bytes().await.unwrap()[..], *body);
        if name.ends_with(".css") {
            let css = std::str::from_utf8(body).unwrap();
            assert!(
                !css.contains("data:"),
                "{name}: a data: URL the CSP refuses"
            );
            for u in css.split("url(").skip(1) {
                urls.push(
                    u[..u.find(')').unwrap()]
                        .trim_matches(['\'', '"'])
                        .to_string(),
                );
            }
        }
    }
    // What the stylesheets refer to is served beside them.
    assert!(urls.iter().any(|u| u.ends_with(".woff2")), "{urls:?}");
    for u in &urls {
        assert!(!u.contains(':') && !u.starts_with('/'), "{u}");
        assert_eq!(b.get(&ag_grid_path(u)).await.status(), 200, "{u}");
    }
    for bad in [
        "/assets/ag-grid/0.0.1/ag-grid.min.css".to_string(),
        ag_grid_path("lib.rs"),
        ag_grid_path("..%2F..%2Fsrc%2Flib.rs"),
    ] {
        assert_eq!(b.get(&bad).await.status(), 404, "{bad}");
    }
    // The pinned version and its licence are recorded beside the files.
    let vendor = include_str!("../assets/vendor/VENDOR.md");
    assert!(vendor.contains(&format!("| ag-grid-community | {AG_GRID_VERSION} |")));
    let licence = include_str!("../assets/vendor/ag-grid/LICENSE.txt");
    assert!(licence.starts_with("The MIT License"));
    // Other pages do not load the grid.
    let front = b.get("/").await.text().await.unwrap();
    assert!(!front.contains("/assets/ag-grid/"));
}

/// One server-sent event.
struct Ev {
    event: String,
    data: Value,
}

struct Feed {
    r: reqwest::Response,
    buf: String,
}

impl Feed {
    async fn next(&mut self) -> Ev {
        loop {
            if let Some(at) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..at + 2).collect();
                let (mut event, mut data) = (String::new(), String::new());
                for l in block.lines() {
                    if let Some(v) = l.strip_prefix("event:") {
                        event = v.trim().to_string();
                    } else if let Some(v) = l.strip_prefix("data:") {
                        data.push_str(v.strip_prefix(' ').unwrap_or(v));
                    }
                }
                if event.is_empty() {
                    continue; // a keep-alive comment
                }
                return Ev {
                    event,
                    data: serde_json::from_str(&data).unwrap(),
                };
            }
            let chunk = tokio::time::timeout(Duration::from_secs(40), self.r.chunk())
                .await
                .expect("an event within 40 s")
                .unwrap()
                .expect("the feed stays open");
            self.buf.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    }
}

/// The page's view of the tables: rows by id, as the script keeps them.
type View = BTreeMap<String, BTreeMap<String, Value>>;

fn apply(view: &mut View, ev: &Ev) {
    let tables = ev.data["tables"].as_object().unwrap();
    for (name, t) in tables {
        let rows = view.entry(name.clone()).or_default();
        let id = |r: &Value| r["id"].as_str().unwrap().to_string();
        match ev.event.as_str() {
            "reset" => {
                rows.clear();
                for r in t.as_array().unwrap() {
                    assert!(rows.insert(id(r), r.clone()).is_none(), "ids are unique");
                }
            }
            "diff" => {
                for r in t["add"].as_array().unwrap() {
                    assert!(rows.insert(id(r), r.clone()).is_none(), "added once");
                }
                for r in t["update"].as_array().unwrap() {
                    assert!(rows.insert(id(r), r.clone()).is_some(), "updates a row");
                }
                for r in t["remove"].as_array().unwrap() {
                    assert!(rows.remove(r.as_str().unwrap()).is_some(), "removes a row");
                }
            }
            e => panic!("event {e}"),
        }
    }
}

fn prs(rows: &[(u64, &str)]) -> String {
    let mut s = "# read 20261128T050000Z\n".to_string();
    for (n, checks) in rows {
        s.push_str(&format!(
            "alpha\t{n}\t{}\topen\tclean\t0\t{checks}\t20261128T050000Z\n",
            n.to_string().repeat(40).get(..40).unwrap()
        ));
    }
    s
}

#[tokio::test]
async fn the_feed_sends_row_diffs() {
    let Some(hub) = start_hub() else { return };
    let admin = Bus::connect_with(&hub.url, Some(hub.creds["hub-admin"].clone()))
        .await
        .unwrap();
    admin.provision().await.unwrap();
    let seat = Bus::connect_with(&hub.url, Some(hub.creds["hub"].clone()))
        .await
        .unwrap();
    let now = hgl_ai_proto::utc_stamp(chrono::Utc::now());
    seat.host_put(&HostRecord {
        host: "lab1".into(),
        seats: vec![SeatRecord {
            seat: Seat::new("w1").unwrap(),
            role: Role::Implementor,
            runtime: Runtime::Claude,
            family: Family::Claude,
            model: "claude-opus-5-5".into(),
        }],
        memcap_gb: 64,
        work_root_free_gb: 500,
        updated_at: now.clone(),
    })
    .await
    .unwrap();
    let tree: TreeRecord = serde_json::from_value(json!({
        "host": "lab1", "seat": "w1", "task": {"repo": "alpha", "issue": 7},
        "path": "/w/alpha-7", "branch": "codex/q7", "created_at": now,
    }))
    .unwrap();
    seat.tree_put(&tree).await.unwrap();
    seat.state_put("prs", &prs(&[(11, "pass"), (12, "pending")]))
        .await
        .unwrap();
    seat.state_put(
        "landed.log",
        "20261128T040000Z alpha #5 0123456789abcdef0123456789abcdef01234567\n",
    )
    .await
    .unwrap();
    seat.builder_put(&BuilderRecord {
        label: "lab1".into(),
        inbox: Seat::new("builder-lab1").unwrap(),
        platform: Platform::Linux,
        arch: "x86_64".into(),
        executor: "local".into(),
        proxy: None,
        reachable: true,
        reason: None,
        state: "idle".into(),
        slots: 2,
        busy: 0,
        free_gb: Some(400),
        toolchains: [("cargo".to_string(), "1.93.0".to_string())].into(),
        updated_at: now.clone(),
    })
    .await
    .unwrap();
    let run = json!({
        "v": 1, "id": "20261128-050000-3fa9c1", "repo": "alpha",
        "head": "8a5c3d264c2edbbdaa61530d810bff276c3eebba", "dirty": false, "host": "lab1",
        "platform": "linux", "executor": "local", "status": "pass",
        "started_at": "20261128T050000Z", "wall_ms": 75000, "log_dir": "/w/logs",
        "steps": [], "errors": [], "errors_total": 0,
        "tests": {"passed": 120, "failed": 0, "skipped": 1}, "warnings_total": 0,
        "warnings": {}
    });
    seat.build_put("run.20261128-050000-3fa9c1", run.to_string().as_bytes())
        .await
        .unwrap();
    seat.usage_put(&UsageRecord {
        runtime: Runtime::Claude,
        account: "0123456789ab".into(),
        label: None,
        host: "lab1".into(),
        plan: Some("max".into()),
        observed_at: now.clone(),
        collected_at: now.clone(),
        source_age_s: 0,
        buckets: vec![UsageBucket {
            scope: "all".into(),
            window: "week".into(),
            used_pct: 41.25,
            resets_at: None,
        }],
        limit_reached: None,
        unparsed: vec![],
    })
    .await
    .unwrap();

    let (pem, public) = apple_key();
    let providers = Providers::start(public).await;
    let mut h = Harness::new(&providers, &pem);
    h.cfg.hub.url = hub.url.clone();
    let app = Arc::new(App::new(h.cfg.clone()).unwrap());
    let bus = Arc::new(
        Bus::connect_with(&hub.url, Some(hub.creds["console"].clone()))
            .await
            .unwrap(),
    );
    app.set_hub(bus);
    hgl_ai_console::service::start_background(app.clone()).await;
    let base = serve(app).await;
    let (mut b, _key) = signed_in(&base).await;
    assert_eq!(
        b.get("/telemetry/events")
            .await
            .headers()
            .get("content-type")
            .unwrap(),
        "text/event-stream"
    );
    let mut feed = Feed {
        r: b.get("/telemetry/events").await,
        buf: String::new(),
    };

    // First a reset with every table; the first reading may come after it, as a diff.
    let mut view = View::new();
    let ev = feed.next().await;
    assert_eq!(ev.event, "reset");
    assert!(ev.data["meta"]["read"].is_string());
    apply(&mut view, &ev);
    while view["prs"].len() < 2 || view["usage"].is_empty() || view["runs"].is_empty() {
        let ev = feed.next().await;
        assert_eq!(ev.event, "diff");
        apply(&mut view, &ev);
    }
    assert_eq!(view["prs"]["alpha#12"]["checks"], "pending");
    assert_eq!(view["hosts"]["lab1/w1"]["model"], "claude-opus-5-5");
    assert_eq!(
        view["tasks"]["lab1/w1/alpha#7/implement"]["stage"],
        "working"
    );
    assert_eq!(view["runs"]["20261128-050000-3fa9c1"]["passed"], 120);
    assert_eq!(view["builders"]["lab1"]["toolchains"], "cargo 1.93.0");
    assert_eq!(
        view["usage"]["claude.0123456789ab/all/week"]["used_pct"],
        41.3
    );
    assert_eq!(view["landings"].len(), 1);
    assert_eq!(view["fleet"]["mode"]["value"], "normal");

    // One pull request lands, one changes, one opens: the next diff says just that.
    seat.state_put("prs", &prs(&[(12, "pass"), (14, "pending")]))
        .await
        .unwrap();
    let ev = loop {
        let ev = feed.next().await;
        assert_eq!(ev.event, "diff");
        apply(&mut view, &ev);
        if !ev.data["tables"]["prs"].is_null() {
            break ev;
        }
    };
    let d = &ev.data["tables"]["prs"];
    assert_eq!(d["remove"], json!(["alpha#11"]));
    assert_eq!(d["update"][0]["id"], "alpha#12");
    assert_eq!(d["update"][0]["checks"], "pass");
    assert_eq!(d["add"][0]["id"], "alpha#14");
    assert_eq!(d["update"].as_array().unwrap().len(), 1, "{d}");
    // Nothing else changed, so nothing else was sent (but a task's age, by the minute).
    let others: Vec<&String> = ev.data["tables"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| *k != "prs" && *k != "tasks")
        .collect();
    assert!(others.is_empty(), "{others:?}");
    assert_eq!(
        view["prs"].keys().collect::<Vec<_>>(),
        ["alpha#12", "alpha#14"]
    );
    // The page's tab label counts follow.
    assert_eq!(ev.data["meta"]["counts"]["prs"], 2);
}
