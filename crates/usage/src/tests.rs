use std::os::unix::fs::PermissionsExt as _;

use hgl_ai_proto::UsageBucket;

use super::*;

/// Writing a stand-in CLI while another test forks fails its exec (`ETXTBSY`): one at a time.
static CLI: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

/// A stand-in CLI: `auth status` and `-p /usage` print the fixtures given.
fn fake_cli(dir: &Path, name: &str, auth: Option<&str>, usage: &str) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let p = bin.join(name);
    let auth = auth.map_or_else(
        || "echo 'not signed in' >&2; exit 1".to_string(),
        |a| format!("cat '{}'", fixture(a).display()),
    );
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\ncase \"$1 $2\" in\n  'auth status') {auth} ;;\n  '-p /usage') cat '{}' ;;\n  *) echo \"unexpected: $*\" >&2; exit 2 ;;\nesac\n",
            fixture(usage).display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn sources(dir: &Path) -> Sources {
    let codex = dir.join("codex");
    let day = codex.join("sessions/2026/11/27");
    std::fs::create_dir_all(&day).unwrap();
    std::fs::copy(
        fixture("codex-auth.synthetic.json"),
        codex.join("auth.json"),
    )
    .unwrap();
    std::fs::copy(
        fixture("codex-rollout.jsonl"),
        day.join("rollout-2026-11-27T21-00-34-a.jsonl"),
    )
    .unwrap();
    let agy_home = dir.join("agy");
    std::fs::create_dir_all(agy_home.join("log")).unwrap();
    std::fs::copy(
        fixture("agy-cli.log"),
        agy_home.join("log/cli-20261127_214126.log"),
    )
    .unwrap();
    Sources {
        claude: fake_cli(dir, "claude", Some("claude-auth.json"), "claude-usage.txt"),
        agy: fake_cli(dir, "agy", None, "agy-usage.txt"),
        codex_home: Some(codex),
        agy_home: Some(agy_home),
        timeout: Duration::from_secs(30),
    }
}

fn rec(c: &Collected) -> &UsageRecord {
    match c {
        Collected::Record(r) => r,
        Collected::Skipped { runtime, why } => panic!("{runtime} skipped: {why}"),
    }
}

#[tokio::test]
async fn collects_one_record_per_login() {
    let _one = CLI.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let src = sources(dir.path());
    let got = collect(&src, &[], "build-1").await;
    assert_eq!(got.len(), 3);
    let (c, x, a) = (rec(&got[0]), rec(&got[1]), rec(&got[2]));

    assert_eq!(c.runtime, Runtime::Claude);
    assert_eq!(
        c.account,
        account_fingerprint("00000000-0000-4000-8000-000000000001/someone@example.com")
    );
    assert_eq!(c.plan.as_deref(), Some("max_20x"));
    assert_eq!(c.buckets.len(), 2);
    assert_eq!(c.source_age_s, 0);

    assert_eq!(x.runtime, Runtime::Codex);
    assert_eq!(x.account, account_fingerprint("acct-fixture-0001"));
    assert_eq!(x.observed_at, "20261127T171650Z");
    assert!(x.source_age_s > 0);
    assert_eq!(
        x.buckets,
        vec![UsageBucket {
            scope: "all".into(),
            window: "week".into(),
            used_pct: 7.0,
            resets_at: Some("20261204T061705Z".into()),
        }]
    );

    assert_eq!(a.runtime, Runtime::Agy);
    assert_eq!(a.account, account_fingerprint("someone@example.com"));
    assert_eq!(a.buckets.len(), 4);

    for r in [c, x, a] {
        r.validate().unwrap();
        assert_eq!(r.host, "build-1");
        // Nothing of the account files but the fingerprint reaches a record.
        let j = serde_json::to_string(r).unwrap();
        for secret in [
            "someone@example.com",
            "acct-fixture",
            "FIXTURE-NOT-A-TOKEN",
            "00000000-",
        ] {
            assert!(!j.contains(secret), "{secret} in {j}");
        }
    }
}

#[tokio::test]
async fn a_failing_source_skips_its_runtime_only() {
    let _one = CLI.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let mut src = sources(dir.path());
    src.claude = fake_cli(dir.path(), "claude2", None, "claude-usage.txt");
    src.agy = dir.path().join("no-such-agy");
    // A newer rollout of another account is passed over.
    let other = dir.path().join("codex/sessions/2026/11/28");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(
        other.join("rollout-2026-11-28T00-00-00-b.jsonl"),
        std::fs::read_to_string(fixture("codex-rollout.jsonl"))
            .unwrap()
            .replace("acct-fixture-0001", "acct-someone-else"),
    )
    .unwrap();
    let got = collect(&src, &[], "build-1").await;
    match &got[0] {
        Collected::Skipped { runtime, why } => {
            assert_eq!(*runtime, Runtime::Claude);
            assert!(why.contains("not signed in"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rec(&got[1]).observed_at, "20261127T171650Z");
    match &got[2] {
        Collected::Skipped { why, .. } => assert!(why.contains("not installed"), "{why}"),
        other => panic!("{other:?}"),
    }
    let only = collect(&src, &[Runtime::Codex], "build-1").await;
    assert_eq!(only.len(), 1);
    std::fs::remove_file(dir.path().join("codex/auth.json")).unwrap();
    match &collect(&src, &[Runtime::Codex], "build-1").await[0] {
        Collected::Skipped { why, .. } => assert!(why.contains("auth.json"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn show_table() {
    let now = parse_stamp("20261127T180000Z").unwrap();
    let r = UsageRecord {
        runtime: Runtime::Codex,
        account: "0123456789ab".into(),
        label: Some("team".into()),
        host: "build-1".into(),
        plan: Some("pro".into()),
        observed_at: "20261127T171650Z".into(),
        collected_at: "20261127T180000Z".into(),
        source_age_s: 2590,
        buckets: vec![
            UsageBucket {
                scope: "all".into(),
                window: "5h".into(),
                used_pct: 41.5,
                resets_at: Some("20261127T193000Z".into()),
            },
            UsageBucket {
                scope: "all".into(),
                window: "week".into(),
                used_pct: 7.0,
                resets_at: None,
            },
        ],
        limit_reached: Some("primary".into()),
        unparsed: vec!["odd line".into()],
    };
    let mut empty = r.clone();
    empty.runtime = Runtime::Agy;
    empty.label = None;
    empty.plan = None;
    empty.buckets.clear();
    empty.limit_reached = None;
    empty.unparsed.clear();
    empty.observed_at = "20261125T180000Z".into();
    assert_eq!(
        table(&[r, empty], now),
        "\
RUNTIME  ACCOUNT       LABEL  HOST     PLAN  SCOPE  WINDOW  USED  RESET  AGE
codex    0123456789ab  team   build-1  pro   all    5h      42%   1.5h   43m
codex    0123456789ab  team   build-1  pro   all    week    7%    -      43m
agy      0123456789ab  -      build-1  -     -      -       -     -      2d
codex.0123456789ab: limit reached: primary
codex.0123456789ab: unparsed: odd line
"
    );
    assert_eq!(hours_to(Some("20261127T173000Z"), now), Some(-0.5));
    assert_eq!(hours_to(None, now), None);
}
