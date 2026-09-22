//! A small parser for unified diffs as produced by `git diff`,
//! `git format-patch` and `git show`.

use std::collections::BTreeSet;

/// One side of a hunk line.
#[derive(Clone, Debug)]
pub struct FilePatch {
    /// Path before the change; `None` for added files.
    pub old_path: Option<String>,
    /// Path after the change; `None` for deleted files.
    pub new_path: Option<String>,
    /// Abbreviated blob ids from the `index` line, if present.
    pub old_blob: Option<String>,
    pub new_blob: Option<String>,
    /// Old-side lines: (line number, text, removed?).
    pub old_lines: Vec<(usize, String, bool)>,
    /// New-side lines: (line number, text, added?).
    pub new_lines: Vec<(usize, String, bool)>,
}

impl FilePatch {
    pub fn removed(&self) -> BTreeSet<usize> {
        self.old_lines.iter().filter(|l| l.2).map(|l| l.0).collect()
    }
    pub fn added(&self) -> BTreeSet<usize> {
        self.new_lines.iter().filter(|l| l.2).map(|l| l.0).collect()
    }
    /// The old file reconstructed from the hunks, with lines the diff does
    /// not show left blank so that line numbers stay true.
    pub fn sparse_old(&self) -> String {
        sparse(&self.old_lines)
    }
    pub fn sparse_new(&self) -> String {
        sparse(&self.new_lines)
    }
    /// The path to display: the new path, or the old one for deletions.
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("")
    }
}

fn sparse(lines: &[(usize, String, bool)]) -> String {
    let max = lines.iter().map(|l| l.0).max().unwrap_or(0);
    let mut out = vec![""; max];
    for (n, t, _) in lines {
        out[n - 1] = t;
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

fn strip_prefix(p: &str) -> Option<String> {
    let p = p.trim_end_matches('\t').trim();
    let p = p.split('\t').next().unwrap_or(p);
    if p == "/dev/null" {
        return None;
    }
    let p = p
        .strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p);
    Some(p.to_string())
}

/// Parses every file diff in `text`.
pub fn parse(text: &str) -> Vec<FilePatch> {
    let mut files: Vec<FilePatch> = Vec::new();
    let mut cur: Option<FilePatch> = None;
    let (mut old_n, mut new_n, mut old_left, mut new_left) = (0usize, 0usize, 0usize, 0usize);
    let new_file = || FilePatch {
        old_path: None,
        new_path: None,
        old_blob: None,
        new_blob: None,
        old_lines: Vec::new(),
        new_lines: Vec::new(),
    };
    for line in text.lines() {
        if old_left > 0 || new_left > 0 {
            if let Some(f) = cur.as_mut() {
                let (tag, rest) = line.split_at(line.len().min(1));
                match tag {
                    " " | "" => {
                        f.old_lines.push((old_n, rest.to_string(), false));
                        f.new_lines.push((new_n, rest.to_string(), false));
                        old_n += 1;
                        new_n += 1;
                        old_left = old_left.saturating_sub(1);
                        new_left = new_left.saturating_sub(1);
                    }
                    "-" => {
                        f.old_lines.push((old_n, rest.to_string(), true));
                        old_n += 1;
                        old_left = old_left.saturating_sub(1);
                    }
                    "+" => {
                        f.new_lines.push((new_n, rest.to_string(), true));
                        new_n += 1;
                        new_left = new_left.saturating_sub(1);
                    }
                    _ => {} // "\ No newline at end of file"
                }
                continue;
            }
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(f) = cur.take() {
                files.push(f);
            }
            let mut f = new_file();
            // Best effort; `---`/`+++` lines override these.
            if let Some((a, b)) = rest.split_once(" b/") {
                f.old_path = strip_prefix(a);
                f.new_path = Some(b.to_string());
            }
            cur = Some(f);
        } else if let Some(rest) = line.strip_prefix("--- ") {
            if cur.is_none() {
                cur = Some(new_file());
            }
            if let Some(f) = cur.as_mut() {
                f.old_path = strip_prefix(rest);
            }
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            if let Some(f) = cur.as_mut() {
                f.new_path = strip_prefix(rest);
            }
        } else if let Some(rest) = line.strip_prefix("new file mode") {
            let _ = rest;
            if let Some(f) = cur.as_mut() {
                f.old_path = None;
            }
        } else if line.starts_with("deleted file mode") {
            if let Some(f) = cur.as_mut() {
                f.new_path = None;
            }
        } else if let Some(rest) = line.strip_prefix("index ") {
            if let Some(f) = cur.as_mut() {
                let ids = rest.split_whitespace().next().unwrap_or("");
                if let Some((a, b)) = ids.split_once("..") {
                    f.old_blob = Some(a.to_string()).filter(|s| !s.trim_matches('0').is_empty());
                    f.new_blob = Some(b.to_string()).filter(|s| !s.trim_matches('0').is_empty());
                }
            }
        } else if let Some(rest) = line.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@
            let mut parts = rest.split_whitespace();
            let old = parts.next().unwrap_or("-0").trim_start_matches('-');
            let new = parts.next().unwrap_or("+0").trim_start_matches('+');
            let range = |s: &str| -> (usize, usize) {
                let (a, b) = s.split_once(',').unwrap_or((s, "1"));
                (a.parse().unwrap_or(0), b.parse().unwrap_or(1))
            };
            let (os, oc) = range(old);
            let (ns, nc) = range(new);
            old_n = os.max(1);
            new_n = ns.max(1);
            if oc == 0 {
                old_n = os + 1;
            }
            if nc == 0 {
                new_n = ns + 1;
            }
            old_left = oc;
            new_left = nc;
        }
    }
    if let Some(f) = cur.take() {
        files.push(f);
    }
    files.retain(|f| f.old_path.is_some() || f.new_path.is_some());
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hunks() {
        let p = "diff --git a/x.cc b/x.cc\nindex 111..222 100644\n--- a/x.cc\n+++ b/x.cc\n@@ -2,3 +2,2 @@ ctx\n a\n-b\n+c\n+d\n-e\n";
        let f = &parse(p)[0];
        assert_eq!(f.old_path.as_deref(), Some("x.cc"));
        assert_eq!(f.removed().into_iter().collect::<Vec<_>>(), vec![3, 4]);
        assert_eq!(f.added().into_iter().collect::<Vec<_>>(), vec![3, 4]);
        assert_eq!(f.sparse_old(), "\na\nb\ne\n");
    }

    #[test]
    fn new_and_deleted_files() {
        let p = "diff --git a/y.rs b/y.rs\nnew file mode 100644\nindex 0000000..abc\n--- /dev/null\n+++ b/y.rs\n@@ -0,0 +1,2 @@\n+fn a() {}\n+fn b() {}\ndiff --git a/z.cc b/z.cc\ndeleted file mode 100644\n--- a/z.cc\n+++ /dev/null\n@@ -1 +0,0 @@\n-int x;\n";
        let fs = parse(p);
        assert_eq!(fs.len(), 2);
        assert_eq!(fs[0].old_path, None);
        assert_eq!(fs[0].added().len(), 2);
        assert_eq!(fs[1].new_path, None);
        assert_eq!(fs[1].removed().len(), 1);
    }
}
