//! Normalization that makes C++ and Rust spellings of the same thing compare
//! equal: `CommitRange` and `commit_range`, `lock_` and `self.lock`,
//! `ZX_ERR_NO_MEMORY` and `Status::NO_MEMORY`.

use regex::Regex;
use std::sync::LazyLock;

/// Converts an identifier to lower snake case and drops leading and trailing
/// underscores, so `CommitRange`, `commit_range` and `commit_range_` agree.
pub fn ident(s: &str) -> String {
    let s = s.strip_prefix("r#").unwrap_or(s);
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev = if i > 0 { chars[i - 1] } else { '_' };
            let next = chars.get(i + 1).copied().unwrap_or('_');
            let boundary = prev.is_ascii_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_ascii_uppercase() && next.is_ascii_lowercase());
            if boundary && !out.ends_with('_') && !out.is_empty() {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

/// Words of an identifier after normalization.
pub fn words(s: &str) -> Vec<String> {
    ident(s)
        .split('_')
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Calls that carry no meaning across the two languages (ownership and
/// conversion plumbing), dropped from the call lists.
const NOISE_CALLS: &[&str] = &[
    "move",
    "forward",
    "get",
    "unwrap",
    "expect",
    "as_ref",
    "as_mut",
    "as_ptr",
    "as_mut_ptr",
    "clone",
    "into",
    "from",
    "try_into",
    "try_from",
    "map_err",
    "ok_or",
    "ok_or_else",
    "ok",
    "err",
    "some",
    "none",
    "box",
    "new",
    "default",
    "deref",
    "deref_mut",
    "borrow",
    "borrow_mut",
    "unwrap_or",
    "unwrap_or_default",
    "addr_of",
    "addr_of_mut",
    "cast",
    "static_cast",
    "reinterpret_cast",
    "const_cast",
    "unsafe",
    "is_ok",
    "is_err",
    "is_error",
    "status_value",
    "take_error",
    "value",
    "error_value",
    "get_mut",
    "index",
    "index_mut",
    "as_slice",
    "as_mut_slice",
    "iter",
    "iter_mut",
    "into_iter",
];

/// Normalizes the name of a called function, method or macro. Returns `None`
/// for calls that are language plumbing rather than program logic.
pub fn call(name: &str) -> Option<String> {
    let name = name.trim().trim_end_matches('!');
    // Drop template arguments and take the last path or member segment.
    let name = strip_generics(name);
    let last = name
        .rsplit([':', '.', '>', '-'])
        .find(|s| !s.is_empty())
        .unwrap_or(&name);
    let n = ident(last);
    if n.is_empty() || NOISE_CALLS.contains(&n.as_str()) {
        return None;
    }
    Some(alias(&n).to_string())
}

/// Maps equivalent C++ and Rust spellings onto one name.
fn alias(n: &str) -> &str {
    match n {
        "debug_assert"
        | "zx_debug_assert"
        | "debug_assert_msg"
        | "zx_debug_assert_msg"
        | "assert"
        | "zx_assert"
        | "zx_assert_msg"
        | "assert_msg"
        | "debug_assert_eq"
        | "debug_assert_ne"
        | "assert_eq"
        | "assert_ne"
        | "static_assert"
        | "const_assert" => "assert",
        "printf" | "dprintf" | "println" | "print" | "kprintf" | "kprintln" | "eprintln" => "print",
        "panic" | "zx_panic" | "platform_panic_start" => "panic",
        "size" | "len" | "count" => "len",
        "empty" | "is_empty" => "is_empty",
        "min" | "ktl_min" => "min",
        "max" | "ktl_max" => "max",
        "memcpy" | "copy_from_slice" | "copy_nonoverlapping" => "memcpy",
        "memset" | "fill" | "write_bytes" => "memset",
        "push_back" | "push" => "push",
        "pop_back" | "pop" => "pop",
        "push_front" => "push_front",
        "pop_front" => "pop_front",
        "reset" | "take" => "take",
        "release" | "unlock" | "drop" => "release",
        other => other,
    }
}

fn strip_generics(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Identifiers that do not help match C++ against Rust.
const NOISE_IDENTS: &[&str] = &[
    "self", "this", "auto", "let", "mut", "const", "unsafe", "ok", "err", "some", "none",
    "nullptr", "null", "true", "false", "status", "result", "zx_ok", "ret", "rc", "res",
];

/// Normalizes an identifier for similarity purposes; `None` for noise.
pub fn ident_feature(s: &str) -> Option<String> {
    // Google-style constants: `kMaxSize` is `MAX_SIZE` in Rust.
    let mut c = s.chars();
    let s = match (c.next(), c.next()) {
        (Some('k'), Some(u)) if u.is_ascii_uppercase() => &s[1..],
        _ => s,
    };
    let n = ident(s);
    if n.is_empty() || NOISE_IDENTS.contains(&n.as_str()) || n.len() < 2 {
        return None;
    }
    Some(n)
}

static ZX_ERR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bZX_ERR_([A-Z0-9_]+)\b").unwrap());
static PATH_ERR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:[A-Za-z_]*(?:Status|Error|Err|status|error))\s*::\s*(?:ERR_)?([A-Z][A-Z0-9_]+)\b",
    )
    .unwrap()
});

/// Error codes mentioned in a piece of source, in order, normalized to the
/// bare name (`ZX_ERR_NO_MEMORY` and `Status::NO_MEMORY` both give
/// `NO_MEMORY`).
pub fn error_codes(text: &str) -> Vec<String> {
    let mut found: Vec<(usize, String)> = Vec::new();
    for c in ZX_ERR.captures_iter(text) {
        found.push((c.get(0).unwrap().start(), c[1].to_string()));
    }
    for c in PATH_ERR.captures_iter(text) {
        let code = &c[1];
        if code != "OK" {
            found.push((c.get(0).unwrap().start(), code.to_string()));
        }
    }
    found.sort_by_key(|(pos, _)| *pos);
    found.into_iter().map(|(_, c)| c).collect()
}

/// Whether the text names the success status.
pub fn mentions_ok(text: &str) -> bool {
    static OK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\bZX_OK\b|\bOk\s*\(|\bzx::ok\s*\(|\bfit::ok\s*\(|::OK\b").unwrap()
    });
    OK.is_match(text)
}

/// Normalizes the expression naming a lock to the name of the lock itself,
/// so `&lock_`, `get_lock()`, `self.lock`, `&handle_table_->lock_` and
/// `table.write_lock()` all give `lock`, and `ThreadLock::Get()` gives
/// `thread_lock`. `method` is the Rust method that acquired it, if any.
pub fn lock_key(receiver: &str, method: Option<&str>) -> String {
    if let Some(m) = method {
        if matches!(
            m,
            "read_lock" | "write_lock" | "lock_read" | "lock_write" | "read_guard" | "write_guard"
        ) {
            return "lock".to_string();
        }
    }
    const DROP: &[&str] = &[
        "this", "self", "get", "mut", "ref", "as", "deref", "borrow", "inner",
    ];
    let cleaned: String = receiver
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                ' '
            }
        })
        .collect();
    for comp in cleaned.split_whitespace().rev() {
        let w: Vec<String> = words(comp)
            .into_iter()
            .filter(|w| !DROP.contains(&w.as_str()))
            .collect();
        if !w.is_empty() {
            return w.join("_");
        }
    }
    "lock".to_string()
}

/// Normalizes comment text to a list of words. Comment markers, Rust doc
/// backticks, Zircon `|name|` references and punctuation are dropped, and
/// identifiers are normalized so `CommitRange()` matches `commit_range`.
pub fn comment_words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut l = line.trim();
        for prefix in ["///", "//!", "//", "/**", "/*!", "/*"] {
            if let Some(rest) = l.strip_prefix(prefix) {
                l = rest;
                break;
            }
        }
        let l = l.trim_end_matches("*/").trim();
        let l = l.strip_prefix('*').unwrap_or(l);
        for raw in l.split_whitespace() {
            let w = raw.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
            if w.is_empty() {
                continue;
            }
            let looks_like_ident = w.contains('_')
                || w.chars().skip(1).any(|c| c.is_ascii_uppercase())
                || raw.contains("()");
            if looks_like_ident {
                out.push(ident(w).replace('_', ""));
            } else {
                out.push(w.to_lowercase());
            }
        }
    }
    out
}

/// Length of the longest common subsequence of two sequences.
pub fn lcs_len<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    for x in a {
        for (j, y) in b.iter().enumerate() {
            cur[j + 1] = if x == y {
                prev[j] + 1
            } else {
                prev[j + 1].max(cur[j])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Similarity of two word sequences in `[0, 1]`, order-sensitive.
pub fn seq_similarity<T: PartialEq>(a: &[T], b: &[T]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    2.0 * lcs_len(a, b) as f64 / (a.len() + b.len()) as f64
}

/// Jaccard similarity of two lists treated as sets, in `[0, 1]`.
pub fn jaccard(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut sa: Vec<&String> = a.iter().collect();
    sa.sort();
    sa.dedup();
    let mut union: Vec<&String> = a.iter().chain(b.iter()).collect();
    union.sort();
    union.dedup();
    let inter = sa.iter().filter(|x| b.contains(x)).count();
    inter as f64 / union.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idents() {
        assert_eq!(ident("CommitRange"), "commit_range");
        assert_eq!(ident("commit_range_"), "commit_range");
        assert_eq!(ident("HTTPServer"), "http_server");
        assert_eq!(ident("VmObjectPaged"), "vm_object_paged");
        assert_eq!(ident("kMaxSize"), "k_max_size");
    }

    #[test]
    fn calls() {
        assert_eq!(
            call("vmo->CommitRangeLocked").as_deref(),
            Some("commit_range_locked")
        );
        assert_eq!(
            call("self.commit_range_locked").as_deref(),
            Some("commit_range_locked")
        );
        assert_eq!(call("ktl::move"), None);
        assert_eq!(call("DEBUG_ASSERT").as_deref(), Some("assert"));
        assert_eq!(call("debug_assert!").as_deref(), Some("assert"));
        assert_eq!(
            call("fbl::AdoptRef<VmObject>").as_deref(),
            Some("adopt_ref")
        );
    }

    #[test]
    fn errors() {
        assert_eq!(
            error_codes("return ZX_ERR_INVALID_ARGS;"),
            vec!["INVALID_ARGS"]
        );
        assert_eq!(
            error_codes("return Err(Status::NO_MEMORY);"),
            vec!["NO_MEMORY"]
        );
        assert_eq!(
            error_codes("Err(zx::Status::OUT_OF_RANGE)"),
            vec!["OUT_OF_RANGE"]
        );
        assert_eq!(
            error_codes("Err(ZxError::ERR_BAD_STATE)"),
            vec!["BAD_STATE"]
        );
        assert!(error_codes("Ok(())").is_empty());
    }

    #[test]
    fn locks() {
        assert_eq!(lock_key("&lock_", None), "lock");
        assert_eq!(lock_key("self.lock", None), "lock");
        assert_eq!(lock_key("get_lock()", None), "lock");
        assert_eq!(lock_key("ThreadLock::Get()", None), "thread_lock");
        assert_eq!(lock_key("&self.page_lock", None), "page_lock");
        assert_eq!(lock_key("&handle_table_->lock_", None), "lock");
        assert_eq!(lock_key("up->handle_table().get_lock()", None), "lock");
        assert_eq!(lock_key("table", Some("write_lock")), "lock");
    }

    #[test]
    fn comments() {
        assert_eq!(
            comment_words("// Call |CommitRange()| with the lock held."),
            comment_words("/// Call `commit_range` with the lock held.")
        );
    }
}
