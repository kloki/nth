//! The few LSP structures nth reads or writes, by hand. Fields nth never
//! looks at are left out; serde skips them on the way in.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Zero-based, with `character` in UTF-16 code units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Code {
    Number(i64),
    String(String),
}

pub const SEVERITY_ERROR: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Diagnostic {
    pub range: Range,
    /// 1 error, 2 warning, 3 information, 4 hint. Servers may leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Code>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub message: String,
}

impl Diagnostic {
    pub fn is_error(&self) -> bool {
        self.severity == Some(SEVERITY_ERROR)
    }
}

// Server → client.

#[derive(Debug, Default, Deserialize)]
pub struct InitializeResult {
    #[serde(default)]
    pub capabilities: ServerCapabilities,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    pub text_document_sync: Option<TextDocumentSync>,
    /// Only its presence matters: the server answers `textDocument/diagnostic`.
    pub diagnostic_provider: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum TextDocumentSync {
    Kind(u8),
    Options { change: Option<u8> },
}

pub const SYNC_INCREMENTAL: u8 = 2;

impl ServerCapabilities {
    pub fn incremental_sync(&self) -> bool {
        let kind = match &self.text_document_sync {
            Some(TextDocumentSync::Kind(kind)) => Some(*kind),
            Some(TextDocumentSync::Options { change }) => *change,
            None => None,
        };
        kind == Some(SYNC_INCREMENTAL)
    }
}

#[derive(Debug, Deserialize)]
pub struct PublishDiagnosticsParams {
    pub uri: String,
    pub version: Option<i32>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registration {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub register_options: Option<DiagnosticRegistrationOptions>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticRegistrationOptions {
    pub identifier: Option<String>,
    #[serde(default)]
    pub workspace_diagnostics: bool,
}

#[derive(Debug, Deserialize)]
pub struct RegistrationParams {
    #[serde(default)]
    pub registrations: Vec<Registration>,
}

#[derive(Debug, Deserialize)]
pub struct Unregistration {
    pub id: String,
    pub method: String,
}

#[derive(Debug, Deserialize)]
pub struct UnregistrationParams {
    /// Misspelled in the spec itself, and kept that way for compatibility.
    #[serde(default)]
    pub unregisterations: Vec<Unregistration>,
}

#[derive(Debug, Deserialize)]
pub struct ConfigurationParams {
    #[serde(default)]
    pub items: Vec<ConfigurationItem>,
}

#[derive(Debug, Deserialize)]
pub struct ConfigurationItem {
    pub section: Option<String>,
}

/// The answer to `textDocument/diagnostic`. An "unchanged" report has no
/// `items`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentDiagnosticReport {
    pub items: Option<Vec<Diagnostic>>,
    #[serde(default)]
    pub related_documents: HashMap<String, DocumentDiagnosticReport>,
}

// Client → server.

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentItem<'a> {
    pub uri: &'a str,
    pub language_id: &'a str,
    pub version: i32,
    pub text: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DidOpenParams<'a> {
    pub text_document: TextDocumentItem<'a>,
}

#[derive(Debug, Serialize)]
pub struct VersionedTextDocumentIdentifier<'a> {
    pub uri: &'a str,
    pub version: i32,
}

#[derive(Debug, Serialize)]
pub struct ContentChange<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
    pub text: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DidChangeParams<'a> {
    pub text_document: VersionedTextDocumentIdentifier<'a>,
    pub content_changes: Vec<ContentChange<'a>>,
}

pub const FILE_CREATED: u8 = 1;
pub const FILE_CHANGED: u8 = 2;

#[derive(Debug, Serialize)]
pub struct FileEvent<'a> {
    pub uri: &'a str,
    #[serde(rename = "type")]
    pub kind: u8,
}

#[derive(Debug, Serialize)]
pub struct DidChangeWatchedFilesParams<'a> {
    pub changes: Vec<FileEvent<'a>>,
}
