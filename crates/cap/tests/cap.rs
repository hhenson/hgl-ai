//! Process tests of the cap. The rlimit fallback is tested everywhere; the
//! systemd scope only where `systemd-run --user --scope` works (else skipped
//! with the reason printed). Memory hogs are this test binary re-run as a
//! helper (`HGL_AI_CAP_TEST_HELPER`).

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hgl_ai_cap::{CapCommand, CapSpec, Method, STATUS_KILL, STATUS_TERM};

const HELPER: &str = "HGL_AI_CAP_TEST_HELPER";
const MIB: u64 = 1 << 20;

/// Not a test: the body of a helper process. `alloc:MIB:HOLD_MS` touches MIB of
/// memory and holds it; `tree:K:MIB:HOLD_MS` starts K such children.
#[test]
fn helper() {
    let Ok(mode) = std::env::var(HELPER) else {
        return;
    };
    let f: Vec<&str> = mode.split(':').collect();
    match f[..] {
        ["alloc", mib, hold] => {
            let n = mib.parse::<usize>().unwrap() << 20;
            let mut v: Vec<u8> = Vec::with_capacity(n);
            for i in (0..n).step_by(4096) {
                // SAFETY: within capacity; the page is touched so it is resident.
                unsafe { v.as_mut_ptr().add(i).write(1) };
            }
            std::thread::sleep(Duration::from_millis(hold.parse().unwrap()));
            std::hint::black_box(&v);
            std::process::exit(0);
        }
        ["tree", k, mib, hold] => {
            let kids: Vec<_> = (0..k.parse::<usize>().unwrap())
                .map(|_| helper_cmd(&format!("alloc:{mib}:{hold}")).spawn().unwrap())
                .collect();
            for mut c in kids {
                c.wait().unwrap();
            }
            std::process::exit(0);
        }
        _ => panic!("unknown helper mode {mode}"),
    }
}

fn helper_cmd(mode: &str) -> Command {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args(["--exact", "helper", "--nocapture", "--test-threads", "1"])
        .env(HELPER, mode)
        // One malloc arena, so the per-process address-space limit is not spent on reservations.
        .env("MALLOC_ARENA_MAX", "1")
        .stdout(Stdio::null());
    c
}

fn sh(script: &str) -> Command {
    let mut c = Command::new("sh");
    c.args(["-c", script]);
    c
}

/// The methods to test here: the fallback always, the scope where it works.
fn methods() -> Vec<Method> {
    let mut m = vec![Method::Rlimit];
    if Method::detect() == Method::Scope {
        m.push(Method::Scope);
    } else {
        eprintln!("scope path skipped: systemd-run --user --scope is not available here");
    }
    m
}

fn quiet(spec: CapSpec, cmd: &Command, method: Method) -> CapCommand {
    CapCommand::new(spec, cmd)
        .method(method)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
}

#[test]
fn status_passes_through() {
    for m in methods() {
        let spec = CapSpec::new(512 * MIB);
        for (script, want) in [
            ("exit 0", 0),
            ("exit 7", 7),
            ("kill -USR1 $$", 128 + libc_sigusr1()),
        ] {
            let o = quiet(spec, &sh(script), m).spawn().unwrap().wait().unwrap();
            assert_eq!(o.status, want, "{m:?} {script}");
            assert!(!o.limit_reached() && !o.stopped && o.started, "{m:?} {o:?}");
            assert!(o.report(&spec).is_empty());
        }
    }
}

fn libc_sigusr1() -> i32 {
    if cfg!(target_os = "linux") { 10 } else { 30 }
}

#[test]
fn env_cwd_and_stdout_are_carried_over() {
    for m in methods() {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = sh("printf '%s %s' \"$CAP_TEST\" \"$(pwd)\"; echo ${CAP_GONE:-unset}");
        cmd.env("CAP_TEST", "value")
            .env_remove("CAP_GONE")
            .current_dir(dir.path());
        let mut child = CapCommand::new(CapSpec::new(512 * MIB), &cmd)
            .method(m)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = String::new();
        child
            .take_stdout()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(child.wait().unwrap().status, 0);
        let want = format!(
            "value {}unset\n",
            dir.path().canonicalize().unwrap().display()
        );
        assert_eq!(out, want, "{m:?}");
    }
}

#[test]
fn time_limit_terms_then_kills() {
    for m in methods() {
        let spec = CapSpec::new(512 * MIB)
            .with_time(Duration::from_secs(1))
            .with_kill_after(Duration::from_secs(1));
        let start = Instant::now();
        let o = quiet(spec, &sh("sleep 30"), m)
            .spawn()
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            (o.status, o.time_limit, o.killed),
            (STATUS_TERM, true, false),
            "{m:?} {o:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(
            o.report(&spec)[0].contains("the 1s time limit was reached; sent TERM"),
            "{:?}",
            o.report(&spec)
        );

        let start = Instant::now();
        let o = quiet(spec, &sh("trap '' TERM; sleep 30"), m)
            .spawn()
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(
            (o.status, o.time_limit, o.killed),
            (STATUS_KILL, true, true),
            "{m:?} {o:?}"
        );
        let took = start.elapsed();
        assert!(
            took >= Duration::from_secs(2) && took < Duration::from_secs(8),
            "{m:?} took {took:?}"
        );
        assert!(
            o.report(&spec)[0]
                .contains("did not stop within 1s of TERM and was killed (exit status 137)")
        );
    }
}

#[test]
fn stopper_ends_the_tree() {
    for m in methods() {
        let spec = CapSpec::new(512 * MIB).with_kill_after(Duration::from_secs(1));
        let child = quiet(spec, &sh("sleep 30 & sleep 30; wait"), m)
            .spawn()
            .unwrap();
        let stop = child.stopper();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            stop.stop();
        });
        let start = Instant::now();
        let o = child.wait().unwrap();
        t.join().unwrap();
        assert_eq!(
            (o.status, o.stopped, o.time_limit),
            (STATUS_TERM, true, false),
            "{m:?} {o:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(!o.limit_reached() && o.report(&spec).is_empty());
    }
}

#[test]
fn fallback_watchdog_kills_a_tree_over_the_cap() {
    // Each process stays under its own address-space limit; together they pass the cap.
    let spec = CapSpec::new(300 * MIB);
    let o = CapCommand::new(spec, &helper_cmd("tree:3:150:5000"))
        .method(Method::Rlimit)
        .spawn()
        .unwrap()
        .wait()
        .unwrap();
    assert!(o.memory_limit, "{o:?}");
    assert_eq!(o.status, STATUS_KILL);
    assert!(o.oom_kills >= 1);
    assert!(o.peak_bytes.unwrap() > spec.mem_bytes);
    let report = o.report(&spec);
    assert!(
        report[0]
            .starts_with("hgl-ai cap: the 300M memory limit was reached (sampled resident total"),
        "{report:?}"
    );
    assert!(o.peak_line(&spec).unwrap().ends_with("of 300M (sampled)"));
}

#[test]
fn fallback_limits_one_process() {
    let spec = CapSpec::new(200 * MIB);
    let o = CapCommand::new(spec, &helper_cmd("alloc:512:0"))
        .method(Method::Rlimit)
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
        .wait()
        .unwrap();
    assert_ne!(
        o.status, 0,
        "the allocation must fail under the address-space limit: {o:?}"
    );
}

#[test]
fn scope_kills_a_command_over_the_cap() {
    if Method::detect() != Method::Scope {
        eprintln!("skipped: systemd-run --user --scope is not available here");
        return;
    }
    let spec = CapSpec::new(100 * MIB);
    let o = CapCommand::new(spec, &helper_cmd("alloc:512:1000"))
        .method(Method::Scope)
        .spawn()
        .unwrap()
        .wait()
        .unwrap();
    assert!(o.memory_limit, "{o:?}");
    assert!([STATUS_KILL, STATUS_TERM].contains(&o.status), "{o:?}");
    assert!(o.oom_kills >= 1);
    let report = o.report(&spec);
    assert!(
        report[0].starts_with("hgl-ai cap: the 100M memory limit was reached; the kernel killed"),
        "{report:?}"
    );

    // A run under the cap reports its peak.
    let o = CapCommand::new(CapSpec::new(512 * MIB), &helper_cmd("alloc:64:0"))
        .method(Method::Scope)
        .spawn()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(o.status, 0, "{o:?}");
    if let Some(p) = o.peak_bytes {
        assert!(p >= 64 * MIB, "peak {p}");
    } else {
        eprintln!("memory.peak not available on this kernel");
    }
}

#[test]
fn run_reports_a_missing_command() {
    let argv = ["hgl-ai-cap-no-such-command".into()];
    assert_eq!(
        hgl_ai_cap::run_with(&CapSpec::new(64 * MIB), Method::Rlimit, &argv).unwrap(),
        127
    );
    assert_eq!(
        hgl_ai_cap::run_with(&CapSpec::new(64 * MIB), Method::Rlimit, &["true".into()]).unwrap(),
        0
    );
}
