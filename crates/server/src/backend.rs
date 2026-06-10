//! The tower-lsp language server backend.
//!
//! Holds open documents as `ropey` ropes, a shared [`Analyzer`] (the embedded
//! schema), and a workspace symbol [`WorkspaceIndex`]. Uses FULL document sync:
//! INI files are small, so re-parsing on each change is simpler and reliably
//! correct, and the analysis is fast enough not to need debouncing.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use dashmap::DashMap;
use genparser_analysis::index::{definitions_in, WorkspaceIndex};
use genparser_analysis::nav::{hover_at, reference_at, HoverInfo};
use genparser_analysis::{completion, diagnostics, semantic, Analyzer};
use ropey::Rope;
use tower_lsp::lsp_types::*;
use tower_lsp::{jsonrpc::Result, Client, LanguageServer};

use crate::convert;

pub struct Backend {
    client: Client,
    analyzer: Arc<Analyzer>,
    /// Open documents, keyed by URI.
    docs: DashMap<Url, Rope>,
    index: RwLock<WorkspaceIndex>,
    /// Workspace roots, captured at `initialize` and scanned in `initialized`.
    roots: Mutex<Vec<PathBuf>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Backend {
            client,
            analyzer: Arc::new(Analyzer::embedded()),
            docs: DashMap::new(),
            index: RwLock::new(WorkspaceIndex::new()),
            roots: Mutex::new(Vec::new()),
        }
    }

    /// Parse `text`, update the index for `uri`, and publish diagnostics.
    async fn refresh(&self, uri: &Url, rope: &Rope) {
        let text = rope.to_string();
        let parse = self.analyzer.parse(&text);

        // Update the cross-file index with this file's definitions.
        let defs = definitions_in(&self.analyzer, &parse, uri.as_str());
        if let Ok(mut idx) = self.index.write() {
            idx.set_file(uri.as_str(), defs);
        }

        let lsp_diags: Vec<Diagnostic> = {
            let idx = self.index.read().ok();
            let diags = diagnostics::diagnose(&self.analyzer, &parse, idx.as_deref());
            diags.iter().map(|d| convert::to_lsp_diagnostic(rope, d)).collect()
        };
        self.client
            .publish_diagnostics(uri.clone(), lsp_diags, None)
            .await;
    }

    /// Best-effort scan of the workspace roots for `.ini` files to seed the
    /// index, so references resolve before a file is opened.
    fn scan_workspace(&self) {
        let roots = self.roots.lock().map(|r| r.clone()).unwrap_or_default();
        let Ok(mut idx) = self.index.write() else { return };
        for root in roots {
            for entry in walkdir::WalkDir::new(&root)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("ini") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(path) else { continue };
                let Ok(uri) = Url::from_file_path(path) else { continue };
                let parse = self.analyzer.parse(&text);
                let defs = definitions_in(&self.analyzer, &parse, uri.as_str());
                idx.set_file(uri.as_str(), defs);
            }
        }
    }

    /// Resolve a URI's text to a rope, preferring open documents and falling
    /// back to disk (for go-to-definition into unopened files).
    fn rope_for(&self, uri: &Url) -> Option<Rope> {
        if let Some(doc) = self.docs.get(uri) {
            return Some(doc.clone());
        }
        let path = uri.to_file_path().ok()?;
        std::fs::read_to_string(path).ok().map(|s| Rope::from_str(&s))
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Capture workspace roots.
        let mut roots = Vec::new();
        if let Some(folders) = params.workspace_folders {
            for f in folders {
                if let Ok(p) = f.uri.to_file_path() {
                    roots.push(p);
                }
            }
        } else if let Some(root) = params.root_uri.and_then(|u| u.to_file_path().ok()) {
            roots.push(root);
        }
        if let Ok(mut r) = self.roots.lock() {
            *r = roots;
        }

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "genparser-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["=".into(), " ".into()]),
                    ..Default::default()
                }),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: convert::semantic_legend(),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: Some(false),
                            ..Default::default()
                        },
                    ),
                ),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.scan_workspace();
        // Re-publish diagnostics for any already-open docs now that the index
        // is populated (so cross-file references resolve).
        let open: Vec<(Url, Rope)> =
            self.docs.iter().map(|e| (e.key().clone(), e.value().clone())).collect();
        for (uri, rope) in open {
            self.refresh(&uri, &rope).await;
        }
        self.client
            .log_message(MessageType::INFO, "genparser language server ready")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let rope = Rope::from_str(&params.text_document.text);
        self.docs.insert(uri.clone(), rope.clone());
        self.refresh(&uri, &rope).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        // FULL sync: the last change carries the entire document text.
        if let Some(change) = params.content_changes.into_iter().last() {
            let rope = Rope::from_str(&change.text);
            self.docs.insert(uri.clone(), rope.clone());
            self.refresh(&uri, &rope).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        // Keep the file's symbols in the index (it still exists on disk); just
        // drop the in-memory buffer.
        self.docs.remove(&params.text_document.uri);
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let Some(rope) = self.docs.get(&uri).map(|d| d.clone()) else {
            return Ok(None);
        };
        let parse = self.analyzer.parse(&rope.to_string());
        let offset = convert::position_to_offset(&rope, pos);
        let idx = self.index.read().ok();
        let items: Vec<CompletionItem> =
            completion::complete(&self.analyzer, &parse, offset, idx.as_deref())
                .into_iter()
                .map(convert::to_lsp_completion)
                .collect();
        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let Some(rope) = self.docs.get(&uri).map(|d| d.clone()) else {
            return Ok(None);
        };
        let parse = self.analyzer.parse(&rope.to_string());
        let tokens = semantic::semantic_tokens(&self.analyzer, &parse);
        let data = convert::to_lsp_semantic_tokens(&rope, &tokens);
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let Some(rope) = self.docs.get(&uri).map(|d| d.clone()) else {
            return Ok(None);
        };
        let parse = self.analyzer.parse(&rope.to_string());
        let offset = convert::position_to_offset(&rope, pos);
        let Some(reference) = reference_at(&self.analyzer, &parse, offset) else {
            return Ok(None);
        };

        let locations: Vec<(String, genparser_analysis::Span)> = {
            let Ok(idx) = self.index.read() else { return Ok(None) };
            idx.locations(reference.kind, &reference.name)
                .iter()
                .map(|l| (l.file.clone(), l.span))
                .collect()
        };

        let mut out = Vec::new();
        for (file, span) in locations {
            let Ok(target_uri) = Url::parse(&file) else { continue };
            if let Some(target_rope) = self.rope_for(&target_uri) {
                out.push(Location {
                    uri: target_uri,
                    range: convert::span_to_range(&target_rope, span),
                });
            }
        }
        if out.is_empty() {
            Ok(None)
        } else {
            Ok(Some(GotoDefinitionResponse::Array(out)))
        }
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let Some(rope) = self.docs.get(&uri).map(|d| d.clone()) else {
            return Ok(None);
        };
        let parse = self.analyzer.parse(&rope.to_string());
        let offset = convert::position_to_offset(&rope, pos);
        let Some(info) = hover_at(&self.analyzer, &parse, offset) else {
            return Ok(None);
        };
        let (markdown, span) = match info {
            HoverInfo::Block { name, span } => {
                let doc = self
                    .analyzer
                    .block(&name)
                    .and_then(|b| b.doc.clone())
                    .unwrap_or_else(|| format!("Top-level block `{name}`."));
                (format!("**block** `{name}`\n\n{doc}"), span)
            }
            HoverInfo::Field {
                name,
                ty,
                parse_fn,
                span,
            } => (
                format!("**field** `{name}`\n\ntype: `{ty:?}`\n\nengine: `{parse_fn}`"),
                span,
            ),
        };
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: markdown,
            }),
            range: Some(convert::span_to_range(&rope, span)),
        }))
    }
}
