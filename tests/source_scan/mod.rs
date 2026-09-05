//! Shared source-scanner support for the test binaries that assert on Rust
//! source text (`candle_repository`'s port-boundary guards, `tauri_backtest`'s
//! `save_run` and application-ring guards, and `tauri_coach`'s coach-wiring
//! guard over both coach composition sites).
//!
//! A `tests/<dir>/mod.rs` module rather than another test binary: cargo compiles
//! only `tests/*.rs` as test targets, so this file is linked into each binary
//! that declares `mod source_scan;` and never runs as a suite of its own.
//!
//! The scanner self-tests below run in EVERY including binary. That is
//! deliberate: each binary's guards are only as trustworthy as the copy of the
//! scanner it was compiled against, so the raw-string and char-literal cases
//! must fail loudly right here, not in some other binary.

use std::path::Path;

/// Read a file relative to the crate manifest, panicking with the path on error.
pub fn read_source(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Blank `//` line and `/* */` block comments, leaving string literals intact — a
/// scanner must read CODE, not the prose that documents the rule.
///
/// **Raw strings are handled, and that is not incidental.** The adapter this scans
/// contains `r#"SELECT 1 AS "one!: i64" ..."#`. A scanner that treats `r#"` as an
/// ordinary quote flips its own in-string state on the embedded quotes, and from
/// there every `//` in the file reads as string content — so the comments survive
/// blanking and a negative assertion matches its own explanatory prose. This scanner
/// tracks the hash count and exits on the matching `"#`.
pub fn blank_comments(source: &str) -> String {
    let bytes: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied();

        // A raw string: `r`, some `#`s, then `"`. The `r` must start a token.
        if c == 'r'
            && i.checked_sub(1)
                .is_none_or(|prev| !bytes[prev].is_alphanumeric() && bytes[prev] != '_')
        {
            let mut j = i + 1;
            let mut hashes = 0;
            while bytes.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            if bytes.get(j) == Some(&'"') {
                // Copy verbatim to the closing `"` + the same number of `#`.
                let close: String = std::iter::once('"')
                    .chain(std::iter::repeat_n('#', hashes))
                    .collect();
                let tail: String = bytes[i..].iter().collect();
                let body_start = j + 1 - i;
                let rel_end = tail[body_start..]
                    .find(&close)
                    .map_or(tail.len(), |k| body_start + k + close.len());
                out.push_str(&tail[..rel_end]);
                i += tail[..rel_end].chars().count();
                continue;
            }
        }

        match c {
            '/' if next == Some('/') => {
                while i < bytes.len() && bytes[i] != '\n' {
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
                let mut depth = 1_usize;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == '/' && bytes.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == '*' && bytes.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        if bytes[i] == '\n' {
                            out.push('\n');
                        }
                        i += 1;
                    }
                }
            }
            // A CHAR literal, which must not be confused with a lifetime. The
            // adapter contains `trim_matches('\"')`: a scanner that ignores char
            // literals reads that inner `\"` as the start of a string and then
            // treats every comment until the next quote — hundreds of lines — as
            // string content, so blanking silently stops working mid-file.
            '\'' if is_char_literal(&bytes, i) => {
                let close = char_literal_end(&bytes, i);
                for ch in &bytes[i..=close] {
                    out.push(*ch);
                }
                i = close + 1;
            }
            '"' => {
                out.push(c);
                i += 1;
                let mut escaped = false;
                while i < bytes.len() {
                    let ch = bytes[i];
                    out.push(ch);
                    i += 1;
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == '"' {
                        break;
                    }
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Whether the `'` at `i` opens a char literal rather than a lifetime.
fn is_char_literal(bytes: &[char], i: usize) -> bool {
    match bytes.get(i + 1) {
        // `'\n'`, `'\''`, `'\\'` …
        Some('\\') => bytes.get(i + 3) == Some(&'\'') || bytes.get(i + 2) == Some(&'\''),
        // `'x'` — a lifetime's next char is followed by an identifier char, not `'`.
        Some(_) => bytes.get(i + 2) == Some(&'\''),
        None => false,
    }
}

/// The index of the closing `'` of the char literal opening at `i`.
fn char_literal_end(bytes: &[char], i: usize) -> usize {
    let mut j = i + 1;
    if bytes.get(j) == Some(&'\\') {
        j += 1;
    }
    while j < bytes.len() && bytes[j] != '\'' {
        j += 1;
    }
    j.min(bytes.len() - 1)
}

#[test]
fn blank_comments_strips_comments_and_keeps_code() {
    let src = "// self.get_run( in a comment\nlet x = 1; /* self.get_run( in a block */\nlet s = \"self.get_run( in a string\";\n";
    let code = blank_comments(src);
    assert!(!code.contains("self.get_run( in a comment"));
    assert!(!code.contains("self.get_run( in a block"));
    assert!(code.contains("self.get_run( in a string"), "{code}");
    assert!(code.contains("let x = 1;"), "{code}");
}

#[test]
fn blank_comments_survives_a_raw_string_with_embedded_quotes() {
    // The exact shape in the adapter being scanned, plus a raw string with a
    // SINGLE embedded quote. A scanner that mishandles `r#"…"#` flips its
    // in-string state on the inner quotes; with balanced inner quotes the flips
    // cancel and it survives by accident, but one embedded quote leaves the
    // state flipped open — every later `//` then reads as string content, so
    // the comment survives blanking and a negative assertion matches its own
    // explanatory prose. The odd-quote line is what makes this test bite.
    let src = "let q = r#\"SELECT 1 AS \"one!: i64\" FROM t\"#;\nlet odd = r#\"one \" two\"#;\n// self.get_run( in a comment\nlet done = 1;\n";
    let code = blank_comments(src);
    assert!(
        code.contains("SELECT 1 AS \"one!: i64\""),
        "the raw string survives verbatim: {code}"
    );
    assert!(
        code.contains("one \" two"),
        "the odd-quote raw string survives verbatim: {code}"
    );
    assert!(
        !code.contains("self.get_run( in a comment"),
        "the comment AFTER a raw string is still blanked: {code}"
    );
    assert!(code.contains("let done = 1;"), "{code}");
}

#[test]
fn blank_comments_survives_a_quote_char_literal() {
    // `trim_matches('"')` is in the adapter this scans. Reading its inner quote as
    // a string opener is how a scanner silently stops blanking mid-file.
    let src = "let t = s.trim_matches('\"');\n// self.get_run( in a comment\nlet done = 1;\n";
    let code = blank_comments(src);
    assert!(code.contains("trim_matches('\"')"), "{code}");
    assert!(
        !code.contains("self.get_run( in a comment"),
        "the comment AFTER a quote char literal is still blanked: {code}"
    );
    assert!(code.contains("let done = 1;"), "{code}");
}
