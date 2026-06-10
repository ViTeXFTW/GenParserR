//! The tower-lsp language server backend.
//!
//! Holds open documents as [`DocumentState`]s — a `ropey` rope plus the cached
//! parse for that text — alongside a shared [`Analyzer`] (the embedded schema)
//! and a workspace symbol [`WorkspaceIndex`]. Document sync is INCREMENTAL:
//! `didChange` deltas are applied to the rope and the document is re-parsed
//! once per change batch; read-only requests reuse the cached parse.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use dashmap::DashMap;
use genparser_analysis::index::{definitions_in, WorkspaceIndex};
use genparser_analysis::nav::{hover_at, reference_at, HoverInfo};
use genparser_analysis::{completion, diagnostics, semantic, Analyzer};
use genparser_syntax::Parse;
use ropey::Rope;
use tower_lsp::lsp_types::*;
use tower_lsp::{jsonrpc::Result, Client, LanguageServer};

use crate::convert::{self, PositionEnc};

/// An open document: its text and the parse of that exact text. `refresh` is
/// the only place a new parse is produced for an open document.
struct DocumentState {
    rope: Rope,
    parse: Arc<Parse>,
    version: i32,
}

pub struct Backend {
    client: Client,
    analyzer: Arc<Analyzer>,
    /// Open documents, keyed by URI.
    docs: DashMap<Url, DocumentState>,
    index: RwLock<WorkspaceIndex>,
    /// Workspace roots, captured at `initialize` and scanned in `initialized`.
    roots: Mutex<Vec<PathBuf>>,
    /// Position encoding negotiated at `initialize` (UTF-16 until then).
    encoding: OnceLock<PositionEnc>,
}

/// Read a file leniently: real INIs predate UTF-8 (Windows-1252 comments), so
/// a strict `read_to_string` would silently drop them from the index.
fn read_lossy(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Backend {
            client,
            analyzer: Arc::new(Analyzer::embedded()),
            docs: DashMap::new(),
            index: RwLock::new(WorkspaceIndex::new()),
            roots: Mutex::new(Vec::new()),
            encoding: OnceLock::new(),
        }
    }

    fn enc(&self) -> PositionEnc {
        self.encoding.get().copied().unwrap_or_default()
    }

    /// Re-parse the document's current text, cache the parse, and publish.
    /// Skips publishing if a newer version landed while parsing.
    async fn refresh(&self, uri: &Url) {
        let Some((rope, version)) = self
            .docs
            .get(uri)
            .map(|d| (d.rope.clone(), d.version))
        else {
            return;
        };
        let parse = Arc::new(self.analyzer.parse(&rope.to_string()));
        {
            let Some(mut entry) = self.docs.get_mut(uri) else { return };
            if entry.version != version {
                return; // superseded; the newer change runs its own refresh
            }
            entry.parse = parse.clone();
        }
        self.publish(uri, &rope, &parse, version).await;
    }

    /// Update the cross-file index from `parse` and publish diagnostics.
    async fn publish(&self, uri: &Url, rope: &Rope, parse: &Parse, version: i32) {
        let defs = definitions_in(&self.analyzer, parse, uri.as_str());
        if let Ok(mut idx) = self.index.write() {
            idx.set_file(uri.as_str(), defs);
        }

        let enc = self.enc();
        let lsp_diags: Vec<Diagnostic> = {
            let idx = self.index.read().ok();
            let diags = diagnostics::diagnose(&self.analyzer, parse, idx.as_deref());
            diags
                .iter()
                .map(|d| convert::to_lsp_diagnostic(rope, d, enc))
                .collect()
        };
        self.client
            .publish_diagnostics(uri.clone(), lsp_diags, Some(version))
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
                let Some(text) = read_lossy(path) else { continue };
                let Ok(uri) = Url::from_file_path(path) else { continue };
                let parse = self.analyzer.parse(&text);
                let defs = definitions_in(&self.analyzer, &parse, uri.as_str());
                idx.set_file(uri.as_str(), defs);
            }
        }
    }

    /// The cached state for an open document (rope + parse), if any.
    fn doc(&self, uri: &Url) -> Option<(Rope, Arc<Parse>)> {
        self.docs
            .get(uri)
            .map(|d| (d.rope.clone(), d.parse.clone()))
    }

    /// Resolve a URI's text to a rope, preferring open documents and falling
    /// back to disk (for go-to-definition into unopened files).
    fn rope_for(&self, uri: &Url) -> Option<Rope> {
        if let Some(doc) = self.docs.get(uri) {
            return Some(doc.rope.clone());
        }
        let path = uri.to_file_path().ok()?;
        read_lossy(&path).map(|s| Rope::from_str(&s))
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

        let (enc, enc_kind) = convert::negotiate_encoding(&params.capabilities);
        let _ = self.encoding.set(enc);

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "genparser-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                position_encoding: Some(enc_kind),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
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
        // is populated (so cross-file references resolve). The cached parse is
        // still valid — only the index changed.
        let open: Vec<(Url, Rope, Arc<Parse>, i32)> = self
            .docs
            .iter()
            .map(|e| (e.key().clone(), e.rope.clone(), e.parse.clone(), e.version))
            .collect();
        for (uri, rope, parse, version) in open {
            self.publish(&uri, &rope, &parse, version).await;
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
        let parse = Arc::new(self.analyzer.parse(&params.text_document.text));
        let version = params.text_document.version;
        self.docs.insert(
            uri.clone(),
            DocumentState {
                rope: rope.clone(),
                parse: parse.clone(),
                version,
            },
        );
        self.publish(&uri, &rope, &parse, version).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        let enc = self.enc();
        {
            let Some(mut entry) = self.docs.get_mut(&uri) else { return };
            // Each change applies to the text produced by the previous one.
            for change in params.content_changes {
                convert::apply_change(&mut entry.rope, change.range, &change.text, enc);
            }
            entry.version = version;
        }
        self.refresh(&uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        // Keep the file's symbols in the index (it still exists on disk); just
        // drop the in-memory buffer.
        self.docs.remove(&params.text_document.uri);
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let Some((rope, parse)) = self.doc(&uri) else {
            return Ok(None);
        };
        let offset = convert::position_to_offset(&rope, pos, self.enc());
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
        let Some((rope, parse)) = self.doc(&uri) else {
            return Ok(None);
        };
        let tokens = semantic::semantic_tokens(&self.analyzer, &parse);
        let data = convert::to_lsp_semantic_tokens(&rope, &tokens, self.enc());
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
        let Some((rope, parse)) = self.doc(&uri) else {
            return Ok(None);
        };
        let enc = self.enc();
        let offset = convert::position_to_offset(&rope, pos, enc);
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
                    range: convert::span_to_range(&target_rope, span, enc),
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
        let Some((rope, parse)) = self.doc(&uri) else {
            return Ok(None);
        };
        let enc = self.enc();
        let offset = convert::position_to_offset(&rope, pos, enc);
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
            range: Some(convert::span_to_range(&rope, span, enc)),
        }))
    }
}
