//! Lexer for the Slowshell configuration language.

use std::sync::Arc;

use crate::color::Color;
use crate::diag::{Diag, DiagKind, Pos, Span};
use crate::value::Value;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(Arc<str>),
    Int(i64),
    Float(f64),
    Str(Arc<str>),
    ColorLit(Color),
    Punct(&'static str),
    Eof,
}

impl Tok {
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident(i) => format!("identifier `{i}`"),
            Tok::Int(i) => format!("number `{i}`"),
            Tok::Float(f) => format!("number `{f}`"),
            Tok::Str(_) => "string".into(),
            Tok::ColorLit(c) => format!("color `{c}`"),
            Tok::Punct(p) => format!("`{p}`"),
            Tok::Eof => "end of file".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

pub struct Lexer {
    file: Arc<str>,
    src: Vec<char>,
    pos: usize,
    line: u32,
    col: u32,
}

/// Multi-character operators, longest first so `<=` wins over `<`.
const PUNCT: &[&str] = &[
    "&&", "||", "==", "!=", "<=", ">=", "=>", "{", "}", "(", ")", "[", "]", ":", ",", "+", "-",
    "*", "/", "%", ".", "?", "!", "<", ">", "=",
];

impl Lexer {
    pub fn new(file: impl Into<Arc<str>>, src: &str) -> Lexer {
        Lexer {
            file: file.into(),
            src: src.chars().collect(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    pub fn tokenize(mut self) -> Result<Vec<Token>, Diag> {
        let mut out = Vec::new();
        loop {
            self.skip_trivia();
            if self.pos >= self.src.len() {
                out.push(Token { tok: Tok::Eof, span: self.here() });
                return Ok(out);
            }
            let start = self.here();
            let c = self.src[self.pos];
            let tok = match c {
                '"' | '\'' => self.lex_string(c)?,
                '#' => self.lex_color()?,
                c if c.is_ascii_digit() => self.lex_number(),
                c if is_ident_start(c) => self.lex_ident(),
                _ => {
                    let rest: String = self.src[self.pos..].iter().take(2).collect();
                    match PUNCT.iter().find(|p| rest.starts_with(**p)) {
                        Some(p) => {
                            for _ in 0..p.chars().count() {
                                self.bump();
                            }
                            Tok::Punct(p)
                        }
                        None => {
                            return Err(Diag::fatal(
                                DiagKind::Syntax,
                                format!("Unexpected character `{c}`"),
                            )
                            .with_span(start)
                            .with_note(format!("The character U+{:04X} is not valid here.", c as u32)))
                        }
                    }
                }
            };
            let end = self.here();
            out.push(Token { tok, span: Span { file: self.file.clone(), start: start.start, end: end.start } });
        }
    }

    fn here(&self) -> Span {
        let p = Pos::new(self.line, self.col, self.pos as u32);
        Span::new(self.file.clone(), p, p)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.src.get(self.pos).copied()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.src.get(self.pos + n).copied()
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('/') if self.peek_at(1) == Some('/') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                Some('/') if self.peek_at(1) == Some('*') => {
                    self.bump();
                    self.bump();
                    while self.pos < self.src.len() {
                        if self.peek() == Some('*') && self.peek_at(1) == Some('/') {
                            self.bump();
                            self.bump();
                            break;
                        }
                        self.bump();
                    }
                }
                _ => return,
            }
        }
    }

    fn lex_ident(&mut self) -> Tok {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
            self.bump();
        }
        let s: String = self.src[start..self.pos].iter().collect();
        Tok::Ident(Arc::from(s.as_str()))
    }

    fn lex_number(&mut self) -> Tok {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.bump();
        }
        let mut is_float = false;
        if self.peek() == Some('.') && matches!(self.peek_at(1), Some(c) if c.is_ascii_digit()) {
            is_float = true;
            self.bump();
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.bump();
            }
        }
        // Allow C-style and underscore digit separators in large numbers.
        if self.peek() == Some('_') && matches!(self.peek_at(1), Some(c) if c.is_ascii_digit()) {
            while matches!(self.peek(), Some(c) if c.is_ascii_digit() || c == '_') {
                self.bump();
            }
        }
        let text: String = self.src[start..self.pos].iter().filter(|c| **c != '_').collect();
        if is_float {
            Tok::Float(text.parse().unwrap_or(0.0))
        } else {
            match text.parse() {
                Ok(i) => Tok::Int(i),
                // Values beyond i64 degrade to float rather than erroring out.
                Err(_) => Tok::Float(text.parse().unwrap_or(0.0)),
            }
        }
    }

    fn lex_string(&mut self, quote: char) -> Result<Tok, Diag> {
        let start = self.here();
        self.bump();
        let mut out = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => {
                    return Err(Diag::fatal(
                        DiagKind::Syntax,
                        "Unterminated string literal",
                    )
                    .with_span(start)
                    .with_note("Strings must be closed on the same line they open."))
                }
                Some(c) if c == quote => break,
                Some('\\') => match self.bump() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('0') => out.push('\0'),
                    Some('\\') => out.push('\\'),
                    Some('"') => out.push('"'),
                    Some('\'') => out.push('\''),
                    Some('u') => {
                        // \u{1F600}
                        let mut hex = String::new();
                        if self.peek() == Some('{') {
                            self.bump();
                            while let Some(c) = self.peek() {
                                if c == '}' {
                                    self.bump();
                                    break;
                                }
                                hex.push(c);
                                self.bump();
                            }
                        } else {
                            for _ in 0..4 {
                                if let Some(c) = self.bump() {
                                    hex.push(c);
                                }
                            }
                        }
                        match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                            Some(ch) => out.push(ch),
                            None => {
                                return Err(Diag::fatal(
                                    DiagKind::Syntax,
                                    format!("Invalid unicode escape `\\u{{{hex}}}`"),
                                )
                                .with_span(start))
                            }
                        }
                    }
                    Some(other) => out.push(other),
                    None => {
                        return Err(Diag::fatal(DiagKind::Syntax, "Unterminated escape sequence")
                            .with_span(start))
                    }
                },
                Some(c) => out.push(c),
            }
        }
        Ok(Tok::Str(Arc::from(out.as_str())))
    }

    fn lex_color(&mut self) -> Result<Tok, Diag> {
        let start = self.here();
        self.bump(); // '#'
        let hex_start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_hexdigit()) {
            self.bump();
        }
        let text: String = self.src[hex_start..self.pos].iter().collect();
        match Color::parse(&format!("#{text}")) {
            Some(c) => Ok(Tok::ColorLit(c)),
            None => Err(Diag::fatal(
                DiagKind::Syntax,
                format!("Invalid color literal `#{text}`"),
            )
            .with_span(start)
            .with_note("Colors look like `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`.")),
        }
    }
}

pub fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

pub fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

/// Turn a token into a literal `Value` where possible. Used by the interpreter's
/// fast path and by tooling that wants values without a full expression tree.
pub fn tok_to_value(t: &Tok) -> Option<Value> {
    Some(match t {
        Tok::Int(i) => Value::Int(*i),
        Tok::Float(f) => Value::Float(*f),
        Tok::Str(s) => Value::Str(s.clone()),
        Tok::ColorLit(c) => Value::Color(*c),
        Tok::Ident(i) => match &**i {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            "null" => Value::Null,
            _ => return None,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        Lexer::new("t.config", src).tokenize().unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn lexes_basic_tokens() {
        let t = toks("Panel { height: 34 text: \"hi\" }");
        assert_eq!(t[0], Tok::Ident("Panel".into()));
        assert_eq!(t[1], Tok::Punct("{"));
        assert_eq!(t[2], Tok::Ident("height".into()));
        assert_eq!(t[3], Tok::Punct(":"));
        assert_eq!(t[4], Tok::Int(34));
        assert!(t.contains(&Tok::Str("hi".into())));
    }

    #[test]
    fn skips_comments() {
        let t = toks("// hi\n/* block\ncomment */ 42");
        assert_eq!(t[0], Tok::Int(42));
    }

    #[test]
    fn handles_nested_string_quotes_and_escapes() {
        let t = toks(r#" text: "a\"b\nc" "#);
        assert!(t.contains(&Tok::Str("a\"b\nc".into())), "{t:?}");
    }

    #[test]
    fn parses_colors_of_all_lengths() {
        for src in ["#f0f", "#f0f8", "#89b4fa", "#89b4fa80"] {
            assert!(toks(src)[0].clone().describe().contains("color"));
        }
    }

    #[test]
    fn tracks_line_and_column() {
        let toks = Lexer::new("t.config", "a\n  bb\n#fff").tokenize().unwrap();
        assert_eq!(toks[0].span.start.line, 1);
        assert_eq!(toks[1].span.start.line, 2);
        assert_eq!(toks[1].span.start.col, 3);
        assert_eq!(toks.last().unwrap().span.start.line, 3);
    }

    #[test]
    fn unterminated_string_is_an_error() {
        let e = Lexer::new("t.config", "text: \"oops").tokenize().unwrap_err();
        assert_eq!(e.kind, DiagKind::Syntax);
    }
}
