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
    /// C++ overrides of the same method in related classes whose bodies the
    /// Rust function also carries, typically as arms of a `match` on an
    /// enum that replaced the class hierarchy.
    pub overrides: Vec<OverrideReport>,
    /// Why the pair was chosen, for readers who need to judge it.
    pub rationale: String,
}

/// A C++ override folded into the Rust function of a pair.
#[derive(Clone, Debug)]
pub struct OverrideReport {
    pub cpp: Function,
    /// Rows of the override against the Rust units it matched. Rust units
    /// that belong to the primary C++ function or other overrides are left
    /// out.
    pub rows: Vec<Row>,
    pub findings: Vec<Finding>,
}

impl PairReport {
    /// Every finding of the pair, including those of its overrides.
    pub fn all_findings(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .chain(self.overrides.iter().flat_map(|o| o.findings.iter()))
    }
    pub fn issues(&self) -> usize {
        self.all_findings()
            .filter(|f| f.severity == Severity::Issue)
            .count()
    }
    pub fn notes(&self) -> usize {
        self.all_findings()
            .filter(|f| f.severity == Severity::Note)
            .count()
    }
}

/// An FFI shim and the Rust function it forwards to.
#[derive(Clone, Debug)]
pub struct Shim {
    pub shim: Function,
    pub target: Option<String>,
    /// Functions the shim could forward to when babeldiff can't tell which.
    pub ambiguous: Vec<String>,
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
    /// Drops notes and the pairs left with no issues, for readers who only
    /// want what needs fixing.
    pub fn retain_issues(&mut self) {
        let strip = |rows: &mut Vec<Row>, findings: &mut Vec<Finding>| {
            for r in rows.iter_mut() {
                r.notes.retain(|n| n.severity == Severity::Issue);
                if r.marker == check::Marker::Note {
                    r.marker = check::Marker::Same;
                }
            }
            findings.retain(|f| f.severity == Severity::Issue);
        };
        for p in &mut self.pairs {
            strip(&mut p.rows, &mut p.findings);
            for o in &mut p.overrides {
                strip(&mut o.rows, &mut o.findings);
            }
        }
        self.pairs.retain(|p| p.issues() > 0);
    }

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
    /// Base classes of the C++ classes defined in the changed files.
    pub cpp_bases: crate::cpp::ClassBases,
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
        // A method and a free function are less alike than two methods or
        // two free functions with the same name.
        (Some(_), None) | (None, Some(_)) => 0.8 * base,
        (None, None) => base,
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

/// What an FFI shim forwards to.
enum Target {
    One(Function),
    /// Several functions fit equally well.
    Ambiguous(Vec<String>),
    None,
}

/// `Foo::bar` as `foo::bar`, the form of [`crate::model::Features::qcalls`].
fn qualified_key(f: &Function) -> Option<String> {
    let class = f.class.as_deref()?;
    let class = class.rsplit("::").next().unwrap_or(class);
    Some(format!(
        "{}::{}",
        normalize::ident(class),
        normalize::ident(&f.base)
    ))
}

/// Fraction of directory names two paths share, in `[0, 1]`.
fn path_affinity(a: &str, b: &str) -> f64 {
    let dirs = |p: &str| -> Vec<String> {
        let mut v: Vec<String> = p.split('/').map(str::to_string).collect();
        v.pop();
        v
    };
    let (da, db) = (dirs(a), dirs(b));
    if da.is_empty() || db.is_empty() {
        return 0.0;
    }
    let shared = da.iter().filter(|d| db.contains(d)).count();
    shared as f64 / da.len().max(db.len()) as f64
}

/// A path's file name without extension and `_ffi` suffix.
fn file_stem(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    let s = base.split('.').next().unwrap_or(base);
    s.strip_suffix("_ffi").unwrap_or(s).to_string()
}

/// Finds the function an FFI shim forwards to. When the shim calls
/// several same-named methods (`A::create` and `B::create`), the type path
/// of the call, the shim's name and its file decide; if they don't, the
/// target is ambiguous rather than guessed.
fn shim_target(shim: &Function, rust: &[Function]) -> Target {
    let und = undecorated(&shim.base);
    // How a call to `r` appears in the shim's call list: `Foo::init` is
    // recorded as a call of `foo`, like a constructor.
    let key = |r: &Function| normalize::call(&r.name).unwrap_or_else(|| normalize::ident(&r.base));
    let scored: Vec<(usize, &Function)> = rust
        .iter()
        .filter(|r| !r.is_ffi && r.name != shim.name)
        .filter(|r| shim.calls.contains(&key(r)))
        .map(|r| {
            let k = key(r);
            let b = normalize::ident(&r.base);
            let q = qualified_key(r);
            let by_path = q.as_ref().is_some_and(|q| shim.qcalls.contains(q));
            let exact = q.as_ref().is_some_and(|q| q.replace("::", "_") == und);
            let same_file = file_stem(&r.path) == file_stem(&shim.path);
            let score = (by_path as usize) * 4000
                + (exact as usize) * 2000
                + (und.ends_with(&b) as usize) * 1000
                + (und.contains(&k) as usize) * 500
                + (same_file as usize) * 100
                + k.len();
            (score, r)
        })
        .collect();
    let Some(best) = scored.iter().map(|(s, _)| *s).max() else {
        return Target::None;
    };
    let top: Vec<&Function> = scored
        .iter()
        .filter(|(s, _)| *s == best)
        .map(|(_, r)| *r)
        .collect();
    match top.as_slice() {
        [one] => Target::One((*one).clone()),
        many => Target::Ambiguous(many.iter().map(|r| r.name.clone()).collect()),
    }
}

/// The last component of a class path, normalized.
fn class_key(f: &Function) -> Option<String> {
    let c = f.class.as_deref()?;
    Some(normalize::ident(c.rsplit("::").next().unwrap_or(c)))
}

/// The C++ class hierarchy of the change, by normalized class name.
struct Hierarchy {
    bases: HashMap<String, Vec<String>>,
}

impl Hierarchy {
    fn new(raw: &crate::cpp::ClassBases) -> Hierarchy {
        let bases = raw
            .iter()
            .map(|(k, v)| {
                (
                    normalize::ident(k),
                    v.iter().map(|b| normalize::ident(b)).collect(),
                )
            })
            .collect();
        Hierarchy { bases }
    }

    fn direct(&self, class: &str) -> &[String] {
        self.bases.get(class).map_or(&[], Vec::as_slice)
    }

    fn ancestors(&self, class: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut todo = vec![class.to_string()];
        while let Some(c) = todo.pop() {
            for b in self.direct(&c) {
                if !out.contains(b) && b != class {
                    out.push(b.clone());
                    todo.push(b.clone());
                }
            }
        }
        out
    }

    fn is_ancestor(&self, a: &str, of: &str) -> bool {
        self.ancestors(of).iter().any(|x| x == a)
    }

    /// One class derives from the other, or both derive directly from the
    /// same class.
    fn related(&self, a: &str, b: &str) -> bool {
        a == b
            || self.is_ancestor(a, b)
            || self.is_ancestor(b, a)
            || self.direct(a).iter().any(|x| self.direct(b).contains(x))
    }
}

/// The name a function goes by for matching overrides: constructors are
/// `new` and destructors `drop`, like their Rust counterparts.
fn override_key(f: &Function) -> String {
    name_words(f).join("_")
}

/// Runs the analysis.
pub fn analyze(inputs: Inputs, opts: &Options, finder: &mut dyn CppFinder) -> Report {
    let Inputs {
        cpp,
        rust,
        all_rust,
        cpp_new_calls,
        forced,
        cpp_bases,
    } = inputs;
    let mut report = Report::default();
    let hierarchy = Hierarchy::new(&cpp_bases);

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
    let shims: Vec<(Function, Target)> = universe
        .iter()
        .filter(|f| f.is_ffi)
        .map(|s| (s.clone(), shim_target(s, &universe)))
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
        if let Target::One(t) = t {
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

    // Types that exist on each side, to tell a port of `A::f` from a
    // same-named method of another class.
    let cpp_classes: Vec<String> = cpp
        .iter()
        .filter_map(class_key)
        .chain(hierarchy.bases.keys().cloned())
        .collect();
    let rust_classes: Vec<String> = universe.iter().filter_map(class_key).collect();

    let mut chosen: Vec<(usize, usize, Link, String)> = Vec::new();
    let mut forwarders: Vec<(usize, usize)> = Vec::new();

    // 1. Explicit pairs.
    for (c, r) in &forced {
        let ci = cpp.iter().position(|f| &f.name == c || &f.base == c);
        let ri = rust_pool.iter().position(|f| &f.name == r || &f.base == r);
        if let (Some(ci), Some(ri)) = (ci, ri) {
            if !cpp_used[ci] && !rust_used[ri] {
                cpp_used[ci] = true;
                rust_used[ri] = true;
                chosen.push((ci, ri, Link::Forced, "requested with --pair".into()));
            }
        }
    }

    // 2. FFI links: the new C++ body calls the shim, or the shim is named
    //    after the C++ function.
    for orig in 0..cpp.len() {
        if cpp_used[orig] {
            continue;
        }
        let c = &cpp[orig];
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
            // An exported Rust function that does the work itself, rather
            // than forwarding, is the port.
            let target = match target {
                Target::One(t) if score(c, shim).0 > score(c, t).0 => shim,
                Target::One(t) => t,
                Target::None => shim,
                Target::Ambiguous(_) if body_len(shim) > 3 => shim,
                // Leave it to the similarity pass rather than guess.
                Target::Ambiguous(_) => continue,
            };
            let Some(ri) = find_rust(&rust_pool, target) else {
                continue;
            };
            if rust_used[ri] {
                continue;
            }
            // A C++ trampoline (`mask_interrupt(v)` calling
            // `manager.MaskInterrupt(v)`) shares the shim's name, but the
            // C++ it calls holds the body the Rust ports.
            let callee = (body_len(c) <= 2)
                .then(|| {
                    cpp.iter().enumerate().find(|(k, d)| {
                        *k != orig
                            && !cpp_used[*k]
                            && body_len(d) > body_len(c)
                            && c.calls.contains(&normalize::ident(&d.base))
                            && score(d, target).0 > score(&cpp[orig], target).0
                    })
                })
                .flatten()
                .map(|(k, _)| k);
            let ci = callee.unwrap_or(orig);
            let c = &cpp[ci];
            // Nothing in common at all: the name led astray.
            if body_len(c) > 2 && body_len(target) > 2 && score(c, target).0 < 0.05 {
                continue;
            }
            let (link, why) = if by_call {
                (
                    Link::Ffi {
                        shim: shim.name.clone(),
                        shim_location: shim.location(),
                    },
                    format!("the new C++ body calls FFI shim {}", shim.name),
                )
            } else {
                (
                    Link::FfiName {
                        shim: shim.name.clone(),
                        shim_location: shim.location(),
                    },
                    format!("FFI shim {} is named after the C++ function", shim.name),
                )
            };
            let why = match callee {
                Some(_) => format!("{why}, through the C++ trampoline {}", cpp[orig].name),
                None => why,
            };
            if callee.is_some() {
                cpp_used[orig] = true;
            }
            cpp_used[ci] = true;
            rust_used[ri] = true;
            chosen.push((ci, ri, link, why));
            break;
        }
    }

    // 3. Same class and method name, where exactly one of each exists: a
    //    name match that close beats body similarity.
    for ci in 0..cpp.len() {
        if cpp_used[ci] || is_cpp_shim(&cpp[ci]) {
            continue;
        }
        let c = &cpp[ci];
        let exact = |r: &Function| {
            !r.is_ffi && class_key(r) == class_key(c) && name_words(r) == name_words(c)
        };
        let hits: Vec<usize> = (0..rust_pool.len())
            .filter(|&ri| !rust_used[ri] && exact(&rust_pool[ri]))
            .collect();
        let rivals = cpp
            .iter()
            .enumerate()
            .filter(|(k, d)| {
                *k != ci
                    && !cpp_used[*k]
                    && hits.iter().any(|&ri| {
                        class_key(d) == class_key(&rust_pool[ri])
                            && name_words(d) == name_words(&rust_pool[ri])
                    })
            })
            .count();
        if let ([ri], 0) = (hits.as_slice(), rivals) {
            cpp_used[ci] = true;
            rust_used[*ri] = true;
            chosen.push((
                ci,
                *ri,
                Link::Similarity,
                "same class and method name".into(),
            ));
        }
    }

    // 4. Similarity, greedily from the best score down.
    let mut cands: Vec<(f64, usize, usize)> = Vec::new();
    for (ci, c) in cpp.iter().enumerate() {
        if cpp_used[ci] {
            continue;
        }
        for (ri, r) in rust_pool.iter().enumerate() {
            if rust_used[ri] || r.is_ffi {
                continue;
            }
            if !class_compatible(c, r, &cpp_classes, &rust_classes, &hierarchy) {
                continue;
            }
            // A C++ method that calls a free function named like `r` is a
            // caller of what `r` ports, not its source.
            if r.class.is_none()
                && c.class.is_some()
                && c.calls.contains(&normalize::ident(&r.base))
            {
                continue;
            }
            let (s, _) = score(c, r);
            if s >= opts.min_score && plausible(c, r) {
                // Among equals, prefer the C++ from the same directory
                // (`arch/riscv64` over `arch/arm64`).
                cands.push((s + 0.01 * path_affinity(&c.path, &r.path), ci, ri));
            }
        }
    }
    cands.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (s, ci, ri) in cands {
        if !cpp_used[ci] && !rust_used[ri] {
            cpp_used[ci] = true;
            rust_used[ri] = true;
            chosen.push((
                ci,
                ri,
                Link::Similarity,
                format!("names and bodies are similar (score {s:.2})"),
            ));
        }
    }

    // A thin Rust method that just calls the method doing the work (say
    // `Widget::dump` calling `WidgetState::dump`) stands in
    // for the C++ only by name; compare against the method doing the work.
    for (ci, ri, _, _) in &mut chosen {
        if let Some(rj) = forwardee(&cpp[*ci], &rust_pool[*ri], &rust_pool, &rust_used) {
            rust_used[rj] = true;
            forwarders.push((rj, *ri));
            *ri = rj;
        }
    }

    // 5. Overrides: a C++ override of a method whose base or sibling version
    //    is paired, in a related class, is folded into the same Rust
    //    function (an enum and `match` replacing virtual dispatch).
    let mut overrides: HashMap<usize, Vec<usize>> = HashMap::new();
    for (ci, c) in cpp.iter().enumerate() {
        if cpp_used[ci] || is_cpp_shim(c) {
            continue;
        }
        let Some(ck) = class_key(c) else { continue };
        let key = override_key(c);
        let host = chosen.iter().position(|(pi, _, _, _)| {
            let p = &cpp[*pi];
            override_key(p) == key && class_key(p).is_some_and(|pk| hierarchy.related(&pk, &ck))
        });
        if let Some(h) = host {
            let (pi, ri, _, _) = &chosen[h];
            let rust_fn = &rust_pool[*ri];
            // Only when the Rust carries something of the override, or the
            // override is too small to say.
            let claimed = override_rows(&cpp[*pi], rust_fn, &[c]);
            let matched = claimed[0]
                .iter()
                .filter(|p| {
                    p.cpp.is_some_and(|i| {
                        !matches!(c.units[i].kind, UnitKind::Signature | UnitKind::Comment)
                    }) && p.rust.is_some()
                })
                .count();
            if matched > 0 || body_len(c) <= 1 {
                cpp_used[ci] = true;
                overrides.entry(h).or_default().push(ci);
            }
        }
    }

    for (h, (ci, ri, link, why)) in chosen.into_iter().enumerate() {
        let ovs: Vec<Function> = overrides
            .get(&h)
            .map(|v| v.iter().map(|&k| cpp[k].clone()).collect())
            .unwrap_or_default();
        let mut pair = build_pair(
            cpp[ci].clone(),
            rust_pool[ri].clone(),
            link,
            CppOrigin::Changed,
            ovs,
        );
        pair.rationale = why;
        pair.forwarder = forwarders
            .iter()
            .find(|(t, _)| *t == ri)
            .map(|(_, f)| rust_pool[*f].clone());
        report.pairs.push(pair);
    }

    // 6. Rust with no C++ in the change: look for C++ the change left alone.
    let in_change = |c: &Function| cpp.iter().any(|x| x.path == c.path && x.name == c.name);
    let mut found_used: Vec<(String, usize)> = Vec::new();
    for (ri, r) in rust_pool.iter().enumerate() {
        if rust_used[ri] || r.is_ffi {
            continue;
        }
        let mut ranked: Vec<(bool, f64, Function)> = finder
            .find(r)
            .into_iter()
            .filter(|c| !in_change(c))
            .filter(|c| !found_used.contains(&(c.path.clone(), c.start_line)))
            .map(|c| {
                let ok = unchanged_class_ok(&c, r, &hierarchy);
                (ok, score(&c, r).0, c)
            })
            .filter(|(_, s, _)| *s >= opts.min_unchanged_score)
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.total_cmp(&a.1)));
        let Some((ok, s, c)) = ranked.first().cloned() else {
            continue;
        };
        // Two equally good candidates in unrelated classes: don't guess.
        let tie = ranked.get(1).is_some_and(|(ok2, s2, c2)| {
            *ok2 == ok && (s - s2).abs() < 0.02 && class_key(c2) != class_key(&c)
        });
        if tie {
            continue;
        }
        rust_used[ri] = true;
        found_used.push((c.path.clone(), c.start_line));
        // The change left this C++ alone, so its comments are all still
        // there.
        let mut c = c;
        for u in &mut c.units {
            if u.kind == UnitKind::Comment {
                u.features.still_in_cpp = true;
            }
        }
        let mut pair = build_pair(
            c,
            r.clone(),
            Link::Similarity,
            CppOrigin::Unchanged,
            Vec::new(),
        );
        pair.rationale = format!(
            "found in the repository, not in the change (score {:.2})",
            pair.score
        );
        report.pairs.push(pair);
    }

    // 7. C++ with no Rust in the change: the Rust may already exist, in a
    //    file the change touched elsewhere.
    for ci in 0..cpp.len() {
        if cpp_used[ci] || is_cpp_shim(&cpp[ci]) {
            continue;
        }
        let c = &cpp[ci];
        let hits: Vec<&Function> = universe
            .iter()
            .filter(|r| !r.is_ffi && class_key(r) == class_key(c) && name_words(r) == name_words(c))
            .filter(|r| find_rust(&rust_pool, r).is_none_or(|ri| !rust_used[ri]))
            .collect();
        let [r] = hits.as_slice() else { continue };
        let (s, _) = score(c, r);
        if s < opts.min_unchanged_score {
            continue;
        }
        let r = (*r).clone();
        match find_rust(&rust_pool, &r) {
            Some(ri) => rust_used[ri] = true,
            None => {
                rust_pool.push(r.clone());
                rust_used.push(true);
            }
        }
        cpp_used[ci] = true;
        let mut pair = build_pair(
            c.clone(),
            r,
            Link::Similarity,
            CppOrigin::Changed,
            Vec::new(),
        );
        pair.rationale = format!(
            "same name; the change left this Rust function alone (score {:.2})",
            pair.score
        );
        report.pairs.push(pair);
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
        .map(|(shim, target)| match target {
            Target::One(t) => Shim {
                shim,
                target: Some(t.name),
                ambiguous: Vec::new(),
            },
            Target::Ambiguous(names) => Shim {
                shim,
                target: None,
                ambiguous: names,
            },
            Target::None => Shim {
                shim,
                target: None,
                ambiguous: Vec::new(),
            },
        })
        .collect();
    report
}

/// Whether C++ `c` and Rust `r` may be the same method, judging by their
/// classes. `A::f` doesn't port to `B::f` when the change has both a C++
/// class named like `B` and a Rust type named like `A`, unless one class
/// derives from the other.
fn class_compatible(
    c: &Function,
    r: &Function,
    cpp_classes: &[String],
    rust_classes: &[String],
    h: &Hierarchy,
) -> bool {
    let (Some(ca), Some(rb)) = (class_key(c), class_key(r)) else {
        return true;
    };
    if ca == rb || h.is_ancestor(&ca, &rb) || h.is_ancestor(&rb, &ca) {
        return true;
    }
    !(rust_classes.contains(&ca) && cpp_classes.contains(&rb))
}

/// Whether unchanged C++ `c` can stand for Rust `r` by class: the same
/// class, or a base of the C++ class named like the Rust type.
fn unchanged_class_ok(c: &Function, r: &Function, h: &Hierarchy) -> bool {
    match (class_key(c), class_key(r)) {
        (Some(ca), Some(rb)) => ca == rb || h.is_ancestor(&ca, &rb),
        _ => true,
    }
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
                && {
                    // `dump` and `dump`, or `create` and `create_locked`.
                    let theirs = normalize::words(&normalize::ident(&t.base));
                    theirs.iter().all(|w| own.contains(w)) || own.iter().all(|w| theirs.contains(w))
                }
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

/// Aligns the primary C++ function, then each override in turn against the
/// Rust units still unmatched. Returns the primary's alignment followed by
/// one alignment per override, in Rust unit indices; the override
/// alignments leave out Rust-only rows.
/// How alike an override's unit and the primary's Rust must be to count as
/// shared code.
const SHARED_MIN: f64 = 0.6;

fn override_rows(primary: &Function, rust: &Function, ovs: &[&Function]) -> Vec<Vec<Pair>> {
    let (main, _) = align::align(&primary.units, &rust.units);
    let mut taken: Vec<bool> = vec![false; rust.units.len()];
    for p in &main {
        if let (Some(_), Some(j)) = (p.cpp, p.rust) {
            taken[j] = true;
        }
    }
    let mut out = Vec::new();
    for o in ovs {
        let free: Vec<usize> = (0..rust.units.len())
            .filter(|&j| !taken[j] && rust.units[j].kind != UnitKind::Signature)
            .collect();
        let units: Vec<crate::model::Unit> = free.iter().map(|&j| rust.units[j].clone()).collect();
        let (pairs, _) = align::align(&o.units, &units);
        let mapped: Vec<Pair> = pairs
            .into_iter()
            .filter(|p| p.cpp.is_some())
            .map(|p| Pair {
                cpp: p.cpp,
                rust: p.rust.map(|j| free[j]),
                score: p.score,
            })
            .collect();
        for p in &mapped {
            if let Some(j) = p.rust {
                taken[j] = true;
            }
        }
        // What the override shares with the primary (an argument check
        // hoisted above the `match`, the final `Ok(())`) matches the Rust
        // the primary matched.
        let mut mapped = mapped;
        let left: Vec<usize> = mapped
            .iter()
            .enumerate()
            .filter(|(_, p)| p.rust.is_none())
            .map(|(k, _)| k)
            .collect();
        let shared: Vec<usize> = main.iter().filter_map(|p| p.cpp.and(p.rust)).collect();
        if !left.is_empty() && !shared.is_empty() {
            let cu: Vec<crate::model::Unit> = left
                .iter()
                .map(|&k| o.units[mapped[k].cpp.unwrap()].clone())
                .collect();
            let ru: Vec<crate::model::Unit> =
                shared.iter().map(|&j| rust.units[j].clone()).collect();
            let (pairs, _) = align::align(&cu, &ru);
            for p in pairs {
                if let (Some(a), Some(b)) = (p.cpp, p.rust) {
                    if p.score >= SHARED_MIN {
                        mapped[left[a]].rust = Some(shared[b]);
                        mapped[left[a]].score = p.score;
                    }
                }
            }
        }
        out.push(mapped);
    }
    out
}

fn build_pair(
    cpp: Function,
    rust: Function,
    link: Link,
    origin: CppOrigin,
    overrides: Vec<Function>,
) -> PairReport {
    let (s, pairs) = score(&cpp, &rust);
    let ov_refs: Vec<&Function> = overrides.iter().collect();
    let ov_rows = if overrides.is_empty() {
        Vec::new()
    } else {
        override_rows(&cpp, &rust, &ov_refs)
    };
    // Rust units an override accounts for are not "only in Rust", nor is
    // the `match` that replaced virtual dispatch.
    let mut claimed: Vec<usize> = ov_rows
        .iter()
        .flatten()
        .filter_map(|p| p.cpp.and(p.rust))
        .collect();
    if !overrides.is_empty() {
        claimed.extend(
            pairs
                .iter()
                .filter(|p| p.cpp.is_none())
                .filter_map(|p| p.rust)
                .filter(|&j| matches!(rust.units[j].kind, UnitKind::Switch | UnitKind::Case)),
        );
    }
    let (rows, findings) = check::check_with(&cpp, &rust, &pairs, &claimed);
    let mut summary = check::summarize(&cpp, &rust, &rows);
    let overrides: Vec<OverrideReport> = overrides
        .into_iter()
        .zip(ov_rows)
        .map(|(o, pairs)| {
            let (rows, mut ov_findings) = check::check_with(&o, &rust, &pairs, &[]);
            // Code the override shares with the primary has been reported
            // once already.
            ov_findings.retain(|f| {
                !findings.iter().any(|g| {
                    g.rust_line.is_some() && g.rust_line == f.rust_line && g.message == f.message
                })
            });
            OverrideReport {
                cpp: o,
                rows,
                findings: ov_findings,
            }
        })
        .collect();
    if !overrides.is_empty() {
        merge_override_summaries(&mut summary, &rust, &overrides);
    }
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
        overrides,
        rationale: String::new(),
    }
}

/// Folds the overrides' facts into a pair's summary. The Rust function does
/// what the primary and each override do, once per arm, so errors and locks
/// are compared as sets, control flow by the busiest C++ version, and the
/// `match` that replaced virtual dispatch is not a difference.
fn merge_override_summaries(s: &mut Summary, rust: &Function, ovs: &[OverrideReport]) {
    for o in ovs {
        let os = check::summarize(&o.cpp, rust, &o.rows);
        s.cpp_errors.extend(os.cpp_errors);
        s.cpp_locks.extend(os.cpp_locks);
        s.comments_total += os.comments_total;
        s.comments_same += os.comments_same;
        s.comments_changed += os.comments_changed;
        for (name, a, _) in os.flow {
            match s.flow.iter_mut().find(|(n, _, _)| *n == name) {
                Some(e) => e.1 = e.1.max(a),
                None => {
                    let b = s
                        .flow
                        .iter()
                        .find(|(n, _, _)| *n == name)
                        .map_or(0, |e| e.2);
                    s.flow.push((name, a, b));
                }
            }
        }
    }
    for v in [
        &mut s.cpp_errors,
        &mut s.rust_errors,
        &mut s.cpp_locks,
        &mut s.rust_locks,
    ] {
        v.sort();
        v.dedup();
    }
    s.flow
        .retain(|(name, a, _)| !(*name == "switch" && *a == 0));
}
