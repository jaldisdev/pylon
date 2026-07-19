//! Turns a `pylon_core::error::PyQLSyntaxError` (position relative to an
//! extracted query string) into an LSP `Diagnostic` at the right location
//! in the original `.py` file, using the byte-for-byte source map built by
//! `scan::PyqlMatch`.

use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position as LspPosition, Range};
use pylon_core::error::Position as PyqlPosition;

use crate::scan::PyqlMatch;

/// Finds the byte offset into `text` for a `pylon_core` `Position`
/// (1-based line/col, incremented per byte — matching `pylon_core`'s own
/// lexer convention exactly, see `parse/lexer.rs`). Clamps to `text.len()`
/// if the position is at or past the end (e.g. an "unexpected EOF" error).
fn locate_byte_offset(text: &str, target: &PyqlPosition) -> usize {
    let bytes = text.as_bytes();
    let mut line = 1u32;
    let mut col = 1u32;
    for (i, &b) in bytes.iter().enumerate() {
        if line == target.line && col == target.col {
            return i;
        }
        if b == b'\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    bytes.len()
}

/// Maps a `pylon_core` syntax-error position to an LSP `Range` in the
/// original file, using `m.raw_pos` (0-based `(line, col)` per decoded
/// byte). The range spans one source byte for a visible, non-zero-width
/// squiggle; falls back to a zero-width point at end-of-file if the error
/// points past the last decoded byte.
pub fn error_range(m: &PyqlMatch, position: &PyqlPosition) -> Range {
    let offset = locate_byte_offset(&m.text, position);

    let start = if offset < m.raw_pos.len() {
        m.raw_pos[offset]
    } else {
        // Position is at/past the end of the decoded text (e.g. "unexpected
        // EOF") — anchor on the last known byte, or the match's own start
        // if the string was empty.
        m.raw_pos.last().copied().unwrap_or((0, 0))
    };

    let end = if offset + 1 < m.raw_pos.len() {
        m.raw_pos[offset + 1]
    } else {
        (start.0, start.1 + 1)
    };

    Range {
        start: LspPosition { line: start.0, character: start.1 },
        end: LspPosition { line: end.0, character: end.1 },
    }
}

/// Builds the LSP `Diagnostic` for one PyQL match that failed to parse or
/// compile. Covers every `PyQLError` variant (syntax, type, unknown
/// property/link/parameter, cardinality, fragment) uniformly, tagging the
/// diagnostic's `code` with the `pylon.exceptions.*` class name so an editor
/// can group/filter by error kind the same way the CLI and web API do.
pub fn diagnostic_for(m: &PyqlMatch, err: &pylon_core::error::PyQLError) -> Diagnostic {
    let (class_name, message, position) = err.class_name_message_position();
    Diagnostic {
        range: error_range(m, position),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String(class_name.to_string())),
        source: Some("pylon".to_string()),
        message: message.to_string(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_match(text: &str) -> PyqlMatch {
        // Simulate a triple-quoted string starting at file line 3, col 4,
        // with verbatim (no escapes) content — the common real-world case.
        let mut raw_pos = Vec::new();
        let mut line = 3u32;
        let mut col = 4u32;
        for b in text.bytes() {
            raw_pos.push((line, col));
            if b == b'\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        PyqlMatch { text: text.to_string(), raw_pos }
    }

    #[test]
    fn maps_single_line_error_position() {
        let m = make_match("select Person filter .name = ");
        // pylon_core positions are 1-based; put the error at line 1, col 8
        // (pointing at "Person").
        let pos = PyqlPosition { line: 1, col: 8 };
        let range = error_range(&m, &pos);
        assert_eq!(range.start, LspPosition { line: 3, character: 11 });
    }

    #[test]
    fn maps_multi_line_error_position_to_correct_file_line() {
        let m = make_match("select Person {\n  nam\n}");
        // Error on the second decoded line (the typo'd "nam"), col 3.
        let pos = PyqlPosition { line: 2, col: 3 };
        let range = error_range(&m, &pos);
        // File line 3 (match start) + 1 decoded newline = file line 4.
        assert_eq!(range.start.line, 4);
    }

    #[test]
    fn eof_error_clamps_to_last_byte() {
        let m = make_match("select Person filter .name =");
        let pos = PyqlPosition { line: 1, col: 100 };
        let range = error_range(&m, &pos);
        let (line, character) = *m.raw_pos.last().unwrap();
        assert_eq!(range.start, LspPosition { line, character });
    }
}
