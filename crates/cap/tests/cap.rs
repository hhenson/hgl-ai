//! Process tests of the cap. The rlimit fallback is tested everywhere; the
//! systemd scope only where `systemd-run --user --scope` works (else skipped
//! with the reason printed). Memory hogs are this test binary re-run as a
//! helper (`HGL_AI_CAP_TEST_HELPER`). The tests that go over a cap on purpose
//! (the kernel or the watchdog kills them, which a desktop reports as an
//! out-of-memory kill) run only with `HGL_AI_OOM_TESTS=1`, as CI sets it.

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
        // A cap inside the cap: `SIZE` for `alloc:MIB:0`, with the detected method.
        ["nested", size, mib] => {
            let spec = CapSpec::new(size.parse::<u64>().unwrap() << 20);
            let inner = helper_cmd(&format!("alloc:{mib}:0"));
            let argv: Vec<std::ffi::OsString> = std::iter::once(inner.get_program().to_owned())
                .chain(inner.get_args().map(|a| a.to_owned()))
                .collect();
            // The helper's own settings, as the environment carries them.
            inner.get_envs().for_each(|(k, v)| {
                // SAFETY: single-threaded at this point of the helper.
                unsafe {
                    match v {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            });
            let code = hgl_ai_cap::run(&spec, &argv).unwrap();
            std::process::exit(code);
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

/// Whether the tests that go over a cap on purpose run here; says why not when they do not.
fn oom_tests(name: &str) -> bool {
    if std::env::var_os("HGL_AI_OOM_TESTS").is_some_and(|v| v == "1") {
        return true;
    }
    eprintln!(
        "skipped: {name} goes over a memory cap on purpose; set HGL_AI_OOM_TESTS=1 to run it"
    );
    false
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
    if !oom_tests("fallback_watchdog_kills_a_tree_over_the_cap") {
        return;
    }
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

// macOS accepts RLIMIT_AS but does not enforce it; there the cap's resident-memory
// watchdog is what limits a command tree (see the tree-wide watchdog tests).
#[test]
#[cfg_attr(target_os = "macos", ignore = "macOS does not enforce RLIMIT_AS")]
fn fallback_limits_one_process() {
    if !oom_tests("fallback_limits_one_process") {
        return;
    }
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
    if !oom_tests("scope_kills_a_command_over_the_cap") {
        return;
    }
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
    // The kernel's `oom_kill` count can lag the kill; the limit is known either way.
    assert!(o.memory_limit, "{o:?}");
    assert!([STATUS_KILL, STATUS_TERM].contains(&o.status), "{o:?}");
    let report = o.report(&spec);
    assert_eq!(report.len(), 1, "{report:?}");
    assert!(
        report[0].starts_with("hgl-ai cap: the 100M memory limit was reached; the kernel killed")
            && report[0].ends_with(&format!("(exit status {})", o.status)),
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

#[test]
fn without_the_address_limit_only_the_watchdog_limits() {
    let inherited = sh("ulimit -v").output().unwrap();
    assert!(inherited.status.success());
    let inherited = String::from_utf8(inherited.stdout).unwrap();
    let read = |spec: CapSpec| {
        let mut child = CapCommand::new(spec, &sh("ulimit -v"))
            .method(Method::Rlimit)
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
        out.trim().to_string()
    };
    let on = CapSpec::new(512 * MIB);
    if cfg!(target_os = "linux") {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `limit` is valid storage for the inherited limit.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut limit) }, 0);
        let expected = (512 * MIB as libc::rlim_t).min(limit.rlim_max) / 1024;
        assert_eq!(read(on), expected.to_string());
    }
    assert_eq!(read(on.with_address_limit(false)), inherited.trim());
    assert!(
        !Method::Rlimit
            .warning(&on.with_address_limit(false))
            .unwrap()
            .contains("address-space")
    );
    // The watchdog still kills a tree over the cap.
    if !oom_tests("without_the_address_limit_only_the_watchdog_limits (its over-cap part)") {
        return;
    }
    let o = CapCommand::new(
        CapSpec::new(200 * MIB).with_address_limit(false),
        &helper_cmd("alloc:400:5000"),
    )
    .method(Method::Rlimit)
    .spawn()
    .unwrap()
    .wait()
    .unwrap();
    assert!(o.memory_limit, "{o:?}");
}

/// A process that left the parent chain (its parent exited) is still in the command's
/// process group, and the group is signalled (all a macOS reaper has, hgl-ai#10).
#[test]
fn the_process_group_reaches_an_orphan() {
    for m in methods() {
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("pid");
        let script = format!(
            "(sleep 60 </dev/null >/dev/null 2>&1 & echo $! > {}); sleep 60",
            pid.display()
        );
        let child = quiet(
            CapSpec::new(512 * MIB)
                .with_time(Duration::from_millis(700))
                .with_kill_after(Duration::from_millis(500)),
            &sh(&script),
            m,
        )
        .process_group()
        .spawn()
        .unwrap();
        assert_eq!(child.pgid(), Some(child.id()));
        let o = child.wait().unwrap();
        assert!(o.time_limit, "{m:?} {o:?}");
        let orphan: i32 = std::fs::read_to_string(&pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        // SAFETY: signal 0 only checks existence.
        while unsafe { libc::kill(orphan, 0) } == 0 && !zombie(orphan) {
            assert!(
                Instant::now() < end,
                "{m:?}: the orphan {orphan} outlived the cap"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn zombie(pid: i32) -> bool {
    Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim_start()
                .starts_with('Z')
        })
}

/// A turn's slice holds the turn's limit for the caps run inside it (Carbon's probe of
/// hgl-ai PR 1: a 128M turn ran a 512M cap that allocated 250M).
#[test]
fn a_cap_inside_a_turn_slice_is_held_to_the_turn_limit() {
    if Method::detect() != Method::Scope {
        eprintln!("skipped: systemd-run --user --scope is not available here");
        return;
    }
    if !oom_tests("a_cap_inside_a_turn_slice_is_held_to_the_turn_limit") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let slice = hgl_ai_cap::turn_slice(&format!("cap-test-{}", std::process::id()));
    let mut cmd = helper_cmd("nested:512:250");
    cmd.env("TMPDIR", tmp.path());
    let o = CapCommand::new(CapSpec::new(128 * MIB), &cmd)
        .method(Method::Scope)
        .slice(&slice)
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(
        o.command_status, STATUS_KILL,
        "the inner cap reports the kill: {o:?}"
    );
}

/// A turn's slice is named to its command, holds it, takes its report file in the
/// command's `TMPDIR`, and is gone with the run.
#[test]
fn a_turn_slice_is_named_to_its_command_and_removed() {
    if Method::detect() != Method::Scope {
        eprintln!("skipped: systemd-run --user --scope is not available here");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let slice = hgl_ai_cap::turn_slice(&format!("cap-test-named-{}", std::process::id()));
    assert!(slice.starts_with("app-hgl_ai_turn_cap_test_named_") && slice.ends_with(".slice"));
    let mut cmd = sh("printf '%s\\n' \"$HGL_AI_SLICE\"; cat /proc/self/cgroup; ls \"$TMPDIR\"");
    cmd.env("TMPDIR", tmp.path());
    let mut child = CapCommand::new(CapSpec::new(128 * MIB), &cmd)
        .method(Method::Scope)
        .slice(&slice)
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
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some(slice.as_str()));
    assert!(
        lines.next().unwrap().contains(&format!("/{slice}/")),
        "{out}"
    );
    assert!(
        lines.any(|l| l.starts_with("hgl-ai-cap-")),
        "the report file is in the command's TMPDIR: {out}"
    );
    // The slice is gone with the run.
    let show = Command::new("systemctl")
        .args(["--user", "show", "-p", "ActiveState", &slice])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&show.stdout).contains("inactive"),
        "{show:?}"
    );
}

/// The slice of a bridge that died before it could stop it is removed by the next one.
#[test]
fn a_dead_bridges_turn_slice_is_removed() {
    if Method::detect() != Method::Scope {
        eprintln!("skipped: systemd-run --user --scope is not available here");
        return;
    }
    let mut c = Command::new("true").spawn().unwrap();
    let dead = c.id();
    c.wait().unwrap();
    let slice = hgl_ai_cap::turn_slice(&format!("cap-test-{dead}-0"));
    let live = hgl_ai_cap::turn_slice(&format!("cap-test-{}-0", std::process::id()));
    for s in [&slice, &live] {
        assert!(
            Command::new("systemctl")
                .args(["--user", "set-property", "--runtime", s, "MemoryMax=1G"])
                .status()
                .unwrap()
                .success()
        );
    }
    let gone = hgl_ai_cap::remove_stale_turn_slices();
    assert!(gone.contains(&slice), "{gone:?}");
    assert!(!gone.contains(&live), "a live process's slice stays");
    let _ = Command::new("systemctl")
        .args(["--user", "revert", &live])
        .output();
}
