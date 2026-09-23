//! Language dispatch for function extraction.

use crate::cpp::{self, DeclComments};
use crate::model::{Function, Lang, UnitKind};
use crate::rust;

/// Splits source into lines with tabs expanded to 8 columns.
pub fn split_lines(src: &str) -> Vec<String> {
    src.lines()
        .map(|l| {
            if !l.contains('\t') {
                return l.trim_end_matches('\r').to_string();
            }
            let mut out = String::new();
            for c in l.trim_end_matches('\r').chars() {
                if c == '\t' {
                    let n = 8 - out.chars().count() % 8;
                    out.extend(std::iter::repeat_n(' ', n));
                } else {
                    out.push(c);
                }
            }
            out
        })
        .collect()
}

/// Functions from one file, plus (for C++) comments found on declarations.
pub struct Extracted {
    pub functions: Vec<Function>,
    pub decl_comments: DeclComments,
    pub bases: cpp::ClassBases,
}

pub fn extract(lang: Lang, path: &str, src: &str) -> Extracted {
    match lang {
        Lang::Cpp => {
            let f = cpp::extract(path, src);
            Extracted {
                functions: f.functions,
                decl_comments: f.decl_comments,
                bases: f.bases,
            }
        }
        Lang::Rust => Extracted {
            functions: rust::extract(path, src),
            decl_comments: Default::default(),
            bases: Default::default(),
        },
    }
}

/// Gives C++ functions that have no comment of their own the comment on
/// their declaration (usually in the class body in a header), since that is
/// where the Rust doc comment came from.
pub fn attach_decl_comments(functions: &mut [Function], decls: &DeclComments) {
    for f in functions.iter_mut().filter(|f| f.lang == Lang::Cpp) {
        let has_leading = f.units.first().is_some_and(|u| u.kind == UnitKind::Comment);
        if has_leading {
            continue;
        }
        let key = (f.class.clone().unwrap_or_default(), f.base.clone());
        let found = decls.get(&key).or_else(|| {
            // Fall back to the innermost class name for nested classes.
            let inner = f
                .class
                .as_deref()
                .and_then(|c| c.rsplit("::").next())
                .unwrap_or("");
            decls.get(&(inner.to_string(), f.base.clone()))
        });
        if let Some(units) = found {
            let mut units = units.clone();
            for u in &mut units {
                u.depth = 0;
            }
            f.units.splice(0..0, units);
        }
    }
}
