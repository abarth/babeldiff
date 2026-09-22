//! Aligns the units of a C++ function with the units of a Rust function.

use crate::model::{Unit, UnitKind};
use crate::normalize::{jaccard, seq_similarity};

/// Minimum similarity for two units to be aligned.
pub const THRESHOLD: f64 = 0.4;

fn kind_group(k: UnitKind) -> u8 {
    match k {
        UnitKind::Signature => 0,
        UnitKind::Comment => 1,
        UnitKind::Stmt => 2,
        UnitKind::If | UnitKind::ElseIf => 3,
        UnitKind::Else => 4,
        UnitKind::Loop => 5,
        UnitKind::Switch => 6,
        UnitKind::Case => 7,
        UnitKind::Return => 8,
        UnitKind::Break => 9,
        UnitKind::Continue => 10,
        UnitKind::Goto => 11,
        UnitKind::Label => 12,
    }
}

/// How similar two units are, in `[0, 1]`. Units of incompatible kinds score 0.
/// A call whose failure one side handles in an `if` and the other side
/// propagates with `?` (or its C++ spelling).
pub fn is_handled_vs_propagated(a: &Unit, b: &Unit) -> bool {
    let handled = |u: &Unit| u.kind == UnitKind::If && u.features.checks_error;
    let propagated = |u: &Unit| u.kind == UnitKind::Stmt && u.features.propagates;
    (handled(a) && propagated(b)) || (propagated(a) && handled(b))
}

pub fn similarity(a: &Unit, b: &Unit) -> f64 {
    if is_handled_vs_propagated(a, b) {
        let (fa, fb) = (&a.features, &b.features);
        if fa.calls.is_empty() || fb.calls.is_empty() {
            return 0.0;
        }
        return 0.2 + 0.8 * jaccard(&fa.calls, &fb.calls);
    }
    if kind_group(a.kind) != kind_group(b.kind) {
        return 0.0;
    }
    let fa = &a.features;
    let fb = &b.features;
    if a.kind == UnitKind::Comment {
        return seq_similarity(&fa.comment, &fb.comment);
    }
    let base = match a.kind {
        UnitKind::Signature => 0.6,
        UnitKind::Else | UnitKind::Break | UnitKind::Continue => 0.5,
        // Control flow of the same kind aligns by position unless something
        // better lines up; the checks then report any differences.
        UnitKind::Return | UnitKind::Loop | UnitKind::Switch => 0.4,
        UnitKind::If | UnitKind::ElseIf => 0.35,
        UnitKind::Case => 0.25,
        _ => 0.1,
    };
    // Weighted overlap of the features that identify what a unit does.
    let mut num = 0.0;
    let mut den = 0.0;
    let mut add = |w: f64, s: f64, present: bool| {
        if present {
            num += w * s;
            den += w;
        }
    };
    let mut add_set = |w: f64, a: &[String], b: &[String]| {
        add(w, jaccard(a, b), !(a.is_empty() && b.is_empty()));
    };
    // Calls and identifiers only count when both sides have some; a field
    // on one side and an accessor call on the other is judged by `names`.
    if !fa.calls.is_empty() && !fb.calls.is_empty() {
        add_set(2.0, &fa.calls, &fb.calls);
    }
    if !fa.idents.is_empty() && !fb.idents.is_empty() {
        add_set(1.0, &fa.idents, &fb.idents);
    }
    add_set(2.0, &fa.names, &fb.names);
    add_set(2.0, &fa.errors, &fb.errors);
    add_set(2.0, &fa.locks, &fb.locks);
    add(
        1.0,
        (fa.propagates == fb.propagates) as u8 as f64,
        fa.propagates || fb.propagates,
    );
    if let (Some(ra), Some(rb)) = (&fa.ret, &fb.ret) {
        add(1.0, (ra == rb) as u8 as f64, true);
    }
    let overlap = if den == 0.0 { 1.0 } else { num / den };
    let mut s = base + (1.0 - base) * overlap;
    if a.depth == b.depth {
        // Prefer alignments that keep nesting intact when otherwise tied.
        s += 0.02;
    }
    s.min(1.0)
}

/// One row of an alignment: a C++ unit, a Rust unit, or both.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pair {
    pub cpp: Option<usize>,
    pub rust: Option<usize>,
    pub score: f64,
}

/// Order-preserving alignment maximizing total similarity (a weighted LCS).
pub fn align(a: &[Unit], b: &[Unit]) -> (Vec<Pair>, f64) {
    let (n, m) = (a.len(), b.len());
    let sim: Vec<Vec<f64>> = a
        .iter()
        .map(|x| b.iter().map(|y| similarity(x, y)).collect())
        .collect();
    let mut dp = vec![vec![0.0f64; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let mut best = dp[i + 1][j].max(dp[i][j + 1]);
            let s = sim[i][j];
            if s >= THRESHOLD {
                best = best.max(dp[i + 1][j + 1] + s);
            }
            dp[i][j] = best;
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    let mut pending_a = Vec::new();
    let mut pending_b = Vec::new();
    let flush = |out: &mut Vec<Pair>, pa: &mut Vec<usize>, pb: &mut Vec<usize>| {
        for x in pa.drain(..) {
            out.push(Pair {
                cpp: Some(x),
                rust: None,
                score: 0.0,
            });
        }
        for y in pb.drain(..) {
            out.push(Pair {
                cpp: None,
                rust: Some(y),
                score: 0.0,
            });
        }
    };
    while i < n && j < m {
        let s = sim[i][j];
        if s >= THRESHOLD && (dp[i][j] - (dp[i + 1][j + 1] + s)).abs() < 1e-9 {
            flush(&mut out, &mut pending_a, &mut pending_b);
            out.push(Pair {
                cpp: Some(i),
                rust: Some(j),
                score: s,
            });
            i += 1;
            j += 1;
        } else if (dp[i][j] - dp[i + 1][j]).abs() < 1e-9 {
            pending_a.push(i);
            i += 1;
        } else {
            pending_b.push(j);
            j += 1;
        }
    }
    pending_a.extend(i..n);
    pending_b.extend(j..m);
    flush(&mut out, &mut pending_a, &mut pending_b);
    let total = dp[0][0];
    let norm = if n + m == 0 {
        1.0
    } else {
        2.0 * total / (n + m) as f64
    };
    (out, norm)
}
