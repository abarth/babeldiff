//! Machine-readable output for agents and scripts.
//!
//! The report is one JSON object. Every finding carries a stable id (the
//! pair's C++ name and the finding's position in it), its severity and
//! category, the rubric rule it checks, and file:line locations on both
//! sides, so an agent can go straight to the code and cite the rule.

use crate::analyze::{CppOrigin, Link, PairReport, Report};
use crate::check::{Finding, Severity};
use crate::model::Function;
use std::fmt::Write;

/// Version of the JSON layout. Bumped when a field changes meaning or is
/// removed; new fields may be added without a bump.
pub const SCHEMA_VERSION: u32 = 1;

pub fn render_json(report: &Report, title: &str) -> String {
    let mut o = Obj::new();
    o.num("version", SCHEMA_VERSION as f64);
    o.str("title", title);
    let mut s = Obj::new();
    s.num("pairs", report.pairs.len() as f64);
    s.num("issues", report.issues() as f64);
    s.num("notes", report.notes() as f64);
    s.num("unpaired_cpp", report.unmatched_cpp.len() as f64);
    s.num("unpaired_rust", report.unmatched_rust.len() as f64);
    let mut by = Obj::new();
    for (c, n) in crate::analyze::issues_by_category(report) {
        by.num(c.name(), n as f64);
    }
    s.raw("issues_by_category", &by.finish());
    o.raw("summary", &s.finish());
    let pairs: Vec<String> = report.pairs.iter().map(pair).collect();
    o.raw("pairs", &array(&pairs));
    o.raw("unpaired_cpp", &array(&functions(&report.unmatched_cpp)));
    o.raw("unpaired_rust", &array(&functions(&report.unmatched_rust)));
    o.raw(
        "removed_cpp_ffi_helpers",
        &array(&functions(&report.removed_cpp_shims)),
    );
    let facades: Vec<String> = report
        .rust_facades
        .iter()
        .map(|(f, callee)| {
            let mut x = Obj::new();
            x.raw("rust", &function(f));
            x.str("calls", callee);
            x.finish()
        })
        .collect();
    o.raw("rust_facades", &array(&facades));
    let shims: Vec<String> = report
        .shims
        .iter()
        .map(|s| {
            let mut x = Obj::new();
            x.raw("shim", &function(&s.shim));
            match &s.target {
                Some(t) => x.str("target", t),
                None => x.raw("target", "null"),
            }
            let amb: Vec<String> = s.ambiguous.iter().map(|a| quote(a)).collect();
            x.raw("ambiguous", &array(&amb));
            x.finish()
        })
        .collect();
    o.raw("shims", &array(&shims));
    let changes: Vec<String> = report
        .cpp_changes
        .iter()
        .map(|c| {
            let mut x = Obj::new();
            x.str("path", &c.path);
            x.num("start_line", c.start_line as f64);
            x.num("end_line", c.end_line as f64);
            let fs: Vec<String> = c.functions.iter().map(|f| quote(f)).collect();
            x.raw("functions", &array(&fs));
            x.str("text", &c.text);
            x.finish()
        })
        .collect();
    o.raw("cpp_changes_outside_port", &array(&changes));
    let mut out = o.finish();
    out.push('\n');
    out
}

fn pair(p: &PairReport) -> String {
    let mut o = Obj::new();
    o.str("id", &p.cpp.name);
    o.raw("cpp", &function(&p.cpp));
    o.raw("rust", &function(&p.rust));
    let (link, shim) = match &p.link {
        Link::Ffi {
            shim,
            shim_location,
        } => ("ffi", Some((shim, shim_location))),
        Link::FfiName {
            shim,
            shim_location,
        } => ("ffi-name", Some((shim, shim_location))),
        Link::Similarity => ("similarity", None),
        Link::Forced => ("forced", None),
    };
    o.str("link", link);
    if let Some((name, loc)) = shim {
        let mut s = Obj::new();
        s.str("name", name);
        s.str("location", loc);
        o.raw("shim", &s.finish());
    }
    if let Some(f) = &p.forwarder {
        o.raw("forwarder", &function(f));
    }
    o.str(
        "origin",
        match p.origin {
            CppOrigin::Changed => "changed",
            CppOrigin::Unchanged => "unchanged",
        },
    );
    o.str("rationale", &p.rationale);
    o.num("score", (p.score * 100.0).round() / 100.0);
    let ovs: Vec<String> = p.overrides.iter().map(|ov| function(&ov.cpp)).collect();
    o.raw("overrides", &array(&ovs));
    let mut findings: Vec<(&Function, &Finding)> = p.findings.iter().map(|f| (&p.cpp, f)).collect();
    for ov in &p.overrides {
        findings.extend(ov.findings.iter().map(|f| (&ov.cpp, f)));
    }
    let list: Vec<String> = findings
        .iter()
        .enumerate()
        .map(|(i, (cf, f))| {
            let id = format!("{}#{}", p.cpp.name, i + 1);
            let ov = (cf.path != p.cpp.path || cf.start_line != p.cpp.start_line).then_some(*cf);
            finding(&id, cf, &p.rust, f, ov)
        })
        .collect();
    o.raw("findings", &array(&list));
    o.finish()
}

/// `override_of` is set when the finding belongs to a C++ override folded
/// into the pair's Rust function.
fn finding(
    id: &str,
    cpp: &Function,
    rust: &Function,
    f: &Finding,
    override_of: Option<&Function>,
) -> String {
    let mut o = Obj::new();
    o.str("id", id);
    o.str(
        "severity",
        match f.severity {
            Severity::Issue => "issue",
            Severity::Note => "note",
        },
    );
    o.str("category", f.category.name());
    o.str("rubric", f.category.rubric());
    o.str("message", &f.message);
    match f.cpp_line {
        Some(l) => {
            let path = f.cpp_file.as_deref().unwrap_or(&cpp.path);
            let text = if f.cpp_file.is_some() {
                ""
            } else {
                cpp.line(l)
            };
            o.raw("cpp", &location(path, l, text));
        }
        None => o.raw("cpp", "null"),
    }
    match f.rust_line {
        Some(l) => o.raw("rust", &location(&rust.path, l, rust.line(l))),
        None => o.raw("rust", "null"),
    }
    if let Some(ov) = override_of {
        o.str("override", &ov.name);
    }
    o.finish()
}

fn location(path: &str, line: usize, text: &str) -> String {
    let mut o = Obj::new();
    o.str("path", path);
    o.num("line", line as f64);
    if !text.is_empty() {
        o.str("text", text.trim());
    }
    o.finish()
}

fn function(f: &Function) -> String {
    let mut o = Obj::new();
    o.str("name", &f.name);
    o.str("path", &f.path);
    o.num("start_line", f.start_line as f64);
    o.num("end_line", f.end_line as f64);
    o.finish()
}

fn functions(fs: &[Function]) -> Vec<String> {
    fs.iter().map(function).collect()
}

/// A JSON object under construction.
struct Obj(String);

impl Obj {
    fn new() -> Obj {
        Obj(String::from("{"))
    }
    fn key(&mut self, k: &str) {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        self.0.push_str(&quote(k));
        self.0.push(':');
    }
    fn str(&mut self, k: &str, v: &str) {
        self.key(k);
        self.0.push_str(&quote(v));
    }
    fn num(&mut self, k: &str, v: f64) {
        self.key(k);
        let _ = write!(self.0, "{v}");
    }
    fn raw(&mut self, k: &str, v: &str) {
        self.key(k);
        self.0.push_str(v);
    }
    fn finish(mut self) -> String {
        self.0.push('}');
        self.0
    }
}

fn array(items: &[String]) -> String {
    format!("[{}]", items.join(","))
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn quotes_control_characters() {
        assert_eq!(quote("a\"b\\c\n\u{1}"), "\"a\\\"b\\\\c\\n\\u0001\"");
    }
}
