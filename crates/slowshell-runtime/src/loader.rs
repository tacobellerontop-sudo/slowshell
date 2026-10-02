//! Reading config from disk, including `include` resolution.
//!
//! The loader is deliberately separate from the compiler: it deals with the file
//! system and produce a single document, and the compiler deals with meaning.
//! That split is what lets a syntax error in an included file be reported against
//! that file rather than against whatever included it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use slowshell_core::diag::{Diag, DiagKind, DiagResult};
use slowshell_core::lang::ast::{Child, Item};
use slowshell_core::lang::Document;

/// A config file plus everything it pulled in.
#[derive(Debug)]
pub struct Loaded {
    pub document: Document,
    /// The files read, in read order, for the reload watcher and diagnostics.
    pub files: Vec<PathBuf>,
    pub diagnostics: Vec<Diag>,
}

/// How deep `include` may nest. Configs are written by hand; a cycle is a bug.
const MAX_INCLUDE_DEPTH: usize = 8;

/// Read and parse a config file, resolving `include` statements.
///
/// A missing root file is an error. A missing *include* is a warning: the rest of
/// the config still builds, which is what lets a user keep a working shell while
/// fixing one broken widget file.
/// Remove a leading byte-order mark.
///
/// Notepad, Visual Studio and most Windows editors write UTF-8 *with* a BOM by
/// default, so this is what a config saved the obvious way looks like on disk.
/// Rejecting it with ``Unexpected character `﻿` `` at line 1 column 1 would
/// blame the user's first line for something they never typed.
///
/// Only a *leading* mark is removed. A U+FEFF in the middle of a file is a real
/// character and is left for the lexer to complain about, because silently
/// deleting it would shift every span after it.
fn strip_bom(mut text: String) -> String {
    if text.starts_with('\u{feff}') {
        text.drain(..3);
    }
    text
}

pub fn load(root: &Path) -> DiagResult<Loaded> {
    let mut diagnostics = Vec::new();
    let mut files = Vec::new();
    let mut visited = HashSet::new();
    let document = read_into(root, &mut files, &mut visited, &mut diagnostics, 0)?;
    Ok(Loaded { document, files, diagnostics })
}

fn read_into(
    path: &Path,
    files: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
    diagnostics: &mut Vec<Diag>,
    depth: usize,
) -> DiagResult<Document> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical.clone()) {
        // Already read. Returning an empty document is the safe answer: a second
        // include of the same file should not duplicate its contents.
        let name = display_name(path);
        return Ok(Document {
            file: std::sync::Arc::from(name.as_str()),
            stmts: Vec::new(),
            span: slowshell_core::Span::unknown(),
        });
    }

    let raw = std::fs::read_to_string(path).map_err(|e| {
        Diag::fatal(
            DiagKind::Io,
            format!("Could not read {}", display_name(path)),
        )
        .with_note(format!("{e}"))
        .with_note("Check the path in shell.config, or run `shellctl config`.".to_string())
    })?;
    let source = strip_bom(raw);

    files.push(path.to_path_buf());
    let name = display_name(path);
    let doc = slowshell_core::parse(name.as_str(), &source)?;

    if depth >= MAX_INCLUDE_DEPTH {
        diagnostics.push(
            Diag::warning(DiagKind::Io, "Includes are nested too deeply")
                .with_note(format!("Stopped at depth {MAX_INCLUDE_DEPTH}; check for a cycle.")),
        );
        return Ok(doc);
    }

    // An `include` splices the included file's elements in as **children** of the
    // including element, at the position the include appears. That is what makes
    // `Panel { include "bar/main" }` put the bar's contents inside the panel rather
    // than beside it. An `include` at the top level of a file splices at the top
    // level instead, so expansion appends to a list rather than returning one.
    let root_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut stmts: Vec<slowshell_core::lang::ast::Stmt> = Vec::with_capacity(doc.stmts.len());
    for stmt in &doc.stmts {
        expand_stmt(stmt, &root_dir, files, visited, diagnostics, depth, &mut stmts);
    }
    Ok(Document { file: std::sync::Arc::from(name.as_str()), stmts, span: doc.span })
}

/// A short, user-facing name for a path.
///
/// Absolute config paths are noise in a diagnostic, so the file name and its
/// parent are enough to locate it.
fn display_name(path: &Path) -> String {
    let file = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    match path.parent().and_then(|p| p.file_name()) {
        Some(dir) => format!("{}\\{}", dir.to_string_lossy(), file),
        None => file,
    }
}

/// Whether a path looks like a config file, used to seed the default config.
pub fn is_config_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("config") | Some("qml") | Some("shell")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("slowshell-test-{name}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn reads_a_single_file() {
        let d = temp_dir("single");
        let p = write(&d, "shell.config", "Panel { position: \"top\" }");
        let loaded = load(&p).expect("must load");
        assert_eq!(loaded.document.roots().count(), 1);
        assert_eq!(loaded.files.len(), 1);
        assert!(loaded.diagnostics.is_empty());
    }

    #[test]
    fn a_config_saved_by_a_windows_editor_loads() {
        // Notepad and Visual Studio write UTF-8 with a BOM by default. This is
        // the exact bytes such an editor produces, and rejecting them would mean
        // a config the user typed by hand did not work.
        let d = temp_dir("bom");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"Panel { position: \"top\" }");
        let p = d.join("shell.config");
        fs::write(&p, &bytes).unwrap();
        let loaded = load(&p).expect("a BOM must not stop a config loading");
        assert_eq!(loaded.document.roots().count(), 1);
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    }

    #[test]
    fn a_bom_in_the_middle_is_left_for_the_lexer() {
        // Silently deleting it would shift every span after it, so a diagnostic
        // pointing at the wrong line is worse than a complaint.
        let text = "Panel { }\u{feff}".to_string();
        assert_eq!(strip_bom(text), "Panel { }\u{feff}");
    }

    #[test]
    fn strip_bom_only_touches_the_start() {
        assert_eq!(strip_bom("\u{feff}Panel {}".into()), "Panel {}");
        assert_eq!(strip_bom("Panel {}".into()), "Panel {}");
        assert_eq!(strip_bom(String::new()), "");
    }

    #[test]
    fn a_missing_root_file_is_fatal() {
        let d = temp_dir("missing");
        let p = d.join("nope.config");
        let e = load(&p).unwrap_err();
        assert_eq!(e.kind, DiagKind::Io);
        assert_eq!(e.severity, slowshell_core::Severity::Fatal);
    }

    #[test]
    fn a_syntax_error_names_the_file_and_line() {
        let d = temp_dir("syntax");
        let p = write(&d, "shell.config", "Panel {\n  position: \n}");
        let e = load(&p).unwrap_err();
        assert!(e.span.is_some());
        assert_eq!(e.span.unwrap().start.line, 3);
    }

    #[test]
    fn an_include_is_spliced_in_order() {
        let d = temp_dir("include");
        write(&d, "bar/clock.config", "Clock { format: \"HH:mm\" }");
        write(&d, "bar/main.config", "Text { text: \"Apps\" }");
        let p = write(
            &d,
            "shell.config",
            "Panel { include \"bar/main\" include \"bar/clock\" }",
        );
        let loaded = load(&p).expect("must load");
        let panel = loaded.document.roots().next().unwrap();
        let names: Vec<String> = panel.item.children.iter().map(|c| c.item.type_name.to_string()).collect();
        assert_eq!(names, vec!["Text".to_string(), "Clock".to_string()]);
        assert_eq!(loaded.files.len(), 3, "all three files must be recorded for watching");
    }

    #[test]
    fn a_nested_include_is_resolved() {
        let d = temp_dir("nested");
        write(&d, "a/one.config", "Text { text: \"one\" }");
        write(&d, "a/two.config", "include \"one\"\nText { text: \"two\" }");
        let p = write(&d, "shell.config", "Panel { include \"a/two\" }");
        let loaded = load(&p).expect("must load");
        let panel = loaded.document.roots().next().unwrap();
        let names: Vec<String> = panel.item.children.iter().map(|c| c.item.type_name.to_string()).collect();
        assert_eq!(names, vec!["Text".to_string(), "Text".to_string()]);
    }

    #[test]
    fn a_missing_include_is_a_warning_not_a_failure() {
        let d = temp_dir("badinclude");
        let p = write(
            &d,
            "shell.config",
            "Panel { Text { text: \"ok\" } include \"nope\" }",
        );
        let loaded = load(&p).expect("the shell must still load");
        assert_eq!(loaded.document.roots().count(), 1);
        assert!(
            loaded.diagnostics.iter().any(|x| x.message.contains("nope")),
            "the missing include must be reported: {:?}",
            loaded.diagnostics
        );
        // It must not be fatal, so the root element survived.
        let panel = loaded.document.roots().next().unwrap();
        assert!(panel.item.children.iter().any(|c| &*c.item.type_name == "Text"));
    }

    #[test]
    fn an_include_cycle_terminates() {
        let d = temp_dir("cycle");
        write(&d, "a.config", "include \"b\"\nText { text: \"a\" }");
        write(&d, "b.config", "include \"a\"\nText { text: \"b\" }");
        let p = write(&d, "shell.config", "Panel { include \"a\" }");
        let loaded = load(&p).expect("a cycle must not hang");
        assert!(loaded.files.len() <= 3, "each file is read at most once, got {:?}", loaded.files);
    }

    #[test]
    fn reading_the_same_include_twice_does_not_duplicate_it() {
        let d = temp_dir("dedupe");
        write(&d, "x.config", "Text { text: \"x\" }");
        let p = write(&d, "shell.config", "Panel { include \"x\" include \"x\" }");
        let loaded = load(&p).expect("must load");
        let panel = loaded.document.roots().next().unwrap();
        let count = panel.item.children.iter().filter(|c| &*c.item.type_name == "Text").count();
        assert_eq!(count, 1, "a repeated include must not duplicate content");
    }

    #[test]
    fn display_names_are_short() {
        let p = PathBuf::from(r"C:\Users\me\AppData\Roaming\Slowshell\widgets\weather.config");
        assert_eq!(display_name(&p), "widgets\\weather.config");
    }

    #[test]
    fn config_extensions_are_recognised() {
        assert!(is_config_file(Path::new("shell.config")));
        assert!(is_config_file(Path::new("bar.qml")));
        assert!(!is_config_file(Path::new("notes.txt")));
    }
}

/// Extensions tried when an `include` names a file without one.
///
/// The documented layout is `bar/main`, not `bar/main.config`, so a bare name must
/// resolve. The literal path is always tried too, so any extension works.
const INCLUDE_EXTENSIONS: &[&str] = &[".config", ".qml", ".shell"];

/// Resolve an `include` path to a file that exists.
///
/// Returns every candidate that was tried when nothing matched, so the
/// diagnostic can name them.
fn resolve_include(root_dir: &Path, rel: &str) -> (std::path::PathBuf, Vec<std::path::PathBuf>) {
    let literal = slowshell_core::paths::resolve_relative(root_dir, rel);
    let mut tried = vec![literal.clone()];
    if literal.exists() {
        return (literal, tried);
    }
    for ext in INCLUDE_EXTENSIONS {
        let candidate = literal.with_extension(ext.trim_start_matches('.'));
        // `with_extension` on a name that already ends in the extension is a
        // no-op, so the literal is not retried.
        if candidate == literal {
            continue;
        }
        if candidate.exists() {
            return (candidate, tried);
        }
        tried.push(candidate);
    }
    (literal, tried)
}
/// Expand the `include` statements of one statement, appending the result.
///
/// A top-level `include` contributes the included file's own top-level
/// statements; an `include` on an element contributes children to that element.
fn expand_stmt(
    stmt: &slowshell_core::lang::ast::Stmt,
    root_dir: &Path,
    files: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
    diagnostics: &mut Vec<Diag>,
    depth: usize,
    out: &mut Vec<slowshell_core::lang::ast::Stmt>,
) {
    let slowshell_core::lang::ast::Stmt::Child(child) = stmt else {
        // A top-level `include`: splice its statements in place.
        if let slowshell_core::lang::ast::Stmt::Include(rel, span) = stmt {
            let (target, tried) = resolve_include(root_dir, rel);
            match read_into(&target, files, visited, diagnostics, depth + 1) {
                Ok(sub) => out.extend(sub.stmts),
                Err(d) => {
                    let mut diag = Diag::warning(
                        DiagKind::Io,
                        format!("Could not include \"{rel}\": {}", d.message),
                    )
                    .with_span(span.clone());
                    if tried.len() > 1 {
                        let names: Vec<String> = tried.iter().map(|p| display_name(p)).collect();
                        diag = diag.with_note(format!("Tried: {}.", names.join(", ")));
                    }
                    diagnostics.push(diag);
                }
            }
            return;
        }
        out.push(stmt.clone());
        return;
    };

    let mut c = child.clone();
    let mut new_children: Vec<Child> = Vec::new();

    // An `include` on the element contributes the included file's top-level
    // elements as its children. The parser records includes and children in
    // separate lists, so the original interleaving is not preserved; included
    // content comes first, which is the natural reading of
    // `Panel { include "bar" Clock {} }`.
    for (rel, span) in &c.item.includes {
        let (target, tried) = resolve_include(root_dir, rel);
        match read_into(&target, files, visited, diagnostics, depth + 1) {
            Ok(sub) => {
                for s in sub.stmts {
                    if let slowshell_core::lang::ast::Stmt::Child(sc) = s {
                        new_children.push(sc);
                    }
                }
            }
            Err(d) => {
                // Naming the candidates saves a round trip of guessing.
                let mut diag = Diag::warning(
                    DiagKind::Io,
                    format!("Could not include \"{rel}\": {}", d.message),
                )
                .with_span(span.clone());
                if tried.len() > 1 {
                    let names: Vec<String> = tried.iter().map(|p| display_name(p)).collect();
                    diag = diag.with_note(format!("Tried: {}.", names.join(", ")));
                }
                diagnostics.push(diag);
            }
        }
    }

    // Then the element's own nested children, which may themselves include.
    for g in &c.item.children {
        let mut nested: Vec<slowshell_core::lang::ast::Stmt> = Vec::new();
        expand_stmt(
            &slowshell_core::lang::ast::Stmt::Child(g.clone()),
            root_dir,
            files,
            visited,
            diagnostics,
            depth,
            &mut nested,
        );
        for s in nested {
            if let slowshell_core::lang::ast::Stmt::Child(gc) = s {
                new_children.push(gc);
            }
        }
    }
    let mut item: Item = (*c.item).clone();
    item.includes = Vec::new();
    item.children = new_children;
    c.item = std::sync::Arc::new(item);
    out.push(slowshell_core::lang::ast::Stmt::Child(c));
}