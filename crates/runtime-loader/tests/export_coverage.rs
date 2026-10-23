//! Enforces that the closure evidence actually covers the umbrella.
//!
//! The fixture is a whitelist: it can only prove the names it happens to use,
//! so a re-export that nothing exercises could be deleted with both "proofs"
//! staying green (third review, B1). This test reads `src/lib.rs`, extracts
//! every `pub use` name, and requires each one to be named by the closure
//! evidence — the vocabulary regression or the loader-only fixture. Adding an
//! export without naming it there fails here.

use std::fs;
use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// Extracts every name a `pub use` statement exports.
///
/// The umbrella uses itemised statements only (no globs), so a small scanner is
/// enough: braced blocks are split on commas and single-name statements after
/// the last `::`. `as` renames export the alias, which is the name a host sees.
fn exported_names(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("pub use ") {
        let statement = &rest[start..];
        let semi = statement.find(';').unwrap_or(statement.len());
        let open = statement.find('{');
        match open {
            Some(open) if open < semi => {
                let Some(close) = statement[open..].find('}') else {
                    break;
                };
                let block = &statement[open + 1..open + close];
                for item in block.split(',') {
                    let item = item.trim();
                    if item.is_empty() {
                        continue;
                    }
                    let name = item.rsplit(" as ").next().unwrap_or(item);
                    let name = name.rsplit("::").next().unwrap_or(name).trim();
                    if name.contains('*') {
                        // A glob nested in a brace block is the same hazard as a
                        // bare one: it adds names this scanner cannot enumerate.
                        names.push(format!("<glob re-export: {item}>"));
                    } else if !name.is_empty() {
                        names.push(name.to_owned());
                    }
                }
                rest = &statement[open + close..];
            }
            _ => {
                let item = statement["pub use ".len()..semi].trim();
                let name = item.rsplit(" as ").next().unwrap_or(item);
                let name = name.rsplit("::").next().unwrap_or(name).trim();
                if name.contains('*') {
                    // A glob would add an unbounded set of names that this
                    // scanner cannot enumerate. Fail closed instead of matching
                    // the literal `*` somewhere in the evidence text.
                    names.push(format!("<glob re-export: {item}>"));
                } else if !name.is_empty() {
                    names.push(name.to_owned());
                }
                rest = &statement[semi..];
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// Drops `//` line comments and `/* ... */` block comments before searching.
///
/// A name mentioned only in a comment is not evidence that a host can name it;
/// without this, adding `// CapabilityFiber` would satisfy the gate. Trailing
/// `//` comments after code are removed too, because the whole line is scanned
/// left to right. A `//` inside a string literal would truncate that line; that
/// can only hide a name (making the gate stricter), never satisfy it.
fn without_comments(source: &str) -> String {
    let characters: Vec<char> = source.chars().collect();
    let mut output = String::with_capacity(source.len());
    let mut index = 0;
    let mut in_block = false;
    while index < characters.len() {
        let current = characters[index];
        let next = characters.get(index + 1).copied();
        if in_block {
            if current == '*' && next == Some('/') {
                in_block = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if current == '/' && next == Some('*') {
            in_block = true;
            index += 2;
            continue;
        }
        if current == '/' && next == Some('/') {
            while index < characters.len() && characters[index] != '\n' {
                index += 1;
            }
            continue;
        }
        output.push(current);
        index += 1;
    }
    output
}

/// Whole-token search, so `Runtime` does not match `RuntimeHandle`.
fn names(corpus: &str, name: &str) -> bool {
    corpus.match_indices(name).any(|(index, _)| {
        let before = corpus[..index].chars().next_back();
        let after = corpus[index + name.len()..].chars().next();
        before.is_none_or(|character| !is_identifier_char(character))
            && after.is_none_or(|character| !is_identifier_char(character))
    })
}

#[test]
fn every_exported_name_is_named_by_the_closure_evidence() {
    let root = manifest_dir();
    let exports = exported_names(&without_comments(&read(&root.join("src/lib.rs"))));
    assert!(
        exports.len() > 50,
        "the export parser found only {} names, which cannot be the umbrella: fix the parser before trusting this test",
        exports.len()
    );

    let mut evidence = without_comments(&read(&root.join("tests/review_r2_exports.rs")));
    evidence.push_str(&without_comments(&read(
        &root.join("../../fixtures/r2-host-fixture/src/lib.rs"),
    )));

    let missing: Vec<&String> = exports
        .iter()
        .filter(|name| !names(&evidence, name))
        .collect();
    assert!(
        missing.is_empty(),
        "these re-exports are never named by the closure evidence, so deleting them would leave both proofs green: {missing:?}"
    );
}

#[test]
fn the_parser_reads_every_block_in_the_umbrella() {
    // A guard on the scanner itself: a regression in the parser would silently
    // empty the check above.
    let exports = exported_names(&read(&manifest_dir().join("src/lib.rs")));
    for expected in [
        "Runtime",
        "ModuleReference",
        "RuntimeLoader",
        "CapabilityDefinition",
        "FileDurableStore",
    ] {
        assert!(
            exports.iter().any(|name| name == expected),
            "the parser missed {expected}; the export list is {exports:?}"
        );
    }
}
