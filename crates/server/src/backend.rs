//! The tower-lsp language server backend.
//!
//! Holds open documents as [`DocumentState`]s — a `ropey` rope plus the cached
//! parse for that text — alongside a shared [`Analyzer`] (the embedded schema)
//! and a workspace symbol [`WorkspaceIndex`]. Document sync is INCREMENTAL:
//! `didChange` deltas are applied to the rope and the document is re-parsed
//! once per change batch; read-only requests reuse the cached parse.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use dashmap::DashMap;
use ropey::Rope;
use serde::Deserialize;
use tower_lsp::lsp_types::*;
use tower_lsp::{jsonrpc::Result, Client, LanguageServer};
use zerosyntax_analysis::diagnostics::DiagnosticsCache;
use zerosyntax_analysis::index::{
    definitions_in, module_tags_in, object_models_in, object_parents_in, references_in,
    ModelMemberStrictness, WorkspaceIndex,
};
use zerosyntax_analysis::nav::{definition_at, hover_at, reference_at, HoverInfo};
use zerosyntax_analysis::{actions, completion, diagnostics, format, outline, semantic, Analyzer};
use zerosyntax_syntax::{Edit, Parse};

use crate::convert::{self, PositionEnc};
use crate::scan::{collect_scan_paths, load_sibling_str_keys, read_lossy, scan_files};
#[cfg(test)]
use crate::scan::{parse_w3d_models, scan_big, scan_roots};

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
    analyzer: RwLock<Arc<Analyzer>>,
    schema_error: Mutex<Option<String>>,
    /// Open documents, keyed by URI.
    docs: DashMap<Url, DocumentState>,
    /// Read-only documents synthesized from configured `.big` archives.
    virtual_files: DashMap<String, Arc<str>>,
    index: RwLock<WorkspaceIndex>,
    /// Workspace roots, captured at `initialize` and scanned in `initialized`.
    roots: Mutex<Vec<PathBuf>>,
    /// User-configured game/mod INI roots. Entries may be directories or `.big`
    /// archives; both seed definitions that map.ini/solo.ini can rely on.
    base_roots: Mutex<Vec<PathBuf>>,
    /// Number of base INI files indexed from configured base roots.
    base_indexed_count: AtomicUsize,
    /// Whether the initial workspace/base scan has completed at least once.
    scan_finished: AtomicBool,
    /// One-shot guard for the map/solo.ini configuration hint.
    base_roots_hint_shown: AtomicBool,
    /// Whether this client shows the empty `baseIniRoots` hint itself.
    client_base_ini_hint: OnceLock<bool>,
    /// Position encoding negotiated at `initialize` (UTF-16 until then).
    encoding: OnceLock<PositionEnc>,
    /// Whether `textDocument/formatting` is enabled, from the client's
    /// `initializationOptions` (`{"format": {"enable": true}}`). Off by
    /// default: format-on-save rewriting a whole hand-indented game file is
    /// surprising, so formatting is opt-in per editor.
    format_enabled: OnceLock<bool>,
    /// Whether the client supports snippet insertText (tab-stops, placeholders).
    /// Captured at `initialize` from the client's completion-item capabilities.
    snippet_support: OnceLock<bool>,
    /// Whether the client supports `window/workDoneProgress` (the scan
    /// spinner). Captured at `initialize`.
    progress_support: OnceLock<bool>,
    /// Monotonic id source for semantic-token results (delta bookkeeping).
    semantic_result_id: std::sync::atomic::AtomicU64,
}

fn load_schema(path: &str) -> std::result::Result<Analyzer, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("could not read `{path}`: {e}"))?;
    let schema = zerosyntax_schema::Schema::from_json(&text)
        .map_err(|e| format!("could not parse `{path}`: {e}"))?;
    Ok(Analyzer::new(schema))
}

fn load_schema_or_embedded(path: &str) -> (Analyzer, Option<String>) {
    match load_schema(path) {
        Ok(analyzer) => (analyzer, None),
        Err(error) => (
            Analyzer::embedded(),
            Some(format!("ZeroSyntax: {error}; using the built-in schema.")),
        ),
    }
}

#[derive(Deserialize)]
pub struct VirtualFileParams {
    uri: String,
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

fn is_map_layer_file(file: &str) -> bool {
    file.rsplit(['/', '\\']).next().is_some_and(|name| {
        name.eq_ignore_ascii_case("map.ini") || name.eq_ignore_ascii_case("solo.ini")
    })
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Backend {
            client,
            analyzer: RwLock::new(Arc::new(Analyzer::embedded())),
            schema_error: Mutex::new(None),
            docs: DashMap::new(),
            virtual_files: DashMap::new(),
            index: RwLock::new(WorkspaceIndex::new()),
            roots: Mutex::new(Vec::new()),
            encoding: OnceLock::new(),
            format_enabled: OnceLock::new(),
            base_roots: Mutex::new(Vec::new()),
            base_indexed_count: AtomicUsize::new(0),
            scan_finished: AtomicBool::new(false),
            base_roots_hint_shown: AtomicBool::new(false),
            client_base_ini_hint: OnceLock::new(),
            snippet_support: OnceLock::new(),
            progress_support: OnceLock::new(),
            semantic_result_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn enc(&self) -> PositionEnc {
        self.encoding.get().copied().unwrap_or_default()
    }

    fn analyzer(&self) -> Arc<Analyzer> {
        self.analyzer
            .read()
            .expect("analyzer lock poisoned")
            .clone()
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
        let analyzer = self.analyzer();
        let defs = definitions_in(&analyzer, &parse, uri.as_str());
        let refs = references_in(&analyzer, &parse);
        let tags = module_tags_in(&analyzer, &parse);
        let object_models = object_models_in(&analyzer, &parse);
        let object_parents = object_parents_in(&parse);
        let str_keys = load_sibling_str_keys(uri);
        if let Ok(mut idx) = self.index.write() {
            idx.set_file(uri.as_str(), defs);
            idx.set_file_refs(uri.as_str(), refs);
            idx.set_file_tags(uri.as_str(), tags);
            idx.set_file_object_models(uri.as_str(), object_models);
            idx.set_file_object_parents(uri.as_str(), object_parents);
            idx.set_ini_string_keys(uri.as_str(), str_keys);
        }

        let enc = self.enc();
        let lsp_diags: Vec<Diagnostic> = {
            let idx = self.index.read().ok();
            let diags = diagnostics::diagnose_with_cache(
                &analyzer,
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
            let Some(mut entry) = self.docs.get_mut(uri) else {
                return;
            };
            if entry.version != version {
                return;
            }
            entry.diag_cache = cache;
        }
        self.client
            .publish_diagnostics(uri.clone(), lsp_diags, Some(version))
            .await;
        self.maybe_warn_missing_base_roots(uri).await;
    }

    async fn maybe_warn_missing_base_roots(&self, uri: &Url) {
        if !is_map_layer_file(uri.as_str()) {
            return;
        }
        if !self.scan_finished.load(Ordering::Relaxed) {
            return;
        }
        if self.base_indexed_count.load(Ordering::Relaxed) > 0 {
            return;
        }
        let roots_empty = self.base_roots.lock().map(|r| r.is_empty()).unwrap_or(true);
        if roots_empty && self.client_base_ini_hint.get().copied().unwrap_or(false) {
            return;
        }
        if self
            .base_roots_hint_shown
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        self.client
            .show_message(
                MessageType::WARNING,
                "ZeroSyntax v2: map/solo.ini diagnostics are limited until base game or mod INIs are configured. Set `zerosyntax.baseIniRoots` to your game/mod `.big` files or INI folder.",
            )
            .await;
    }

    /// Best-effort scan of the workspace roots for `.ini` files to seed the
    /// index, so references resolve before a file is opened. The walk +
    /// parse runs on a blocking thread (full-mod folders take seconds); the
    /// results are applied under the index lock afterwards. Reported to the
    /// client as `$/progress` (a status-bar spinner with a `done/total`
    /// counter in VS Code) so users can tell "still indexing" apart from
    /// "nothing was found".
    async fn scan_workspace(&self) {
        let progress_token = self.begin_scan_progress().await;
        let roots = self.roots.lock().map(|r| r.clone()).unwrap_or_default();
        let base_roots = self
            .base_roots
            .lock()
            .map(|r| r.clone())
            .unwrap_or_default();
        let analyzer = self.analyzer();
        // The blocking scan streams (done, total) over a channel; forward
        // each update as a progress report while waiting for the results.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(usize, usize)>();
        let handle = tokio::task::spawn_blocking(move || {
            let workspace_paths = collect_scan_paths(&roots);
            let base_paths = collect_scan_paths(&base_roots);
            let total = workspace_paths.len() + base_paths.len();
            let mut done = 0;
            let mut last_percent = u32::MAX;
            // Throttle to whole-percent changes (plus the final count) so a
            // 10k-file scan sends ~100 notifications, not 10k.
            let mut progress = |_done_in_batch: usize, _batch_total: usize| {
                done += 1;
                let percent = (done * 100 / total.max(1)) as u32;
                if percent != last_percent || done == total {
                    last_percent = percent;
                    let _ = tx.send((done, total));
                }
            };
            let scanned = scan_files(&analyzer, &workspace_paths, &mut progress);
            let base_scanned = scan_files(&analyzer, &base_paths, &mut progress);
            (scanned, base_scanned)
        });
        while let Some((done, total)) = rx.recv().await {
            self.report_scan_progress(&progress_token, done, total)
                .await;
        }
        let (scanned, base_scanned) = handle.await.unwrap_or_default();
        let base_ini_count = base_scanned
            .iter()
            .filter(|(_, _, _, _, _, _, models, _)| models.is_empty())
            .count();
        self.base_indexed_count
            .store(base_ini_count, Ordering::Relaxed);
        self.scan_finished.store(true, Ordering::Relaxed);
        let ini_total = base_ini_count
            + scanned
                .iter()
                .filter(|(_, _, _, _, _, _, models, _)| models.is_empty())
                .count();
        let model_total: usize = base_scanned
            .iter()
            .chain(scanned.iter())
            .map(|(_, _, _, _, _, _, models, _)| models.len())
            .sum();
        // Don't overwrite index entries for already-open documents with stale
        // disk content; `initialized` calls `refresh` for each open doc right
        // after this returns, so they will populate the index from live text.
        let open: std::collections::HashSet<String> = self
            .docs
            .iter()
            .map(|e| e.key().as_str().to_string())
            .collect();
        {
            let Ok(mut idx) = self.index.write() else {
                return;
            };
            for (uri, defs, refs, tags, object_models, object_parents, models, text) in
                base_scanned.into_iter().chain(scanned)
            {
                if let Some(text) = text {
                    self.virtual_files.insert(uri.clone(), text);
                }
                if !open.contains(&uri) {
                    idx.set_file(&uri, defs);
                    idx.set_file_refs(&uri, refs);
                    idx.set_file_tags(&uri, tags);
                    idx.set_file_object_models(&uri, object_models);
                    idx.set_file_object_parents(&uri, object_parents);
                    idx.set_file_models(&uri, models);
                }
            }
        }
        self.end_scan_progress(progress_token, ini_total, model_total)
            .await;
    }

    /// Ask the client to show an indexing spinner. Returns the token to end
    /// it with, or `None` when the client doesn't support work-done progress.
    async fn begin_scan_progress(&self) -> Option<NumberOrString> {
        if !self.progress_support.get().copied().unwrap_or(false) {
            return None;
        }
        let token = NumberOrString::String("zerosyntax/indexing".into());
        self.client
            .send_request::<request::WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
                token: token.clone(),
            })
            .await
            .ok()?;
        self.client
            .send_notification::<notification::Progress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: "Indexing game data".into(),
                        message: Some("scanning workspace and base INI roots".into()),
                        cancellable: Some(false),
                        // Signals that reports will carry a percentage.
                        percentage: Some(0),
                    },
                )),
            })
            .await;
        Some(token)
    }

    /// Forward one `done/total` update to the client's progress UI.
    async fn report_scan_progress(
        &self,
        token: &Option<NumberOrString>,
        done: usize,
        total: usize,
    ) {
        let Some(token) = token else { return };
        self.client
            .send_notification::<notification::Progress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Report(
                    WorkDoneProgressReport {
                        message: Some(format!("{done}/{total} files")),
                        percentage: Some((done * 100 / total.max(1)) as u32),
                        cancellable: Some(false),
                    },
                )),
            })
            .await;
    }

    async fn end_scan_progress(
        &self,
        token: Option<NumberOrString>,
        ini_total: usize,
        model_total: usize,
    ) {
        let Some(token) = token else { return };
        self.client
            .send_notification::<notification::Progress>(ProgressParams {
                token,
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                    message: Some(format!(
                        "{ini_total} INI files, {model_total} W3D models indexed"
                    )),
                })),
            })
            .await;
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
        if uri.scheme() == "big" {
            return self
                .virtual_files
                .get(uri.as_str())
                .map(|text| Rope::from_str(&text));
        }
        let path = uri.to_file_path().ok()?;
        read_lossy(&path).ok().map(|s| Rope::from_str(&s))
    }

    pub async fn read_virtual_file(&self, params: VirtualFileParams) -> Result<Option<String>> {
        Ok(self
            .virtual_files
            .get(&params.uri)
            .map(|text| text.to_string()))
    }

    /// The (kind, name, span) under the cursor — a reference-typed value token
    /// or a definition's name token. The shared entry point for
    /// find-references and rename, which work from either end of an edge.
    fn symbol_at(&self, uri: &Url, pos: Position) -> Option<zerosyntax_analysis::nav::ReferenceAt> {
        let (rope, parse) = self.doc(uri)?;
        let offset = convert::position_to_offset(&rope, pos, self.enc());
        let analyzer = self.analyzer();
        reference_at(&analyzer, &parse, offset).or_else(|| definition_at(&analyzer, &parse, offset))
    }

    /// Convert `(file uri, span)` pairs to LSP locations, reading each file's
    /// rope at most once.
    fn to_locations(&self, raw: Vec<(String, zerosyntax_analysis::Span)>) -> Vec<Location> {
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
        // automatically). Shape:
        // `{ "format": {"enable": bool}, "schemaPath": "schema.json",
        //    "analysis": {"modelMemberStrictness": "compatible"},
        //    "baseIniRoots": ["dir-or-big", ...],
        //    "clientBaseIniHint": bool }`.
        let format_enabled = params
            .initialization_options
            .as_ref()
            .and_then(|v| v.get("format"))
            .and_then(|f| f.get("enable"))
            .and_then(|e| e.as_bool())
            .unwrap_or(false);
        let _ = self.format_enabled.set(format_enabled);

        let model_member_strictness = params
            .initialization_options
            .as_ref()
            .and_then(|v| v.get("analysis"))
            .and_then(|v| v.get("modelMemberStrictness"))
            .and_then(|v| v.as_str())
            .map(|value| match value {
                "off" => ModelMemberStrictness::Off,
                "strict" => ModelMemberStrictness::Strict,
                _ => ModelMemberStrictness::Compatible,
            })
            .unwrap_or_default();
        if let Ok(mut index) = self.index.write() {
            index.set_model_member_strictness(model_member_strictness);
        }

        if let Some(path) = params
            .initialization_options
            .as_ref()
            .and_then(|v| v.get("schemaPath"))
            .and_then(|v| v.as_str())
            .filter(|path| !path.trim().is_empty())
        {
            let (analyzer, error) = load_schema_or_embedded(path);
            if let Ok(mut current) = self.analyzer.write() {
                *current = Arc::new(analyzer);
            }
            if let Ok(mut current) = self.schema_error.lock() {
                *current = error;
            }
        }

        let base_roots = params
            .initialization_options
            .as_ref()
            .and_then(|v| v.get("baseIniRoots"))
            .and_then(|v| v.as_array())
            .map(|roots| {
                roots
                    .iter()
                    .filter_map(|root| root.as_str())
                    .filter(|root| !root.trim().is_empty())
                    .map(PathBuf::from)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Ok(mut roots) = self.base_roots.lock() {
            *roots = base_roots;
        }
        let client_base_ini_hint = params
            .initialization_options
            .as_ref()
            .and_then(|v| v.get("clientBaseIniHint"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let _ = self.client_base_ini_hint.set(client_base_ini_hint);

        let snippet_support = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|td| td.completion.as_ref())
            .and_then(|c| c.completion_item.as_ref())
            .and_then(|ci| ci.snippet_support)
            .unwrap_or(false);
        let _ = self.snippet_support.set(snippet_support);

        let progress_support = params
            .capabilities
            .window
            .as_ref()
            .and_then(|w| w.work_done_progress)
            .unwrap_or(false);
        let _ = self.progress_support.set(progress_support);

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "zerosyntax-lsp".into(),
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
        if let Some(error) = self.schema_error.lock().ok().and_then(|mut e| e.take()) {
            self.client.show_message(MessageType::WARNING, error).await;
        }
        self.scan_workspace().await;
        // Re-publish diagnostics for any already-open docs now that the index
        // is populated (so cross-file references resolve). The cached parse is
        // still valid — only the index changed.
        let open: Vec<Url> = self.docs.iter().map(|e| e.key().clone()).collect();
        for uri in open {
            self.refresh(&uri).await;
        }
        let (ini, models) = {
            let idx = self.index.read().ok();
            let models = idx
                .as_ref()
                .map(|i| i.model_names().count())
                .unwrap_or_default();
            (self.base_indexed_count.load(Ordering::Relaxed), models)
        };
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "zerosyntax language server ready ({ini} base INI files, {models} W3D models indexed)"
                ),
            )
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = canonical_uri(params.text_document.uri);
        let text: Arc<str> = params.text_document.text.into();
        let rope = Rope::from_str(&text);
        let parse = Arc::new(self.analyzer().parse(&text));
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
            let Some(mut entry) = self.docs.get_mut(&uri) else {
                return;
            };
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
                entry.parse = Arc::new(self.analyzer().parse(&entry.text));
                entry.version = version;
            } else {
                // Each change applies to the text produced by the previous
                // one. The parse is kept in lockstep via incremental reparse,
                // so the cost per keystroke is the edited block, not the
                // whole file.
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
                                self.analyzer()
                                    .reparse(&entry.parse, &entry.text, &new_text, edit);
                            entry.parse = Arc::new(parse);
                            entry.text = new_text;
                        }
                        None => {
                            // Full-document replacement.
                            entry.rope = Rope::from_str(&change.text);
                            entry.text = change.text.into();
                            entry.parse = Arc::new(self.analyzer().parse(&entry.text));
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
        let snippets = self.snippet_support.get().copied().unwrap_or(false);
        let items: Vec<CompletionItem> = completion::complete(
            &self.analyzer(),
            &parse,
            offset,
            idx.as_deref(),
            Some(uri.as_str()),
        )
        .into_iter()
        .map(|c| convert::to_lsp_completion(c, snippets))
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
        let tokens = semantic::semantic_tokens(&self.analyzer(), &parse);
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
        let tokens = semantic::semantic_tokens(&self.analyzer(), &parse);
        let data = convert::to_lsp_semantic_tokens(&rope, &tokens, self.enc());
        let id = self.next_semantic_id();
        let previous = self
            .docs
            .get_mut(&uri)
            .and_then(|mut doc| doc.last_semantic.replace((id, data.clone())));
        // Only splice against the exact result the client says it holds;
        // anything else (stale id, no history) falls back to a full response.
        match previous {
            Some((prev_id, prev)) if prev_id.to_string() == params.previous_result_id => Ok(Some(
                SemanticTokensFullDeltaResult::TokensDelta(SemanticTokensDelta {
                    result_id: Some(id.to_string()),
                    edits: vec![convert::semantic_tokens_splice(&prev, &data)],
                }),
            )),
            _ => Ok(Some(SemanticTokensFullDeltaResult::Tokens(
                SemanticTokens {
                    result_id: Some(id.to_string()),
                    data,
                },
            ))),
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
        let Some((rope, parse, text, version, mut cache)) = self.docs.get_mut(&uri).map(|mut d| {
            (
                d.rope.clone(),
                d.parse.clone(),
                Arc::clone(&d.text),
                d.version,
                std::mem::take(&mut d.diag_cache),
            )
        }) else {
            return Ok(None);
        };

        let enc = self.enc();
        let start = convert::position_to_offset(&rope, params.range.start, enc);
        let end = convert::position_to_offset(&rope, params.range.end, enc);

        let range_span = zerosyntax_analysis::Span::new(start, end);
        let fixes = {
            let idx = self.index.read().ok();
            let diags = diagnostics::diagnose_with_cache(
                &self.analyzer(),
                &parse,
                idx.as_deref(),
                Some(uri.as_str()),
                &mut cache,
            );
            let mut f = actions::fixes(
                &self.analyzer(),
                &parse,
                &text,
                range_span,
                &diags,
                idx.as_deref(),
            );
            // Origin-copy fix: requires file I/O, so computed here in the server.
            if let Some(idx) = idx.as_deref() {
                f.extend(origin_copy_fixes(
                    &self.analyzer(),
                    &parse,
                    &text,
                    range_span,
                    &diags,
                    idx,
                    |base_uri| self.rope_for(base_uri).map(|rope| rope.to_string()),
                ));
            }
            f
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
            &self.analyzer(),
            &parse,
            zerosyntax_analysis::Span::new(start, end),
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
        let Some(reference) = reference_at(&self.analyzer(), &parse, offset) else {
            return Ok(None);
        };

        let locations: Vec<(String, zerosyntax_analysis::Span)> = {
            let Ok(idx) = self.index.read() else {
                return Ok(None);
            };
            idx.locations(reference.kind, &reference.name)
                .iter()
                .map(|l| (l.file.clone(), l.span))
                .collect()
        };

        let mut out = Vec::new();
        for (file, span) in locations {
            let Ok(target_uri) = Url::parse(&file) else {
                continue;
            };
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
        let raw: Vec<(
            zerosyntax_schema::RefKind,
            String,
            String,
            zerosyntax_analysis::Span,
        )> = {
            let Ok(idx) = self.index.read() else {
                return Ok(None);
            };
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
        let mut raw: Vec<(String, zerosyntax_analysis::Span)> = {
            let Ok(idx) = self.index.read() else {
                return Ok(None);
            };
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
        let Some(rope) = self.rope_for(&uri) else {
            return Ok(None);
        };
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
        let mut raw: Vec<(String, zerosyntax_analysis::Span)> = {
            let Ok(idx) = self.index.read() else {
                return Ok(None);
            };
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
            let Some((file_uri, rope)) = entry else {
                continue;
            };
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
        let analyzer = self.analyzer();
        let Some(info) = hover_at(&analyzer, &parse, offset) else {
            return Ok(None);
        };
        let (markdown, span) = match info {
            HoverInfo::Block { name, span } => {
                let doc = analyzer
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

/// Build "Insert reference copy of <name>" fixes for any `overrides` diagnostic
/// intersecting `range`. Reads the base definition file via `read_file` and
/// extracts the full Object block text to insert at the end of the current file.
fn origin_copy_fixes(
    analyzer: &Analyzer,
    parse: &zerosyntax_syntax::Parse,
    text: &str,
    range: zerosyntax_analysis::Span,
    diags: &[zerosyntax_analysis::diagnostics::Diagnostic],
    index: &WorkspaceIndex,
    read_file: impl Fn(&Url) -> Option<String>,
) -> Vec<actions::Fix> {
    use zerosyntax_analysis::index::Location;
    use zerosyntax_syntax::ast::Block;
    use zerosyntax_syntax::SyntaxKind;

    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for d in diags {
        if d.code != "overrides" {
            continue;
        }
        if d.span.end < range.start || d.span.start > range.end {
            continue;
        }
        // Find the top-level BLOCK whose name token's span contains the diagnostic.
        let block_node = parse
            .syntax()
            .children()
            .filter(|n| n.kind() == SyntaxKind::BLOCK)
            .find(|n| {
                let nr = n.text_range();
                u32::from(nr.start()) <= d.span.start && d.span.end <= u32::from(nr.end())
            });
        let Some(block_node) = block_node else {
            continue;
        };
        let block = Block(block_node.clone());
        let Some(kw) = block.keyword() else { continue };
        let Some(schema_block) = analyzer.block(kw.text()) else {
            continue;
        };
        let Some(kind) = schema_block.defines else {
            continue;
        };
        let Some(name_tok) = block.name() else {
            continue;
        };
        let name = name_tok.text().to_string();
        if !seen.insert(name.to_ascii_lowercase()) {
            continue;
        }
        // Find a base-game (non-override-layer) definition location.
        let base_loc: Option<&Location> = index.locations(kind, &name).iter().find(|l| {
            !l.file.rsplit(['/', '\\']).next().is_some_and(|f| {
                f.eq_ignore_ascii_case("map.ini") || f.eq_ignore_ascii_case("solo.ini")
            })
        });
        let Some(base_loc) = base_loc else { continue };
        let base_url = match Url::parse(&base_loc.file) {
            Ok(u) => u,
            Err(_) => continue,
        };
        let Some(base_text) = read_file(&base_url) else {
            continue;
        };
        // Re-parse the base file and extract the block at the name token.
        let base_parse = analyzer.parse(&base_text);
        let base_block_text: Option<String> = base_parse
            .syntax()
            .children()
            .filter(|n| n.kind() == SyntaxKind::BLOCK)
            .find(|n| {
                Block(n.clone())
                    .name()
                    .map(|t| t.text().eq_ignore_ascii_case(&name))
                    .unwrap_or(false)
            })
            .map(|n| {
                let r = n.text_range();
                base_text[usize::from(r.start())..usize::from(r.end())].to_string()
            });
        let Some(block_text) = base_block_text else {
            continue;
        };
        let base_short = base_url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .unwrap_or("base");
        let lead = if text.is_empty() || text.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let at = text.len() as u32;
        out.push(actions::Fix {
            title: format!("Insert reference copy of `{name}` from {base_short}"),
            span: zerosyntax_analysis::Span::new(at, at),
            new_text: format!(
                "{lead}\n; === Reference copy of {name} from {base_short} ===\n; Remove the fields and modules you don't need.\n{block_text}\n"
            ),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_schema_changes_analysis() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/custom-schema.json");
        let analyzer = load_schema(path.to_str().unwrap()).unwrap();
        let parse = analyzer.parse("TestBlock Test\n  CustomOnly = Yes\nEnd\n");
        let codes: Vec<_> = diagnostics::diagnose(&analyzer, &parse, None, None)
            .into_iter()
            .map(|diagnostic| diagnostic.code)
            .collect();
        assert!(codes.is_empty(), "{codes:?}");

        let embedded = Analyzer::embedded();
        let parse = embedded.parse("TestBlock Test\n  CustomOnly = Yes\nEnd\n");
        assert!(diagnostics::diagnose(&embedded, &parse, None, None)
            .iter()
            .any(|diagnostic| diagnostic.code == "unknown-block"));
    }

    #[test]
    fn invalid_custom_schema_falls_back_to_embedded() {
        let path = std::env::temp_dir().join(format!(
            "zerosyntax-invalid-schema-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, "not json").unwrap();
        let (analyzer, warning) = load_schema_or_embedded(path.to_str().unwrap());
        assert!(analyzer.block("Object").is_some());
        assert!(warning.is_some_and(|warning| warning.contains("using the built-in schema")));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn canonical_uri_pass_through_non_file() {
        let u = Url::parse("untitled:///buffer").unwrap();
        assert_eq!(canonical_uri(u.clone()), u);
    }

    #[test]
    fn detects_map_layer_filenames() {
        assert!(is_map_layer_file("file:///C:/Maps/Foo/map.ini"));
        assert!(is_map_layer_file("C:\\Maps\\Foo\\solo.ini"));
        assert!(!is_map_layer_file("C:/Data/INI/Object.ini"));
    }

    #[test]
    fn scans_ini_from_big_archive() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zerosyntax-test-{}.big", std::process::id()));
        let entry_name = b"Data\\INI\\Test.ini\0";
        let ini = b"Object BigArchiveObject\nEnd\n";
        let data_offset = 0x10 + 8 + entry_name.len();
        let archive_size = data_offset + ini.len();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"BIGF");
        bytes.extend_from_slice(&(archive_size as u32).to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&(data_offset as u32).to_be_bytes());
        bytes.extend_from_slice(&(ini.len() as u32).to_be_bytes());
        bytes.extend_from_slice(entry_name);
        bytes.extend_from_slice(ini);
        std::fs::write(&path, bytes).unwrap();

        let analyzer = Analyzer::embedded();
        let scanned = scan_big(&analyzer, &path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(scanned.len(), 1);
        assert!(Url::parse(&scanned[0].0).is_ok());
        assert!(scanned[0].1.iter().any(|d| d.name == "BigArchiveObject"));
        assert_eq!(
            scanned[0].7.as_deref(),
            Some("Object BigArchiveObject\nEnd\n")
        );
    }

    #[test]
    fn w3d_root_scan_powers_model_and_bone_completions() {
        // End-to-end over the `baseIniRoots` path: a directory containing a
        // loose .w3d file is scanned, indexed, and drives completions.
        let mut pivots = Vec::new();
        pivots.extend_from_slice(b"Tire01");
        pivots.resize(60, 0);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x0000_0102u32.to_le_bytes());
        bytes.extend_from_slice(&(pivots.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&pivots);

        let dir = std::env::temp_dir().join(format!("zerosyntax-w3d-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Good.w3d"), bytes).unwrap();

        let analyzer = Analyzer::embedded();
        let scanned = scan_roots(&analyzer, std::slice::from_ref(&dir));
        let _ = std::fs::remove_dir_all(&dir);

        let mut idx = WorkspaceIndex::new();
        for (uri, defs, refs, tags, object_models, object_parents, models, _) in scanned {
            idx.set_file(&uri, defs);
            idx.set_file_refs(&uri, refs);
            idx.set_file_tags(&uri, tags);
            idx.set_file_object_models(&uri, object_models);
            idx.set_file_object_parents(&uri, object_parents);
            idx.set_file_models(&uri, models);
        }
        assert!(idx.is_model_asset("Good"), "model name from file stem");

        let src = "\
Object Tank
  Draw = W3DTankDraw ModuleTag_01
    DefaultConditionState
      Model = 
      HideSubObject = 
    End
  End
End
";
        let parse = analyzer.parse(src);
        let labels_at = |offset: usize| -> Vec<String> {
            completion::complete(&analyzer, &parse, offset as u32, Some(&idx), None)
                .into_iter()
                .map(|c| c.label)
                .collect()
        };
        let model_offset = src.find("Model = ").unwrap() + "Model = ".len();
        assert!(labels_at(model_offset).contains(&"Good".to_string()));

        let src = src.replace("Model = ", "Model = Good");
        let parse = analyzer.parse(&src);
        let bone_offset = src.find("HideSubObject = ").unwrap() + "HideSubObject = ".len();
        let labels: Vec<String> =
            completion::complete(&analyzer, &parse, bone_offset as u32, Some(&idx), None)
                .into_iter()
                .map(|c| c.label)
                .collect();
        assert!(labels.contains(&"Tire".to_string()), "{labels:?}");
    }

    #[test]
    fn parses_w3d_model_names_and_members() {
        fn fixed<const N: usize>(name: &str) -> [u8; N] {
            let mut out = [0; N];
            let bytes = name.as_bytes();
            out[..bytes.len().min(N)].copy_from_slice(&bytes[..bytes.len().min(N)]);
            out
        }
        fn chunk(kind: u32, payload: Vec<u8>) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&payload);
            out
        }

        let mut hlod = Vec::new();
        hlod.extend_from_slice(&1u32.to_le_bytes());
        hlod.extend_from_slice(&1u32.to_le_bytes());
        hlod.extend_from_slice(&fixed::<16>("Good"));
        hlod.extend_from_slice(&fixed::<16>("Good"));

        let mut pivot = Vec::new();
        pivot.extend_from_slice(&fixed::<16>("Tire01"));
        pivot.resize(60, 0);

        let mut sub = Vec::new();
        sub.extend_from_slice(&0u32.to_le_bytes());
        sub.extend_from_slice(&fixed::<32>("Good.Cargo01"));

        let mut bytes = Vec::new();
        bytes.extend(chunk(0x0000_0701, hlod));
        bytes.extend(chunk(0x0000_0102, pivot));
        bytes.extend(chunk(0x0000_0704, sub));

        let models = parse_w3d_models(&bytes, "Fallback");
        let good = models.iter().find(|m| m.name == "Good").unwrap();
        assert!(good.members.iter().any(|m| m == "Tire01"), "{good:?}");
        assert!(good.members.iter().any(|m| m == "Cargo01"), "{good:?}");
    }

    #[test]
    fn parses_w3d_aggregate_and_emitter_names() {
        fn fixed<const N: usize>(name: &str) -> [u8; N] {
            let mut out = [0; N];
            let bytes = name.as_bytes();
            out[..bytes.len().min(N)].copy_from_slice(&bytes[..bytes.len().min(N)]);
            out
        }
        fn chunk(kind: u32, payload: Vec<u8>) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&payload);
            out
        }
        // Version + Name[16] header, wrapped in its container chunk.
        let mut header = Vec::new();
        header.extend_from_slice(&1u32.to_le_bytes());
        header.extend_from_slice(&fixed::<16>("Aggro"));
        let mut bytes = chunk(0x0000_0600, chunk(0x0000_0601, header));

        let mut header = Vec::new();
        header.extend_from_slice(&1u32.to_le_bytes());
        header.extend_from_slice(&fixed::<16>("Smoke"));
        bytes.extend(chunk(0x0000_0500, chunk(0x0000_0501, header)));

        let models = parse_w3d_models(&bytes, "");
        assert!(models.iter().any(|m| m.name == "Aggro"), "{models:?}");
        assert!(models.iter().any(|m| m.name == "Smoke"), "{models:?}");
    }

    #[test]
    fn w3d_chunk_walker_survives_hostile_deep_nesting() {
        // A self-nesting container at every level: each 8-byte header claims
        // the rest of the file as payload. Without a depth cap this recursed
        // once per 8 bytes and could overflow the stack on a large file.
        let total: usize = 64 * 1024;
        let mut bytes = Vec::with_capacity(total);
        let mut remaining = total;
        while remaining >= 8 {
            bytes.extend_from_slice(&0x0000_0700u32.to_le_bytes());
            bytes.extend_from_slice(&((remaining - 8) as u32).to_le_bytes());
            remaining -= 8;
        }
        // Must terminate without smashing the stack; content is garbage.
        let _ = parse_w3d_models(&bytes, "Fallback");
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
