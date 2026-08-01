//
// This source file is part of the Pylon open source project.
//
// Copyright (c) 2026 Jaldis B.V.
//
// Licensed under the MIT OR Apache-2.0 license (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://opensource.org/licenses/MIT
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

//! Finds embedded PyQL query strings inside Python source text.
//!
//! Not a full Python parser — just enough of a hand-rolled tokenizer (byte-
//! based, matching `pylon_core`'s own lexer convention of counting columns
//! per byte rather than per Unicode scalar) to reliably find
//! `NAME ('.' NAME)* '(' [NAME '='] STRING` call-head patterns for a fixed
//! allowlist of Pylon entry-point names, and extract the first argument's
//! string content together with a byte-for-byte map back to its source
//! location — so escape sequences (`\n`, `\t`, ...) decode correctly
//! without breaking position-mapping for diagnostics.

/// Pylon entry points that take a raw PyQL string as their first argument
/// (`pylon.query.compile`, and every `Client` query/execute method).
/// `execute` is deliberately included despite being a generic name shared
/// by other DB clients (asyncpg, psycopg2, sqlite3, ...) — this is a v1
/// tradeoff: an occasional false positive on an unrelated `.execute(sql)`
/// call is preferable to silently missing real `client.execute(pyql)` call
/// sites. See the design plan for the follow-up (import-aware tightening).
const ALLOWLIST: &[&str] = &[
    "compile",
    "query",
    "query_single",
    "query_required_single",
    "query_json",
    "query_single_json",
    "query_required_single_json",
    "execute",
];

/// One detected PyQL call-site argument.
#[derive(Debug, Clone, PartialEq)]
pub struct PyqlMatch {
    /// Decoded string value — i.e. the actual runtime string
    /// `pylon_core::parse::parse` would receive, with Python escape
    /// sequences resolved (or left verbatim for raw-prefixed strings).
    pub text: String,
    /// `raw_pos[i]` is the 0-based `(line, col)` in the original source
    /// file of the source byte(s) that produced `text.as_bytes()[i]`.
    /// Always the same length as `text.len()`.
    pub raw_pos: Vec<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PrevToken {
    Ident,
    Def,
    Dot,
    Other,
}

struct Scanner<'a> {
    bytes: &'a [u8],
    pos: usize,
    line: u32,
    col: u32,
}

impl<'a> Scanner<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Scanner { bytes, pos: 0, line: 0, col: 0 }
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn peek(&self) -> Option<u8> {
        self.peek_at(0)
    }

    /// Consume one byte, updating line/col. Returns the consumed byte.
    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 0;
        } else {
            self.col += 1;
        }
        Some(b)
    }

    fn pos_now(&self) -> (u32, u32) {
        (self.line, self.col)
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') => {
                    self.bump();
                }
                Some(b'#') => {
                    while let Some(b) = self.peek() {
                        if b == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                // Line continuation outside a string.
                Some(b'\\') if self.peek_at(1) == Some(b'\n') => {
                    self.bump();
                    self.bump();
                }
                _ => break,
            }
        }
    }

    fn is_ident_start(b: u8) -> bool {
        b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
    }

    fn is_ident_continue(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
    }

    fn read_ident(&mut self) -> String {
        let mut s = String::new();
        while let Some(b) = self.peek() {
            if Self::is_ident_continue(b) {
                s.push(b as char);
                self.bump();
            } else {
                break;
            }
        }
        s
    }

    /// Looks (without side effects beyond the actual consume-on-match) for
    /// a string-literal prefix (`r`/`R`/`b`/`B`/`f`/`F`/`u`/`U`, 0-2 of
    /// them) immediately followed by a quote character. Returns the
    /// prefix's lowercase letters if this position really is a string
    /// literal start, so the caller can fall back to identifier tokenizing
    /// otherwise (a bare `r`/`f`/... identifier is common and must not be
    /// swallowed).
    fn string_prefix_len(&self) -> Option<usize> {
        let is_prefix_letter = |b: u8| matches!(b, b'r' | b'R' | b'b' | b'B' | b'f' | b'F' | b'u' | b'U');
        for len in [2usize, 1, 0] {
            if len > 0 {
                let mut ok = true;
                for i in 0..len {
                    match self.peek_at(i) {
                        Some(b) if is_prefix_letter(b) => {}
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    continue;
                }
            }
            if matches!(self.peek_at(len), Some(b'\'') | Some(b'"')) {
                return Some(len);
            }
        }
        None
    }

    /// Reads a string literal (the scanner must be positioned exactly at
    /// its start, as confirmed by `string_prefix_len`). Returns `None`
    /// for f-strings (deliberately unsupported — can't be statically
    /// resolved as literal PyQL text) after still fully consuming them so
    /// the outer scan doesn't get confused by their contents.
    fn read_string(&mut self) -> Option<PyqlMatch> {
        let prefix_len = self.string_prefix_len()?;
        let mut prefix = String::new();
        for _ in 0..prefix_len {
            prefix.push(self.bump().unwrap() as char);
        }
        let is_raw = prefix.to_ascii_lowercase().contains('r');
        let is_fstring = prefix.to_ascii_lowercase().contains('f');

        let quote = self.peek().unwrap();
        let triple = self.peek_at(1) == Some(quote) && self.peek_at(2) == Some(quote);
        if triple {
            self.bump();
            self.bump();
            self.bump();
        } else {
            self.bump();
        }

        let mut text = String::new();
        let mut raw_pos = Vec::new();

        while let Some(b) = self.peek() {
            if b == quote {
                if triple {
                    if self.peek_at(1) == Some(quote) && self.peek_at(2) == Some(quote) {
                        self.bump();
                        self.bump();
                        self.bump();
                        break;
                    }
                } else {
                    self.bump();
                    break;
                }
            }

            if !triple && b == b'\n' {
                // Unterminated single-line string — bail out, treating
                // whatever we've consumed as the (malformed) content.
                break;
            }

            if b == b'\\' && self.peek_at(1).is_some() {
                let esc_pos = self.pos_now();
                self.bump(); // backslash
                let next_pos = self.pos_now();
                let next = self.bump().unwrap(); // escaped char — never a terminator

                if is_raw {
                    text.push('\\');
                    raw_pos.push(esc_pos);
                    text.push(next as char);
                    raw_pos.push(next_pos);
                } else {
                    match next {
                        b'\n' => {} // line continuation: escape swallows the newline entirely
                        b'n' => {
                            text.push('\n');
                            raw_pos.push(esc_pos);
                        }
                        b't' => {
                            text.push('\t');
                            raw_pos.push(esc_pos);
                        }
                        b'r' => {
                            text.push('\r');
                            raw_pos.push(esc_pos);
                        }
                        b'\\' | b'\'' | b'"' => {
                            text.push(next as char);
                            raw_pos.push(esc_pos);
                        }
                        _ => {
                            // Unrecognized escape: CPython keeps it literal
                            // (backslash + char), which is also exactly
                            // what we need for position-mapping fidelity.
                            text.push('\\');
                            raw_pos.push(esc_pos);
                            text.push(next as char);
                            raw_pos.push(next_pos);
                        }
                    }
                }
                continue;
            }

            let p = self.pos_now();
            self.bump();
            text.push(b as char);
            raw_pos.push(p);
        }

        if is_fstring {
            None
        } else {
            Some(PyqlMatch { text, raw_pos })
        }
    }

    /// After a matched call-head `(`, look for `[NAME '='] STRING` as the
    /// first argument — the only shape we treat as a PyQL literal.
    fn try_first_arg_string(&mut self) -> Option<PyqlMatch> {
        self.skip_ws_and_comments();
        // Optional `name =` keyword-argument prefix (but not `==`).
        if self.peek().map(Self::is_ident_start).unwrap_or(false) {
            let save = (self.pos, self.line, self.col);
            let _ = self.read_ident();
            self.skip_ws_and_comments();
            if self.peek() == Some(b'=') && self.peek_at(1) != Some(b'=') {
                self.bump();
                self.skip_ws_and_comments();
            } else {
                (self.pos, self.line, self.col) = save;
            }
        }
        if self.string_prefix_len().is_some() {
            self.read_string()
        } else {
            None
        }
    }
}

/// Scan `source` (the full text of a `.py` file) for PyQL call-site string
/// arguments.
pub fn scan(source: &str) -> Vec<PyqlMatch> {
    let mut s = Scanner::new(source.as_bytes());
    let mut matches = Vec::new();
    let mut prev = PrevToken::Other;

    while let Some(b) = s.peek() {
        if b == b' ' || b == b'\t' || b == b'\r' || b == b'\n' {
            s.bump();
            continue;
        }
        if b == b'#' {
            s.skip_ws_and_comments();
            continue;
        }
        if b == b'\\' && s.peek_at(1) == Some(b'\n') {
            s.bump();
            s.bump();
            continue;
        }

        if Scanner::is_ident_start(b) && s.string_prefix_len().is_some() {
            // A string literal (possibly prefixed) — never a call head.
            s.read_string();
            prev = PrevToken::Other;
            continue;
        }

        if Scanner::is_ident_start(b) {
            let ident = s.read_ident();
            prev = if ident == "def" || ident == "class" {
                PrevToken::Def
            } else {
                if ALLOWLIST.contains(&ident.as_str()) && !matches!(prev, PrevToken::Def) {
                    // Candidate call head — check what follows (skipping
                    // whitespace/comments, since `foo (x)` is valid Python).
                    let save = (s.pos, s.line, s.col);
                    s.skip_ws_and_comments();
                    if s.peek() == Some(b'(') {
                        s.bump();
                        if let Some(m) = s.try_first_arg_string() {
                            matches.push(m);
                        }
                    } else {
                        (s.pos, s.line, s.col) = save;
                    }
                }
                PrevToken::Ident
            };
            continue;
        }

        if b == b'.' {
            s.bump();
            prev = PrevToken::Dot;
            continue;
        }

        // Any other token (operators, numbers, punctuation, unmatched
        // parens from a call we didn't treat specially, ...): consume one
        // byte and move on. We don't need full expression parsing — only
        // identifier-chain and call-head tracking.
        s.bump();
        prev = PrevToken::Other;
    }

    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_pylon_query_compile() {
        let matches = scan(r#"pylon.query.compile("select Person")"#);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].text, "select Person");
    }

    #[test]
    fn detects_every_allowlisted_client_method() {
        for name in ALLOWLIST {
            let src = format!(r#"client.{name}("select Person")"#);
            let matches = scan(&src);
            assert_eq!(matches.len(), 1, "expected a match for client.{name}(...)");
            assert_eq!(matches[0].text, "select Person");
        }
    }

    #[test]
    fn ignores_unrelated_method_names() {
        let matches = scan(r#"foo.bar("select Person")"#);
        assert!(matches.is_empty());
    }

    #[test]
    fn ignores_function_definitions_named_like_entry_points() {
        // `def compile(...)` is a definition, not a call — must not match.
        let matches = scan("def compile(query=\"not pyql, just a default\"):\n    pass\n");
        assert!(matches.is_empty());
    }

    #[test]
    fn accepts_keyword_argument_form() {
        let matches = scan(r#"pylon.query.compile(query="select Person")"#);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].text, "select Person");
    }

    #[test]
    fn skips_f_strings_entirely() {
        let matches = scan(r#"client.query(f"select {thing}")"#);
        assert!(matches.is_empty());
    }

    #[test]
    fn skips_f_strings_but_still_finds_the_next_call() {
        let src = r#"client.query(f"select {thing}")
client.query("select Person")
"#;
        let matches = scan(src);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].text, "select Person");
    }

    #[test]
    fn handles_raw_string_prefix() {
        let matches = scan(r#"client.query(r"select Person")"#);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].text, "select Person");
    }

    #[test]
    fn decodes_escaped_quote_and_finds_correct_string_end() {
        let src = r#"client.query('select Person filter .name = \'Alice\'')
client.query("select Company")
"#;
        let matches = scan(src);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].text, "select Person filter .name = 'Alice'");
        assert_eq!(matches[1].text, "select Company");
    }

    #[test]
    fn triple_quoted_multiline_query_maps_positions_per_line() {
        let src = "client.query(\"\"\"\n  select Person\n  filter .name = 'x'\n\"\"\")\n";
        let matches = scan(src);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].text, "\n  select Person\n  filter .name = 'x'\n");
        // First byte of the decoded text is the newline right after the
        // opening \"\"\", i.e. still on file line 0 (0-based).
        assert_eq!(matches[0].raw_pos[0].0, 0);
        // The 'select' text starts on the next file line.
        let select_byte = matches[0].text.find("select").unwrap();
        assert_eq!(matches[0].raw_pos[select_byte].0, 1);
    }

    #[test]
    fn nested_unrelated_calls_dont_confuse_the_scanner() {
        let src = r#"foo(bar(1, 2), baz("nope"))
client.query("select Person")
"#;
        let matches = scan(src);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].text, "select Person");
    }

    #[test]
    fn ignores_call_with_non_string_first_argument() {
        let matches = scan(r#"client.query(some_variable)"#);
        assert!(matches.is_empty());
    }

    #[test]
    fn allows_whitespace_between_callee_and_paren() {
        let matches = scan("client.query (\"select Person\")");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].text, "select Person");
    }
}
