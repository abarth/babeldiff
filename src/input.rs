//! Turns files, patches and git revisions into analysis inputs.

use crate::analyze::Inputs;
use crate::cpp::DeclComments;
use crate::extract::{self, attach_decl_comments};
use crate::model::{Function, Lang, UnitKind};
use crate::patch::FilePatch;
use std::collections::BTreeSet;

/// One version of a file and the lines the change touched in it.
#[derive(Clone, Debug)]
pub struct Version {
    pub path: String,
    pub text: String,
    /// Lines removed (old side) or added (new side). `None` means every
    /// function in the file is of interest.
    pub changed: Option<BTreeSet<usize>>,
}

/// The files a change touches, by language and side.
#[derive(Clone, Debug, Default)]
pub struct ChangeSet {
    /// C++ files before the change.
    pub cpp_old: Vec<Version>,
    /// C++ files after the change (used to follow calls into FFI shims).
    pub cpp_new: Vec<Version>,
    /// Rust files after the change.
    pub rust_new: Vec<Version>,
}

impl ChangeSet {
    /// Every function of the given files.
    pub fn from_files(files: &[(String, String)]) -> ChangeSet {
        let mut cs = ChangeSet::default();
        for (path, text) in files {
            let v = Version {
                path: path.clone(),
                text: text.clone(),
                changed: None,
            };
            match Lang::from_path(path) {
                Some(Lang::Cpp) => cs.cpp_old.push(v),
                Some(Lang::Rust) => cs.rust_new.push(v),
                None => {}
            }
        }
        cs
    }

    /// Builds a change set from a parsed patch. `full_text` may supply the
    /// complete contents of a blob (by abbreviated id); otherwise the file is
    /// reconstructed from the hunks alone.
    pub fn from_patch(
        files: &[FilePatch],
        full_text: &mut dyn FnMut(&str) -> Option<String>,
    ) -> ChangeSet {
        let mut cs = ChangeSet::default();
        for f in files {
            let Some(lang) = Lang::from_path(f.path()) else {
                continue;
            };
            let old_text = |ft: &mut dyn FnMut(&str) -> Option<String>| {
                f.old_blob
                    .as_deref()
                    .and_then(&mut *ft)
                    .unwrap_or_else(|| f.sparse_old())
            };
            let new_text = |ft: &mut dyn FnMut(&str) -> Option<String>| {
                f.new_blob
                    .as_deref()
                    .and_then(&mut *ft)
                    .unwrap_or_else(|| f.sparse_new())
            };
            match lang {
                Lang::Cpp => {
                    if let Some(p) = &f.old_path {
                        cs.cpp_old.push(Version {
                            path: p.clone(),
                            text: old_text(full_text),
                            changed: Some(f.removed()),
                        });
                    }
                    if let Some(p) = &f.new_path {
                        cs.cpp_new.push(Version {
                            path: p.clone(),
                            text: new_text(full_text),
                            changed: Some(f.added()),
                        });
                    }
                }
                Lang::Rust => {
                    if let Some(p) = &f.new_path {
                        cs.rust_new.push(Version {
                            path: p.clone(),
                            text: new_text(full_text),
                            changed: Some(f.added()),
                        });
                    }
                }
            }
        }
        cs
    }
}

/// Fraction of a function's body lines that the change touched.
fn changed_fraction(f: &Function, changed: &BTreeSet<usize>) -> f64 {
    let body_start = f
        .units
        .iter()
        .find(|u| u.kind == UnitKind::Signature)
        .map_or(f.start_line, |u| u.start_line);
    let span = f.end_line.saturating_sub(body_start) + 1;
    let n = changed.range(body_start..=f.end_line).count();
    n as f64 / span as f64
}

/// Extracts functions and selects those the change rewrote.
pub fn build_inputs(cs: &ChangeSet, min_changed: f64) -> Inputs {
    let mut inputs = Inputs::default();

    let mut decls: DeclComments = DeclComments::new();
    let mut cpp_all: Vec<(Function, Option<&BTreeSet<usize>>)> = Vec::new();
    for v in &cs.cpp_old {
        let e = extract::extract(Lang::Cpp, &v.path, &v.text);
        for (k, u) in e.decl_comments {
            decls.entry(k).or_insert(u);
        }
        for (k, b) in e.bases {
            inputs.cpp_bases.entry(k).or_insert(b);
        }
        cpp_all.extend(e.functions.into_iter().map(|f| (f, v.changed.as_ref())));
    }
    let mut cpp: Vec<Function> = cpp_all
        .into_iter()
        .filter(|(f, ch)| ch.is_none_or(|c| changed_fraction(f, c) >= min_changed))
        .map(|(f, _)| f)
        .collect();
    attach_decl_comments(&mut cpp, &decls);
    inputs.cpp = cpp;

    for v in &cs.cpp_new {
        let e = extract::extract(Lang::Cpp, &v.path, &v.text);
        for (k, b) in e.bases {
            inputs.cpp_bases.entry(k).or_insert(b);
        }
        for f in e.functions {
            inputs
                .cpp_new_calls
                .entry(f.name.clone())
                .or_default()
                .extend(f.calls);
        }
    }

    for v in &cs.rust_new {
        for f in extract::extract(Lang::Rust, &v.path, &v.text).functions {
            let selected = v
                .changed
                .as_ref()
                .is_none_or(|c| changed_fraction(&f, c) >= min_changed);
            if selected {
                inputs.rust.push(f.clone());
            }
            inputs.all_rust.push(f);
        }
    }
    inputs
}
