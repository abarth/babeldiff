//! End-to-end tests over the fixtures in `tests/fixtures`.
//!
//! Each test renders a report and compares it with an `expected*.txt` file
//! next to the fixture. Run with `BLESS=1` to rewrite the expected output
//! after an intentional change, and review the diff.

use babeldiff::analyze::{CppOrigin, Link, NoFinder, Options, Report};
use babeldiff::check::Severity;
use babeldiff::git::{Git, RepoFinder};
use babeldiff::input::ChangeSet;
use babeldiff::render::{render, Layout, RenderOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn check_golden(path: &Path, actual: &str) {
    if std::env::var_os("BLESS").is_some() {
        std::fs::write(path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(path)
        .unwrap_or_else(|_| panic!("missing {}; run with BLESS=1", path.display()));
    if expected != actual {
        let first = expected
            .lines()
            .zip(actual.lines())
            .position(|(a, b)| a != b)
            .unwrap_or(expected.lines().count().min(actual.lines().count()));
        panic!(
            "{} differs from the actual output starting at line {}:\n  expected: {:?}\n  actual:   {:?}\nRun with BLESS=1 to update.",
            path.display(),
            first + 1,
            expected.lines().nth(first),
            actual.lines().nth(first)
        );
    }
}

fn opts(layout: Layout) -> RenderOptions {
    RenderOptions {
        layout,
        width: 160,
        context: None,
        summary_only: false,
    }
}

/// A synthetic port with no planted differences, read from a patch.
fn beacon_report() -> Report {
    let text = std::fs::read_to_string(fixture("beacon/beacon.patch")).unwrap();
    let files = babeldiff::patch::parse(&text);
    let cs = ChangeSet::from_patch(&files, &mut |_| None);
    babeldiff::run(&cs, &Options::default(), &mut NoFinder)
}

#[test]
fn beacon_golden() {
    let report = beacon_report();
    check_golden(
        &fixture("beacon/expected.txt"),
        &render(&report, &opts(Layout::SideBySide)),
    );
}

#[test]
fn beacon_follows_ffi_shims_without_false_positives() {
    let report = beacon_report();
    let pair = report
        .pairs
        .iter()
        .find(|p| p.cpp.name == "BeaconDispatcher::Subscribe")
        .expect("Subscribe is paired");
    assert_eq!(pair.rust.name, "BeaconDispatcher::subscribe");
    assert!(
        matches!(&pair.link, Link::Ffi { shim, .. } if shim == "rust_beacon_dispatcher_subscribe")
    );
    // A faithful port: every function pairs and none has an issue.
    assert_eq!(report.pairs.len(), 6);
    for p in &report.pairs {
        assert_eq!(p.issues(), 0, "{}: {:#?}", p.cpp.name, p.findings);
        assert_eq!(
            p.summary.cpp_errors, p.summary.rust_errors,
            "{}",
            p.cpp.name
        );
    }
    assert!(report.unmatched_cpp.is_empty());
    assert!(report.unmatched_rust.is_empty());
    // Safety comments and `# Safety` docs are expected in Rust: no finding.
    for name in [
        "BeaconDispatcher::FindLocked",
        "BeaconDispatcher::GetSubscriber",
    ] {
        let p = report.pairs.iter().find(|p| p.cpp.name == name).unwrap();
        let safety: Vec<usize> = p
            .rust
            .units
            .iter()
            .filter(|u| u.features.safety)
            .map(|u| u.start_line)
            .collect();
        assert!(!safety.is_empty(), "{name}");
        for f in &p.findings {
            assert!(
                f.rust_line.is_none_or(|l| !safety.contains(&l)),
                "{name}: {f:?}"
            );
        }
    }
    // Shims are reported as shims, not as unpaired Rust.
    assert!(report
        .shims
        .iter()
        .any(|s| s.shim.name == "rust_beacon_dispatcher_flash"
            && s.target.as_deref() == Some("BeaconDispatcher::flash")));
}

/// Builds a git repository with the fixture's `before` and `after` trees as
/// two commits.
fn fifo_repo(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
            .args(args)
            .output()
            .expect("git is installed");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let copy = |from: &Path| {
        for entry in walk(from) {
            let rel = entry.strip_prefix(from).unwrap();
            let to = dir.join(rel);
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::copy(&entry, &to).unwrap();
        }
    };
    git(&["init", "-q"]);
    copy(&fixture("fifo/before"));
    git(&["add", "-A"]);
    git(&["commit", "-qm", "before"]);
    std::fs::remove_dir_all(dir.join("zircon")).unwrap();
    copy(&fixture("fifo/after"));
    git(&["add", "-A"]);
    git(&["commit", "-qm", "after"]);
    dir
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out.sort();
    out
}

fn fifo_report(name: &str) -> Report {
    let repo = fifo_repo(name);
    let git = Git::new(&repo);
    let (base, head) = Git::range("HEAD");
    let cs = git.changeset(&base, &head).unwrap();
    let mut finder = RepoFinder::new(git, base);
    let report = babeldiff::run(&cs, &Options::default(), &mut finder);
    let _ = std::fs::remove_dir_all(&repo);
    report
}

#[test]
fn fifo_golden() {
    let report = fifo_report("fifo-golden");
    check_golden(
        &fixture("fifo/expected.txt"),
        &render(&report, &opts(Layout::SideBySide)),
    );
    check_golden(
        &fixture("fifo/expected-stacked.txt"),
        &render(&report, &opts(Layout::Stacked)),
    );
}

#[test]
fn fifo_finds_planted_differences() {
    let report = fifo_report("fifo-planted");
    let pair = |name: &str| report.pairs.iter().find(|p| p.cpp.name == name).unwrap();
    let has = |name: &str, needle: &str| {
        pair(name)
            .findings
            .iter()
            .any(|f| f.severity == Severity::Issue && f.message.contains(needle))
    };
    // A different error code.
    assert!(has(
        "FifoDispatcher::WriteFromUser",
        "C++ returns PEER_CLOSED, Rust returns BAD_STATE"
    ));
    // A rollback path replaced by `?`.
    assert!(has(
        "FifoDispatcher::WriteSelfLocked",
        "C++ handles this call's error in its own branch, but Rust propagates it"
    ));
    assert!(has("FifoDispatcher::WriteSelfLocked", "only in C++"));
    // The lock is taken before the argument checks in Rust, after in C++.
    assert!(has("FifoDispatcher::ReadToUser", "order may differ"));
    // Comments carried over from the header's declaration comments.
    assert_eq!(
        pair("FifoDispatcher::WriteFromUser").summary.comments_same,
        2
    );
    // C++ the change left alone is found in the repository.
    let full = pair("FifoDispatcher::IsFullLocked");
    assert_eq!(full.origin, CppOrigin::Unchanged);
    assert_eq!(full.rust.name, "FifoDispatcher::is_full_locked");
    assert_eq!(full.issues(), 0);
}

#[test]
fn cli_exit_status_reflects_issues() {
    let bin = env!("CARGO_BIN_EXE_babeldiff");
    let before = fixture("fifo/before/zircon/kernel/object/fifo_dispatcher.cc");
    let after = fixture("fifo/after/zircon/kernel/object/fifo_dispatcher.rs");
    let out = Command::new(bin)
        .args(["--summary", "files"])
        .arg(&before)
        .arg(&after)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("FifoDispatcher::WriteSelfLocked  <->  FifoDispatcher::write_self_locked")
    );

    let same = Command::new(bin)
        .args(["files"])
        .arg(&before)
        .arg(&before)
        .output()
        .unwrap();
    assert_eq!(same.status.code(), Some(0));
}

#[test]
fn html_report_is_self_contained() {
    use babeldiff::html::{render_html, HtmlOptions};
    let report = fifo_report("fifo-html");
    let html = render_html(
        &report,
        &HtmlOptions {
            title: "fifo <test>".into(),
        },
    );
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("<title>babeldiff: fifo &lt;test&gt;</title>"));
    // Nothing is loaded from elsewhere.
    for needle in ["<link", "src=", "http://", "https://", "@import", "url("] {
        assert!(!html.contains(needle), "found {needle:?}");
    }
    // One section per pair, and a row for the planted error-code change.
    assert_eq!(
        html.matches("<section class=\"pair ").count(),
        report.pairs.len()
    );
    assert!(html.contains("error code differs: C++ returns PEER_CLOSED, Rust returns BAD_STATE"));
    assert!(html.contains("data-k=\"e:PEER_CLOSED\""));
    assert!(html.contains("data-k=\"e:BAD_STATE\""));
    // Source text is escaped.
    assert!(!html.contains("<const"));
    assert!(html.contains("&lt;<span class=\"kw\">const</span>"));
}

#[test]
fn cli_writes_html() {
    let bin = env!("CARGO_BIN_EXE_babeldiff");
    let out_path =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("beacon-{}.html", std::process::id()));
    let out = Command::new(bin)
        .args(["patch", "--format", "html", "-o"])
        .arg(&out_path)
        .arg(fixture("beacon/beacon.patch"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let html = std::fs::read_to_string(&out_path).unwrap();
    let _ = std::fs::remove_file(&out_path);
    assert!(html.contains(
        "<title>babeldiff: [kernel] Port BeaconDispatcher subscriptions to Rust</title>"
    ));
    assert_eq!(html.matches("<section class=\"pair ").count(), 6);
}
