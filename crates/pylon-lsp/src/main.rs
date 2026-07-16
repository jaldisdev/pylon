//! `pylon-lsp` — a secondary LSP server attached to the `"Python"` language
//! (alongside whatever primary Python server is already configured, e.g.
//! basedpyright/ruff) that finds embedded PyQL query strings and republishes
//! `pylon_core`'s own parse diagnostics at the right location in the `.py`
//! file. Syntax-only for now — see the design plan for why schema-aware
//! type-checking is a separate, later effort.

mod diagnostics;
mod scan;

use std::error::Error;

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

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    eprintln!("pylon-lsp: starting");
    let (connection, io_threads) = Connection::stdio();

    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        ..Default::default()
    };
    let init_params = connection.initialize(serde_json::to_value(&capabilities)?)?;
    let _params: InitializeParams = serde_json::from_value(init_params)?;

    main_loop(&connection)?;
    io_threads.join()?;
    eprintln!("pylon-lsp: shut down");
    Ok(())
}

fn main_loop(connection: &Connection) -> Result<(), Box<dyn Error + Sync + Send>> {
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
                    publish_for(connection, &params.text_document.uri, &params.text_document.text)?;
                }
                DidChangeTextDocument::METHOD => {
                    let params: DidChangeTextDocumentParams = serde_json::from_value(not.params)?;
                    if let Some(change) = params.content_changes.into_iter().last() {
                        publish_for(connection, &params.text_document.uri, &change.text)?;
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

/// Scans `text` for PyQL call-site strings, parses each, and publishes any
/// syntax errors as diagnostics for `uri` (replacing whatever was
/// previously published for it, per standard LSP `publishDiagnostics`
/// semantics).
fn publish_for(
    connection: &Connection,
    uri: &Uri,
    text: &str,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    let matches = scan::scan(text);
    let mut diags: Vec<Diagnostic> = Vec::new();
    for m in &matches {
        if let Err(err) = pylon_core::parse::parse(&m.text) {
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
