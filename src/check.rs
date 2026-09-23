//! Compares aligned units and reports where the Rust does something
//! different from the C++.

use crate::align::{similarity, Pair};
use crate::model::{Function, Lang, Ret, Unit, UnitKind};
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

/// What a finding is about. Reviewers and agents can filter on it, and each
/// maps to the part of the porting rubric it checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    /// A comment lost, added or reworded.
    Comment,
    /// Error codes, error propagation, or success versus failure.
    ErrorPath,
    /// Locks taken or released.
    Lock,
    /// Branches, loops, returns and other control flow.
    ControlFlow,
    /// Calls one side makes and the other doesn't.
    Call,
    /// Assertions.
    Assert,
    /// Trace and debug printing.
    Trace,
    /// The same steps in a different order.
    Order,
    /// How the two sides were paired.
    Pairing,
    /// Memory ordering of atomic operations.
    Atomic,
}

impl Category {
    /// Short, stable name for output formats.
    pub fn name(self) -> &'static str {
        match self {
            Category::Comment => "comment",
            Category::ErrorPath => "error-path",
            Category::Lock => "lock",
            Category::ControlFlow => "control-flow",
            Category::Call => "call",
            Category::Assert => "assert",
            Category::Trace => "trace",
            Category::Order => "order",
            Category::Pairing => "pairing",
            Category::Atomic => "atomic",
        }
    }

    /// The part of Zircon's C++ to Rust porting rubric the finding relates to.
    pub fn rubric(self) -> &'static str {
        match self {
            Category::Comment => "comment parity (rubric 3.15, pitfall 22)",
            Category::ErrorPath => "behavioral parity of error paths",
            Category::Lock => "locking parity (rubric 3.4)",
            Category::ControlFlow | Category::Call | Category::Order => "direct translation",
            Category::Assert => "assertions (pitfall 23)",
            Category::Trace => "trace parity (rubric 3.9, pitfall 16)",
            Category::Pairing => "pairing",
            Category::Atomic => "atomics and memory ordering",
        }
    }
}

/// A finding attached to a row.
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub severity: Severity,
    pub category: Category,
    pub message: String,
}

impl Note {
    pub fn new(severity: Severity, category: Category, message: impl Into<String>) -> Note {
        Note {
            severity,
            category,
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub severity: Severity,
    pub category: Category,
    pub cpp_line: Option<usize>,
    pub rust_line: Option<usize>,
    /// The file of the C++ line when it isn't the function's own, such as a
    /// doc comment in a header.
    pub cpp_file: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub cpp: Option<usize>,
    pub rust: Option<usize>,
    pub marker: Marker,
    pub notes: Vec<Note>,
}

/// Builds the rows and findings for an aligned pair of functions.
pub fn check(cpp: &Function, rust: &Function, pairs: &[Pair]) -> (Vec<Row>, Vec<Finding>) {
    check_with(cpp, rust, pairs, &Context::default())
}

/// What else [`check_with`] may consult.
#[derive(Default)]
pub struct Context<'a> {
    /// Rust units accounted for elsewhere (by a C++ override folded into
    /// the same function), not reported as only in Rust.
    pub claimed: &'a [usize],
    /// Functions the Rust calls that code may have moved into: `cpp_*`
    /// helpers the change added, and other Rust functions. C++ found there
    /// was moved, not dropped.
    pub elsewhere: &'a [&'a Function],
}

/// Like [`check`], with more to go on.
pub fn check_with(
    cpp: &Function,
    rust: &Function,
    pairs: &[Pair],
    ctx: &Context,
) -> (Vec<Row>, Vec<Finding>) {
    let claimed = ctx.claimed;
    // Control flow of a kind that both sides have as much of was most
    // likely rewritten (a `switch` as `matches!`, a clamp as `min`), not
    // dropped or added.
    let flow_balanced = |k: UnitKind| {
        let g = |u: &Unit| flow_group(u.kind);
        let (a, b) = (
            cpp.units.iter().filter(|u| g(u) == flow_group(k)).count(),
            rust.units.iter().filter(|u| g(u) == flow_group(k)).count(),
        );
        a == b
    };
    let mut rows = Vec::new();
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
        let mut notes: Vec<Note> = Vec::new();
        let marker = match (p.cpp, p.rust) {
            (Some(i), Some(j)) => {
                compare(&cpp.units[i], &rust.units[j], &mut notes);
                condition_diff(i, j, cpp, rust, &mut notes);
                ordering_diff(
                    &unit_text(cpp, &cpp.units[i]),
                    &unit_text(rust, &rust.units[j]),
                    cpp.units[i].features.asserts,
                    &mut notes,
                );
                if notes.iter().any(|n| n.severity == Severity::Issue) {
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
                if u.kind == UnitKind::Comment && restates_signature(u, cpp) {
                    notes.push(Note::new(
                        Severity::Note,
                        Category::Comment,
                        "comment only in C++; it restates the function's name",
                    ));
                } else if u.kind == UnitKind::Comment && u.features.still_in_cpp {
                    notes.push(Note::new(
                        Severity::Note,
                        Category::Comment,
                        "comment only in C++; it is still in the C++ after the change",
                    ));
                } else if u.kind == UnitKind::Comment && is_banner(u) {
                    notes.push(Note::new(
                        Severity::Note,
                        Category::Comment,
                        "comment only in C++; it labels a section",
                    ));
                } else if u.kind == UnitKind::Comment && words_kept(u, rust) {
                    notes.push(Note::new(
                        Severity::Note,
                        Category::Comment,
                        "comment only in C++, but the Rust comments carry its words (merged or reworded)",
                    ));
                } else if !u.features.plumbing && u.kind != UnitKind::Signature {
                    let n = soften_if_called(only_in(u, "C++", "Rust", moved), u, rust);
                    let n = soften_balanced(n, u, flow_balanced(u.kind));
                    notes.push(soften_moved(n, u, ctx.elsewhere));
                }
                Marker::CppOnly
            }
            (None, Some(j)) => {
                let u = &rust.units[j];
                let moved =
                    find_moved(u, &cpp.units, &unmatched_c).map(|i| cpp.units[i].start_line);
                // Safety comments and plumbing are expected additions, not
                // findings.
                let expected = claimed.contains(&j)
                    || u.kind == UnitKind::Comment && u.features.safety
                    || u.features.lock_plumbing
                    || u.features.plumbing
                    || (new_doc(rust, j) && !has_doc(cpp));
                // `return Foo();` in C++ is `foo()?; Ok(())` in Rust.
                let ok_after_status = u.kind == UnitKind::Return
                    && u.features.ret == Some(Ret::Ok)
                    && rows.last().is_some_and(|r: &Row| {
                        r.cpp
                            .is_some_and(|i| cpp.units[i].features.ret == Some(Ret::Status))
                    });
                if !expected && !ok_after_status {
                    let n = soften_if_called(only_in(u, "Rust", "C++", moved), u, cpp);
                    notes.push(soften_balanced(n, u, flow_balanced(u.kind)));
                }
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
    let findings = findings_of(&rows, cpp, rust);
    (rows, findings)
}

/// Whether the unit at `j` is part of the function's leading doc comment.
fn new_doc(f: &Function, j: usize) -> bool {
    f.units[j].kind == UnitKind::Comment
        && f.units[j + 1..]
            .iter()
            .find(|u| u.kind != UnitKind::Comment)
            .is_some_and(|u| u.kind == UnitKind::Signature)
}

/// Whether a function has a leading comment, on its definition or on its
/// declaration.
fn has_doc(f: &Function) -> bool {
    f.units
        .iter()
        .take_while(|u| u.kind != UnitKind::Signature)
        .any(|u| u.kind == UnitKind::Comment)
}

/// The findings of a list of rows, in row order.
pub fn findings_of(rows: &[Row], cpp: &Function, rust: &Function) -> Vec<Finding> {
    let mut findings = Vec::new();
    for r in rows {
        for n in &r.notes {
            findings.push(Finding {
                severity: n.severity,
                category: n.category,
                cpp_line: r.cpp.map(|i| cpp.units[i].start_line),
                rust_line: r.rust.map(|j| rust.units[j].start_line),
                cpp_file: r.cpp.and_then(|i| cpp.units[i].file.clone()),
                message: n.message.clone(),
            });
        }
    }
    findings
}

/// Collapses runs of three or more consecutive one-sided rows into a single
/// note on the first row, so a block of code with no counterpart reads as
/// one finding rather than one per statement. Comments are left out of the
/// runs: each lost comment is its own finding, because it is what a
/// reviewer has to carry over.
fn group_runs(rows: &mut [Row], cpp: &Function, rust: &Function) {
    let unit = |r: &Row, m: Marker| -> Option<&Unit> {
        if m == Marker::CppOnly {
            r.cpp.map(|k| &cpp.units[k])
        } else {
            r.rust.map(|k| &rust.units[k])
        }
    };
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
        // Rows with no note (expected additions such as safety comments)
        // stay in the run but don't count toward it, and neither do
        // comments, traces, asserts and moved steps, which are reported
        // alone.
        let noted: Vec<usize> = (i..j)
            .filter(|&k| {
                !rows[k].notes.is_empty()
                    && rows[k].notes.iter().all(|n| {
                        !matches!(
                            n.category,
                            Category::Comment
                                | Category::Trace
                                | Category::Assert
                                | Category::Order
                        )
                    })
            })
            .collect();
        if noted.len() >= 3 {
            let (side, other) = if m == Marker::CppOnly {
                ("C++", "Rust")
            } else {
                ("Rust", "C++")
            };
            let units: Vec<&Unit> = noted.iter().filter_map(|&k| unit(&rows[k], m)).collect();
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
            let worst = noted
                .iter()
                .flat_map(|&k| rows[k].notes.iter())
                .min_by_key(|n| (n.severity, n.category))
                .map(|n| (n.severity, n.category))
                .unwrap_or((Severity::Note, Category::ControlFlow));
            let first = units.first().map_or(0, |u| u.start_line);
            let last = units.last().map_or(0, |u| u.end_line);
            let what: Vec<String> = counts.iter().map(|(n, c)| format!("{c} {n}")).collect();
            let span = if first == last {
                format!("line {first}")
            } else {
                format!("lines {first}-{last}")
            };
            // Keep what each statement did: the calls and errors are what a
            // reviewer checks for.
            let mut did: Vec<String> = Vec::new();
            for u in &units {
                for c in u.features.calls.iter().chain(u.features.errors.iter()) {
                    if !did.contains(c) {
                        did.push(c.clone());
                    }
                }
            }
            let mut msg = format!(
                "{span} only in {side}, with no {other} counterpart: {}",
                what.join(", ")
            );
            if !did.is_empty() {
                did.truncate(8);
                msg.push_str(&format!(" ({})", did.join(", ")));
            }
            for &k in &noted {
                rows[k].notes.clear();
            }
            rows[noted[0]].notes.push(Note::new(worst.0, worst.1, msg));
        }
        i = j;
    }
}

pub(crate) fn find_moved(u: &Unit, others: &[Unit], candidates: &[usize]) -> Option<usize> {
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

/// A statement that only traces or prints.
fn trace_only(f: &crate::model::Features) -> bool {
    !f.calls.is_empty()
        && f.calls
            .iter()
            .all(|c| matches!(c.as_str(), "trace" | "print"))
        && f.locks.is_empty()
        && f.errors.is_empty()
        && !f.propagates
        && !f.unlocks
}

/// A statement that only asserts.
fn assert_only(f: &crate::model::Features) -> bool {
    f.calls.iter().any(|c| c == "assert")
        && f.locks.is_empty()
        && f.errors.is_empty()
        && !f.propagates
        && !f.unlocks
}

/// A comment that only names the function, such as `// zx_status_t
/// zx_port_wait` above `sys_port_wait`, carries nothing a reader would miss.
fn restates_signature(u: &Unit, f: &Function) -> bool {
    // Comment words are identifiers with underscores removed.
    const TYPE_WORDS: &[&str] = &["zxstatust", "void", "int", "bool", "static", "const"];
    let strip = |w: &str| -> String {
        let w = w.strip_prefix("zx").unwrap_or(w);
        w.strip_prefix("sys").unwrap_or(w).to_string()
    };
    let name = strip(&crate::normalize::ident(&f.base).replace('_', ""));
    let words = &u.features.comment;
    if words.is_empty() || words.len() > 4 || name.is_empty() {
        return false;
    }
    let mut named = false;
    for w in words {
        // `// zx_status_t zx_thread_start` also heads `sys_thread_start_regs`.
        if strip(w) == name
            || (w.starts_with("zx") && w.len() > 4 && !TYPE_WORDS.contains(&w.as_str()))
        {
            named = true;
        } else if !TYPE_WORDS.contains(&w.as_str()) {
            return false;
        }
    }
    named
}

/// Kinds of control flow counted together when judging balance.
fn flow_group(k: UnitKind) -> u8 {
    match k {
        UnitKind::If | UnitKind::ElseIf => 1,
        UnitKind::Loop => 2,
        UnitKind::Switch => 3,
        UnitKind::Case => 4,
        _ => 0,
    }
}

/// A one-sided `if`, loop, `switch` or `case` when both functions have as
/// many of them: a rewrite, so a note.
fn soften_balanced(n: Note, u: &Unit, balanced: bool) -> Note {
    if n.severity != Severity::Issue || flow_group(u.kind) == 0 || !balanced {
        return n;
    }
    Note::new(
        Severity::Note,
        n.category,
        format!(
            "{}; both sides have as many, so it was probably rewritten",
            n.message
        ),
    )
}

/// A C++ step that now lives in a function the Rust calls (a `cpp_*`
/// helper, or another Rust function) was moved, not dropped.
fn soften_moved(n: Note, u: &Unit, elsewhere: &[&Function]) -> Note {
    if n.severity != Severity::Issue || u.kind == UnitKind::Comment {
        return n;
    }
    // A bare `else` or a generic status check matches too much to prove
    // anything moved; a test must name something specific.
    const GENERIC: &[&str] = &["status", "result", "ok", "err", "res", "rc"];
    let specific: Vec<&String> = u
        .features
        .names
        .iter()
        .filter(|n| !GENERIC.contains(&n.as_str()))
        .collect();
    let test = matches!(u.kind, UnitKind::If | UnitKind::ElseIf | UnitKind::Else);
    if test && specific.is_empty() {
        return n;
    }
    let found = elsewhere.iter().find(|f| {
        f.units.iter().any(|v| {
            v.kind != UnitKind::Signature
                && similarity(u, v) >= 0.6
                && (!test || specific.iter().all(|s| v.features.names.contains(s)))
                && sorted(&u.features.errors) == sorted(&v.features.errors)
        })
    });
    match found {
        Some(f) => Note::new(
            Severity::Note,
            n.category,
            format!(
                "{}; it appears in {}, which the Rust calls",
                n.message, f.name
            ),
        ),
        None => n,
    }
}

/// A statement that only makes calls the other function also makes
/// somewhere was most likely split, merged or moved, not dropped: a note.
fn soften_if_called(n: Note, u: &Unit, other: &Function) -> Note {
    if n.severity != Severity::Issue
        || n.category != Category::Call
        || u.kind != UnitKind::Stmt
        || u.features.calls.is_empty()
        || !u.features.calls.iter().all(|c| other.calls.contains(c))
    {
        return n;
    }
    Note::new(
        Severity::Note,
        Category::Call,
        format!(
            "{}; the other side makes the same calls elsewhere",
            n.message
        ),
    )
}

/// A section label such as `// Socket methods.` or `/* external api */`.
fn is_banner(u: &Unit) -> bool {
    const LAST: &[&str] = &["methods", "implementation", "api", "functions", "helpers"];
    let w = &u.features.comment;
    !w.is_empty() && w.len() <= 4 && w.last().is_some_and(|l| LAST.contains(&l.as_str()))
}

/// Whether most of a C++ comment's words appear in the Rust function's
/// comments, as when several comments were merged into one Rust block.
fn words_kept(u: &Unit, rust: &Function) -> bool {
    let words = &u.features.comment;
    if words.len() < 4 {
        return false;
    }
    let rust_words: std::collections::HashSet<&str> = rust
        .units
        .iter()
        .filter(|r| r.kind == UnitKind::Comment)
        .flat_map(|r| r.features.comment.iter().map(String::as_str))
        .collect();
    let kept = words
        .iter()
        .filter(|w| rust_words.contains(w.as_str()))
        .count();
    kept as f64 >= 0.8 * words.len() as f64
}

fn only_in(u: &Unit, here: &str, there: &str, moved: Option<usize>) -> Note {
    let f = &u.features;
    let what = match u.kind {
        UnitKind::Comment => "comment".to_string(),
        UnitKind::Stmt if trace_only(f) => "trace/print statement".to_string(),
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
    let from_cpp = here == "C++";
    let (significant, category) = match u.kind {
        // A comment lost in translation matters; an added one is worth a look.
        UnitKind::Comment => (from_cpp, Category::Comment),
        // Returning a plain value (often a tail expression) is how Rust ends
        // constructors and accessors; success and error returns still count.
        UnitKind::Return => (
            !matches!(f.ret, Some(Ret::Value) | None),
            if matches!(f.ret, Some(Ret::Error(_)) | Some(Ret::Status)) {
                Category::ErrorPath
            } else {
                Category::ControlFlow
            },
        ),
        // The rubric asks for every trace statement to be ported; one the
        // Rust adds is worth a look.
        UnitKind::Stmt if trace_only(f) => (from_cpp, Category::Trace),
        // A check the Rust adds is worth a look, but one it drops is an issue.
        UnitKind::Stmt if assert_only(f) => (from_cpp, Category::Assert),
        UnitKind::Stmt => {
            let cat = if !f.locks.is_empty() || f.unlocks {
                Category::Lock
            } else if !f.errors.is_empty() || f.propagates {
                Category::ErrorPath
            } else {
                Category::Call
            };
            (
                !f.calls.is_empty()
                    || !f.locks.is_empty()
                    || !f.errors.is_empty()
                    || f.propagates
                    || f.unlocks,
                cat,
            )
        }
        UnitKind::Signature => (false, Category::ControlFlow),
        // An `else`, `break`, `goto` or label on its own is a change of shape
        // (`goto done` became `break`, an `else` became an early return);
        // the flow counts show any real imbalance.
        UnitKind::Else
        | UnitKind::Break
        | UnitKind::Continue
        | UnitKind::Goto
        | UnitKind::Label => (false, Category::ControlFlow),
        // Rust's exhaustive `match` needs arms C++'s `switch` doesn't.
        UnitKind::Case => (from_cpp, Category::ControlFlow),
        _ => (true, Category::ControlFlow),
    };
    let sev = if significant {
        Severity::Issue
    } else {
        Severity::Note
    };
    let mut msg = format!("{what} only in {here}");
    let mut category = category;
    if let Some(line) = moved {
        msg.push_str(&format!(
            "; resembles {there} line {line}, so the order may differ"
        ));
        if category != Category::Comment {
            category = Category::Order;
        }
    }
    Note::new(sev, category, msg)
}

fn compare(a: &Unit, b: &Unit, notes: &mut Vec<Note>) {
    let fa = &a.features;
    let fb = &b.features;
    if crate::align::is_handled_vs_propagated(a, b) {
        let msg = if a.features.propagates {
            "C++ propagates this call's error, but Rust handles it in its own branch"
        } else {
            "C++ handles this call's error in its own branch, but Rust propagates it"
        };
        notes.push(Note::new(Severity::Issue, Category::ErrorPath, msg));
        return;
    }
    if a.kind != b.kind && !equivalent_kinds(a.kind, b.kind) {
        notes.push(Note::new(
            Severity::Note,
            Category::ControlFlow,
            format!("C++ {} vs Rust {}", a.kind.name(), b.kind.name()),
        ));
    }
    if a.kind == UnitKind::Comment {
        if fa.comment != fb.comment {
            // Rewording is fine; losing a TODO or a negation is not.
            const WEIGHTY: &[&str] = &[
                "todo", "not", "never", "no", "must", "cannot", "can't", "don't", "doesn't",
                "only", "always",
            ];
            let lost: Vec<&str> = WEIGHTY
                .iter()
                .copied()
                .filter(|w| fa.comment.iter().any(|x| x == w) != fb.comment.iter().any(|x| x == w))
                .collect();
            // A comment still in the C++ is not lost, however Rust words it.
            let (sev, msg) = if lost.is_empty() || fa.still_in_cpp {
                (Severity::Note, comment_diff(&fa.comment, &fb.comment))
            } else {
                (
                    Severity::Issue,
                    format!(
                        "{}; only one side says {}",
                        comment_diff(&fa.comment, &fb.comment),
                        lost.iter()
                            .map(|w| format!("\"{w}\""))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
            };
            notes.push(Note::new(sev, Category::Comment, msg));
        }
        return;
    }
    // A trace aligned with a statement that doesn't trace was dropped.
    if trace_only(fa) && !trace_only(fb) && !fb.calls.iter().any(|c| c == "trace" || c == "print") {
        notes.push(Note::new(
            Severity::Issue,
            Category::Trace,
            "trace/print statement only in C++",
        ));
    }
    match (&fa.ret, &fb.ret) {
        (Some(Ret::Error(x)), Some(Ret::Error(y))) if x != y => {
            notes.push(Note::new(
                Severity::Issue,
                Category::ErrorPath,
                format!("error code differs: C++ returns {x}, Rust returns {y}"),
            ));
        }
        // Passing a callee's status on reads as a value in Rust, whose tail
        // expression returns the callee's `Result`.
        (Some(Ret::Status), Some(Ret::Value)) | (Some(Ret::Value), Some(Ret::Status))
            if !fa.calls.is_empty() && !fb.calls.is_empty() => {}
        (Some(ra), Some(rb)) if ra != rb => {
            // Success versus failure is a behavior change; the other
            // mismatches (a status variable, a value, `Ok`) are usually
            // just the two languages' ways of saying the same thing.
            let error = |r: &Ret| matches!(r, Ret::Error(_));
            let success = |r: &Ret| matches!(r, Ret::Ok | Ret::Value);
            // C++ mapping a failure to a specific code where Rust passes
            // the callee's status on changes what the caller sees.
            let remapped = error(ra) && *rb == Ret::Status;
            let sev = if (error(ra) && success(rb)) || (success(ra) && error(rb)) || remapped {
                Severity::Issue
            } else {
                Severity::Note
            };
            notes.push(Note::new(
                sev,
                Category::ErrorPath,
                format!("C++ returns {ra}, Rust returns {rb}"),
            ));
        }
        _ => {}
    }
    if fa.ret.is_none() || fb.ret.is_none() {
        let (ea, eb) = (sorted(&fa.errors), sorted(&fb.errors));
        if ea != eb {
            notes.push(Note::new(
                Severity::Issue,
                Category::ErrorPath,
                format!(
                    "error codes differ: C++ [{}], Rust [{}]",
                    ea.join(", "),
                    eb.join(", ")
                ),
            ));
        }
    }
    if fa.locks != fb.locks {
        // One lock on each side under different names is usually the same
        // lock spelled two ways; a lock taken on one side only is not.
        let renamed = fa.locks.len() == fb.locks.len();
        notes.push(Note::new(
            if renamed {
                Severity::Note
            } else {
                Severity::Issue
            },
            Category::Lock,
            format!(
                "locks differ: C++ acquires [{}], Rust acquires [{}]",
                fa.locks.join(", "),
                fb.locks.join(", ")
            ),
        ));
    }
    if fa.propagates != fb.propagates && !propagation_is_implied(a, b) {
        let (who, other) = if fa.propagates {
            ("C++", "Rust")
        } else {
            ("Rust", "C++")
        };
        notes.push(Note::new(
            Severity::Issue,
            Category::ErrorPath,
            format!("{who} propagates an error here but {other} does not"),
        ));
    }
    if fa.unlocks != fb.unlocks {
        let who = if fa.unlocks { "C++" } else { "Rust" };
        notes.push(Note::new(
            Severity::Note,
            Category::Lock,
            format!("only {who} releases a lock here"),
        ));
    }
    if fa.asserts != fb.asserts {
        let who = if fa.asserts { "C++" } else { "Rust" };
        notes.push(Note::new(
            if fa.asserts {
                Severity::Issue
            } else {
                Severity::Note
            },
            Category::Assert,
            format!("only {who} asserts here"),
        ));
    }
    let mut only_a: Vec<&String> = uniq(&fa.calls)
        .into_iter()
        .filter(|c| !fb.calls.contains(c) && !name_matches(c, fb))
        .collect();
    let mut only_b: Vec<&String> = uniq(&fb.calls)
        .into_iter()
        .filter(|c| !fa.calls.contains(c) && !name_matches(c, fa))
        .collect();
    // Rust calling back into C++ through `cpp_<class>_<method>` makes the
    // call C++ made directly.
    let via_ffi = |x: &str, y: &str| y.starts_with("cpp_") && y.ends_with(&format!("_{x}"));
    while let Some((i, j)) = only_a
        .iter()
        .enumerate()
        .find_map(|(i, x)| only_b.iter().position(|y| via_ffi(x, y)).map(|j| (i, j)))
    {
        only_a.remove(i);
        only_b.remove(j);
    }
    for (x, y) in crate::normalize::EQUIVALENT_CALLS {
        let (i, j) = (
            only_a.iter().position(|c| c.as_str() == *x),
            only_b.iter().position(|c| c.as_str() == *y),
        );
        if let (Some(i), Some(j)) = (i, j) {
            only_a.remove(i);
            only_b.remove(j);
        }
    }
    if !only_a.is_empty() || !only_b.is_empty() {
        let mut parts = Vec::new();
        if !only_a.is_empty() {
            parts.push(format!("only C++ calls {}", join(&only_a)));
        }
        if !only_b.is_empty() {
            parts.push(format!("only Rust calls {}", join(&only_b)));
        }
        notes.push(Note::new(Severity::Note, Category::Call, parts.join("; ")));
    }
}

/// Kinds that are the same step spelled differently: a C++ `if` that
/// returns early and Rust's `let ... else`, or `else { if }` and `else if`.
fn equivalent_kinds(a: UnitKind, b: UnitKind) -> bool {
    matches!(
        (a, b),
        (UnitKind::If, UnitKind::ElseIf) | (UnitKind::ElseIf, UnitKind::If)
    )
}

/// Whether a call on one side is a field or accessor on the other: C++
/// `allocation()` and Rust `self.allocation`, or `set_key(k)` and `key_ = k`.
fn name_matches(call: &str, other: &crate::model::Features) -> bool {
    let bare = call
        .strip_prefix("set_")
        .or_else(|| call.strip_prefix("get_"))
        .unwrap_or(call)
        .replace('_', "");
    other.names.contains(&bare) || other.idents.contains(&bare)
}

/// One side propagates with `?` where the other returns the status it
/// checked in the same unit, which is the same thing.
fn propagation_is_implied(a: &Unit, b: &Unit) -> bool {
    let returns_status = |u: &Unit| {
        matches!(u.features.ret, Some(Ret::Status) | Some(Ret::Value))
            && !u.features.calls.is_empty()
    };
    (a.features.propagates && returns_status(b)) || (b.features.propagates && returns_status(a))
}

/// Every condition operand a function tests.
/// Conjuncts of the conditions near unit `k`: a C++ `if (a && b)` split
/// into nested Rust `if`s (or the reverse) keeps its tests close by.
fn nearby_conjuncts(f: &Function, k: usize) -> Vec<&crate::model::Conjunct> {
    let lo = k.saturating_sub(3);
    let hi = (k + 4).min(f.units.len());
    f.units[lo..hi]
        .iter()
        .flat_map(|u| u.features.conjuncts.iter())
        .collect()
}

/// Whether two normalized names are the same, or one contains the other
/// (`modewrite` and `write`).
fn name_like(x: &str, y: &str) -> bool {
    x == y || (x.len() >= 3 && y.len() >= 3 && (x.contains(y) || y.contains(x)))
}

/// Reports tests one side's `if` makes and the other's doesn't, operand by
/// operand. A test the other function makes in a condition close by (a
/// C++ `if (a && b)` split into nested Rust `if`s) doesn't count. When both
/// conditions have as many tests but they read differently, the operands
/// were probably renamed or hoisted into locals: that is a note. A test
/// added or dropped outright is an issue.
fn condition_diff(i: usize, j: usize, cpp: &Function, rust: &Function, notes: &mut Vec<Note>) {
    let (a, b) = (&cpp.units[i], &rust.units[j]);
    let conds = |u: &Unit| matches!(u.kind, UnitKind::If | UnitKind::ElseIf);
    let (fa, fb) = (&a.features, &b.features);
    if !conds(a) || !conds(b) || fa.checks_error || fb.checks_error {
        return;
    }
    if fa.conjuncts.is_empty() || fb.conjuncts.is_empty() {
        return;
    }
    // Status tests (`status != ZX_OK`) have no names left once noise is
    // dropped; they are compared as error checks instead.
    if fa
        .conjuncts
        .iter()
        .chain(&fb.conjuncts)
        .any(|c| c.names.is_empty())
    {
        return;
    }
    // Tests of the same names with different comparisons (`x == 0` and
    // `x > MAX`) are different tests.
    let same = |x: &crate::model::Conjunct, y: &crate::model::Conjunct| {
        let covered =
            |p: &[String], q: &[String]| p.iter().all(|w| q.iter().any(|v| name_like(w, v)));
        let names = crate::normalize::jaccard(&x.names, &y.names) >= 0.5
            || covered(&x.names, &y.names)
            || covered(&y.names, &x.names);
        let (ox, oy) = (comparison(&x.text), comparison(&y.text));
        names && (ox.is_none() || oy.is_none() || ox == oy)
    };
    let (near_a, near_b) = (nearby_conjuncts(cpp, i), nearby_conjuncts(rust, j));
    let missing =
        |xs: &[crate::model::Conjunct], near: &[&crate::model::Conjunct]| -> Vec<String> {
            xs.iter()
                .filter(|x| !near.iter().any(|y| same(x, y)))
                .map(|x| x.text.clone())
                .collect()
        };
    let only_b = missing(&fb.conjuncts, &near_a);
    let only_a = missing(&fa.conjuncts, &near_b);
    if only_a.is_empty() && only_b.is_empty() {
        return;
    }
    let (na, nb) = (fa.conjuncts.len(), fb.conjuncts.len());
    if na == nb && !only_a.is_empty() && !only_b.is_empty() {
        notes.push(Note::new(
            Severity::Note,
            Category::ControlFlow,
            format!(
                "condition reads differently: C++ tests {}, Rust tests {}",
                only_a.join("; "),
                only_b.join("; ")
            ),
        ));
        return;
    }
    if !only_b.is_empty() {
        notes.push(Note::new(
            if nb > na {
                Severity::Issue
            } else {
                Severity::Note
            },
            Category::ControlFlow,
            format!(
                "Rust's condition adds a test that C++ doesn't make: {}",
                only_b.join("; ")
            ),
        ));
    }
    if !only_a.is_empty() {
        notes.push(Note::new(
            if na > nb {
                Severity::Issue
            } else {
                Severity::Note
            },
            Category::ControlFlow,
            format!(
                "C++'s condition tests {}, which the Rust doesn't",
                only_a.join("; ")
            ),
        ));
    }
}

/// The kind of comparison a condition makes, with `<` and `>` (and `<=`
/// and `>=`) alike, since either can be written with its operands swapped.
fn comparison(text: &str) -> Option<&'static str> {
    static OP: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\s(==|!=|<=|>=|<|>)\s").unwrap());
    let op = OP.captures(text)?.get(1)?.as_str();
    Some(match op {
        "==" => "eq",
        "!=" => "ne",
        "<" | ">" => "lt",
        _ => "le",
    })
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
            .filter(|u| !u.features.plumbing)
            .filter_map(|u| match (&u.features.ret, u.features.propagates) {
                (Some(Ret::Error(e)), _) => Some(e.clone()),
                // `ok_or(X)?` and `if (!x) return ZX_ERR_X;` return X.
                (_, true) if !u.features.errors.is_empty() => {
                    Some(format!("{}?", u.features.errors.join("|")))
                }
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

/// The source text of a unit.
fn unit_text(f: &Function, u: &Unit) -> String {
    if u.file.is_some() {
        return u.ext_lines.join("\n");
    }
    (u.start_line..=u.end_line)
        .map(|l| f.line(l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Rank of a memory ordering, weakest first. Acquire and release are
/// ranked together: each is weaker than acq_rel, neither than the other.
fn ordering_rank(o: &str) -> u8 {
    match o.to_ascii_lowercase().as_str() {
        "relaxed" => 0,
        "consume" | "acquire" | "release" => 1,
        "acq_rel" | "acqrel" => 2,
        _ => 3,
    }
}

/// The memory orderings of the atomic operations in C++ or Rust source. A
/// C++ operation with no explicit order is sequentially consistent.
fn orderings(text: &str, lang: Lang) -> Vec<String> {
    static CPP_OP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?:\.|->)(?:load|store|exchange|fetch_\w+|compare_exchange_\w+)\s*\(")
            .unwrap()
    });
    static CPP_ORDER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"memory_order(?:_|::)(relaxed|consume|acquire|release|acq_rel|seq_cst)")
            .unwrap()
    });
    static RUST_ORDER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\bOrdering::(Relaxed|Acquire|Release|AcqRel|SeqCst)\b").unwrap()
    });
    let re = match lang {
        Lang::Rust => &RUST_ORDER,
        Lang::Cpp => &CPP_ORDER,
    };
    let mut out: Vec<String> = re
        .captures_iter(text)
        .map(|c| c[1].to_ascii_lowercase())
        .collect();
    if lang == Lang::Cpp && out.is_empty() && CPP_OP.is_match(text) {
        out.push("seq_cst".to_string());
    }
    out
}

/// Rust atomics with a different memory ordering from the C++ they replace:
/// an issue when weaker, a note when stronger.
/// An assertion's ordering barely matters, so there it is only a note.
fn ordering_diff(cpp: &str, rust: &str, asserts: bool, notes: &mut Vec<Note>) {
    let (a, b) = (orderings(cpp, Lang::Cpp), orderings(rust, Lang::Rust));
    if a.is_empty() || b.is_empty() {
        return;
    }
    let weakest = |v: &[String]| v.iter().map(|o| ordering_rank(o)).min().unwrap_or(3);
    let show = |v: &[String]| {
        let mut v = v.to_vec();
        v.dedup();
        v.join(", ")
    };
    let (ra, rb) = (weakest(&a), weakest(&b));
    let (severity, word) = match rb.cmp(&ra) {
        std::cmp::Ordering::Less if !asserts => (Severity::Issue, "weaker"),
        std::cmp::Ordering::Less => (Severity::Note, "weaker"),
        std::cmp::Ordering::Greater => (Severity::Note, "stronger"),
        std::cmp::Ordering::Equal => return,
    };
    notes.push(Note::new(
        severity,
        Category::Atomic,
        format!(
            "Rust uses a {word} memory ordering ({}) than C++ ({})",
            show(&b),
            show(&a)
        ),
    ));
}

#[cfg(test)]
mod ordering_tests {
    use super::*;

    fn diff(cpp: &str, rust: &str) -> Vec<Note> {
        let mut notes = Vec::new();
        ordering_diff(cpp, rust, false, &mut notes);
        notes
    }

    #[test]
    fn default_cpp_ordering_is_seq_cst() {
        let n = diff(
            "state_.exchange(0);",
            "self.state.swap(0, Ordering::Relaxed);",
        );
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].severity, Severity::Issue);
        assert!(n[0].message.contains("weaker"), "{}", n[0].message);
    }

    #[test]
    fn same_or_stronger_ordering() {
        assert!(diff(
            "x.load(ktl::memory_order_acquire);",
            "x.load(Ordering::Acquire)"
        )
        .is_empty());
        let n = diff(
            "x.store(1, std::memory_order_relaxed);",
            "x.store(1, Ordering::SeqCst)",
        );
        assert_eq!(n[0].severity, Severity::Note);
    }

    #[test]
    fn no_atomics_no_finding() {
        assert!(diff("count_ = 0;", "self.count = 0;").is_empty());
    }
}
