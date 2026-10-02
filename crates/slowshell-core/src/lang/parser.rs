//! Recursive-descent parser with precedence climbing for binary operators.
//!
//! The grammar has two entry points that share the expression parser:
//!
//! ```text
//! document  := stmt*
//! stmt      := IDENT ':' expr        // property
//!            | IDENT block           // child element or named group
//!            | 'include' STRING      // file splice
//! block     := '{' stmt* '}'
//! ```
//!
//! Because a block is also an expression, `items: [ Row { ... } ]` needs no
//! special handling.

use std::sync::Arc;

use super::ast::*;
use super::lexer::{Lexer, Tok, Token};
use crate::diag::{Diag, DiagKind, DiagResult, Span};
use crate::value::Value;

pub fn parse(file: impl Into<Arc<str>>, src: &str) -> DiagResult<Document> {
    let toks = Lexer::new(file.into(), src).tokenize()?;
    let file = {
        // Reuse the lexer's file name for spans.
        match toks.first() {
            Some(t) => t.span.file.clone(),
            None => Arc::from("shell.config"),
        }
    };
    let mut p = Parser { toks, i: 0 };
    let stmts = p.parse_stmts_until_eof()?;
    Ok(Document { file, stmts, span: Span::unknown() })
}

struct Parser {
    toks: Vec<Token>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.i.min(self.toks.len() - 1)].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.i + n).min(self.toks.len() - 1)].tok
    }

    fn span(&self) -> Span {
        self.toks[self.i.min(self.toks.len() - 1)].span.clone()
    }

    fn prev_span(&self) -> Span {
        self.toks[self.i.saturating_sub(1).min(self.toks.len() - 1)].span.clone()
    }

    fn advance(&mut self) -> Token {
        let t = self.toks[self.i.min(self.toks.len() - 1)].clone();
        if self.i < self.toks.len() - 1 {
            self.i += 1;
        }
        t
    }

    fn at_punct(&self, p: &str) -> bool {
        matches!(self.peek(), Tok::Punct(x) if *x == p)
    }

    fn eat_punct(&mut self, p: &str) -> bool {
        if self.at_punct(p) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_punct(&mut self, p: &'static str) -> DiagResult<Span> {
        if self.at_punct(p) {
            Ok(self.advance().span)
        } else {
            Err(Diag::fatal(
                DiagKind::Syntax,
                format!("Expected `{p}` but found {}", self.peek().describe()),
            )
            .with_span(self.span())
            .with_note(self.context_note()))
        }
    }

    fn expect_ident(&mut self) -> DiagResult<(Arc<str>, Span)> {
        match self.peek().clone() {
            Tok::Ident(i) => {
                let s = self.advance().span;
                Ok((i, s))
            }
            other => Err(Diag::fatal(
                DiagKind::Syntax,
                format!("Expected a name but found {}", other.describe()),
            )
            .with_span(self.span())
            .with_note(self.context_note())),
        }
    }

    /// "Did you mean" style hint naming what the parser was trying to read.
    fn context_note(&self) -> String {
        let next = self.peek_at(1);
        if matches!(next, Tok::Punct("{")) {
            "An element needs a name and a `{ ... }` body, e.g. `Text { text: \"Hi\" }`."
                .to_string()
        } else {
            "Check for a missing `:` after a property name, or a missing comma in a list."
                .to_string()
        }
    }

    fn parse_stmts_until_eof(&mut self) -> DiagResult<Vec<Stmt>> {
        let mut out = Vec::new();
        while !matches!(self.peek(), Tok::Eof) {
            out.push(self.parse_stmt()?);
        }
        Ok(out)
    }

    fn parse_body(&mut self) -> DiagResult<Vec<Stmt>> {
        self.expect_punct("{")?;
        let mut out = Vec::new();
        loop {
            if self.eat_punct("}") {
                return Ok(out);
            }
            if matches!(self.peek(), Tok::Eof) {
                return Err(Diag::fatal(DiagKind::Syntax, "Unexpected end of file: missing `}`")
                    .with_span(self.prev_span())
                    .with_note(
                        "Every `{` needs a matching `}`. The error overlay can show the \
                         matching brace if you nest less deeply.",
                    ));
            }
            out.push(self.parse_stmt()?);
        }
    }

    fn parse_stmt(&mut self) -> DiagResult<Stmt> {
        let start = self.span();
        // `include "path"`
        if let Tok::Ident(name) = self.peek().clone() {
            if &*name == "include" {
                self.advance();
                let path = match self.peek().clone() {
                    Tok::Str(s) => {
                        self.advance();
                        s
                    }
                    other => {
                        return Err(Diag::fatal(
                            DiagKind::Syntax,
                            format!("`include` expects a file path, found {}", other.describe()),
                        )
                        .with_span(self.span()))
                    }
                };
                return Ok(Stmt::Include(path, start));
            }

            self.advance();
            if self.at_punct(":") {
                self.advance();
                let value = self.parse_expr()?;
                return Ok(Stmt::Prop(Prop { name, value, span: start }));
            }
            if self.at_punct("{") {
                let item = self.parse_item_tail(name.clone(), start.clone())?;
                return Ok(Stmt::Child(Child { name, item, span: start }));
            }
            // `WindowManager.acrylic: true` â€” a member-looking name followed by `:`.
            if matches!(self.peek(), Tok::Punct(".")) {
                // Rewind and let the expression parser handle a complex name.
                self.i -= 1;
                self.advance();
                let value = self.parse_expr()?;
                return Ok(Stmt::Prop(Prop { name, value, span: start }));
            }

            return Err(Diag::fatal(
                DiagKind::Syntax,
                format!("Expected `:` or `{{` after `{name}` but found {}", self.peek().describe()),
            )
            .with_span(self.span())
            .with_note(
                "Inside an element body a name is either a property (`key: value`) or a \
                 child element (`Name { ... }`).",
            ));
        }

        Err(Diag::fatal(
            DiagKind::Syntax,
            format!("Expected a property or element but found {}", self.peek().describe()),
        )
        .with_span(self.span()))
    }

    fn parse_item_tail(&mut self, type_name: Arc<str>, start: Span) -> DiagResult<Arc<Item>> {
        let mut args = Vec::new();
        // Positional args: `Notification("Title", "Body") { ... }`
        if self.eat_punct("(") {
            if !self.at_punct(")") {
                loop {
                    args.push(self.parse_expr()?);
                    if !self.eat_punct(",") {
                        break;
                    }
                }
            }
            self.expect_punct(")")?;
        }
        let (props, children, includes) = if self.at_punct("{") {
            let stmts = self.parse_body()?;
            let mut props = Vec::new();
            let mut children = Vec::new();
            let mut includes = Vec::new();
            for s in stmts {
                match s {
                    Stmt::Prop(p) => props.push(p),
                    Stmt::Child(c) => children.push(c),
                    Stmt::Include(p, sp) => includes.push((p, sp)),
                }
            }
            (props, children, includes)
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
        let end = self.prev_span();
        Ok(Arc::new(Item { type_name, args, props, children, span: start.merge(&end), includes }))
    }

    // ---- expressions ------------------------------------------------------

    fn parse_expr(&mut self) -> DiagResult<Expr> {
        self.parse_ternary()
    }

    fn parse_ternary(&mut self) -> DiagResult<Expr> {
        let cond = self.parse_binary(0)?;
        if self.eat_punct("?") {
            let then = self.parse_expr()?;
            self.expect_punct(":")?;
            let els = self.parse_ternary()?;
            return Ok(Expr::Cond(Box::new(cond), Box::new(then), Box::new(els)));
        }
        Ok(cond)
    }

    fn parse_binary(&mut self, min_prec: u8) -> DiagResult<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let (op, prec) = match self.peek() {
                Tok::Punct(p) => match binop(p) {
                    Some(op) => (op, op.precedence()),
                    None => break,
                },
                _ => break,
            };
            if prec < min_prec {
                break;
            }
            self.advance();
            // All our binary operators are left-associative.
            let rhs = self.parse_binary(prec + 1)?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> DiagResult<Expr> {
        if self.eat_punct("-") {
            return Ok(Expr::Unary(UnOp::Neg, Box::new(self.parse_unary()?)));
        }
        if self.eat_punct("!") {
            return Ok(Expr::Unary(UnOp::Not, Box::new(self.parse_unary()?)));
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> DiagResult<Expr> {
        let mut e = self.parse_primary()?;
        loop {
            if self.eat_punct(".") {
                let (name, _) = self.expect_ident()?;
                e = Expr::Member(Box::new(e), name);
            } else if self.eat_punct("[") {
                let idx = self.parse_expr()?;
                self.expect_punct("]")?;
                e = Expr::Index(Box::new(e), Box::new(idx));
            } else if self.eat_punct("(") {
                let mut args = Vec::new();
                if !self.at_punct(")") {
                    loop {
                        args.push(self.parse_expr()?);
                        if !self.eat_punct(",") {
                            break;
                        }
                    }
                }
                self.expect_punct(")")?;
                e = Expr::Call(Box::new(e), args);
            } else {
                return Ok(e);
            }
        }
    }

    fn parse_primary(&mut self) -> DiagResult<Expr> {
        let span = self.span();
        match self.peek().clone() {
            Tok::Int(i) => {
                self.advance();
                Ok(Expr::Literal(Value::Int(i)))
            }
            Tok::Float(f) => {
                self.advance();
                Ok(Expr::Literal(Value::Float(f)))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Literal(Value::Str(s)))
            }
            Tok::ColorLit(c) => {
                self.advance();
                Ok(Expr::ColorLit(c))
            }
            Tok::Ident(name) => {
                self.advance();
                match &*name {
                    "true" => return Ok(Expr::Literal(Value::Bool(true))),
                    "false" => return Ok(Expr::Literal(Value::Bool(false))),
                    "null" => return Ok(Expr::Literal(Value::Null)),
                    _ => {}
                }
                if self.at_punct("(") {
                    // A function literal: `fn(delta) { delta * 2 }`.
                    if &*name == "fn" {
                        return self.parse_lambda(name);
                    }
                    // Plain call: `open()`.
                    self.advance(); // (
                    let mut args = Vec::new();
                    if !self.at_punct(")") {
                        loop {
                            args.push(self.parse_expr()?);
                            if !self.eat_punct(",") {
                                break;
                            }
                        }
                    }
                    self.expect_punct(")")?;
                    return Ok(Expr::Call(Box::new(Expr::Ident(name)), args));
                }
                if self.at_punct("{") {
                    // Element literal used as a value: `background: Acrylic { ... }`.
                    let item = self.parse_item_tail(name.clone(), span)?;
                    return Ok(Expr::Item(item));
                }
                Ok(Expr::Ident(name))
            }
            Tok::Punct("(") => {
                self.advance();
                let e = self.parse_expr()?;
                self.expect_punct(")")?;
                Ok(e)
            }
            Tok::Punct("[") => {
                self.advance();
                let mut items = Vec::new();
                loop {
                    if self.at_punct("]") {
                        break;
                    }
                    if matches!(self.peek(), Tok::Eof) {
                        return Err(Diag::fatal(
                            DiagKind::Syntax,
                            "Unexpected end of file inside a list",
                        )
                        .with_span(self.span())
                        .with_note("A list opened with `[` must be closed with `]`."));
                    }
                    items.push(self.parse_expr()?);
                    // Commas are optional: `[MenuItem {…} MenuSeparator {}]` reads
                    // better in a config than the same list with commas.
                    self.eat_punct(",");
                }
                self.expect_punct("]")?;
                Ok(Expr::List(items))
            }
            Tok::Punct("{") => {
                let stmts = self.parse_body()?;
                let mut props = Vec::new();
                for s in stmts {
                    if let Stmt::Prop(p) = s {
                        props.push((p.name, p.value));
                    }
                }
                Ok(Expr::Map(props))
            }
            other => Err(Diag::fatal(
                DiagKind::Syntax,
                format!("Expected a value but found {}", other.describe()),
            )
            .with_span(span)
            .with_note(
                "A value can be a number, a quoted string, a color, `true`/`false`, a name, \
                 or a call like `battery.percentage`.",
            )),
        }
    }

    fn parse_lambda(&mut self, _name: Arc<str>) -> DiagResult<Expr> {
        self.expect_punct("(")?;
        let mut params = Vec::new();
        while !self.at_punct(")") {
            let (p, _) = self.expect_ident()?;
            params.push(p);
            if !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct(")")?;
        self.expect_punct("{")?;
        // The body is a single expression; a block of statements is not needed yet.
        let body = self.parse_expr()?;
        self.expect_punct("}")?;
        // Reuse `Lazy` as the closure carrier until functions are wired to state.
        let _ = params;
        Ok(Expr::Lazy(Arc::new(body)))
    }
}

fn binop(p: &str) -> Option<BinOp> {
    Some(match p {
        "+" => BinOp::Add,
        "-" => BinOp::Sub,
        "*" => BinOp::Mul,
        "/" => BinOp::Div,
        "%" => BinOp::Rem,
        "==" => BinOp::Eq,
        "!=" => BinOp::Ne,
        "<" => BinOp::Lt,
        "<=" => BinOp::Le,
        ">" => BinOp::Gt,
        ">=" => BinOp::Ge,
        "&&" => BinOp::And,
        "||" => BinOp::Or,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Color;

    fn p(src: &str) -> DiagResult<Document> {
        parse("shell.config", src)
    }

    #[test]
    fn parses_the_spec_minimal_example() {
        let src = r#"
Panel {
    position: "top"

    Row {
        Text { text: "Hello" }
        Clock { format: "HH:mm" }
    }
}
"#;
        let doc = p(src).expect("should parse");
        let roots: Vec<_> = doc.roots().collect();
        assert_eq!(roots.len(), 1);
        assert_eq!(&*roots[0].item.type_name, "Panel");
        assert_eq!(roots[0].item.children.len(), 1);
    }

    #[test]
    fn parses_named_groups() {
        let doc = p(
            r#"Panel { left { Launcher {} } center { Clock {} } right { Network {} } }"#,
        )
        .unwrap();
        let panel = &doc.roots().next().unwrap().item;
        let names: Vec<_> = panel.children.iter().map(|c| c.name.to_string()).collect();
        assert_eq!(names, vec!["left", "center", "right"]);
    }

    #[test]
    fn parses_menu_item_lists() {
        let doc = p(
            r#"Menu { items: [ MenuItem { text: "Open" } MenuSeparator {} MenuItem { text: "Quit" } ] }"#,
        )
        .unwrap();
        let menu = &doc.roots().next().unwrap().item;
        let items = menu.prop("items").expect("items property");
        match &items.value {
            Expr::List(v) => assert_eq!(v.len(), 3),
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn parses_binary_precedence() {
        let doc = p(r#"Text { text: a.b + 2 * 3 }"#).unwrap();
        let t = doc.roots().next().unwrap().item.prop("text").unwrap();
        match &t.value {
            Expr::Binary(BinOp::Add, _, rhs) => {
                assert!(matches!(rhs.as_ref(), Expr::Binary(BinOp::Mul, _, _)));
            }
            other => panic!("expected add at top, got {other:?}"),
        }
    }

    #[test]
    fn parses_indexing_and_calls() {
        let doc = p(r#"Text { text: screens[1].width } Notification { body: audio.setVolume(50) }"#)
            .unwrap();
        let roots: Vec<_> = doc.roots().collect();
        match &roots[0].item.prop("text").unwrap().value {
            Expr::Member(idx, w) => {
                assert!(matches!(idx.as_ref(), Expr::Index(_, _)));
                assert_eq!(&**w, "width");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_include() {
        let doc = p(r#"Panel { include "bar/clock" Clock {} }"#).unwrap();
        let panel = &doc.roots().next().unwrap().item;
        assert_eq!(panel.includes.len(), 1);
        assert_eq!(&*panel.includes[0].0, "bar/clock");
    }

    #[test]
    fn reports_missing_brace_with_position() {
        let e = p("Panel { \n  Clock { \n").unwrap_err();
        assert_eq!(e.kind, DiagKind::Syntax);
        assert!(e.message.contains("missing `}`"), "{}", e.message);
    }

    #[test]
    fn reports_property_without_colon() {
        let e = p("Panel { height 34 }").unwrap_err();
        assert!(e.message.contains("Expected"), "{}", e.message);
    }

    #[test]
    fn element_literal_is_an_expression() {
        // `background: Acrylic { tint: "#112233" }` must parse without special
        // casing. A two-hash raw string is required because the body contains `"#`.
        let doc = p(r##"Panel { background: Acrylic { tint: "#112233" } }"##).unwrap();
        let bg = doc.roots().next().unwrap().item.prop("background").unwrap();
        assert!(matches!(bg.value, Expr::Item(_)));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let doc = p("// header\n\n/* block */\nPanel {\n  // inner\n  height: 1 // trailing\n}\n")
            .unwrap();
        assert_eq!(doc.roots().count(), 1);
    }

    #[test]
    fn colors_parse_as_literals() {
        let doc = p(r#"Panel { color: #89b4fa }"#).unwrap();
        let c = doc.roots().next().unwrap().item.prop("color").unwrap();
        assert!(matches!(c.value, Expr::ColorLit(Color { .. })));
    }
}


