//! Every GraphQL query or mutation in `src/` has balanced braces and parentheses.
//!
//! The fake forges parse no query, so only a real host rejects an unbalanced one.

use std::fs;
use std::path::Path;

use proc_macro2::{TokenStream, TokenTree};
use syn::visit::Visit;

/// What opens a GraphQL document. A string literal starting with one of these is checked.
const DOCUMENT_STARTS: [&str; 3] = ["query ", "mutation ", "subscription "];

/// The first problem in the brackets of `text`, or `None` when they nest and close.
///
/// Doubled braces of a format string still balance, so the literal is read as is.
fn bracket_problem(text: &str) -> Option<String> {
    let mut open: Vec<char> = Vec::new();
    for (index, character) in text.char_indices() {
        let wanted = match character {
            '{' | '(' | '[' => {
                open.push(character);
                continue;
            }
            '}' => '{',
            ')' => '(',
            ']' => '[',
            _ => continue,
        };
        if open.pop() != Some(wanted) {
            return Some(format!("unmatched `{character}` at byte {index}"));
        }
    }
    open.last()
        .map(|character| format!("`{character}` is never closed"))
}

/// Collects GraphQL string literals, including inside macro calls, which `syn` leaves as tokens.
#[derive(Default)]
struct Documents {
    found: Vec<String>,
}

impl Documents {
    fn literal(&mut self, value: String) {
        if DOCUMENT_STARTS.iter().any(|start| value.starts_with(start)) {
            self.found.push(value);
        }
    }

    fn tokens(&mut self, stream: TokenStream) {
        for token in stream {
            match token {
                TokenTree::Group(group) => self.tokens(group.stream()),
                TokenTree::Literal(literal) => {
                    if let Ok(string) = syn::parse_str::<syn::LitStr>(&literal.to_string()) {
                        self.literal(string.value());
                    }
                }
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for Documents {
    fn visit_lit_str(&mut self, literal: &'ast syn::LitStr) {
        self.literal(literal.value());
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.tokens(mac.tokens.clone());
        syn::visit::visit_macro(self, mac);
    }
}

fn documents(code: &str) -> Vec<String> {
    let syntax = syn::parse_file(code).expect("source parses");
    let mut scan = Documents::default();
    scan.visit_file(&syntax);
    scan.found
}

#[test]
fn every_graphql_document_in_the_sources_has_balanced_brackets() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    crate::agt::sources(&root, &mut files);
    let mut checked = 0;
    let mut bad = Vec::new();
    for file in files {
        for document in documents(&fs::read_to_string(&file).unwrap()) {
            checked += 1;
            if let Some(problem) = bracket_problem(&document) {
                let name = document.split(['(', '{']).next().unwrap_or("").trim();
                bad.push(format!("{}: {name}: {problem}", file.display()));
            }
        }
    }
    assert!(checked >= 10, "found only {checked} GraphQL documents");
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn the_bracket_check_finds_an_extra_or_missing_bracket() {
    assert_eq!(bracket_problem("query Q($a:Int){x(a:$a){y}}"), None);
    assert_eq!(bracket_problem("query Q{{x{{y}}}}"), None);
    for text in [
        "query Q{x{y}}}",
        "query Q{x{y}",
        "query Q($a:Int{x}",
        "query Q{x)}",
    ] {
        assert!(bracket_problem(text).is_some(), "{text}");
    }
}

#[test]
fn the_document_scan_reads_constants_and_macro_literals() {
    let code = r#"
        const A: &str = "query A{x}";
        fn f() { let _ = format!("mutation B{{y({z})}}"); let _ = "not a document {"; }
    "#;
    assert_eq!(documents(code), ["query A{x}", "mutation B{{y({z})}}"]);
}
