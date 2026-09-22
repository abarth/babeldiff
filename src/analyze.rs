//! Pairs removed C++ functions with added Rust functions and checks each
//! pair.

use crate::align::{self, Pair};
use crate::check::{self, Finding, Row, Severity, Summary};
use crate::model::{Function, UnitKind};
use crate::normalize::{self, seq_similarity};
use std::collections::HashMap;

/// How a pair was found.
#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    /// The new C++ body calls an FFI shim that forwards to this Rust function.
    Ffi { shim: String, shim_location: String },
    /// The FFI shim's name follows the `rust_<class>_<method>` convention.
    FfiName { shim: String, shim_location: String },
    /// Names and bodies are similar.
    Similarity,
    /// Requested explicitly.
    Forced,
}

/// Where the C++ side came from.
#[derive(Clone, Debug, PartialEq)]
pub enum CppOrigin {
    /// Removed or rewritten by the change.
    Changed,
    /// Not touched by the change; found in the repository.
    Unchanged,
}

#[derive(Clone, Debug)]
pub struct PairReport {
    pub cpp: Function,
    pub rust: Function,
    pub link: Link,
    /// A thin Rust function that forwards to `rust`, standing in for the C++
    /// by name.
    pub forwarder: Option<Function>,
    pub origin: CppOrigin,
    pub score: f64,
    pub rows: Vec<Row>,
    pub findings: Vec<Finding>,
    pub summary: Summary,
}

impl PairReport {
    pub fn issues(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Issue)
            .count()
    }
    pub fn notes(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Note)
            .count()
    }
}

/// An FFI shim and the Rust function it forwards to.
#[derive(Clone, Debug)]
pub struct Shim {
    pub shim: Function,
    pub target: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub pairs: Vec<PairReport>,
    pub unmatched_cpp: Vec<Function>,
    /// Changed or removed C++ helpers that Rust calls through FFI
    /// (`cpp_<class>_<method>` in a `*_ffi.cc` file). They have no Rust
    /// counterpart.
    pub removed_cpp_shims: Vec<Function>,
    /// Rust functions that only call into C++ through a `cpp_*` FFI helper,
    /// with the helper's name. They wrap C++ rather than convert it.
    pub rust_facades: Vec<(Function, String)>,
    pub unmatched_rust: Vec<Function>,
    pub shims: Vec<Shim>,
}

impl Report {
    pub fn issues(&self) -> usize {
        self.pairs.iter().map(PairReport::issues).sum()
    }
    pub fn notes(&self) -> usize {
        self.pairs.iter().map(PairReport::notes).sum()
    }
}

/// Inputs to [`analyze`].
#[derive(Default)]
pub struct Inputs {
    /// C++ functions as they were before the change.
    pub cpp: Vec<Function>,
    /// Rust functions added or rewritten by the change.
    pub rust: Vec<Function>,
    /// Every Rust function in the touched files, used to follow FFI shims.
    pub all_rust: Vec<Function>,
    /// Calls made by the post-change version of each C++ function, keyed by
    /// qualified name. This is how C++ bodies that became FFI calls are
    /// linked to the Rust that replaced them.
    pub cpp_new_calls: HashMap<String, Vec<String>>,
    /// Explicit `(C++ name, Rust name)` pairs.
    pub forced: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Minimum score for pairing by similarity.
    pub min_score: f64,
    /// Minimum score for pairing with C++ that the change did not touch.
    pub min_unchanged_score: f64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            min_score: 0.35,
            min_unchanged_score: 0.5,
        }
    }
}

/// Looks up C++ functions outside the change that might correspond to a
/// Rust function. Implemented by the git layer.
pub trait CppFinder {
    fn find(&mut self, rust: &Function) -> Vec<Function>;
}

/// A finder that finds nothing.
pub struct NoFinder;
impl CppFinder for NoFinder {
    fn find(&mut self, _: &Function) -> Vec<Function> {
        Vec::new()
    }
}

fn operator_alias(base: &str) -> &str {
    match base.split_whitespace().collect::<String>().as_str() {
        "operator==" => "eq",
        "operator!=" => "ne",
        "operator<" => "lt",
        "operator<=" => "le",
        "operator>" => "gt",
        "operator>=" => "ge",
        "operator[]" => "index",
        "operator()" => "call",
        "operator=" => "assign",
        "operatorbool" => "is_valid",
        _ => base,
    }
}

fn name_words(f: &Function) -> Vec<String> {
    if f.lang == crate::model::Lang::Cpp {
        let class = f
            .class
            .as_deref()
            .map(|c| c.rsplit("::").next().unwrap_or(c));
        if f.base.starts_with('~') {
            // Destructors become `Drop::drop`.
            return vec!["drop".to_string()];
        }
        if class == Some(f.base.as_str()) {
            // Constructors become `new` or `init`.
            return vec!["new".to_string()];
        }
    }
    normalize::words(operator_alias(&f.base))
}

/// Similarity of two function names, tolerant of CamelCase vs snake_case.
pub fn name_similarity(a: &Function, b: &Function) -> f64 {
    let base = seq_similarity(&name_words(a), &name_words(b));
    let last = |f: &Function| {
        f.class
            .as_deref()
            .map(|c| normalize::words(c.rsplit("::").next().unwrap_or(c)))
    };
    match (last(a), last(b)) {
        (Some(ca), Some(cb)) => 0.8 * base + 0.2 * seq_similarity(&ca, &cb),
        _ => base,
    }
}

/// Body similarity with comments weighted double, since comments are
/// supposed to carry over unchanged.
fn body_similarity(a: &Function, b: &Function, pairs: &[Pair]) -> f64 {
    let w = |u: &crate::model::Unit| match u.kind {
        UnitKind::Comment => 2.0,
        // Signatures always align; they say nothing about the bodies.
        UnitKind::Signature => 0.0,
        _ => 1.0,
    };
    let total: f64 = a.units.iter().map(w).sum::<f64>() + b.units.iter().map(w).sum::<f64>();
    if total == 0.0 {
        return 0.0;
    }
    let matched: f64 = pairs
        .iter()
        .filter_map(|p| Some((p.cpp?, p.rust?, p.score)))
        .map(|(i, j, s)| s * (w(&a.units[i]) + w(&b.units[j])))
        .sum();
    matched / total
}

/// Small bodies look alike by accident, so short functions need similar names.
fn plausible(a: &Function, b: &Function) -> bool {
    let body = |f: &Function| {
        f.units
            .iter()
            .filter(|u| !matches!(u.kind, UnitKind::Signature | UnitKind::Comment))
            .count()
    };
    body(a).min(body(b)) >= 3 || name_similarity(a, b) >= 0.5
}

fn score(a: &Function, b: &Function) -> (f64, Vec<Pair>) {
    let (pairs, _) = align::align(&a.units, &b.units);
    let s = 0.35 * name_similarity(a, b) + 0.65 * body_similarity(a, b, &pairs);
    (s, pairs)
}

/// The shim's name with the FFI decoration removed:
/// `rust_beacon_dispatcher_flash` -> `beacon_dispatcher_flash`.
fn undecorated(shim: &str) -> String {
    let n = normalize::ident(shim);
    let n = n.strip_prefix("rust_").unwrap_or(&n);
    let n = n.strip_suffix("_ffi").unwrap_or(n);
    n.to_string()
}

/// Finds the function an FFI shim forwards to.
fn shim_target<'a>(shim: &Function, rust: &'a [Function]) -> Option<&'a Function> {
    let und = undecorated(&shim.base);
    // How a call to `r` appears in the shim's call list: `Foo::init` is
    // recorded as a call of `foo`, like a constructor.
    let key = |r: &Function| normalize::call(&r.name).unwrap_or_else(|| normalize::ident(&r.base));
    rust.iter()
        .filter(|r| !r.is_ffi && r.name != shim.name)
        .filter(|r| shim.calls.contains(&key(r)))
        // Prefer the callee whose name is embedded in the shim's name.
        .max_by_key(|r| {
            let k = key(r);
            let b = normalize::ident(&r.base);
            (und.ends_with(&b) as usize) * 1000 + (und.contains(&k) as usize) * 500 + k.len()
        })
}

/// Runs the analysis.
pub fn analyze(inputs: Inputs, opts: &Options, finder: &mut dyn CppFinder) -> Report {
    let Inputs {
        cpp,
        rust,
        all_rust,
        cpp_new_calls,
        forced,
    } = inputs;
    let mut report = Report::default();

    // Resolve FFI shims among the Rust functions.
    let mut universe: Vec<Function> = all_rust;
    for r in &rust {
        if !universe
            .iter()
            .any(|u| u.path == r.path && u.start_line == r.start_line)
        {
            universe.push(r.clone());
        }
    }
    let shims: Vec<(Function, Option<Function>)> = universe
        .iter()
        .filter(|f| f.is_ffi)
        .map(|s| (s.clone(), shim_target(s, &universe).cloned()))
        .collect();

    let mut cpp_used = vec![false; cpp.len()];
    let mut rust_pool: Vec<Function> = rust;
    for (s, _) in &shims {
        if !rust_pool
            .iter()
            .any(|r| r.path == s.path && r.start_line == s.start_line)
        {
            rust_pool.push(s.clone());
        }
    }
    // Shim targets may live in files the change did not touch much; make
    // sure they are pairable.
    for (_, t) in &shims {
        if let Some(t) = t {
            if !rust_pool
                .iter()
                .any(|r| r.path == t.path && r.start_line == t.start_line)
            {
                rust_pool.push(t.clone());
            }
        }
    }
    let mut rust_used = vec![false; rust_pool.len()];
    let find_rust = |pool: &[Function], f: &Function| {
        pool.iter()
            .position(|r| r.path == f.path && r.start_line == f.start_line)
    };

    let mut chosen: Vec<(usize, usize, Link)> = Vec::new();
    let mut forwarders: Vec<(usize, usize)> = Vec::new();

    // 1. Explicit pairs.
    for (c, r) in &forced {
        let ci = cpp.iter().position(|f| &f.name == c || &f.base == c);
        let ri = rust_pool.iter().position(|f| &f.name == r || &f.base == r);
        if let (Some(ci), Some(ri)) = (ci, ri) {
            if !cpp_used[ci] && !rust_used[ri] {
                cpp_used[ci] = true;
                rust_used[ri] = true;
                chosen.push((ci, ri, Link::Forced));
            }
        }
    }

    // 2. FFI links: the new C++ body calls the shim, or the shim is named
    //    after the C++ function.
    for (ci, c) in cpp.iter().enumerate() {
        if cpp_used[ci] {
            continue;
        }
        let new_calls = cpp_new_calls.get(&c.name);
        let qualified = normalize::ident(&format!(
            "{}_{}",
            c.class
                .as_deref()
                .map(|k| k.rsplit("::").next().unwrap_or(k))
                .unwrap_or(""),
            operator_alias(&c.base)
        ));
        for (shim, target) in &shims {
            let shim_name = normalize::ident(&shim.base);
            let by_call = new_calls.is_some_and(|calls| calls.contains(&shim_name));
            let by_name = !by_call && undecorated(&shim.base) == qualified;
            if !(by_call || by_name) {
                continue;
            }
            let target = target.as_ref().unwrap_or(shim);
            let Some(ri) = find_rust(&rust_pool, target) else {
                continue;
            };
            if rust_used[ri] {
                continue;
            }
            let link = if by_call {
                Link::Ffi {
                    shim: shim.name.clone(),
                    shim_location: shim.location(),
                }
            } else {
                Link::FfiName {
                    shim: shim.name.clone(),
                    shim_location: shim.location(),
                }
            };
            cpp_used[ci] = true;
            rust_used[ri] = true;
            chosen.push((ci, ri, link));
            break;
        }
    }

    // 3. Similarity, greedily from the best score down.
    let mut cands: Vec<(f64, usize, usize)> = Vec::new();
    for (ci, c) in cpp.iter().enumerate() {
        if cpp_used[ci] {
            continue;
        }
        for (ri, r) in rust_pool.iter().enumerate() {
            if rust_used[ri] || r.is_ffi {
                continue;
            }
            let (s, _) = score(c, r);
            if s >= opts.min_score && plausible(c, r) {
                cands.push((s, ci, ri));
            }
        }
    }
    cands.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (_, ci, ri) in cands {
        if !cpp_used[ci] && !rust_used[ri] {
            cpp_used[ci] = true;
            rust_used[ri] = true;
            chosen.push((ci, ri, Link::Similarity));
        }
    }

    // A thin Rust method that just calls the method doing the work (say
    // `Widget::dump` calling `WidgetState::dump`) stands in
    // for the C++ only by name; compare against the method doing the work.
    for (ci, ri, _) in &mut chosen {
        if let Some(rj) = forwardee(&cpp[*ci], &rust_pool[*ri], &rust_pool, &rust_used) {
            rust_used[rj] = true;
            forwarders.push((rj, *ri));
            *ri = rj;
        }
    }

    for (ci, ri, link) in chosen {
        let mut pair = build_pair(
            cpp[ci].clone(),
            rust_pool[ri].clone(),
            link,
            CppOrigin::Changed,
        );
        pair.forwarder = forwarders
            .iter()
            .find(|(t, _)| *t == ri)
            .map(|(_, f)| rust_pool[*f].clone());
        report.pairs.push(pair);
    }

    // 4. Rust with no C++ in the change: look for C++ the change left alone.
    for (ri, r) in rust_pool.iter().enumerate() {
        if rust_used[ri] || r.is_ffi {
            continue;
        }
        let best = finder
            .find(r)
            .into_iter()
            .map(|c| (score(&c, r).0, c))
            .filter(|(s, _)| *s >= opts.min_unchanged_score)
            .max_by(|a, b| a.0.total_cmp(&b.0));
        if let Some((_, c)) = best {
            rust_used[ri] = true;
            report.pairs.push(build_pair(
                c,
                r.clone(),
                Link::Similarity,
                CppOrigin::Unchanged,
            ));
        }
    }

    report.pairs.sort_by(|a, b| {
        (a.cpp.path.as_str(), a.cpp.start_line).cmp(&(b.cpp.path.as_str(), b.cpp.start_line))
    });
    let (removed_shims, unmatched): (Vec<Function>, Vec<Function>) = cpp
        .into_iter()
        .zip(cpp_used)
        .filter(|(_, u)| !u)
        .map(|(f, _)| f)
        .partition(is_cpp_shim);
    report.unmatched_cpp = unmatched;
    report.removed_cpp_shims = removed_shims;
    report.unmatched_rust = rust_pool
        .into_iter()
        .zip(rust_used)
        .filter(|(f, u)| !u && !f.is_ffi)
        .map(|(f, _)| f)
        .collect();
    let (facades, unmatched): (Vec<Function>, Vec<Function>) =
        std::mem::take(&mut report.unmatched_rust)
            .into_iter()
            .partition(|f| body_len(f) <= 3 && f.calls.iter().any(|c| c.starts_with("cpp_")));
    report.unmatched_rust = unmatched;
    report.rust_facades = facades
        .into_iter()
        .map(|f| {
            let callee = f
                .calls
                .iter()
                .find(|c| c.starts_with("cpp_"))
                .cloned()
                .unwrap_or_default();
            (f, callee)
        })
        .collect();
    report.shims = shims
        .into_iter()
        .map(|(shim, target)| Shim {
            shim,
            target: target.map(|t| t.name),
        })
        .collect();
    report
}

/// A C++ function that exists for Rust to call through FFI.
fn is_cpp_shim(f: &Function) -> bool {
    f.base.starts_with("cpp_") && (f.path.ends_with("_ffi.cc") || f.path.ends_with("_ffi.cpp"))
}

/// Number of units in a function's body that do something.
fn body_len(f: &Function) -> usize {
    f.units
        .iter()
        .filter(|u| !matches!(u.kind, UnitKind::Signature | UnitKind::Comment))
        .count()
}

/// If `r` only forwards to another Rust function in `pool` that is named
/// like it and resembles `c` better, that function's index.
fn forwardee(c: &Function, r: &Function, pool: &[Function], used: &[bool]) -> Option<usize> {
    if body_len(r) > 5 {
        return None;
    }
    let own = normalize::words(&normalize::ident(&r.base));
    let current = score(c, r).0;
    pool.iter()
        .enumerate()
        .filter(|(j, t)| {
            !used[*j]
                && !t.is_ffi
                && (t.path != r.path || t.start_line != r.start_line)
                && body_len(t) > body_len(r)
                && calls_function(r, t)
                && normalize::words(&normalize::ident(&t.base))
                    .iter()
                    .all(|w| own.contains(w))
        })
        .map(|(j, t)| (j, score(c, t).0))
        .filter(|(_, s)| *s > current)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

/// Whether `r` calls `t`, allowing for `Type::init()` normalizing to the
/// type's name.
fn calls_function(r: &Function, t: &Function) -> bool {
    let by_type = matches!(t.base.as_str(), "new" | "init")
        && t.class
            .as_deref()
            .is_some_and(|c| r.calls.contains(&normalize::ident(c)));
    by_type || r.calls.contains(&normalize::ident(&t.base))
}

fn build_pair(cpp: Function, rust: Function, link: Link, origin: CppOrigin) -> PairReport {
    let (s, pairs) = score(&cpp, &rust);
    let (rows, findings) = check::check(&cpp, &rust, &pairs);
    let summary = check::summarize(&cpp, &rust, &rows);
    PairReport {
        cpp,
        rust,
        link,
        forwarder: None,
        origin,
        score: s,
        rows,
        findings,
        summary,
    }
}
