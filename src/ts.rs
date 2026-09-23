//! Thin helpers over tree-sitter shared by the C++ and Rust extractors.

use crate::model::{Features, Lang, Ret, Unit, UnitKind};
use crate::normalize;
use regex::Regex;
use std::sync::LazyLock;
use tree_sitter::{Node, Parser, Tree};

pub fn parse(lang: Lang, src: &str) -> Tree {
    let mut parser = Parser::new();
    let language = match lang {
        Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
    };
    parser
        .set_language(&language)
        .expect("tree-sitter grammar version mismatch");
    parser
        .parse(src, None)
        .expect("tree-sitter parse cancelled")
}

pub fn text<'a>(n: Node, src: &'a [u8]) -> &'a str {
    n.utf8_text(src).unwrap_or("")
}

pub fn named_children<'t>(n: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = n.walk();
    n.named_children(&mut cursor).collect()
}

pub fn children<'t>(n: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = n.walk();
    n.children(&mut cursor).collect()
}

/// 1-based first line of a node.
pub fn line(n: Node) -> usize {
    n.start_position().row + 1
}

/// 1-based last line of a node.
pub fn end_line(n: Node) -> usize {
    let end = n.end_position();
    // A node ending at column 0 ends on the previous line.
    if end.column == 0 && end.row > n.start_position().row {
        end.row
    } else {
        end.row + 1
    }
}

pub fn is_comment(n: Node) -> bool {
    matches!(n.kind(), "comment" | "line_comment" | "block_comment")
}

/// Source text of `n` with the byte ranges of `skip` nodes blanked out.
pub fn text_without(n: Node, src: &[u8], skip: &[Node]) -> String {
    let start = n.start_byte();
    let mut bytes = src[start..n.end_byte()].to_vec();
    for s in skip {
        let (a, b) = (s.start_byte().max(start), s.end_byte().min(n.end_byte()));
        for byte in bytes
            .iter_mut()
            .take(b.saturating_sub(start))
            .skip(a.saturating_sub(start))
        {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Accumulates features while walking a syntax subtree.
#[derive(Default)]
pub struct FeatureAcc {
    pub calls: Vec<String>,
    pub idents: Vec<String>,
    pub text: String,
    pub propagates: bool,
    /// Names of called functions as written, so they are not also counted
    /// as identifiers.
    pub callees: Vec<String>,
    /// Calls through a type path, as `type::method` (`Foo::create`).
    pub qcalls: Vec<String>,
}

impl FeatureAcc {
    pub fn call(&mut self, name: &str) {
        let last = name.rsplit(['.', ':', '>']).next().unwrap_or(name);
        self.callees
            .push(normalize::ident(last.trim_end_matches('!')));
        if let Some(c) = normalize::call(name) {
            self.calls.push(c);
        }
        if let Some(q) = normalize::qualified_call(name) {
            self.qcalls.push(q);
        }
    }

    pub fn ident(&mut self, name: &str) {
        if let Some(i) = normalize::ident_feature(name) {
            self.idents.push(i);
        }
    }

    pub fn finish(self, lang: Lang) -> Features {
        let mut idents = self.idents;
        idents.sort();
        idents.dedup();
        // Called names are compared as calls, not identifiers.
        let calls = &self.calls;
        let callees = &self.callees;
        idents.retain(|i| {
            !calls.contains(i)
                && !callees.contains(i)
                && normalize::call(i).is_none_or(|c| !calls.contains(&c))
        });
        let errors = normalize::error_codes(&self.text);
        let locks = match lang {
            Lang::Cpp => cpp_locks(&self.text),
            Lang::Rust => rust_locks(&self.text),
        };
        let mut calls = self.calls;
        if !locks.is_empty() {
            // Acquisitions are compared as locks, not calls.
            const LOCK_CALLS: &[&str] = &[
                "lock",
                "read_lock",
                "write_lock",
                "lock_read",
                "lock_write",
                "lock_irqsave",
                "lock_irq",
                "try_lock",
                "acquire",
                "acquire_irq_save",
            ];
            calls.retain(|c| !LOCK_CALLS.contains(&c.as_str()) && !c.contains("lock"));
            idents.retain(|i| !i.contains("lock"));
        }
        // Names are compared without underscores, so a C++ enumerator
        // `NEEDACK` matches the Rust variant `NeedAck`.
        let mut names: Vec<String> = calls
            .iter()
            .chain(idents.iter())
            .map(|n| {
                let n = n.strip_prefix("set_").unwrap_or(n);
                n.strip_prefix("get_").unwrap_or(n).replace('_', "")
            })
            .collect();
        let idents: Vec<String> = idents.iter().map(|i| i.replace('_', "")).collect();
        names.sort();
        names.dedup();
        let asserts = calls.iter().any(|c| c == "assert");
        let unlocks = calls.iter().any(|c| c == "release")
            && (self.text.contains("guard") || self.text.contains("lock"));
        Features {
            calls,
            idents,
            names,
            errors,
            locks,
            unlocks,
            propagates: self.propagates,
            asserts,
            checks_error: false,
            ret: None,
            comment: Vec::new(),
            safety: false,
            lock_plumbing: false,
            plumbing: false,
            conjuncts: Vec::new(),
            qcalls: self.qcalls,
        }
    }
}

static CPP_GUARD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(\w*Guard|AutoLock|AutoSpinLock\w*|lock_guard|unique_lock|scoped_lock|shared_lock)\s*(<(?:[^<>]|<[^<>]*>)*>)?\s+\w+\s*(?:[{(]\s*([^;]*?)\s*[})])?\s*;",
    )
    .unwrap()
});
static CPP_LOCK_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([A-Za-z_][\w]*(?:\s*(?:\.|->|::)\s*[A-Za-z_]\w*(?:\(\))?)*)\s*(?:\.|->)\s*(Acquire|AcquireIrqSave|Lock|ReadLock|WriteLock|lock|lock_shared)\s*\(")
        .unwrap()
});
static RUST_LOCK_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([A-Za-z_][\w]*(?:\s*(?:\.|::)\s*[A-Za-z_]\w*(?:\(\))?)*)\s*\.\s*(lock|lock_irqsave|lock_irq|try_lock|read_lock|write_lock|lock_read|lock_write|acquire|lock_[a-z]\w*)\s*\(")
        .unwrap()
});
static RUST_GUARD_NEW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(\w*Guard)\s*(?:::\s*<[^>]*>)?\s*::\s*new\s*\(\s*([^,)]*)").unwrap()
});

fn cpp_locks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for c in CPP_GUARD.captures_iter(text) {
        let ty = &c[1];
        let targs = c.get(2).map_or("", |m| m.as_str());
        let arg = c.get(3).map_or("", |m| m.as_str());
        let arg = arg.split(',').next().unwrap_or("").trim();
        let name = if arg.is_empty() {
            guard_type_name(ty)
        } else {
            normalize::lock_key(arg, None)
        };
        let mode = if targs.contains("Reader") || targs.contains("Shared") || ty == "shared_lock" {
            " (read)"
        } else if targs.contains("Writer") {
            " (write)"
        } else {
            ""
        };
        out.push(format!("{name}{mode}"));
    }
    for c in CPP_LOCK_CALL.captures_iter(text) {
        let method = &c[2];
        let mode = match method {
            "ReadLock" | "lock_shared" => " (read)",
            "WriteLock" => " (write)",
            _ => "",
        };
        out.push(format!("{}{mode}", normalize::lock_key(&c[1], None)));
    }
    out
}

fn rust_locks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for c in RUST_LOCK_CALL.captures_iter(text) {
        let method = &c[2];
        let mode = match method {
            "read_lock" | "lock_read" => " (read)",
            "write_lock" | "lock_write" => " (write)",
            _ => "",
        };
        out.push(format!(
            "{}{mode}",
            normalize::lock_key(&c[1], Some(method))
        ));
    }
    for c in RUST_GUARD_NEW.captures_iter(text) {
        let arg = c[2].trim();
        let name = if arg.is_empty() {
            guard_type_name(&c[1])
        } else {
            normalize::lock_key(arg, None)
        };
        out.push(name);
    }
    out
}

/// `InterruptDisableGuard` -> `interrupt_disable`.
fn guard_type_name(ty: &str) -> String {
    let n = normalize::ident(ty);
    let n = n.trim_end_matches("guard").trim_end_matches('_');
    if n.is_empty() {
        "guard".to_string()
    } else {
        n.to_string()
    }
}

static STATUS_VAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\(?\s*(?:[a-z_]*status|st|rc|ret|res|result|err|e|error|r)\s*\)?$|take_error\s*\(|error_value\s*\(|status_value\s*\(|^(?:zx::error|fit::error|zx::error_result|Err)\s*\(\s*[a-z_]+\s*\)$")
        .unwrap()
});

/// Classifies the expression of a return statement or tail expression.
pub fn classify_return(expr: &str) -> Ret {
    let expr = expr.trim();
    let errors = normalize::error_codes(expr);
    if let Some(e) = errors.into_iter().next() {
        Ret::Error(e)
    } else if normalize::mentions_ok(expr) {
        Ret::Ok
    } else if STATUS_VAR.is_match(expr) {
        Ret::Status
    } else {
        Ret::Value
    }
}

/// Builds the flat list of units for a function body.
pub struct UnitBuilder {
    pub units: Vec<Unit>,
}

impl UnitBuilder {
    pub fn new() -> Self {
        UnitBuilder { units: Vec::new() }
    }

    pub fn push(
        &mut self,
        kind: UnitKind,
        start: usize,
        end: usize,
        depth: usize,
        features: Features,
    ) {
        self.units.push(Unit {
            kind,
            start_line: start,
            end_line: end.max(start),
            depth,
            features,
            file: None,
            ext_lines: Vec::new(),
        });
    }

    /// Adds a comment, merging it into the previous comment unit when both
    /// are line comments on adjacent lines at the same depth.
    pub fn comment(&mut self, raw: &str, start: usize, end: usize, depth: usize) {
        let words = normalize::comment_words(raw);
        if words.is_empty() || words == ["static"] {
            // Empty comments and Zircon's `// static` marker carry no meaning,
            // but a blank `///` line doesn't end a `# Safety` section.
            if let Some(last) = self.units.last_mut() {
                if words.is_empty()
                    && last.kind == UnitKind::Comment
                    && last.features.safety
                    && last.depth == depth
                    && last.end_line + 1 == start
                    && last.file.is_none()
                {
                    last.end_line = end;
                }
            }
            return;
        }
        let is_line = raw.trim_start().starts_with("//");
        let body = raw
            .trim_start()
            .trim_start_matches(['/', '!', '*'])
            .trim_start();
        // Safety comments start their own unit; lines that follow join it.
        let starts_safety = body.starts_with("SAFETY:") || body.starts_with("# Safety");
        if let Some(last) = self.units.last_mut() {
            if is_line
                && (!starts_safety || last.features.safety)
                && last.kind == UnitKind::Comment
                && last.depth == depth
                && last.end_line + 1 == start
                && last.file.is_none()
            {
                last.end_line = end;
                last.features.comment.extend(words);
                return;
            }
        }
        let features = Features {
            comment: words,
            safety: starts_safety,
            ..Features::default()
        };
        self.push(UnitKind::Comment, start, end, depth, features);
    }
}

impl Default for UnitBuilder {
    fn default() -> Self {
        Self::new()
    }
}
