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
use genparser_analysis::index::{definitions_in, references_in, WorkspaceIndex};
use genparser_analysis::nav::{definition_at, hover_at, reference_at, HoverInfo};
use genparser_analysis::{actions, completion, diagnostics, format, outline, semantic, Analyzer};
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
    /// The last `semanticTokens/full` response (result id + encoded data),
    /// kept so `full/delta` can answer with a splice instead of the world.
    last_semantic: Option<(u64, Vec<SemanticToken>)>,
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
    /// Whether `textDocument/formatting` is enabled, from the client's
    /// `initializationOptions` (`{"format": {"enable": true}}`). Off by
    /// default: format-on-save rewriting a whole hand-indented game file is
    /// surprising, so formatting is opt-in per editor.
    format_enabled: OnceLock<bool>,
    /// Monotonic id source for semantic-token results (delta bookkeeping).
    semantic_result_id: std::sync::atomic::AtomicU64,
}

/// Read a file leniently: real INIs predate UTF-8 (Windows-1252 comments), so
/// a strict `read_to_string` would silently drop them from the index.
fn read_lossy(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Normalise a client-supplied URI to the form [`Url::from_file_path`] produces.
/// On Windows, VS Code sends `file:///c%3A/…` (percent-encoded colon, lowercase
/// drive letter) while `from_file_path` produces `file:///C:/…`. The mismatch
/// makes the same file land in the `WorkspaceIndex` under two different keys,
/// so every definition appears duplicated. Round-tripping through the file-path
/// canonicalises both percent-encoding and drive-letter casing. Non-`file:`
/// schemes are returned unchanged.
fn canonical_uri(uri: Url) -> Url {
    if uri.scheme() == "file" {
        if let Ok(path) = uri.to_file_path() {
            if let Ok(canonical) = Url::from_file_path(path) {
                return canonical;
            }
        }
    }
    uri
}

/// Walk `roots` and index every `.ini` file (definitions + reference sites).
/// CPU/IO heavy — runs via `spawn_blocking` from [`Backend::scan_workspace`].
fn scan_roots(
    analyzer: &Analyzer,
    roots: &[PathBuf],
) -> Vec<(
    String,
    Vec<genparser_analysis::index::Definition>,
    Vec<genparser_analysis::index::ReferenceSite>,
)> {
    let mut out = Vec::new();
    for root in roots {
        for entry in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
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
            let parse = analyzer.parse(&text);
            let defs = definitions_in(analyzer, &parse, uri.as_str());
            let refs = references_in(analyzer, &parse);
            out.push((uri.to_string(), defs, refs));
        }
    }
    out
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
            format_enabled: OnceLock::new(),
            semantic_result_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn enc(&self) -> PositionEnc {
        self.encoding.get().copied().unwrap_or_default()
    }

    fn format_enabled(&self) -> bool {
        self.format_enabled.get().copied().unwrap_or(false)
    }

    fn next_semantic_id(&self) -> u64 {
        self.semantic_result_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
        // Reference sites never bump it.
        let defs = definitions_in(&self.analyzer, &parse, uri.as_str());
        let refs = references_in(&self.analyzer, &parse);
        if let Ok(mut idx) = self.index.write() {
            idx.set_file(uri.as_str(), defs);
            idx.set_file_refs(uri.as_str(), refs);
        }

        let enc = self.enc();
        let lsp_diags: Vec<Diagnostic> = {
            let idx = self.index.read().ok();
            let diags = diagnostics::diagnose_with_cache(
                &self.analyzer,
                &parse,
                idx.as_deref(),
                Some(uri.as_str()),
                &mut cache,
            );
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
    /// index, so references resolve before a file is opened. The walk +
    /// parse runs on a blocking thread (full-mod folders take seconds); the
    /// results are applied under the index lock afterwards.
    async fn scan_workspace(&self) {
        let roots = self.roots.lock().map(|r| r.clone()).unwrap_or_default();
        let analyzer = self.analyzer.clone();
        let scanned = tokio::task::spawn_blocking(move || scan_roots(&analyzer, &roots))
            .await
            .unwrap_or_default();
        // Don't overwrite index entries for already-open documents with stale
        // disk content; `initialized` calls `refresh` for each open doc right
        // after this returns, so they will populate the index from live text.
        let open: std::collections::HashSet<String> = self
            .docs
            .iter()
            .map(|e| e.key().as_str().to_string())
            .collect();
        let Ok(mut idx) = self.index.write() else { return };
        for (uri, defs, refs) in scanned {
            if !open.contains(&uri) {
                idx.set_file(&uri, defs);
                idx.set_file_refs(&uri, refs);
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

    /// The (kind, name, span) under the cursor — a reference-typed value token
    /// or a definition's name token. The shared entry point for
    /// find-references and rename, which work from either end of an edge.
    fn symbol_at(
        &self,
        uri: &Url,
        pos: Position,
    ) -> Option<genparser_analysis::nav::ReferenceAt> {
        let (rope, parse) = self.doc(uri)?;
        let offset = convert::position_to_offset(&rope, pos, self.enc());
        reference_at(&self.analyzer, &parse, offset)
            .or_else(|| definition_at(&self.analyzer, &parse, offset))
    }

    /// Convert `(file uri, span)` pairs to LSP locations, reading each file's
    /// rope at most once.
    fn to_locations(&self, raw: Vec<(String, genparser_analysis::Span)>) -> Vec<Location> {
        let enc = self.enc();
        let mut ropes: std::collections::HashMap<String, Option<(Url, Rope)>> =
            std::collections::HashMap::new();
        let mut out = Vec::with_capacity(raw.len());
        for (file, span) in raw {
            let entry = ropes.entry(file.clone()).or_insert_with(|| {
                let uri = Url::parse(&file).ok()?;
                let rope = self.rope_for(&uri)?;
                Some((uri, rope))
            });
            if let Some((uri, rope)) = entry {
                out.push(Location {
                    uri: uri.clone(),
                    range: convert::span_to_range(rope, span, enc),
                });
            }
        }
        out
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

        // Editor-facing settings arrive as `initializationOptions`; a change
        // requires a client restart (the VS Code extension does this
        // automatically). Currently just `{"format": {"enable": bool}}`.
        let format_enabled = params
            .initialization_options
            .as_ref()
            .and_then(|v| v.get("format"))
            .and_then(|f| f.get("enable"))
            .and_then(|e| e.as_bool())
            .unwrap_or(false);
        let _ = self.format_enabled.set(format_enabled);

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
                            full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
                            range: Some(true),
                            ..Default::default()
                        },
                    ),
                ),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                // Only advertised when opted in, so format-on-save in clients
                // never invokes a formatter the user didn't ask for.
                document_formatting_provider: format_enabled.then_some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.scan_workspace().await;
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
        let uri = canonical_uri(params.text_document.uri);
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
                last_semantic: None,
            },
        );
        self.refresh(&uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = canonical_uri(params.text_document.uri);
        let version = params.text_document.version;
        let enc = self.enc();
        {
            let Some(mut entry) = self.docs.get_mut(&uri) else { return };
            let entry = entry.value_mut();
            // Bulk batches (e.g. format-on-save applying coalesced reindent
            // edits) take a different path: incremental reparse needs the full
            // old/new text per edit, so running it per change is
            // O(changes × file). Apply every delta to the rope first, then
            // rebuild the text and parse once — one full parse beats a pile
            // of incremental ones. Triggers on many changes or on a
            // multi-change batch with a large total payload (a few coalesced
            // format edits can each carry kilobytes).
            const BULK_CHANGE_THRESHOLD: usize = 8;
            const BULK_TEXT_BYTES: usize = 32 * 1024;
            let bulk = params.content_changes.len() > BULK_CHANGE_THRESHOLD
                || (params.content_changes.len() > 1
                    && params
                        .content_changes
                        .iter()
                        .map(|c| c.text.len())
                        .sum::<usize>()
                        > BULK_TEXT_BYTES);
            if bulk && params.content_changes.iter().all(|c| c.range.is_some()) {
                for change in params.content_changes {
                    convert::apply_change(&mut entry.rope, change.range, &change.text, enc);
                }
                entry.text = entry.rope.to_string().into();
                entry.parse = Arc::new(self.analyzer.parse(&entry.text));
                entry.version = version;
            } else {
                // Each change applies to the text produced by the previous
                // one. The parse is kept in lockstep via incremental reparse,
                // so the cost per keystroke is the edited block, not the
                // whole file.
                for change in params.content_changes {
                    match change.range {
                        Some(range) => {
                            let start =
                                convert::position_to_offset(&entry.rope, range.start, enc);
                            let old_end =
                                convert::position_to_offset(&entry.rope, range.end, enc);
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
        }
        self.refresh(&uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        // Keep the file's symbols in the index (it still exists on disk); just
        // drop the in-memory buffer.
        self.docs.remove(&canonical_uri(params.text_document.uri));
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = canonical_uri(params.text_document_position.text_document.uri);
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
        let uri = canonical_uri(params.text_document.uri);
        let Some((rope, parse)) = self.doc(&uri) else {
            return Ok(None);
        };
        let tokens = semantic::semantic_tokens(&self.analyzer, &parse);
        let data = convert::to_lsp_semantic_tokens(&rope, &tokens, self.enc());
        let id = self.next_semantic_id();
        if let Some(mut doc) = self.docs.get_mut(&uri) {
            doc.last_semantic = Some((id, data.clone()));
        }
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: Some(id.to_string()),
            data,
        })))
    }

    async fn semantic_tokens_full_delta(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> Result<Option<SemanticTokensFullDeltaResult>> {
        let uri = canonical_uri(params.text_document.uri);
        let Some((rope, parse)) = self.doc(&uri) else {
            return Ok(None);
        };
        let tokens = semantic::semantic_tokens(&self.analyzer, &parse);
        let data = convert::to_lsp_semantic_tokens(&rope, &tokens, self.enc());
        let id = self.next_semantic_id();
        let previous = self.docs.get_mut(&uri).and_then(|mut doc| {
            doc.last_semantic.replace((id, data.clone()))
        });
        // Only splice against the exact result the client says it holds;
        // anything else (stale id, no history) falls back to a full response.
        match previous {
            Some((prev_id, prev)) if prev_id.to_string() == params.previous_result_id => {
                Ok(Some(SemanticTokensFullDeltaResult::TokensDelta(
                    SemanticTokensDelta {
                        result_id: Some(id.to_string()),
                        edits: vec![convert::semantic_tokens_splice(&prev, &data)],
                    },
                )))
            }
            _ => Ok(Some(SemanticTokensFullDeltaResult::Tokens(SemanticTokens {
                result_id: Some(id.to_string()),
                data,
            }))),
        }
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        // Belt and braces for clients that call despite the capability being
        // withheld (formatting is opt-in via initializationOptions).
        if !self.format_enabled() {
            return Ok(None);
        }
        let uri = canonical_uri(params.text_document.uri);
        // `text` is kept in lockstep with `rope` by did_open/did_change, so no
        // rope-to-string rebuild is needed here.
        let Some((rope, text, parse)) = self
            .docs
            .get(&uri)
            .map(|d| (d.rope.clone(), d.text.clone(), d.parse.clone()))
        else {
            return Ok(None);
        };
        let indent = if params.options.insert_spaces {
            " ".repeat(params.options.tab_size as usize)
        } else {
            "\t".to_string()
        };
        let enc = self.enc();
        let edits = format::format_edits(&parse, &text, &indent)
            .into_iter()
            .map(|e| TextEdit {
                range: convert::span_to_range(&rope, e.span, enc),
                new_text: e.new_text,
            })
            .collect();
        Ok(Some(edits))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = canonical_uri(params.text_document.uri);

        // Take the diagnostics cache out without holding the DashMap entry
        // across the index lock (avoids lock-order deadlock with the index RwLock).
        let Some((rope, parse, text, version, mut cache)) =
            self.docs.get_mut(&uri).map(|mut d| {
                (
                    d.rope.clone(),
                    d.parse.clone(),
                    Arc::clone(&d.text),
                    d.version,
                    std::mem::take(&mut d.diag_cache),
                )
            })
        else {
            return Ok(None);
        };

        let enc = self.enc();
        let start = convert::position_to_offset(&rope, params.range.start, enc);
        let end = convert::position_to_offset(&rope, params.range.end, enc);

        let fixes = {
            let idx = self.index.read().ok();
            let diags = diagnostics::diagnose_with_cache(
                &self.analyzer,
                &parse,
                idx.as_deref(),
                Some(uri.as_str()),
                &mut cache,
            );
            actions::fixes(
                &self.analyzer,
                &parse,
                &text,
                genparser_analysis::Span::new(start, end),
                &diags,
                idx.as_deref(),
            )
        };

        // Hand the warmed cache back unless a newer change superseded us.
        if let Some(mut entry) = self.docs.get_mut(&uri) {
            if entry.version == version {
                entry.diag_cache = cache;
            }
        }

        let response: CodeActionResponse = fixes
            .into_iter()
            .map(|f| {
                let edit = TextEdit {
                    range: convert::span_to_range(&rope, f.span, enc),
                    new_text: f.new_text,
                };
                CodeActionOrCommand::CodeAction(CodeAction {
                    title: f.title,
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(WorkspaceEdit {
                        changes: Some([(uri.clone(), vec![edit])].into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            })
            .collect();
        Ok((!response.is_empty()).then_some(response))
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        let uri = canonical_uri(params.text_document.uri);
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
        let uri = canonical_uri(params.text_document_position_params.text_document.uri);
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

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = canonical_uri(params.text_document.uri);
        let Some((rope, parse)) = self.doc(&uri) else {
            return Ok(None);
        };
        let enc = self.enc();
        let symbols: Vec<DocumentSymbol> = outline::document_symbols(&parse)
            .iter()
            .map(|s| convert::to_lsp_document_symbol(&rope, s, enc))
            .collect();
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let uri = canonical_uri(params.text_document.uri);
        let Some((rope, parse)) = self.doc(&uri) else {
            return Ok(None);
        };
        let enc = self.enc();
        let ranges = outline::folding_ranges(&parse)
            .into_iter()
            .filter_map(|span| convert::to_lsp_folding_range(&rope, span, enc))
            .collect();
        Ok(Some(ranges))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        // Cap the result set: an empty query over full game data matches
        // thousands of names, and clients re-query as the user types.
        const MAX_RESULTS: usize = 256;
        let raw: Vec<(genparser_schema::RefKind, String, String, genparser_analysis::Span)> = {
            let Ok(idx) = self.index.read() else { return Ok(None) };
            idx.symbols(&params.query)
                .take(MAX_RESULTS)
                .map(|(kind, name, loc)| (kind, name.to_string(), loc.file.clone(), loc.span))
                .collect()
        };
        let enc = self.enc();
        let mut ropes: std::collections::HashMap<String, Option<(Url, Rope)>> =
            std::collections::HashMap::new();
        let mut out = Vec::with_capacity(raw.len());
        for (kind, name, file, span) in raw {
            let entry = ropes.entry(file.clone()).or_insert_with(|| {
                let uri = Url::parse(&file).ok()?;
                let rope = self.rope_for(&uri)?;
                Some((uri, rope))
            });
            let Some((uri, rope)) = entry else { continue };
            #[allow(deprecated)] // `deprecated` field is required by the struct literal
            out.push(SymbolInformation {
                name,
                kind: SymbolKind::CLASS,
                tags: None,
                deprecated: None,
                location: Location {
                    uri: uri.clone(),
                    range: convert::span_to_range(rope, span, enc),
                },
                container_name: Some(format!("{kind:?}")),
            });
        }
        Ok(Some(out))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = canonical_uri(params.text_document_position.text_document.uri);
        let pos = params.text_document_position.position;
        let Some(sym) = self.symbol_at(&uri, pos) else {
            return Ok(None);
        };
        let mut raw: Vec<(String, genparser_analysis::Span)> = {
            let Ok(idx) = self.index.read() else { return Ok(None) };
            let mut v: Vec<_> = idx
                .reference_sites(sym.kind, &sym.name)
                .iter()
                .map(|l| (l.file.clone(), l.span))
                .collect();
            if params.context.include_declaration {
                v.extend(
                    idx.locations(sym.kind, &sym.name)
                        .iter()
                        .map(|l| (l.file.clone(), l.span)),
                );
            }
            v
        };
        raw.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.start.cmp(&b.1.start)));
        raw.dedup();
        let out = self.to_locations(raw);
        Ok((!out.is_empty()).then_some(out))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = canonical_uri(params.text_document.uri);
        let Some(sym) = self.symbol_at(&uri, params.position) else {
            return Ok(None);
        };
        let Some(rope) = self.rope_for(&uri) else { return Ok(None) };
        Ok(Some(PrepareRenameResponse::Range(convert::span_to_range(
            &rope,
            sym.span,
            self.enc(),
        ))))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = canonical_uri(params.text_document_position.text_document.uri);
        let pos = params.text_document_position.position;
        let new_name = params.new_name;
        // A definition name must survive the engine tokenizer as one token.
        if new_name.is_empty() || new_name.contains([' ', '\t', '\r', '\n', '=', ';', '"']) {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(
                "name must be a single token (no whitespace, `=`, `;` or quotes)",
            ));
        }
        let Some(sym) = self.symbol_at(&uri, pos) else {
            return Ok(None);
        };
        let mut raw: Vec<(String, genparser_analysis::Span)> = {
            let Ok(idx) = self.index.read() else { return Ok(None) };
            idx.reference_sites(sym.kind, &sym.name)
                .iter()
                .chain(idx.locations(sym.kind, &sym.name).iter())
                .map(|l| (l.file.clone(), l.span))
                .collect()
        };
        raw.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.start.cmp(&b.1.start)));
        raw.dedup();

        let enc = self.enc();
        let mut changes: std::collections::HashMap<Url, Vec<TextEdit>> =
            std::collections::HashMap::new();
        let mut ropes: std::collections::HashMap<String, Option<(Url, Rope)>> =
            std::collections::HashMap::new();
        for (file, span) in raw {
            let entry = ropes.entry(file.clone()).or_insert_with(|| {
                let uri = Url::parse(&file).ok()?;
                let rope = self.rope_for(&uri)?;
                Some((uri, rope))
            });
            let Some((file_uri, rope)) = entry else { continue };
            changes.entry(file_uri.clone()).or_default().push(TextEdit {
                range: convert::span_to_range(rope, span, enc),
                new_text: new_name.clone(),
            });
        }
        if changes.is_empty() {
            return Ok(None);
        }
        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = canonical_uri(params.text_document_position_params.text_document.uri);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_uri_pass_through_non_file() {
        let u = Url::parse("untitled:///buffer").unwrap();
        assert_eq!(canonical_uri(u.clone()), u);
    }

    #[test]
    #[cfg(windows)]
    fn canonical_uri_normalises_windows_file_uri() {
        // VS Code on Windows sends percent-encoded colon + lowercase drive
        // letter; `from_file_path` produces uppercase drive + no encoding.
        let client = Url::parse("file:///c%3A/CodeProjects/mod/Object.ini").unwrap();
        let expected = Url::from_file_path(r"C:\CodeProjects\mod\Object.ini").unwrap();
        assert_eq!(canonical_uri(client), expected);
    }

    #[test]
    #[cfg(windows)]
    fn canonical_uri_idempotent_on_well_formed_windows_uri() {
        let well_formed = Url::from_file_path(r"C:\CodeProjects\mod\Object.ini").unwrap();
        assert_eq!(canonical_uri(well_formed.clone()), well_formed);
    }
}
