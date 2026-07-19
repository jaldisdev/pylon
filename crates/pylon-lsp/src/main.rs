//! `pylon-lsp` — a secondary LSP server attached to the `"Python"` language
//! (alongside whatever primary Python server is already configured, e.g.
//! basedpyright/ruff) that finds embedded PyQL query strings and republishes
//! `pylon_core`'s own compile diagnostics at the right location in the `.py`
//! file. Runs the full compiler (syntax, type, and "did you mean"
//! resolution diagnostics) once a schema has been loaded from
//! `.pylon/schema.json` (see `schema.rs`); falls back to parser-only
//! (syntax) diagnostics when no schema export is found yet.

mod diagnostics;
mod scan;
mod schema;

use std::error::Error;
use std::path::PathBuf;

use lsp_server::{Connection, Message, Notification as LspNotification};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification,
    PublishDiagnostics,
};
use lsp_types::{
    Diagnostic, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, InitializeParams, PublishDiagnosticsParams, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};

use schema::SchemaState;

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    eprintln!("pylon-lsp: starting");
    let (connection, io_threads) = Connection::stdio();

    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        ..Default::default()
    };
    let init_params = connection.initialize(serde_json::to_value(&capabilities)?)?;
    let params: InitializeParams = serde_json::from_value(init_params)?;

    let workspace_root = workspace_root(&params);
    let mut schema_state = SchemaState::new(workspace_root);

    main_loop(&connection, &mut schema_state)?;
    io_threads.join()?;
    eprintln!("pylon-lsp: shut down");
    Ok(())
}

/// Prefers the first `workspace_folders` entry (the modern field) over the
/// deprecated single `root_uri`, matching what most clients (including Zed)
/// actually populate.
fn workspace_root(params: &InitializeParams) -> Option<PathBuf> {
    let uri = params
        .workspace_folders
        .as_ref()
        .and_then(|folders| folders.first())
        .map(|f| &f.uri)
        .or(params.root_uri.as_ref())?;
    uri_to_path(uri)
}

/// Minimal `file://` URI → filesystem path decoder (percent-decodes escapes)
/// — `lsp_types::Uri` wraps `fluent_uri` with no built-in file-path accessor.
fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let rest = uri.as_str().strip_prefix("file://")?;
    Some(PathBuf::from(percent_decode(rest)))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn main_loop(
    connection: &Connection,
    schema_state: &mut SchemaState,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                // No other requests handled in v1 (diagnostics-only server).
            }
            Message::Notification(not) => match not.method.as_str() {
                DidOpenTextDocument::METHOD => {
                    let params: DidOpenTextDocumentParams = serde_json::from_value(not.params)?;
                    publish_for(
                        connection,
                        schema_state,
                        &params.text_document.uri,
                        &params.text_document.text,
                    )?;
                }
                DidChangeTextDocument::METHOD => {
                    let params: DidChangeTextDocumentParams = serde_json::from_value(not.params)?;
                    if let Some(change) = params.content_changes.into_iter().last() {
                        publish_for(connection, schema_state, &params.text_document.uri, &change.text)?;
                    }
                }
                DidCloseTextDocument::METHOD => {
                    let params: DidCloseTextDocumentParams = serde_json::from_value(not.params)?;
                    send_diagnostics(connection, params.text_document.uri, vec![])?;
                }
                _ => {}
            },
            Message::Response(_) => {}
        }
    }
    Ok(())
}

/// Scans `text` for PyQL call-site strings and publishes diagnostics for
/// `uri` (replacing whatever was previously published for it, per standard
/// LSP `publishDiagnostics` semantics). Runs the full compiler
/// (`pylon_core::query::compile`) against the schema loaded from
/// `.pylon/schema.json` when one is available — surfacing semantic errors
/// (unknown property/link with "did you mean", type mismatches) alongside
/// syntax errors — and falls back to parser-only checking otherwise.
fn publish_for(
    connection: &Connection,
    schema_state: &mut SchemaState,
    uri: &Uri,
    text: &str,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    schema_state.reload_if_changed();
    let matches = scan::scan(text);
    let mut diags: Vec<Diagnostic> = Vec::new();
    for m in &matches {
        let result: Result<(), pylon_core::error::PyQLError> = match schema_state.get() {
            Some(schema) => pylon_core::query::compile(&m.text, schema).map(|_| ()),
            None => pylon_core::parse::parse(&m.text)
                .map(|_| ())
                .map_err(pylon_core::error::PyQLError::Syntax),
        };
        if let Err(err) = result {
            diags.push(diagnostics::diagnostic_for(m, &err));
        }
    }
    send_diagnostics(connection, uri.clone(), diags)
}

fn send_diagnostics(
    connection: &Connection,
    uri: Uri,
    diagnostics: Vec<Diagnostic>,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    let params = PublishDiagnosticsParams { uri, diagnostics, version: None };
    let notification = LspNotification {
        method: PublishDiagnostics::METHOD.to_string(),
        params: serde_json::to_value(&params)?,
    };
    connection.sender.send(Message::Notification(notification))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn percent_decode_leaves_plain_paths_untouched() {
        assert_eq!(percent_decode("/workspace/my-project"), "/workspace/my-project");
    }

    #[test]
    fn percent_decode_resolves_escaped_spaces() {
        assert_eq!(percent_decode("/workspace/My%20Project"), "/workspace/My Project");
    }

    #[test]
    fn percent_decode_ignores_a_trailing_incomplete_escape() {
        assert_eq!(percent_decode("/foo%2"), "/foo%2");
    }

    #[test]
    fn uri_to_path_strips_file_scheme() {
        let uri = Uri::from_str("file:///workspace/my-project").unwrap();
        assert_eq!(uri_to_path(&uri), Some(PathBuf::from("/workspace/my-project")));
    }

    #[test]
    fn uri_to_path_rejects_non_file_schemes() {
        let uri = Uri::from_str("untitled:Untitled-1").unwrap();
        assert_eq!(uri_to_path(&uri), None);
    }
}
