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
use genparser_analysis::diagnostics::DiagnosticsCache;
use genparser_analysis::index::{definitions_in, WorkspaceIndex};
use genparser_analysis::nav::{hover_at, reference_at, HoverInfo};
use genparser_analysis::{completion, diagnostics, semantic, Analyzer};
use genparser_syntax::{Edit, Parse};
use ropey::Rope;
use tower_lsp::lsp_types::*;
use tower_lsp::{jsonrpc::Result, Client, LanguageServer};

use crate::convert::{self, PositionEnc};

/// An open document: its text (as both a rope for position math and a string
/// for the parser) and the parse of that exact text. `did_open`/`did_change`
/// are the only places a new parse is produced for an open document;
/// `did_change` reparses incrementally by splicing at block boundaries.
struct DocumentState {
    rope: Rope,
    /// The same text as `rope`; the source the cached `parse` was built from.
    text: Arc<str>,
    parse: Arc<Parse>,
    version: i32,
    /// Per-block diagnostics, reused across edits for unchanged blocks.
    diag_cache: DiagnosticsCache,
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

    /// Update the cross-file index from the document's cached parse, run
    /// diagnostics (via the per-block cache), and publish. The parse itself is
    /// maintained synchronously by `did_open`/`did_change`.
    async fn refresh(&self, uri: &Url) {
        // Take the cache out so diagnostics run without holding the doc entry
        // (avoids lock-order entanglement with the index RwLock).
        let Some((rope, parse, version, mut cache)) = self.docs.get_mut(uri).map(|mut d| {
            (
                d.rope.clone(),
                d.parse.clone(),
                d.version,
                std::mem::take(&mut d.diag_cache),
            )
        }) else {
            return;
        };

        // `set_file` bumps the index generation only when definition *names*
        // changed, so ordinary keystrokes keep diagnostics caches warm.
        let defs = definitions_in(&self.analyzer, &parse, uri.as_str());
        if let Ok(mut idx) = self.index.write() {
            idx.set_file(uri.as_str(), defs);
        }

        let enc = self.enc();
        let lsp_diags: Vec<Diagnostic> = {
            let idx = self.index.read().ok();
            let diags =
                diagnostics::diagnose_with_cache(&self.analyzer, &parse, idx.as_deref(), &mut cache);
            diags
                .iter()
                .map(|d| convert::to_lsp_diagnostic(&rope, d, enc))
                .collect()
        };

        // Hand the warmed cache back unless a newer change superseded us (the
        // newer change runs its own refresh against its own parse).
        {
            let Some(mut entry) = self.docs.get_mut(uri) else { return };
            if entry.version != version {
                return;
            }
            entry.diag_cache = cache;
        }
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
                // Real game data mixes extension casing (`*.ini` / `*.INI`,
                // e.g. the MappedImages files), so compare case-insensitively.
                if !path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("ini"))
                {
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
                            range: Some(true),
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
        let open: Vec<Url> = self.docs.iter().map(|e| e.key().clone()).collect();
        for uri in open {
            self.refresh(&uri).await;
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
        let text: Arc<str> = params.text_document.text.into();
        let rope = Rope::from_str(&text);
        let parse = Arc::new(self.analyzer.parse(&text));
        let version = params.text_document.version;
        self.docs.insert(
            uri.clone(),
            DocumentState {
                rope,
                text,
                parse,
                version,
                diag_cache: DiagnosticsCache::new(),
            },
        );
        self.refresh(&uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        let enc = self.enc();
        {
            let Some(mut entry) = self.docs.get_mut(&uri) else { return };
            let entry = entry.value_mut();
            // Each change applies to the text produced by the previous one.
            // The parse is kept in lockstep via incremental reparse, so the
            // cost per keystroke is the edited block, not the whole file.
            for change in params.content_changes {
                match change.range {
                    Some(range) => {
                        let start = convert::position_to_offset(&entry.rope, range.start, enc);
                        let old_end = convert::position_to_offset(&entry.rope, range.end, enc);
                        convert::apply_change(&mut entry.rope, Some(range), &change.text, enc);
                        let new_text: Arc<str> = entry.rope.to_string().into();
                        let edit = Edit {
                            start: start as usize,
                            old_end: old_end as usize,
                            new_len: change.text.len(),
                        };
                        let (parse, _strategy) =
                            self.analyzer
                                .reparse(&entry.parse, &entry.text, &new_text, edit);
                        entry.parse = Arc::new(parse);
                        entry.text = new_text;
                    }
                    None => {
                        // Full-document replacement.
                        entry.rope = Rope::from_str(&change.text);
                        entry.text = change.text.into();
                        entry.parse = Arc::new(self.analyzer.parse(&entry.text));
                    }
                }
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

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        let uri = params.text_document.uri;
        let Some((rope, parse)) = self.doc(&uri) else {
            return Ok(None);
        };
        let enc = self.enc();
        let start = convert::position_to_offset(&rope, params.range.start, enc);
        let end = convert::position_to_offset(&rope, params.range.end, enc);
        let tokens = semantic::semantic_tokens_range(
            &self.analyzer,
            &parse,
            genparser_analysis::Span::new(start, end),
        );
        let data = convert::to_lsp_semantic_tokens(&rope, &tokens, enc);
        Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
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
