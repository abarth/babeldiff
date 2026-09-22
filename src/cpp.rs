//! Extracts functions and their units from C++ source.

use crate::model::{Features, Function, Lang, Ret, Unit, UnitKind};
use crate::ts::{self, FeatureAcc, UnitBuilder};
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;
use tree_sitter::Node;

/// Comments attached to a function declaration (typically in a class body
/// in a header), keyed by `(class, base name)`.
pub type DeclComments = HashMap<(String, String), Vec<Unit>>;

/// Everything extracted from one C++ file.
pub struct CppFile {
    pub functions: Vec<Function>,
    pub decl_comments: DeclComments,
}

pub fn extract(path: &str, src: &str) -> CppFile {
    let tree = ts::parse(Lang::Cpp, src);
    let lines = crate::extract::split_lines(src);
    let mut out = CppFile {
        functions: Vec::new(),
        decl_comments: HashMap::new(),
    };
    let ctx = Ctx {
        path,
        src: src.as_bytes(),
        lines: &lines,
    };
    ctx.walk_scope(tree.root_node(), &[], &mut out);
    out
}

struct Ctx<'a> {
    path: &'a str,
    src: &'a [u8],
    lines: &'a [String],
}

impl<'a> Ctx<'a> {
    fn text(&self, n: Node) -> &'a str {
        ts::text(n, self.src)
    }

    fn walk_scope(&self, scope: Node, class: &[String], out: &mut CppFile) {
        for child in ts::named_children(scope) {
            self.walk_item(child, child, class, out);
        }
    }

    /// `outer` is the node whose preceding siblings hold leading comments
    /// (a template declaration wraps the function definition).
    fn walk_item(&self, n: Node, outer: Node, class: &[String], out: &mut CppFile) {
        match n.kind() {
            "function_definition" => {
                if let Some(f) = self.function(n, outer, class) {
                    out.functions.push(f);
                }
            }
            "template_declaration" => {
                for c in ts::named_children(n) {
                    if matches!(
                        c.kind(),
                        "function_definition"
                            | "declaration"
                            | "class_specifier"
                            | "struct_specifier"
                            | "template_declaration"
                            | "field_declaration"
                    ) {
                        self.walk_item(c, outer, class, out);
                    }
                }
            }
            "namespace_definition" | "linkage_specification" => {
                if let Some(body) = n.child_by_field_name("body") {
                    self.walk_scope(body, class, out);
                }
            }
            "declaration_list" | "preproc_if" | "preproc_ifdef" | "preproc_else"
            | "preproc_elif" => self.walk_scope(n, class, out),
            "class_specifier" | "struct_specifier" | "union_specifier" => {
                self.class(n, class, out);
            }
            "declaration" | "field_declaration" => {
                // Class definitions inside declarations: `class Foo { ... } foo;`
                if let Some(ty) = n.child_by_field_name("type") {
                    if matches!(ty.kind(), "class_specifier" | "struct_specifier") {
                        self.class(ty, class, out);
                    }
                }
                self.declaration(n, outer, class, out);
            }
            _ => {}
        }
    }

    fn class(&self, n: Node, class: &[String], out: &mut CppFile) {
        let (Some(name), Some(body)) =
            (n.child_by_field_name("name"), n.child_by_field_name("body"))
        else {
            return;
        };
        let mut path = class.to_vec();
        path.push(self.text(name).to_string());
        self.walk_scope(body, &path, out);
    }

    /// Records leading comments of a function declaration so they can be
    /// compared with the Rust doc comments of the corresponding function.
    fn declaration(&self, n: Node, outer: Node, class: &[String], out: &mut CppFile) {
        let Some(decl) = n.child_by_field_name("declarator") else {
            return;
        };
        let Some(fdecl) = find_function_declarator(decl) else {
            return;
        };
        let Some(name) = fdecl.child_by_field_name("declarator") else {
            return;
        };
        let (cls, base) = split_name(self.text(name), class);
        let mut b = UnitBuilder::new();
        let _ = self.leading_comments(outer, &mut b);
        if b.units.is_empty() {
            return;
        }
        for u in &mut b.units {
            u.file = Some(self.path.to_string());
            u.ext_lines = (u.start_line..=u.end_line)
                .map(|l| self.lines.get(l - 1).cloned().unwrap_or_default())
                .collect();
        }
        out.decl_comments
            .entry((cls.unwrap_or_default(), base))
            .or_insert(b.units);
    }

    /// Adds comment units for the comments directly above `outer` and returns
    /// the first line they occupy.
    fn leading_comments(&self, outer: Node, b: &mut UnitBuilder) -> usize {
        let mut comments = Vec::new();
        let mut first = ts::line(outer);
        let mut cur = outer.prev_sibling();
        while let Some(p) = cur {
            if !ts::is_comment(p) || ts::end_line(p) + 1 < first {
                break;
            }
            // A trailing comment on the previous item's line belongs to it.
            if let Some(pp) = p.prev_sibling() {
                if ts::end_line(pp) == ts::line(p) && !ts::is_comment(pp) {
                    break;
                }
            }
            first = ts::line(p);
            comments.push(p);
            cur = p.prev_sibling();
        }
        for c in comments.iter().rev() {
            b.comment(self.text(*c), ts::line(*c), ts::end_line(*c), 0);
        }
        first
    }

    fn function(&self, n: Node, outer: Node, class: &[String]) -> Option<Function> {
        let decl = n.child_by_field_name("declarator")?;
        let fdecl = find_function_declarator(decl)?;
        let name_node = fdecl.child_by_field_name("declarator")?;
        let (cls, base) = split_name(self.text(name_node), class);
        let body = n.child_by_field_name("body")?;

        let mut b = UnitBuilder::new();
        let start = self.leading_comments(outer, &mut b);

        // Signature: from the start of the definition to the opening brace.
        let mut acc = FeatureAcc::default();
        acc.ident(&base);
        if let Some(params) = fdecl.child_by_field_name("parameters") {
            self.collect(params, &mut acc, &[]);
        }
        acc.text = String::new();
        let sig = acc.finish(Lang::Cpp);
        b.push(UnitKind::Signature, ts::line(outer), ts::line(body), 0, sig);

        if body.kind() == "compound_statement" {
            self.block(body, 1, &mut b);
        }
        // `return Foo();` in a function returning a status passes Foo's status
        // on, which Rust writes `foo()?; Ok(())`.
        let returns_status = n
            .child_by_field_name("type")
            .is_some_and(|t| self.text(t) == "zx_status_t");
        if returns_status {
            for u in &mut b.units {
                if u.kind == UnitKind::Return
                    && u.features.ret == Some(Ret::Value)
                    && !u.features.calls.is_empty()
                {
                    u.features.ret = Some(Ret::Status);
                }
            }
        }

        let end = ts::end_line(n);
        let mut calls: Vec<String> = b
            .units
            .iter()
            .flat_map(|u| u.features.calls.clone())
            .collect();
        calls.sort();
        calls.dedup();
        let name = match &cls {
            Some(c) => format!("{c}::{base}"),
            None => base.clone(),
        };
        Some(Function {
            lang: Lang::Cpp,
            path: self.path.to_string(),
            name,
            base,
            class: cls,
            start_line: start,
            end_line: end,
            lines: self.lines[start - 1..end.min(self.lines.len())].to_vec(),
            units: b.units,
            calls,
            is_ffi: false,
        })
    }

    /// Collects call and identifier features from a subtree, skipping the
    /// subtrees in `skip`.
    fn collect(&self, n: Node, acc: &mut FeatureAcc, skip: &[Node]) {
        if skip.iter().any(|s| s.id() == n.id()) {
            return;
        }
        match n.kind() {
            "call_expression" => {
                if let Some(f) = n.child_by_field_name("function") {
                    let name = match f.kind() {
                        "field_expression" => f.child_by_field_name("field").map(|x| self.text(x)),
                        "template_function" => f.child_by_field_name("name").map(|x| self.text(x)),
                        _ => Some(self.text(f)),
                    };
                    if let Some(name) = name {
                        acc.call(name);
                    }
                }
            }
            "identifier" | "field_identifier" => acc.ident(self.text(n)),
            "declaration" => {
                // `Foo foo{args};` constructs a Foo, like Rust `Foo::new(args)`.
                let ty = n.child_by_field_name("type");
                let ctor = n.child_by_field_name("declarator").is_some_and(|d| {
                    d.kind() == "init_declarator"
                        && d.child_by_field_name("value").is_some_and(|v| {
                            matches!(v.kind(), "initializer_list" | "argument_list")
                                && ts::named_children(v).iter().any(|a| !ts::is_comment(*a))
                        })
                });
                if let Some(ty) = ty.filter(|t| {
                    ctor && matches!(t.kind(), "type_identifier" | "qualified_identifier")
                }) {
                    acc.call(self.text(ty));
                }
            }
            _ => {}
        }
        for c in ts::named_children(n) {
            self.collect(c, acc, skip);
        }
    }

    /// Features of `n`, excluding nested bodies in `skip`.
    fn features(&self, n: Node, skip: &[Node]) -> Features {
        let mut acc = FeatureAcc::default();
        self.collect(n, &mut acc, skip);
        acc.text = ts::text_without(n, self.src, skip);
        acc.finish(Lang::Cpp)
    }

    fn block(&self, n: Node, depth: usize, b: &mut UnitBuilder) {
        for c in ts::named_children(n) {
            self.statement(c, depth, b);
        }
    }

    fn statement(&self, n: Node, depth: usize, b: &mut UnitBuilder) {
        let line = ts::line(n);
        match n.kind() {
            "comment" => b.comment(self.text(n), line, ts::end_line(n), depth),
            "compound_statement" => self.block(n, depth + 1, b),
            "attributed_statement" => {
                // `[[likely]] { ... }` and friends: the attribute doesn't matter.
                for c in ts::named_children(n) {
                    if c.kind() != "attribute_declaration" {
                        self.statement(c, depth, b);
                    }
                }
            }
            "preproc_if" | "preproc_ifdef" | "preproc_else" | "preproc_elif" => {
                for c in ts::named_children(n) {
                    if !matches!(
                        c.kind(),
                        "identifier" | "preproc_defined" | "binary_expression"
                    ) {
                        self.statement(c, depth, b);
                    }
                }
            }
            "if_statement" => self.if_statement(n, depth, false, b),
            "for_statement" | "for_range_loop" | "while_statement" | "do_statement" => {
                let body = n.child_by_field_name("body");
                let header_end = body.map_or(ts::end_line(n), |bd| ts::line(bd));
                let skip: Vec<Node> = body.into_iter().collect();
                let f = self.features(n, &skip);
                b.push(UnitKind::Loop, line, header_end, depth, f);
                if let Some(bd) = body {
                    self.body(bd, depth + 1, b);
                }
            }
            "switch_statement" => {
                let body = n.child_by_field_name("body");
                let header_end = body.map_or(ts::end_line(n), |bd| ts::line(bd));
                let skip: Vec<Node> = body.into_iter().collect();
                let f = self.features(n, &skip);
                b.push(UnitKind::Switch, line, header_end, depth, f);
                if let Some(bd) = body {
                    for c in ts::named_children(bd) {
                        if c.kind() == "case_statement" {
                            self.case(c, depth + 1, b);
                        } else {
                            self.statement(c, depth + 1, b);
                        }
                    }
                }
            }
            "case_statement" => self.case(n, depth, b),
            "return_statement" => self.return_statement(n, depth, b),
            "break_statement" => b.push(
                UnitKind::Break,
                line,
                ts::end_line(n),
                depth,
                Features::default(),
            ),
            "continue_statement" => b.push(
                UnitKind::Continue,
                line,
                ts::end_line(n),
                depth,
                Features::default(),
            ),
            "goto_statement" => {
                let f = self.features(n, &[]);
                b.push(UnitKind::Goto, line, ts::end_line(n), depth, f)
            }
            "labeled_statement" => {
                let label = n.child_by_field_name("label");
                let mut f = Features::default();
                if let Some(l) = label {
                    f.idents.push(crate::normalize::ident(self.text(l)));
                }
                b.push(UnitKind::Label, line, line, depth, f);
                for c in ts::named_children(n) {
                    if c.kind() != "statement_identifier" {
                        self.statement(c, depth, b);
                    }
                }
            }
            _ => self.plain(n, depth, b),
        }
    }

    /// A statement or declaration. Lambda bodies spanning several lines are
    /// split out so their statements align individually.
    fn plain(&self, n: Node, depth: usize, b: &mut UnitBuilder) {
        let mut lambdas = Vec::new();
        find_bodies(n, &mut lambdas);
        let lambdas: Vec<Node> = lambdas
            .into_iter()
            .filter(|l| ts::end_line(*l) > ts::line(*l))
            .collect();
        let f = self.features(n, &lambdas);
        let end = lambdas.first().map_or(ts::end_line(n), |l| ts::line(*l));
        b.push(UnitKind::Stmt, ts::line(n), end, depth, f);
        for l in lambdas {
            self.block(l, depth + 1, b);
        }
    }

    fn body(&self, n: Node, depth: usize, b: &mut UnitBuilder) {
        if n.kind() == "attributed_statement" {
            for c in ts::named_children(n) {
                if c.kind() != "attribute_declaration" {
                    self.body(c, depth, b);
                }
            }
        } else if n.kind() == "compound_statement" {
            self.block(n, depth, b);
        } else {
            self.statement(n, depth, b);
        }
    }

    fn case(&self, n: Node, depth: usize, b: &mut UnitBuilder) {
        let value = n.child_by_field_name("value");
        let mut f = match value {
            Some(v) => self.features(v, &[]),
            None => Features::default(),
        };
        if value.is_none() {
            f.idents.push("default".to_string());
        }
        let mut stmts: Vec<Node> = ts::named_children(n);
        if let Some(v) = value {
            stmts.retain(|s| s.id() != v.id());
        }
        drop_trailing_break(&mut stmts);
        // `case A: case B:` falls through, which Rust writes as `A | B =>`.
        if let Some(last) = b.units.last_mut() {
            if last.kind == UnitKind::Case
                && last.depth == depth
                && last.end_line + 1 == ts::line(n)
            {
                last.end_line = ts::line(n);
                last.features.idents.extend(f.idents);
                last.features.errors.extend(f.errors);
                for s in stmts {
                    self.case_body(s, depth, b);
                }
                return;
            }
        }
        b.push(UnitKind::Case, ts::line(n), ts::line(n), depth, f);
        drop_trailing_break(&mut stmts);
        for s in stmts {
            self.case_body(s, depth, b);
        }
    }

    fn case_body(&self, s: Node, depth: usize, b: &mut UnitBuilder) {
        if s.kind() == "compound_statement" {
            let mut stmts = ts::named_children(s);
            drop_trailing_break(&mut stmts);
            for c in stmts {
                self.statement(c, depth + 1, b);
            }
        } else {
            self.statement(s, depth + 1, b);
        }
    }

    fn return_statement(&self, n: Node, depth: usize, b: &mut UnitBuilder) {
        let expr = ts::named_children(n)
            .into_iter()
            .find(|c| !ts::is_comment(*c));
        let (line, end) = (ts::line(n), ts::end_line(n));
        // `return c ? a : b;` reads as `if c { return a } else { return b }`,
        // which is how Rust spells it.
        if let Some(cond) = expr
            .map(strip_parens)
            .filter(|e| e.kind() == "conditional_expression")
        {
            if let (Some(c), Some(t), Some(e)) = (
                cond.child_by_field_name("condition"),
                cond.child_by_field_name("consequence"),
                cond.child_by_field_name("alternative"),
            ) {
                b.push(UnitKind::If, line, end, depth, self.features(c, &[]));
                self.return_unit(Some(t), line, end, depth + 1, b);
                b.push(UnitKind::Else, line, end, depth, Features::default());
                self.return_unit(Some(e), line, end, depth + 1, b);
                return;
            }
        }
        self.return_unit(expr, line, end, depth, b);
    }

    fn return_unit(
        &self,
        expr: Option<Node>,
        line: usize,
        end: usize,
        depth: usize,
        b: &mut UnitBuilder,
    ) {
        let mut f = match expr {
            Some(e) => self.features(e, &[]),
            None => Features::default(),
        };
        f.ret = Some(match expr {
            Some(e) => ts::classify_return(self.text(e)),
            None => Ret::Value,
        });
        // `*actual = n; return ZX_OK;` is how C++ returns a value with a
        // status; Rust returns `Ok(n)`.
        static OUT_PARAM: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^\*\s*\w+\s*=[^=]").unwrap());
        let mut line = line;
        if f.ret == Some(Ret::Ok) {
            if let Some(prev) = b.units.last() {
                let text = self.lines.get(prev.start_line - 1).map_or("", |l| l.trim());
                if prev.kind == UnitKind::Stmt
                    && prev.depth == depth
                    && prev.file.is_none()
                    && OUT_PARAM.is_match(text)
                {
                    let prev = b.units.pop().unwrap();
                    line = prev.start_line;
                    f.calls.extend(prev.features.calls);
                    f.idents.extend(prev.features.idents);
                    f.names.extend(prev.features.names);
                }
            }
        }
        b.push(UnitKind::Return, line, end, depth, f);
    }

    fn if_statement(&self, n: Node, depth: usize, is_else_if: bool, b: &mut UnitBuilder) {
        let line = ts::line(n);
        let cond = n.child_by_field_name("condition");
        let cons = n.child_by_field_name("consequence");
        let alt = n.child_by_field_name("alternative");

        // `if (status != ZX_OK) { return status; }` is how C++ spells `?`.
        if let (Some(c), Some(t), None) = (cond, cons, alt) {
            if self.is_propagation(c, t) {
                if is_else_if {
                    b.push(UnitKind::Else, line, line, depth - 1, Features::default());
                }
                let mut f = self.features(c, &[]);
                f.propagates = true;
                let end = ts::end_line(n);
                let has_init = c.child_by_field_name("initializer").is_some();
                if !has_init && !is_else_if {
                    if let Some(prev) = b.units.last_mut() {
                        let var = propagation_var(self.text(c));
                        if prev.kind == UnitKind::Stmt
                            && prev.depth == depth
                            && !prev.features.propagates
                            && var.is_some_and(|v| self.mentions(prev, &v))
                        {
                            prev.features.propagates = true;
                            prev.end_line = end;
                            return;
                        }
                    }
                }
                b.push(UnitKind::Stmt, line, end, depth, f);
                return;
            }
        }

        // `Foo* x = Get(); if (!x) return ZX_ERR_X;` is how C++ spells
        // `let x = get().ok_or(X)?;`.
        if let (Some(c), Some(t), None, false) = (cond, cons, alt, is_else_if) {
            if let Some((var, errors)) = self.null_check(c, t) {
                if let Some(prev) = b.units.last_mut() {
                    if prev.kind == UnitKind::Stmt
                        && prev.depth == depth
                        && prev.file.is_none()
                        && !prev.features.propagates
                        && self.mentions(prev, &var)
                    {
                        prev.features.propagates = true;
                        prev.features.errors.extend(errors);
                        prev.end_line = ts::end_line(n);
                        return;
                    }
                }
            }
        }

        let header_end = cons.map_or(line, |c| {
            let inner = if c.kind() == "attributed_statement" {
                ts::named_children(c)
                    .into_iter()
                    .find(|x| x.kind() != "attribute_declaration")
                    .unwrap_or(c)
            } else {
                c
            };
            if inner.kind() == "compound_statement" {
                ts::line(inner)
            } else {
                ts::line(c).saturating_sub(1).max(line)
            }
        });
        let mut f = match cond {
            Some(c) => self.features(c, &[]),
            None => Features::default(),
        };
        let kind = if is_else_if {
            UnitKind::ElseIf
        } else {
            UnitKind::If
        };
        let d = if is_else_if { depth - 1 } else { depth };
        let mut line = line;
        // `zx_status_t status = Foo(); if (status != ZX_OK) { ... }` checks
        // the call itself, as Rust's `if foo().is_err() { ... }` does.
        if let Some(var) = cond.and_then(|c| propagation_var(self.text(c))) {
            if let Some(prev) = b.units.last() {
                if !is_else_if
                    && prev.kind == UnitKind::Stmt
                    && prev.depth == d
                    && prev.file.is_none()
                    && !prev.features.propagates
                    && self.mentions(prev, &var)
                    && !prev.features.calls.is_empty()
                {
                    let prev = b.units.pop().unwrap();
                    line = prev.start_line;
                    f.checks_error = true;
                    f.calls.splice(0..0, prev.features.calls);
                    f.names.extend(prev.features.names);
                    f.idents.extend(prev.features.idents);
                    f.errors.extend(prev.features.errors);
                    f.locks.extend(prev.features.locks);
                }
            }
        }
        b.push(kind, line, header_end, d, f);
        if let Some(c) = cons {
            self.body(c, d + 1, b);
        }
        if let Some(a) = alt {
            // else_clause: `else` followed by a statement.
            let inner = ts::named_children(a)
                .into_iter()
                .find(|x| !ts::is_comment(*x));
            match inner {
                Some(i) if i.kind() == "if_statement" => self.if_statement(i, d + 1, true, b),
                Some(i) => {
                    let header_end = if i.kind() == "compound_statement" {
                        ts::line(i)
                    } else {
                        ts::line(a)
                    };
                    b.push(
                        UnitKind::Else,
                        ts::line(a),
                        header_end,
                        d,
                        Features::default(),
                    );
                    self.body(i, d + 1, b);
                }
                None => {}
            }
        }
    }

    /// Whether a unit's source mentions `name` as a whole word.
    fn mentions(&self, u: &crate::model::Unit, name: &str) -> bool {
        (u.start_line..=u.end_line).any(|l| {
            self.lines.get(l - 1).is_some_and(|t| {
                t.match_indices(name).any(|(i, _)| {
                    let before = t[..i].chars().next_back();
                    let after = t[i + name.len()..].chars().next();
                    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
                    !word(before) && !word(after)
                })
            })
        })
    }

    /// For `if (!x) return ZX_ERR_X;` (or `x == nullptr`), the variable and
    /// the error codes returned.
    fn null_check(&self, cond: Node, cons: Node) -> Option<(String, Vec<String>)> {
        static NULL: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"^\(\s*(?:!\s*(\w+)|(\w+)\s*==\s*nullptr|nullptr\s*==\s*(\w+))\s*\)$")
                .unwrap()
        });
        let c = NULL.captures(self.text(cond).trim())?;
        let var = (1..=3).find_map(|i| c.get(i))?.as_str().to_string();
        let ret = single_statement(cons)?;
        if ret.kind() != "return_statement" {
            return None;
        }
        let errors = crate::normalize::error_codes(self.text(ret));
        (!errors.is_empty()).then_some((var, errors))
    }

    fn is_propagation(&self, cond: Node, cons: Node) -> bool {
        static FAIL: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"!=\s*ZX_OK|ZX_OK\s*!=|\.is_error\(\)|!\s*[\w.>-]+(?:\.|->)is_ok\(\)|\bstatus\s*<\s*0")
                .unwrap()
        });
        if !FAIL.is_match(self.text(cond)) {
            return false;
        }
        let ret = if cons.kind() == "compound_statement" {
            let stmts: Vec<Node> = ts::named_children(cons)
                .into_iter()
                .filter(|c| !ts::is_comment(*c))
                .collect();
            if stmts.len() != 1 {
                return false;
            }
            stmts[0]
        } else {
            cons
        };
        if ret.kind() != "return_statement" {
            return false;
        }
        let expr = ts::named_children(ret)
            .into_iter()
            .find(|c| !ts::is_comment(*c));
        expr.is_some_and(|e| ts::classify_return(self.text(e)) == Ret::Status)
    }
}

/// The variable a failure check tests, e.g. `status` in `status != ZX_OK`.
fn propagation_var(cond: &str) -> Option<String> {
    static VAR: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(\w+)\s*!=\s*ZX_OK|ZX_OK\s*!=\s*(\w+)|(\w+)\s*(?:\.|->)is_error\(\)|!\s*(\w+)\s*(?:\.|->)is_ok\(\)|(\w+)\s*<\s*0")
            .unwrap()
    });
    let c = VAR.captures(cond)?;
    let v = (1..=5).find_map(|i| c.get(i))?;
    Some(v.as_str().to_string())
}

fn strip_parens(mut n: Node) -> Node {
    while n.kind() == "parenthesized_expression" {
        match ts::named_children(n).into_iter().next() {
            Some(c) => n = c,
            None => break,
        }
    }
    n
}

/// Outermost lambda bodies inside `n`.
fn find_bodies<'t>(n: Node<'t>, out: &mut Vec<Node<'t>>) {
    for c in ts::named_children(n) {
        if c.kind() == "lambda_expression" {
            if let Some(body) = c.child_by_field_name("body") {
                out.push(body);
            }
        } else {
            find_bodies(c, out);
        }
    }
}

fn find_function_declarator(n: Node) -> Option<Node> {
    if n.kind() == "function_declarator" {
        return Some(n);
    }
    if matches!(
        n.kind(),
        "pointer_declarator"
            | "reference_declarator"
            | "attributed_declarator"
            | "parenthesized_declarator"
    ) {
        if let Some(d) = n.child_by_field_name("declarator") {
            return find_function_declarator(d);
        }
        for c in ts::named_children(n) {
            if let Some(f) = find_function_declarator(c) {
                return Some(f);
            }
        }
    }
    None
}

/// Splits a possibly qualified function name into (class, base), using the
/// enclosing class path for inline definitions.
fn split_name(name: &str, class: &[String]) -> (Option<String>, String) {
    let name: String = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut parts: Vec<String> = class.to_vec();
    // Split on `::` outside template arguments.
    let mut depth = 0;
    let mut cur = String::new();
    let chars: Vec<char> = name.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '<' {
            depth += 1;
        } else if c == '>' && depth > 0 {
            depth -= 1;
        }
        if depth == 0 && c == ':' && chars.get(i + 1) == Some(&':') {
            parts.push(std::mem::take(&mut cur));
            i += 2;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    let base = cur;
    let cls = if parts.is_empty() {
        None
    } else {
        Some(parts.join("::"))
    };
    (cls, base)
}

/// Removes the `break` that ends a switch case, which Rust match arms don't
/// need. Trailing comments stay.
fn drop_trailing_break(stmts: &mut Vec<Node>) {
    if let Some(i) = stmts.iter().rposition(|c| !ts::is_comment(*c)) {
        // A break inside `case A: { ...; break; }` is handled by `case_body`.
        if stmts[i].kind() == "break_statement" {
            stmts.remove(i);
        }
    }
}

/// The one statement in `n`, looking through braces.
fn single_statement(n: Node) -> Option<Node> {
    if n.kind() != "compound_statement" {
        return Some(n);
    }
    let stmts: Vec<Node> = ts::named_children(n)
        .into_iter()
        .filter(|c| !ts::is_comment(*c))
        .collect();
    (stmts.len() == 1).then(|| stmts[0])
}
