//! Every parser against a small real-looking sample (`tests/fixtures/`).

use std::path::Path;

use hgl_ai_build::parse::{Outcome, Parsed, Reports, parse};
use hgl_ai_build::recipe::{ParserKind, Patterns};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn run(kind: ParserKind, name: &str) -> (Parsed, Vec<u8>) {
    let log = fixture(name);
    let p = parse(kind, &log, None, &Reports { files: vec![] });
    (p, log)
}

fn at(log: &[u8], start: u64, end: u64) -> String {
    String::from_utf8_lossy(&log[start as usize..end as usize]).into_owned()
}

#[test]
fn cargo_diagnostics() {
    let (p, log) = run(ParserKind::Cargo, "cargo-clippy.jsonl");
    let e: Vec<_> = p
        .errors
        .iter()
        .map(|e| {
            (
                e.file.as_deref(),
                e.line,
                e.col,
                e.code.as_deref(),
                e.message.as_str(),
            )
        })
        .collect();
    assert_eq!(
        e,
        [
            (
                Some("src/lib.rs"),
                Some(8),
                Some(5),
                Some("E0308"),
                "mismatched types"
            ),
            (
                Some("src/lib.rs"),
                Some(12),
                Some(12),
                Some("clippy::if_same_then_else"),
                "this `if` has identical blocks"
            ),
        ],
        "the primary span, no echo of the count, no `could not compile`"
    );
    assert_eq!(p.warnings.len(), 1, "the lib and test copies fold");
    assert_eq!(p.warnings[0].count, 2);
    assert_eq!(p.warnings[0].kind(), "unused_variables");
    assert!(at(&log, p.errors[0].start, p.errors[0].end).contains("\"E0308\""));
    assert_eq!(p.cache, Some("partial"));
}

#[test]
fn cargo_tests() {
    let (p, log) = run(ParserKind::Cargo, "cargo-test.txt");
    let names: Vec<_> = p
        .tests
        .iter()
        .map(|t| (t.name.as_str(), t.outcome))
        .collect();
    assert_eq!(
        names,
        [
            ("tests::adds", Outcome::Pass),
            ("tests::subtracts", Outcome::Fail),
            ("tests::divides", Outcome::Fail),
            ("tests::slow", Outcome::Skip),
            ("tests::parses", Outcome::Pass),
        ]
    );
    let sub = &p.tests[1];
    assert_eq!(
        sub.message,
        "assertion `left == right` failed\n  left: 1\n right: 2\nat src/lib.rs:21:9"
    );
    assert!(at(&log, sub.start, sub.end).starts_with("---- tests::subtracts stdout ----"));
    assert_eq!(
        p.tests[2].message,
        "attempt to divide by zero\nat src/lib.rs:30:5"
    );
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    assert_eq!(p.cache, Some("hit"));
}

#[test]
fn gcc() {
    let (p, log) = run(ParserKind::Gcc, "gcc.txt");
    let e: Vec<_> = p
        .errors
        .iter()
        .map(|e| (e.file.as_deref(), e.line, e.code.as_deref(), e.count))
        .collect();
    assert_eq!(
        e,
        [
            (
                Some("/work/src/runtime/cpp/include/hglrt/values.h"),
                Some(42),
                None,
                2
            ),
            (
                Some("/work/src/runtime/cpp/src/text.cpp"),
                Some(9),
                Some("-Werror=format"),
                1
            ),
            (Some("main.cpp"), None, Some("ld"), 1),
            (None, None, Some("collect2"), 1),
        ]
    );
    assert_eq!(
        p.errors[0].message,
        "'widht' was not declared in this scope; did you mean 'width'?"
    );
    assert!(!at(&log, p.errors[0].start, p.errors[0].end).contains("In file included"));
    assert_eq!(p.warnings.len(), 1);
    assert_eq!(p.warnings[0].kind(), "-Wunused-variable");
    assert_eq!(p.warnings[0].col, Some(9));
}

#[test]
fn clang() {
    let (p, _) = run(ParserKind::Clang, "clang.txt");
    let msgs: Vec<_> = p.errors.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(msgs[0], "'hgraph/types/generated.h' file not found");
    assert_eq!(p.errors[0].code.as_deref(), Some("fatal"));
    assert!(msgs.contains(&"undefined symbols for architecture arm64: _hglrt_start, _hglrt_stop"));
    assert!(msgs.contains(&"symbol(s) not found for architecture arm64"));
    assert!(msgs.iter().any(|m| m.starts_with("linker command failed")));
    let cm = p
        .errors
        .iter()
        .find(|e| e.code.as_deref() == Some("cmake"))
        .unwrap();
    assert_eq!(
        (cm.file.as_deref(), cm.line),
        (Some("CMakeLists.txt"), Some(14))
    );
    assert!(
        cm.message
            .starts_with("Could not find a package configuration file provided by \"hgraph\"")
    );
    assert!(cm.message.contains("hgraph-config.cmake"));
    assert_eq!(p.warnings[0].kind(), "-Wunused-but-set-variable");
}

#[test]
fn msvc() {
    let (p, _) = run(ParserKind::Msvc, "msvc.txt");
    let e: Vec<_> = p
        .errors
        .iter()
        .map(|e| (e.file.as_deref(), e.line, e.col, e.code.as_deref()))
        .collect();
    assert_eq!(
        e,
        [
            (
                Some(r"C:\work\src\runtime\cpp\src\values.cpp"),
                Some(42),
                Some(12),
                Some("C2065")
            ),
            (Some("main.obj"), None, None, Some("LNK2019")),
            (None, None, None, Some("LNK1120")),
        ]
    );
    assert_eq!(p.errors[0].message, "'widht': undeclared identifier");
    assert!(
        p.errors[1].message.ends_with("referenced in function main"),
        "the project suffix is cut"
    );
    let kinds: Vec<_> = p.warnings.iter().map(|w| w.kind()).collect();
    assert_eq!(kinds, ["C4101", "C4996", "D9002"]);
}

#[test]
fn ctest() {
    let (p, log) = run(ParserKind::Ctest, "ctest.txt");
    let t: Vec<_> = p
        .tests
        .iter()
        .map(|t| (t.name.as_str(), t.outcome))
        .collect();
    assert_eq!(
        t,
        [
            ("hglrt_values", Outcome::Pass),
            ("hglrt_scalar", Outcome::Fail),
            ("hglrt_takes_no_stdlib", Outcome::Fail),
            ("hglrt_slow", Outcome::Skip),
        ]
    );
    assert_eq!(
        p.tests[1].message,
        "Failed: /work/src/runtime/cpp/tests/scalar.cpp:88: FAILED:"
    );
    assert!(at(&log, p.tests[1].start, p.tests[1].end).contains("3 == 4"));
    assert_eq!(p.tests[2].message, "Exception: SegFault");
}

#[test]
fn junit_from_reports_and_log() {
    for (file, kind) in [
        ("catch2.xml", ParserKind::Catch2),
        ("gtest.xml", ParserKind::Junit),
    ] {
        let xml = fixture(file);
        // As an artifact.
        let p = parse(
            kind,
            b"unrelated log\n",
            None,
            &Reports {
                files: vec![(format!("artifacts/test/{file}"), &xml)],
            },
        );
        // And inside the log.
        let mut log = b"running tests\n".to_vec();
        log.extend_from_slice(&xml);
        let q = parse(kind, &log, None, &Reports { files: vec![] });
        assert_eq!(p.tests.len(), q.tests.len(), "{file}");
        let failed: Vec<_> = p
            .tests
            .iter()
            .filter(|t| t.outcome == Outcome::Fail)
            .collect();
        assert_eq!(failed.len(), 1, "{file}");
        assert!(failed[0].source.as_deref().unwrap().ends_with(file));
        let skipped = p
            .tests
            .iter()
            .filter(|t| t.outcome == Outcome::Skip)
            .count();
        assert_eq!(skipped, 1, "{file}");
        let seg = at(&xml, failed[0].start, failed[0].end);
        assert!(
            seg.starts_with("<testcase") && seg.ends_with("</testcase>"),
            "{seg}"
        );
        if file == "catch2.xml" {
            assert_eq!(failed[0].name, "division & zero policy");
            assert_eq!(
                failed[0].message,
                "hglrt::div__f64_f64(1.0, 0.0) == 0.0\nat /work/src/runtime/cpp/tests/scalar.cpp:120"
            );
            assert_eq!(p.tests.len(), 4);
        } else {
            assert_eq!(failed[0].name, "Clock.Rewinds");
            assert!(
                failed[0]
                    .message
                    .starts_with("/work/src/tests/clock.cpp:40\nExpected equality")
            );
            assert_eq!(p.tests.len(), 3);
        }
    }
}

#[test]
fn hgl_test() {
    let (p, _) = run(ParserKind::HglTest, "hgl-test.txt");
    let t: Vec<_> = p
        .tests
        .iter()
        .map(|t| (t.name.as_str(), t.outcome))
        .collect();
    assert_eq!(
        t,
        [
            ("rejection demo.calc::rejects_cycles", Outcome::Pass),
            ("rejection demo.calc::rejects_bad_key", Outcome::Fail),
            ("executable demo.calc::adds", Outcome::Pass),
            ("executable demo.calc::divides", Outcome::Fail),
            ("executable demo.calc::streams", Outcome::Skip),
        ]
    );
    assert_eq!(
        p.tests[1].message,
        "expected rejection type/type.mismatch at cases/pack.hgl:13:9\ngot none"
    );
    let e: Vec<_> = p
        .errors
        .iter()
        .map(|e| (e.file.as_deref(), e.line, e.code.as_deref()))
        .collect();
    assert_eq!(
        e,
        [
            (Some("cases/pack.hgl"), Some(12), Some("type/type.mismatch")),
            (Some("cases/units.hgl"), Some(4), None),
        ]
    );
    // Totals alone.
    let q = parse(
        ParserKind::HglTest,
        b"rejection cases: 3 passed, 0 failed\nexecutable cases: 5 passed, 1 failed, 2 not run\n",
        None,
        &Reports { files: vec![] },
    );
    assert_eq!(q.unnamed, (8, 1, 2));
}

#[test]
fn lines_with_patterns_and_defaults() {
    let log = fixture("lines.txt");
    let pats = Patterns {
        error: vec![r"^ERROR (?P<file>[^:]+):(?P<line>\d+): (?P<message>.*)$".into()],
        warning: vec![r"^WARN\s+(?P<file>[^:]+):(?P<line>\d+): (?P<message>.*)$".into()],
    };
    let p = parse(
        ParserKind::Lines,
        &log,
        Some(&pats),
        &Reports { files: vec![] },
    );
    assert_eq!(p.errors.len(), 1);
    assert_eq!(p.errors[0].count, 2);
    assert_eq!(p.errors[0].message, "unknown native `hgl.add2`");
    assert_eq!(
        (p.errors[0].file.as_deref(), p.errors[0].line),
        (Some("core/lib.hgl"), Some(7))
    );
    assert_eq!(p.warnings[0].message, "deprecated spelling");
    let d = parse(
        ParserKind::Lines,
        b"ok\nerror: disk full\nWarning: slow\nfatal error: gone\n",
        None,
        &Reports { files: vec![] },
    );
    let m: Vec<_> = d.errors.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(m, ["disk full", "gone"]);
    assert_eq!(d.warnings[0].message, "slow");
}

/// The recipes kept in the repository parse and validate.
#[test]
fn repository_recipes_validate() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recipes");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "toml") {
            let text = std::fs::read_to_string(&p).unwrap();
            let r = hgl_ai_build::Recipe::parse(&text)
                .unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            assert_eq!(p.file_stem().unwrap().to_str().unwrap(), r.repo);
            n += 1;
        }
    }
    assert_eq!(n, 3);
}
