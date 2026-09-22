//! Extracts functions and their units from Rust source.

use crate::model::{Features, Function, Lang, Ret, UnitKind};
use crate::ts::{self, FeatureAcc, UnitBuilder};
use regex::Regex;
use std::sync::LazyLock;
use tree_sitter::Node;

pub fn extract(path: &str, src: &str) -> Vec<Function> {
    let tree = ts::parse(Lang::Rust, src);
    let lines = crate::extract::split_lines(src);
    let ctx = Ctx {
        path,
        src: src.as_bytes(),
        lines: &lines,
    };
    let mut out = Vec::new();
    ctx.walk_scope(tree.root_node(), None, &mut out);
    out
}

struct Ctx<'a> {
    path: &'a str,
    src: &'a [u8],
    lines: &'a [String],
}

/// Per-function state threaded through the statement walk.
#[derive(Clone, Copy)]
struct FnCtx {
    returns_value: bool,
}

impl<'a> Ctx<'a> {
    fn text(&self, n: Node) -> &'a str {
        ts::text(n, self.src)
    }

    fn walk_scope(&self, scope: Node, class: Option<&str>, out: &mut Vec<Function>) {
        for child in ts::named_children(scope) {
            match child.kind() {
                "function_item" => {
                    if let Some(f) = self.function(child, class) {
                        out.push(f);
                    }
                }
                "mod_item" | "foreign_mod_item" => {
                    if let Some(body) = child.child_by_field_name("body") {
                        self.walk_scope(body, class, out);
                    }
                }
                "impl_item" => {
                    let ty = child
                        .child_by_field_name("type")
                        .map(|t| type_name(self.text(t)));
                    if let Some(body) = child.child_by_field_name("body") {
                        self.walk_scope(body, ty.as_deref(), out);
                    }
                }
                "trait_item" => {
                    let name = child
                        .child_by_field_name("name")
                        .map(|t| self.text(t).to_string());
                    if let Some(body) = child.child_by_field_name("body") {
                        self.walk_scope(body, name.as_deref(), out);
                    }
                }
                _ => {}
            }
        }
    }

    fn function(&self, n: Node, class: Option<&str>) -> Option<Function> {
        let base = self.text(n.child_by_field_name("name")?).to_string();
        let body = n.child_by_field_name("body")?;
        let mut b = UnitBuilder::new();

        // Leading doc comments and attributes.
        let mut leading = Vec::new();
        let mut first = ts::line(n);
        let mut is_ffi = false;
        let mut cur = n.prev_sibling();
        while let Some(p) = cur {
            let is_attr = p.kind() == "attribute_item";
            if !(is_attr || ts::is_comment(p)) || ts::end_line(p) + 1 < first {
                break;
            }
            if let Some(pp) = p.prev_sibling() {
                if ts::end_line(pp) == ts::line(p)
                    && !ts::is_comment(pp)
                    && pp.kind() != "attribute_item"
                {
                    break;
                }
            }
            if is_attr && self.text(p).contains("no_mangle") {
                is_ffi = true;
            }
            first = ts::line(p);
            leading.push(p);
            cur = p.prev_sibling();
        }
        for p in leading.iter().rev() {
            if ts::is_comment(*p) {
                b.comment(self.text(*p), ts::line(*p), ts::end_line(*p), 0);
            }
        }
        for c in ts::children(n) {
            if c.kind() == "function_modifiers" && self.text(c).contains("extern") {
                is_ffi = true;
            }
        }

        let mut acc = FeatureAcc::default();
        acc.ident(&base);
        if let Some(params) = n.child_by_field_name("parameters") {
            self.collect(params, &mut acc, &[]);
        }
        let sig = acc.finish(Lang::Rust);
        b.push(UnitKind::Signature, ts::line(n), ts::line(body), 0, sig);

        let returns_value = n
            .child_by_field_name("return_type")
            .is_some_and(|t| self.text(t).trim() != "()");
        let fcx = FnCtx { returns_value };
        self.block(body, 1, true, fcx, &mut b);

        let end = ts::end_line(n);
        let mut calls: Vec<String> = b
            .units
            .iter()
            .flat_map(|u| u.features.calls.clone())
            .collect();
        calls.sort();
        calls.dedup();
        let name = match class {
            Some(c) => format!("{c}::{base}"),
            None => base.clone(),
        };
        Some(Function {
            lang: Lang::Rust,
            path: self.path.to_string(),
            name,
            base,
            class: class.map(str::to_string),
            start_line: first,
            end_line: end,
            lines: self.lines[first - 1..end.min(self.lines.len())].to_vec(),
            units: b.units,
            calls,
            is_ffi,
        })
    }

    fn collect(&self, n: Node, acc: &mut FeatureAcc, skip: &[Node]) {
        static MACRO_CALL: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\b([A-Za-z_]\w*)\s*(?:::\s*<[^>()]*>)?\s*\(").unwrap());
        if skip.iter().any(|s| s.id() == n.id()) {
            return;
        }
        match n.kind() {
            "call_expression" => {
                if let Some(f) = n.child_by_field_name("function") {
                    acc.call(self.text(f));
                }
            }
            "macro_invocation" => {
                if let Some(m) = n.child_by_field_name("macro") {
                    acc.call(self.text(m));
                }
                for tt in ts::named_children(n)
                    .into_iter()
                    .filter(|c| c.kind() == "token_tree")
                {
                    let t = ts::text_without(tt, self.src, skip);
                    for c in MACRO_CALL.captures_iter(&t) {
                        acc.call(&c[1]);
                    }
                }
            }
            "try_expression" => acc.propagates = true,
            "identifier" | "field_identifier" => acc.ident(self.text(n)),
            _ => {}
        }
        for c in ts::named_children(n) {
            self.collect(c, acc, skip);
        }
    }

    fn features(&self, n: Node, skip: &[Node]) -> Features {
        let mut acc = FeatureAcc::default();
        self.collect(n, &mut acc, skip);
        acc.text = ts::text_without(n, self.src, skip);
        acc.finish(Lang::Rust)
    }

    /// Walks a block. `tail` says whether the block's value is the function's
    /// return value.
    fn block(&self, n: Node, depth: usize, tail: bool, fcx: FnCtx, b: &mut UnitBuilder) {
        let children = ts::named_children(n);
        let last = children.iter().rposition(|c| !ts::is_comment(*c));
        for (i, c) in children.iter().enumerate() {
            let is_tail = tail && Some(i) == last && self.is_value_position(*c);
            self.statement(*c, depth, is_tail, fcx, b);
        }
    }

    /// Whether a block's last child produces the block's value (no trailing
    /// semicolon).
    fn is_value_position(&self, n: Node) -> bool {
        match n.kind() {
            "let_declaration" | "function_item" | "const_item" | "static_item"
            | "use_declaration" | "struct_item" | "enum_item" | "impl_item"
            | "macro_definition" => false,
            "expression_statement" => !self.text(n).trim_end().ends_with(';'),
            _ => true,
        }
    }

    fn statement(&self, n: Node, depth: usize, tail: bool, fcx: FnCtx, b: &mut UnitBuilder) {
        match n.kind() {
            "line_comment" | "block_comment" => {
                b.comment(self.text(n), ts::line(n), ts::end_line(n), depth)
            }
            "let_declaration" => self.let_declaration(n, depth, fcx, b),
            "expression_statement" => {
                let inner = ts::named_children(n)
                    .into_iter()
                    .find(|c| !ts::is_comment(*c));
                if let Some(e) = inner {
                    self.expression(e, n, depth, tail, fcx, b)
                }
            }
            "attribute_item" | "empty_statement" => {}
            _ => self.expression(n, n, depth, tail, fcx, b),
        }
    }

    /// `stmt` is the enclosing statement, whose lines the unit covers.
    fn expression(
        &self,
        e: Node,
        stmt: Node,
        depth: usize,
        tail: bool,
        fcx: FnCtx,
        b: &mut UnitBuilder,
    ) {
        let (line, end) = (ts::line(stmt), ts::end_line(stmt));
        match e.kind() {
            "if_expression" => self.if_expression(e, depth, tail, false, fcx, b),
            "match_expression" => self.match_expression(e, depth, tail, fcx, b),
            "loop_expression" | "while_expression" | "for_expression" => {
                let body = e.child_by_field_name("body");
                let header_end = body.map_or(end, |bd| ts::line(bd));
                let skip: Vec<Node> = body.into_iter().collect();
                b.push(
                    UnitKind::Loop,
                    line,
                    header_end,
                    depth,
                    self.features(e, &skip),
                );
                if let Some(bd) = body {
                    self.block(bd, depth + 1, false, fcx, b);
                }
            }
            "return_expression" => {
                let inner = ts::named_children(e).into_iter().next();
                self.return_unit(inner, line, end, depth, b);
            }
            "break_expression" => b.push(UnitKind::Break, line, end, depth, Features::default()),
            "continue_expression" => {
                b.push(UnitKind::Continue, line, end, depth, Features::default())
            }
            "unsafe_block" | "block" => {
                let inner = if e.kind() == "unsafe_block" {
                    ts::named_children(e)
                        .into_iter()
                        .find(|c| c.kind() == "block")
                } else {
                    Some(e)
                };
                if let Some(bl) = inner {
                    if ts::end_line(bl) == ts::line(bl) && !tail {
                        self.plain(stmt, depth, b);
                    } else {
                        self.block(bl, depth, tail, fcx, b);
                    }
                }
            }
            _ if tail && fcx.returns_value => self.return_unit(Some(e), line, end, depth, b),
            _ => self.plain(stmt, depth, b),
        }
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
        b.push(UnitKind::Return, line, end, depth, f);
    }

    /// A plain statement; multi-line closure bodies are split out.
    fn plain(&self, n: Node, depth: usize, b: &mut UnitBuilder) {
        let mut bodies = Vec::new();
        find_closure_bodies(n, &mut bodies);
        let bodies: Vec<Node> = bodies
            .into_iter()
            .filter(|x| ts::end_line(*x) > ts::line(*x))
            .collect();
        let f = self.features(n, &bodies);
        let end = bodies.first().map_or(ts::end_line(n), |x| ts::line(*x));
        b.push(UnitKind::Stmt, ts::line(n), end, depth, f);
        for body in bodies {
            let fcx = FnCtx {
                returns_value: false,
            };
            if body.kind() == "block" {
                self.block(body, depth + 1, false, fcx, b);
            } else {
                self.plain(body, depth + 1, b);
            }
        }
    }

    fn let_declaration(&self, n: Node, depth: usize, fcx: FnCtx, b: &mut UnitBuilder) {
        let line = ts::line(n);
        let value = n.child_by_field_name("value");
        if let Some(alt) = n.child_by_field_name("alternative") {
            // `let Some(x) = foo() else { return ... };` is a statement plus
            // a failure check, like the C++ it replaces.
            let skip = [alt];
            b.push(
                UnitKind::Stmt,
                line,
                ts::line(alt),
                depth,
                self.features(n, &skip),
            );
            let mut f = Features::default();
            if let Some(p) = n.child_by_field_name("pattern") {
                f = self.features(p, &[]);
            }
            b.push(UnitKind::If, ts::line(alt), ts::line(alt), depth, f);
            self.block(alt, depth + 1, false, fcx, b);
            return;
        }
        if let Some(v) = value {
            let compound = matches!(
                v.kind(),
                "if_expression" | "match_expression" | "block" | "unsafe_block" | "loop_expression"
            );
            if compound && ts::end_line(v) > ts::line(v) {
                let skip = [v];
                let mut f = self.features(n, &skip);
                if v.kind() == "unsafe_block" || v.kind() == "block" {
                    // The block's statements become their own units.
                    f.propagates = false;
                }
                b.push(UnitKind::Stmt, line, ts::line(v), depth, f);
                let inner_fcx = FnCtx {
                    returns_value: false,
                };
                match v.kind() {
                    "if_expression" => self.if_expression(v, depth + 1, false, false, inner_fcx, b),
                    "match_expression" => self.match_expression(v, depth + 1, false, inner_fcx, b),
                    _ => self.expression(v, v, depth + 1, false, inner_fcx, b),
                }
                return;
            }
        }
        self.plain(n, depth, b);
    }

    fn if_expression(
        &self,
        n: Node,
        depth: usize,
        tail: bool,
        is_else_if: bool,
        fcx: FnCtx,
        b: &mut UnitBuilder,
    ) {
        let line = ts::line(n);
        let cond = n.child_by_field_name("condition");
        let cons = n.child_by_field_name("consequence");
        let alt = n.child_by_field_name("alternative");
        let d = if is_else_if { depth - 1 } else { depth };

        // `if let Err(e) = foo() { return Err(e); }` is `foo()?`.
        if let (Some(c), Some(t), None) = (cond, cons, alt) {
            if self.is_propagation(c, t) {
                if is_else_if {
                    b.push(UnitKind::Else, line, line, d, Features::default());
                }
                let mut f = self.features(c, &[]);
                f.propagates = true;
                b.push(
                    UnitKind::Stmt,
                    line,
                    ts::end_line(n),
                    if is_else_if { d + 1 } else { d },
                    f,
                );
                return;
            }
        }

        let header_end = cons.map_or(line, |c| ts::line(c));
        let f = match cond {
            Some(c) => self.features(c, &[]),
            None => Features::default(),
        };
        let kind = if is_else_if {
            UnitKind::ElseIf
        } else {
            UnitKind::If
        };
        b.push(kind, line, header_end, d, f);
        if let Some(c) = cons {
            self.block(c, d + 1, tail, fcx, b);
        }
        if let Some(a) = alt {
            let inner = ts::named_children(a)
                .into_iter()
                .find(|x| !ts::is_comment(*x));
            match inner {
                Some(i) if i.kind() == "if_expression" => {
                    self.if_expression(i, d + 1, tail, true, fcx, b)
                }
                Some(i) => {
                    // The `else` keyword sits on the line where the else clause starts.
                    b.push(
                        UnitKind::Else,
                        ts::line(a),
                        ts::line(i),
                        d,
                        Features::default(),
                    );
                    self.block(i, d + 1, tail, fcx, b);
                }
                None => {}
            }
        }
    }

    fn is_propagation(&self, cond: Node, cons: Node) -> bool {
        static ERR_PAT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^let\s+Err\s*\(").unwrap());
        if cond.kind() != "let_condition" || !ERR_PAT.is_match(self.text(cond).trim()) {
            return false;
        }
        let stmts: Vec<Node> = ts::named_children(cons)
            .into_iter()
            .filter(|c| !ts::is_comment(*c))
            .collect();
        if stmts.len() != 1 {
            return false;
        }
        let t = self.text(stmts[0]);
        t.trim_start().starts_with("return")
            && ts::classify_return(
                t.trim_start()
                    .trim_start_matches("return")
                    .trim()
                    .trim_end_matches(';'),
            ) == Ret::Status
    }

    fn match_expression(&self, n: Node, depth: usize, tail: bool, fcx: FnCtx, b: &mut UnitBuilder) {
        let body = n.child_by_field_name("body");
        let header_end = body.map_or(ts::line(n), |bd| ts::line(bd));
        let f = match n.child_by_field_name("value") {
            Some(v) => self.features(v, &[]),
            None => Features::default(),
        };
        b.push(UnitKind::Switch, ts::line(n), header_end, depth, f);
        let Some(body) = body else { return };
        for arm in ts::named_children(body) {
            if ts::is_comment(arm) {
                b.comment(self.text(arm), ts::line(arm), ts::end_line(arm), depth + 1);
                continue;
            }
            if arm.kind() != "match_arm" {
                continue;
            }
            let pattern = arm.child_by_field_name("pattern");
            let value = arm.child_by_field_name("value");
            let mut f = match pattern {
                Some(p) => self.features(p, &[]),
                None => Features::default(),
            };
            if pattern.is_some_and(|p| self.text(p).trim() == "_") {
                f.idents.push("default".to_string());
            }
            let header_end = value.map_or(ts::line(arm), |v| ts::line(v));
            b.push(UnitKind::Case, ts::line(arm), header_end, depth + 1, f);
            if let Some(v) = value {
                match v.kind() {
                    "block" => self.block(v, depth + 2, tail, fcx, b),
                    _ => self.expression(v, v, depth + 2, tail, fcx, b),
                }
            }
            for c in ts::named_children(arm) {
                if ts::is_comment(c) {
                    b.comment(self.text(c), ts::line(c), ts::end_line(c), depth + 2);
                }
            }
        }
    }
}

fn find_closure_bodies<'t>(n: Node<'t>, out: &mut Vec<Node<'t>>) {
    for c in ts::named_children(n) {
        if c.kind() == "closure_expression" {
            if let Some(body) = c.child_by_field_name("body") {
                out.push(body);
            }
        } else {
            find_closure_bodies(c, out);
        }
    }
}

/// `Policy<'a>` -> `Policy`, `&mut Foo` -> `Foo`.
fn type_name(t: &str) -> String {
    let t = t.trim_start_matches('&').trim_start_matches("mut ").trim();
    let t = t.split('<').next().unwrap_or(t);
    t.rsplit("::").next().unwrap_or(t).trim().to_string()
}
