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
    check_with(cpp, rust, pairs, &[])
}

/// Like [`check`], where the Rust units in `claimed` are accounted for
/// elsewhere (by a C++ override folded into the same function) and are not
/// reported as only in Rust.
pub fn check_with(
    cpp: &Function,
    rust: &Function,
    pairs: &[Pair],
    claimed: &[usize],
) -> (Vec<Row>, Vec<Finding>) {
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
                condition_diff(&cpp.units[i], &rust.units[j], cpp, rust, &mut notes);
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
                } else if !u.features.plumbing && u.kind != UnitKind::Signature {
                    notes.push(only_in(u, "C++", "Rust", moved));
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
                if !expected {
                    notes.push(only_in(u, "Rust", "C++", moved));
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
        // comments or trace-only statements, which are reported alone.
        let noted: Vec<usize> = (i..j)
            .filter(|&k| {
                !rows[k].notes.is_empty()
                    && rows[k]
                        .notes
                        .iter()
                        .all(|n| !matches!(n.category, Category::Comment | Category::Trace))
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
        && f
            .calls
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
        if strip(w) == name {
            named = true;
        } else if !TYPE_WORDS.contains(&w.as_str()) {
            return false;
        }
    }
    named
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
            notes.push(Note::new(
                Severity::Note,
                Category::Comment,
                comment_diff(&fa.comment, &fb.comment),
            ));
        }
        return;
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
            let sev = if (error(ra) && success(rb)) || (success(ra) && error(rb)) {
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
        notes.push(Note::new(
            Severity::Issue,
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
    other.names.iter().any(|n| *n == bare) || other.idents.iter().any(|n| *n == bare)
}

/// One side propagates with `?` where the other returns the status it
/// checked in the same unit, which is the same thing.
fn propagation_is_implied(a: &Unit, b: &Unit) -> bool {
    let returns_status = |u: &Unit| {
        matches!(u.features.ret, Some(Ret::Status) | Some(Ret::Value)) && !u.features.calls.is_empty()
    };
    (a.features.propagates && returns_status(b)) || (b.features.propagates && returns_status(a))
}

/// Every condition operand a function tests.
fn all_conjuncts(f: &Function) -> Vec<&crate::model::Conjunct> {
    f.units
        .iter()
        .flat_map(|u| u.features.conjuncts.iter())
        .collect()
}

/// Reports tests one side's `if` makes and the other's doesn't, operand by
/// operand. A test the other function makes in some other condition (a
/// C++ `if (a && b)` split into nested Rust `if`s) doesn't count.
fn condition_diff(a: &Unit, b: &Unit, cpp: &Function, rust: &Function, notes: &mut Vec<Note>) {
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
    if fa.conjuncts.iter().chain(&fb.conjuncts).any(|c| c.names.is_empty()) {
        return;
    }
    let same = |x: &crate::model::Conjunct, y: &crate::model::Conjunct| {
        crate::normalize::jaccard(&x.names, &y.names) >= 0.5
            || x.names.iter().all(|w| y.names.contains(w))
            || y.names.iter().all(|w| x.names.contains(w))
    };
    let (all_a, all_b) = (all_conjuncts(cpp), all_conjuncts(rust));
    let missing = |xs: &[crate::model::Conjunct], ys: &[crate::model::Conjunct], all: &[&crate::model::Conjunct]| -> Vec<String> {
        xs.iter()
            .filter(|x| !ys.iter().any(|y| same(x, y)) && !all.iter().any(|y| same(x, y)))
            .map(|x| x.text.clone())
            .collect()
    };
    let only_b = missing(&fb.conjuncts, &fa.conjuncts, &all_a);
    let only_a = missing(&fa.conjuncts, &fb.conjuncts, &all_b);
    if !only_b.is_empty() {
        notes.push(Note::new(
            Severity::Issue,
            Category::ControlFlow,
            format!(
                "Rust's condition adds a test that C++ doesn't make: {}",
                only_b.join("; ")
            ),
        ));
    }
    if !only_a.is_empty() {
        notes.push(Note::new(
            Severity::Issue,
            Category::ControlFlow,
            format!(
                "C++'s condition tests {}, which the Rust doesn't",
                only_a.join("; ")
            ),
        ));
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
