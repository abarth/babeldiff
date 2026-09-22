//! Compares aligned units and reports where the Rust does something
//! different from the C++.

use crate::align::{similarity, Pair};
use crate::model::{Function, Ret, Unit, UnitKind};
use crate::normalize::lcs_len;

/// How much a finding matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// A behavioral difference or something missing: a different error
    /// code, lock, control flow step, call, or a lost comment.
    Issue,
    /// A difference worth a look that is often just translation, such as
    /// reworded comments or different helper calls.
    Note,
}

impl Severity {
    pub fn marker(self) -> char {
        match self {
            Severity::Issue => '!',
            Severity::Note => '~',
        }
    }
}

/// How a row of the side-by-side view is marked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    /// Aligned and equivalent.
    Same,
    /// Aligned, with a note.
    Note,
    /// Aligned, with an issue.
    Issue,
    /// Only in C++.
    CppOnly,
    /// Only in Rust.
    RustOnly,
}

impl Marker {
    pub fn symbol(self) -> char {
        match self {
            Marker::Same => '=',
            Marker::Note => '~',
            Marker::Issue => '!',
            Marker::CppOnly => '<',
            Marker::RustOnly => '>',
        }
    }
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub severity: Severity,
    pub cpp_line: Option<usize>,
    pub rust_line: Option<usize>,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub cpp: Option<usize>,
    pub rust: Option<usize>,
    pub marker: Marker,
    pub notes: Vec<(Severity, String)>,
}

/// Builds the rows and findings for an aligned pair of functions.
pub fn check(cpp: &Function, rust: &Function, pairs: &[Pair]) -> (Vec<Row>, Vec<Finding>) {
    let mut rows = Vec::new();
    let mut findings = Vec::new();
    let unmatched_c: Vec<usize> = pairs
        .iter()
        .filter(|p| p.rust.is_none())
        .filter_map(|p| p.cpp)
        .collect();
    let unmatched_r: Vec<usize> = pairs
        .iter()
        .filter(|p| p.cpp.is_none())
        .filter_map(|p| p.rust)
        .collect();

    for p in pairs {
        let mut notes: Vec<(Severity, String)> = Vec::new();
        let marker = match (p.cpp, p.rust) {
            (Some(i), Some(j)) => {
                compare(&cpp.units[i], &rust.units[j], &mut notes);
                if notes.iter().any(|(s, _)| *s == Severity::Issue) {
                    Marker::Issue
                } else if notes.is_empty() {
                    Marker::Same
                } else {
                    Marker::Note
                }
            }
            (Some(i), None) => {
                let u = &cpp.units[i];
                let moved =
                    find_moved(u, &rust.units, &unmatched_r).map(|j| rust.units[j].start_line);
                notes.push(only_in(u, "C++", "Rust", moved));
                Marker::CppOnly
            }
            (None, Some(j)) => {
                let u = &rust.units[j];
                let moved =
                    find_moved(u, &cpp.units, &unmatched_c).map(|i| cpp.units[i].start_line);
                notes.push(only_in(u, "Rust", "C++", moved));
                Marker::RustOnly
            }
            (None, None) => continue,
        };
        rows.push(Row {
            cpp: p.cpp,
            rust: p.rust,
            marker,
            notes,
        });
    }
    group_runs(&mut rows, cpp, rust);
    for r in &rows {
        for (sev, msg) in &r.notes {
            findings.push(Finding {
                severity: *sev,
                cpp_line: r.cpp.map(|i| cpp.units[i].start_line),
                rust_line: r.rust.map(|j| rust.units[j].start_line),
                message: msg.clone(),
            });
        }
    }
    (rows, findings)
}

/// Collapses runs of three or more consecutive one-sided rows into a single
/// note on the first row, so a block of code with no counterpart reads as
/// one finding rather than one per statement.
fn group_runs(rows: &mut [Row], cpp: &Function, rust: &Function) {
    let mut i = 0;
    while i < rows.len() {
        let m = rows[i].marker;
        if !matches!(m, Marker::CppOnly | Marker::RustOnly) {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < rows.len() && rows[j].marker == m {
            j += 1;
        }
        if j - i >= 3 {
            let (f, side, other) = if m == Marker::CppOnly {
                (cpp, "C++", "Rust")
            } else {
                (rust, "Rust", "C++")
            };
            let units: Vec<&Unit> = rows[i..j]
                .iter()
                .filter_map(|r| if m == Marker::CppOnly { r.cpp } else { r.rust })
                .map(|k| &f.units[k])
                .collect();
            let mut counts: Vec<(&'static str, usize)> = Vec::new();
            for u in &units {
                let name = match u.kind {
                    UnitKind::Stmt => "statement",
                    k => k.name(),
                };
                match counts.iter_mut().find(|(n, _)| *n == name) {
                    Some(c) => c.1 += 1,
                    None => counts.push((name, 1)),
                }
            }
            let sev = rows[i..j]
                .iter()
                .flat_map(|r| r.notes.iter().map(|n| n.0))
                .min()
                .unwrap_or(Severity::Note);
            let first = units.first().map_or(0, |u| u.start_line);
            let last = units.last().map_or(0, |u| u.end_line);
            let what: Vec<String> = counts.iter().map(|(n, c)| format!("{c} {n}")).collect();
            let msg = format!(
                "lines {first}-{last} only in {side}, with no {other} counterpart: {}",
                what.join(", ")
            );
            for r in &mut rows[i..j] {
                r.notes.clear();
            }
            rows[i].notes.push((sev, msg));
        }
        i = j;
    }
}

fn find_moved(u: &Unit, others: &[Unit], candidates: &[usize]) -> Option<usize> {
    let trivial = u.kind != UnitKind::Comment
        && u.features.calls.is_empty()
        && u.features.errors.is_empty()
        && u.features.locks.is_empty();
    if trivial {
        return None;
    }
    candidates
        .iter()
        .map(|&j| (j, similarity(u, &others[j])))
        .filter(|(_, s)| *s >= 0.7)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

fn only_in(u: &Unit, here: &str, there: &str, moved: Option<usize>) -> (Severity, String) {
    let f = &u.features;
    let what = match u.kind {
        UnitKind::Comment => "comment".to_string(),
        UnitKind::Stmt => {
            let mut parts = Vec::new();
            if !f.locks.is_empty() {
                parts.push(format!("acquires {}", f.locks.join(", ")));
            }
            if !f.errors.is_empty() {
                parts.push(format!("uses {}", f.errors.join(", ")));
            }
            if f.propagates {
                parts.push("propagates an error".to_string());
            }
            if !f.calls.is_empty() {
                parts.push(format!("calls {}", f.calls.join(", ")));
            }
            if parts.is_empty() {
                "statement".to_string()
            } else {
                format!("statement ({})", parts.join("; "))
            }
        }
        UnitKind::Return => match &f.ret {
            Some(r) => format!("return of {r}"),
            None => "return".to_string(),
        },
        k => k.name().to_string(),
    };
    let significant = match u.kind {
        // A comment lost in translation matters; an added one is worth a look.
        UnitKind::Comment => here == "C++",
        UnitKind::Stmt => {
            !f.calls.is_empty()
                || !f.locks.is_empty()
                || !f.errors.is_empty()
                || f.propagates
                || f.unlocks
        }
        UnitKind::Signature => false,
        _ => true,
    };
    let sev = if significant {
        Severity::Issue
    } else {
        Severity::Note
    };
    let mut msg = format!("{what} only in {here}");
    if let Some(line) = moved {
        msg.push_str(&format!(
            "; resembles {there} line {line}, so the order may differ"
        ));
    }
    (sev, msg)
}

fn compare(a: &Unit, b: &Unit, notes: &mut Vec<(Severity, String)>) {
    let fa = &a.features;
    let fb = &b.features;
    if crate::align::is_handled_vs_propagated(a, b) {
        let msg = if a.features.propagates {
            "C++ propagates this call's error, but Rust handles it in its own branch"
        } else {
            "C++ handles this call's error in its own branch, but Rust propagates it"
        };
        notes.push((Severity::Issue, msg.to_string()));
        return;
    }
    if a.kind != b.kind {
        notes.push((
            Severity::Note,
            format!("C++ {} vs Rust {}", a.kind.name(), b.kind.name()),
        ));
    }
    if a.kind == UnitKind::Comment {
        if fa.comment != fb.comment {
            notes.push((Severity::Note, comment_diff(&fa.comment, &fb.comment)));
        }
        return;
    }
    match (&fa.ret, &fb.ret) {
        (Some(Ret::Error(x)), Some(Ret::Error(y))) if x != y => {
            notes.push((
                Severity::Issue,
                format!("error code differs: C++ returns {x}, Rust returns {y}"),
            ));
        }
        (Some(ra @ (Ret::Error(_) | Ret::Ok)), Some(rb))
        | (Some(rb), Some(ra @ (Ret::Error(_) | Ret::Ok)))
            if ra != rb && !matches!((ra, rb), (Ret::Error(_), Ret::Error(_))) =>
        {
            let (c, r) = if fa.ret.as_ref() == Some(ra) {
                (ra, rb)
            } else {
                (rb, ra)
            };
            notes.push((
                Severity::Issue,
                format!("C++ returns {c}, Rust returns {r}"),
            ));
        }
        _ => {}
    }
    if fa.ret.is_none() || fb.ret.is_none() {
        let (ea, eb) = (sorted(&fa.errors), sorted(&fb.errors));
        if ea != eb {
            notes.push((
                Severity::Issue,
                format!(
                    "error codes differ: C++ [{}], Rust [{}]",
                    ea.join(", "),
                    eb.join(", ")
                ),
            ));
        }
    }
    if fa.locks != fb.locks {
        notes.push((
            Severity::Issue,
            format!(
                "locks differ: C++ acquires [{}], Rust acquires [{}]",
                fa.locks.join(", "),
                fb.locks.join(", ")
            ),
        ));
    }
    if fa.propagates != fb.propagates {
        let (who, other) = if fa.propagates {
            ("C++", "Rust")
        } else {
            ("Rust", "C++")
        };
        notes.push((
            Severity::Issue,
            format!("{who} propagates an error here but {other} does not"),
        ));
    }
    if fa.unlocks != fb.unlocks {
        let who = if fa.unlocks { "C++" } else { "Rust" };
        notes.push((Severity::Note, format!("only {who} releases a lock here")));
    }
    if fa.asserts != fb.asserts {
        let who = if fa.asserts { "C++" } else { "Rust" };
        notes.push((Severity::Note, format!("only {who} asserts here")));
    }
    let only_a: Vec<&String> = uniq(&fa.calls)
        .into_iter()
        .filter(|c| !fb.calls.contains(c))
        .collect();
    let only_b: Vec<&String> = uniq(&fb.calls)
        .into_iter()
        .filter(|c| !fa.calls.contains(c))
        .collect();
    if !only_a.is_empty() || !only_b.is_empty() {
        let mut parts = Vec::new();
        if !only_a.is_empty() {
            parts.push(format!("only C++ calls {}", join(&only_a)));
        }
        if !only_b.is_empty() {
            parts.push(format!("only Rust calls {}", join(&only_b)));
        }
        notes.push((Severity::Note, parts.join("; ")));
    }
}

fn sorted(v: &[String]) -> Vec<String> {
    let mut v = v.to_vec();
    v.sort();
    v.dedup();
    v
}

fn uniq(v: &[String]) -> Vec<&String> {
    let mut out: Vec<&String> = Vec::new();
    for x in v {
        if !out.contains(&x) {
            out.push(x);
        }
    }
    out
}

fn join(v: &[&String]) -> String {
    v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
}

/// Describes how two comments' words differ.
fn comment_diff(a: &[String], b: &[String]) -> String {
    // Walk the LCS to find words unique to each side.
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let (mut only_a, mut only_b) = (Vec::new(), Vec::new());
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j >= m || (i < n && dp[i + 1][j] >= dp[i][j + 1]) {
            only_a.push(a[i].as_str());
            i += 1;
        } else {
            only_b.push(b[j].as_str());
            j += 1;
        }
    }
    let common = lcs_len(a, b);
    let clip = |v: &[&str]| {
        if v.len() > 12 {
            format!("{} ...", v[..12].join(" "))
        } else {
            v.join(" ")
        }
    };
    let mut parts = vec![format!("comment differs ({common} words shared)")];
    if !only_a.is_empty() {
        parts.push(format!("C++ only: \"{}\"", clip(&only_a)));
    }
    if !only_b.is_empty() {
        parts.push(format!("Rust only: \"{}\"", clip(&only_b)));
    }
    parts.join("; ")
}

/// Per-function summaries of the facts a reviewer checks first.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub cpp_errors: Vec<String>,
    pub rust_errors: Vec<String>,
    pub cpp_locks: Vec<String>,
    pub rust_locks: Vec<String>,
    pub comments_total: usize,
    pub comments_same: usize,
    pub comments_changed: usize,
    /// (kind, C++ count, Rust count)
    pub flow: Vec<(&'static str, usize, usize)>,
}

pub fn summarize(cpp: &Function, rust: &Function, rows: &[Row]) -> Summary {
    let errs = |f: &Function| -> Vec<String> {
        f.units
            .iter()
            .filter_map(|u| match (&u.features.ret, u.features.propagates) {
                (Some(Ret::Error(e)), _) => Some(e.clone()),
                (_, true) => Some("?".to_string()),
                _ => None,
            })
            .collect()
    };
    let locks = |f: &Function| -> Vec<String> {
        f.units
            .iter()
            .flat_map(|u| u.features.locks.clone())
            .collect()
    };
    let mut s = Summary {
        cpp_errors: errs(cpp),
        rust_errors: errs(rust),
        cpp_locks: locks(cpp),
        rust_locks: locks(rust),
        ..Summary::default()
    };
    for r in rows {
        if let Some(i) = r.cpp {
            if cpp.units[i].kind == UnitKind::Comment {
                s.comments_total += 1;
                match r.marker {
                    Marker::Same => s.comments_same += 1,
                    Marker::Note | Marker::Issue => s.comments_changed += 1,
                    _ => {}
                }
            }
        }
    }
    let kinds: [(&'static str, &[UnitKind]); 6] = [
        ("if", &[UnitKind::If, UnitKind::ElseIf]),
        ("else", &[UnitKind::Else]),
        ("loop", &[UnitKind::Loop]),
        ("switch", &[UnitKind::Switch, UnitKind::Case]),
        ("return", &[UnitKind::Return]),
        ("break/continue", &[UnitKind::Break, UnitKind::Continue]),
    ];
    for (name, ks) in kinds {
        let count = |f: &Function| f.units.iter().filter(|u| ks.contains(&u.kind)).count();
        let (a, b) = (count(cpp), count(rust));
        if a + b > 0 {
            s.flow.push((name, a, b));
        }
    }
    s
}
