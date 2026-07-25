//! The tower-lsp language server backend.
//!
//! Holds open documents as [`DocumentState`]s — a `ropey` rope plus the cached
//! parse for that text — alongside a shared [`Analyzer`] (the embedded schema)
//! and a workspace symbol [`WorkspaceIndex`]. Document sync is INCREMENTAL:
//! `didChange` deltas are applied to the rope and the document is re-parsed
//! once per change batch; read-only requests reuse the cached parse.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use dashmap::DashMap;
use percent_encoding::percent_decode_str;
use ropey::Rope;
use serde::Deserialize;
use tower_lsp::lsp_types::*;
use tower_lsp::{jsonrpc::Result, Client, LanguageServer};
use zerosyntax_analysis::diagnostics::DiagnosticsCache;
use zerosyntax_analysis::index::{
    definitions_in, module_tags_in, object_models_in, object_parents_in, references_in,
    ModelMemberStrictness, WorkspaceIndex,
};
use zerosyntax_analysis::nav::{
    definition_at, hover_at, module_tag_definition_at, module_tag_reference_at, reference_at,
    HoverInfo, ModuleTagReferenceAt, ReferenceAt,
};
use zerosyntax_analysis::{actions, completion, diagnostics, format, outline, semantic, Analyzer};
use zerosyntax_syntax::{Edit, Parse};

use crate::convert::{self, PositionEnc};
use crate::scan::{
    clear_index_cache, index_cache_path, load_sibling_str_keys, read_lossy, scan_with_cache,
};
#[cfg(test)]
use crate::scan::{parse_w3d_models, scan_big, scan_roots};

const CLEAR_INDEX_CACHE_COMMAND: &str = "zerosyntax.clearIndexCache";
const REBUILD_INDEX_CACHE_COMMAND: &str = "zerosyntax.rebuildIndexCache";

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

enum SymbolAt {
    Reference(ReferenceAt),
    ModuleTag {
        symbol: ModuleTagReferenceAt,
        before: Option<u32>,
    },
}

impl SymbolAt {
    fn span(&self) -> zerosyntax_analysis::Span {
        match self {
            Self::Reference(symbol) => symbol.span,
            Self::ModuleTag { symbol, .. } => symbol.span,
        }
    }
}

const DEFAULT_ANALYSIS_DEBOUNCE_MS: u64 = 250;
const MAX_ANALYSIS_DEBOUNCE_MS: u64 = 5_000;
const FORMATTING_REGISTRATION_ID: &str = "zerosyntax-formatting";

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeSettings {
    format_enabled: bool,
    schema_path: String,
    base_ini_roots: Vec<PathBuf>,
    model_member_strictness: ModelMemberStrictness,
    allow_bare_percentages: bool,
    map_ordering_diagnostics: bool,
    debounce_ms: u64,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            format_enabled: false,
            schema_path: String::new(),
            base_ini_roots: Vec::new(),
            model_member_strictness: ModelMemberStrictness::Compatible,
            allow_bare_percentages: false,
            map_ordering_diagnostics: true,
            debounce_ms: DEFAULT_ANALYSIS_DEBOUNCE_MS,
        }
    }
}

impl RuntimeSettings {
    fn from_value(value: Option<&serde_json::Value>) -> Self {
        let Some(value) = value else {
            return Self::default();
        };
        let value = value.get("zerosyntax").unwrap_or(value);
        let analysis = value.get("analysis");
        let debounce_ms =
            normalized_debounce_ms(analysis.and_then(|analysis| analysis.get("debounceMs")));
        Self {
            format_enabled: value
                .get("format")
                .and_then(|format| format.get("enable"))
                .and_then(|enabled| enabled.as_bool())
                .unwrap_or(false),
            schema_path: value
                .get("schemaPath")
                .or_else(|| value.get("schema").and_then(|schema| schema.get("path")))
                .and_then(|path| path.as_str())
                .unwrap_or_default()
                .trim()
                .to_string(),
            base_ini_roots: value
                .get("baseIniRoots")
                .and_then(|roots| roots.as_array())
                .map(|roots| {
                    roots
                        .iter()
                        .filter_map(|root| root.as_str())
                        .filter(|root| !root.trim().is_empty())
                        .map(PathBuf::from)
                        .collect()
                })
                .unwrap_or_default(),
            model_member_strictness: analysis
                .and_then(|analysis| analysis.get("modelMemberStrictness"))
                .and_then(|value| value.as_str())
                .map(|value| match value {
                    "off" => ModelMemberStrictness::Off,
                    "strict" => ModelMemberStrictness::Strict,
                    _ => ModelMemberStrictness::Compatible,
                })
                .unwrap_or_default(),
            allow_bare_percentages: analysis
                .and_then(|analysis| analysis.get("allowPercentagesWithoutSign"))
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            map_ordering_diagnostics: analysis
                .and_then(|analysis| analysis.get("mapOrderingDiagnostics"))
                .and_then(|value| value.as_bool())
                .unwrap_or(true),
            debounce_ms,
        }
    }
}

fn normalized_debounce_ms(value: Option<&serde_json::Value>) -> u64 {
    value
        .and_then(|v| {
            v.as_i64()
                .map(|n| n.clamp(0, MAX_ANALYSIS_DEBOUNCE_MS as i64) as u64)
                .or_else(|| v.as_u64().map(|n| n.min(MAX_ANALYSIS_DEBOUNCE_MS)))
        })
        .unwrap_or(DEFAULT_ANALYSIS_DEBOUNCE_MS)
}

pub struct Backend {
    client: Client,
    analyzer: Arc<RwLock<Arc<Analyzer>>>,
    settings: Mutex<RuntimeSettings>,
    reload_lock: tokio::sync::Mutex<()>,
    schema_error: Mutex<Option<String>>,
    /// Open documents, keyed by URI.
    docs: Arc<DashMap<Url, DocumentState>>,
    /// Read-only documents synthesized from configured `.big` archives.
    virtual_files: DashMap<String, Arc<str>>,
    index: Arc<RwLock<WorkspaceIndex>>,
    /// Workspace roots, captured at `initialize` and scanned in `initialized`.
    roots: Mutex<Vec<PathBuf>>,
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
    /// Whether `textDocument/formatting` is currently enabled. Off by default:
    /// format-on-save rewriting a whole hand-indented game file is surprising,
    /// so formatting is opt-in per editor.
    format_enabled: AtomicBool,
    formatting_dynamic_registration: OnceLock<bool>,
    /// Whether source-backed map/solo.ini forward-order warnings are emitted.
    /// Defaults on; clients can set `analysis.mapOrderingDiagnostics` to false.
    map_ordering_diagnostics: Arc<AtomicBool>,
    /// Whether the client supports snippet insertText (tab-stops, placeholders).
    /// Captured at `initialize` from the client's completion-item capabilities.
    snippet_support: OnceLock<bool>,
    /// Whether the client supports `window/workDoneProgress` (the scan
    /// spinner). Captured at `initialize`.
    progress_support: OnceLock<bool>,
    /// Delay after the latest edit before whole-document indexes and
    /// diagnostics refresh. Parsing and definition-name indexing stay eager.
    analysis_debounce_ms: AtomicU64,
    /// Monotonic id source for semantic-token results (delta bookkeeping).
    semantic_result_id: AtomicU64,
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
/// schemes other than `big:` are returned unchanged.
fn canonical_uri(uri: Url) -> Url {
    if uri.scheme() == "file" {
        if let Ok(path) = uri.to_file_path() {
            if let Ok(canonical) = Url::from_file_path(path) {
                return canonical;
            }
        }
    } else if uri.scheme() == "big" {
        let Ok(mut path) = percent_decode_str(uri.path())
            .decode_utf8()
            .map(|path| path.into_owned())
        else {
            return uri;
        };
        if path.as_bytes().get(1).is_some_and(u8::is_ascii_lowercase)
            && path.as_bytes().get(2) == Some(&b':')
        {
            let drive = char::from(path.as_bytes()[1].to_ascii_uppercase()).to_string();
            path.replace_range(1..2, &drive);
        }
        let mut canonical = Url::parse("big:///").expect("static BIG URI is valid");
        canonical.set_path(&path);
        return canonical;
    }
    uri
}

fn is_map_layer_file(file: &str) -> bool {
    file.rsplit(['/', '\\']).next().is_some_and(|name| {
        name.eq_ignore_ascii_case("map.ini") || name.eq_ignore_ascii_case("solo.ini")
    })
}

fn filter_map_ordering_diagnostics(
    diagnostics: &mut Vec<zerosyntax_analysis::Diagnostic>,
    enabled: bool,
) {
    if !enabled {
        diagnostics.retain(|diagnostic| diagnostic.code != "map-forward-reference");
    }
}

#[derive(Clone, Copy)]
struct RefreshOptions {
    enc: PositionEnc,
    expected_version: Option<i32>,
}

async fn refresh_document(
    client: Client,
    analyzer: Arc<RwLock<Arc<Analyzer>>>,
    docs: Arc<DashMap<Url, DocumentState>>,
    index: Arc<RwLock<WorkspaceIndex>>,
    map_ordering_diagnostics: Arc<AtomicBool>,
    uri: Url,
    options: RefreshOptions,
) {
    let analyzer = analyzer.read().expect("analyzer lock poisoned").clone();
    let Some((rope, parse, version)) = docs.get(&uri).and_then(|d| {
        if options
            .expected_version
            .is_some_and(|expected| expected != d.version)
        {
            None
        } else {
            Some((d.rope.clone(), d.parse.clone(), d.version))
        }
    }) else {
        return;
    };

    let defs = definitions_in(&analyzer, &parse, uri.as_str());
    let refs = references_in(&analyzer, &parse);
    let tags = module_tags_in(&analyzer, &parse);
    let object_models = object_models_in(&analyzer, &parse);
    let object_parents = object_parents_in(&parse);
    let str_keys = load_sibling_str_keys(&uri);

    // Keep this document guard through the short index commit so didChange
    // cannot advance the document and then be overwritten by this snapshot.
    let Some(entry) = docs.get(&uri) else { return };
    if entry.version != version {
        return;
    }
    if let Ok(mut idx) = index.write() {
        idx.set_file(uri.as_str(), defs);
        idx.set_file_refs(uri.as_str(), refs);
        idx.set_file_tags(uri.as_str(), tags);
        idx.set_file_object_models(uri.as_str(), object_models);
        idx.set_file_object_parents(uri.as_str(), object_parents);
        idx.set_ini_string_keys(uri.as_str(), str_keys);
    }
    drop(entry);

    // Take the cache only after the versioned index commit. Expensive work
    // above never empties the live document's cache when an edit supersedes it.
    let Some(mut entry) = docs.get_mut(&uri) else {
        return;
    };
    if entry.version != version {
        return;
    }
    let mut cache = std::mem::take(&mut entry.diag_cache);
    drop(entry);

    let lsp_diags: Vec<Diagnostic> = {
        let idx = index.read().ok();
        let mut diags = diagnostics::diagnose_with_cache(
            &analyzer,
            &parse,
            idx.as_deref(),
            Some(uri.as_str()),
            &mut cache,
        );
        filter_map_ordering_diagnostics(
            &mut diags,
            map_ordering_diagnostics.load(Ordering::Relaxed),
        );
        diags
            .iter()
            .map(|d| convert::to_lsp_diagnostic(&rope, d, options.enc))
            .collect()
    };

    let Some(mut entry) = docs.get_mut(&uri) else {
        return;
    };
    if entry.version != version {
        return;
    }
    entry.diag_cache = cache;
    drop(entry);

    client
        .publish_diagnostics(uri, lsp_diags, Some(version))
        .await;
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Backend {
            client,
            analyzer: Arc::new(RwLock::new(Arc::new(Analyzer::embedded()))),
            settings: Mutex::new(RuntimeSettings::default()),
            reload_lock: tokio::sync::Mutex::new(()),
            schema_error: Mutex::new(None),
            docs: Arc::new(DashMap::new()),
            virtual_files: DashMap::new(),
            index: Arc::new(RwLock::new(WorkspaceIndex::new())),
            roots: Mutex::new(Vec::new()),
            encoding: OnceLock::new(),
            format_enabled: AtomicBool::new(false),
            formatting_dynamic_registration: OnceLock::new(),
            map_ordering_diagnostics: Arc::new(AtomicBool::new(true)),
            base_indexed_count: AtomicUsize::new(0),
            scan_finished: AtomicBool::new(false),
            base_roots_hint_shown: AtomicBool::new(false),
            client_base_ini_hint: OnceLock::new(),
            snippet_support: OnceLock::new(),
            progress_support: OnceLock::new(),
            analysis_debounce_ms: AtomicU64::new(DEFAULT_ANALYSIS_DEBOUNCE_MS),
            semantic_result_id: AtomicU64::new(1),
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
        self.format_enabled.load(Ordering::Relaxed)
    }

    fn map_ordering_diagnostics_enabled(&self) -> bool {
        self.map_ordering_diagnostics.load(Ordering::Relaxed)
    }

    fn next_semantic_id(&self) -> u64 {
        self.semantic_result_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Update the cross-file index from the document's cached parse, run
    /// diagnostics (via the per-block cache), and publish. The parse itself is
    /// maintained synchronously by `did_open`/`did_change`.
    async fn refresh(&self, uri: &Url, expected_version: Option<i32>) {
        refresh_document(
            self.client.clone(),
            self.analyzer.clone(),
            self.docs.clone(),
            self.index.clone(),
            self.map_ordering_diagnostics.clone(),
            uri.clone(),
            RefreshOptions {
                enc: self.enc(),
                expected_version,
            },
        )
        .await;
        self.maybe_warn_missing_base_roots(uri).await;
    }

    fn schedule_refresh(&self, uri: Url, version: i32) {
        let client = self.client.clone();
        let analyzer = self.analyzer.clone();
        let docs = self.docs.clone();
        let index = self.index.clone();
        let enc = self.enc();
        let map_ordering_diagnostics = self.map_ordering_diagnostics.clone();
        let delay = Duration::from_millis(self.analysis_debounce_ms.load(Ordering::Relaxed));
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            refresh_document(
                client,
                analyzer,
                docs,
                index,
                map_ordering_diagnostics,
                uri,
                RefreshOptions {
                    enc,
                    expected_version: Some(version),
                },
            )
            .await;
        });
    }

    async fn refresh_all(&self) {
        let open: Vec<Url> = self
            .docs
            .iter()
            .map(|document| document.key().clone())
            .collect();
        for uri in open {
            self.refresh(&uri, None).await;
        }
    }

    fn clear_diagnostic_caches(&self) {
        for mut document in self.docs.iter_mut() {
            document.diag_cache = DiagnosticsCache::new();
        }
    }

    async fn set_formatting_enabled(&self, enabled: bool) {
        self.format_enabled.store(enabled, Ordering::Relaxed);
        if !self
            .formatting_dynamic_registration
            .get()
            .copied()
            .unwrap_or(false)
        {
            return;
        }
        let result = if enabled {
            self.client
                .register_capability(vec![Registration {
                    id: FORMATTING_REGISTRATION_ID.into(),
                    method: "textDocument/formatting".into(),
                    register_options: Some(serde_json::json!({
                        "documentSelector": [{"scheme": "file", "language": "generals-ini"}]
                    })),
                }])
                .await
        } else {
            // The request guard is already false, so a client that fails to
            // unregister can only receive a harmless null response.
            self.client
                .unregister_capability(vec![Unregistration {
                    id: FORMATTING_REGISTRATION_ID.into(),
                    method: "textDocument/formatting".into(),
                }])
                .await
        };
        if let Err(error) = result {
            self.client
                .log_message(
                    MessageType::ERROR,
                    format!("failed to update formatting capability: {error}"),
                )
                .await;
        }
    }

    async fn apply_settings(&self, settings: RuntimeSettings) {
        let _reload = self.reload_lock.lock().await;
        let previous = {
            let Ok(mut current) = self.settings.lock() else {
                return;
            };
            if *current == settings {
                return;
            }
            let previous = current.clone();
            *current = settings.clone();
            previous
        };

        self.analysis_debounce_ms
            .store(settings.debounce_ms, Ordering::Relaxed);
        self.map_ordering_diagnostics
            .store(settings.map_ordering_diagnostics, Ordering::Relaxed);
        if previous.format_enabled != settings.format_enabled {
            self.set_formatting_enabled(settings.format_enabled).await;
        }

        let schema_changed = previous.schema_path != settings.schema_path;
        let roots_changed = previous.base_ini_roots != settings.base_ini_roots;
        let bare_changed = previous.allow_bare_percentages != settings.allow_bare_percentages;
        let strictness_changed =
            previous.model_member_strictness != settings.model_member_strictness;
        let map_ordering_changed =
            previous.map_ordering_diagnostics != settings.map_ordering_diagnostics;

        if schema_changed || roots_changed {
            let (mut analyzer, warning) = if schema_changed {
                if settings.schema_path.is_empty() {
                    (Analyzer::embedded(), None)
                } else {
                    load_schema_or_embedded(&settings.schema_path)
                }
            } else if bare_changed {
                (Analyzer::new(self.analyzer().schema().clone()), None)
            } else {
                self.scan_workspace(self.analyzer(), false).await;
                self.refresh_all().await;
                return;
            };
            analyzer.set_allow_bare_percentages(settings.allow_bare_percentages);
            let analyzer = Arc::new(analyzer);
            if bare_changed && !schema_changed {
                if let Ok(mut current) = self.analyzer.write() {
                    *current = analyzer.clone();
                }
            }
            if let Some(warning) = warning {
                self.client
                    .show_message(MessageType::WARNING, warning)
                    .await;
            }
            self.scan_workspace(analyzer, schema_changed).await;
            self.refresh_all().await;
            return;
        }

        if bare_changed {
            let mut analyzer = Analyzer::new(self.analyzer().schema().clone());
            analyzer.set_allow_bare_percentages(settings.allow_bare_percentages);
            if let Ok(mut current) = self.analyzer.write() {
                *current = Arc::new(analyzer);
            }
            self.clear_diagnostic_caches();
        }
        if strictness_changed {
            if let Ok(mut index) = self.index.write() {
                index.set_model_member_strictness(settings.model_member_strictness);
            }
        }
        if bare_changed || strictness_changed || map_ordering_changed {
            self.refresh_all().await;
        }
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
        let roots_empty = self
            .settings
            .lock()
            .map(|settings| settings.base_ini_roots.is_empty())
            .unwrap_or(true);
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
                "ZeroSyntax v2: map/solo.ini diagnostics are limited until base game or mod data is configured. Set `zerosyntax.baseIniRoots` to your game/mod `.big` files or data folders.",
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
    async fn scan_workspace(&self, analyzer: Arc<Analyzer>, replace_analyzer: bool) {
        let progress_token = self.begin_scan_progress().await;
        let roots = self.roots.lock().map(|r| r.clone()).unwrap_or_default();
        let (base_roots, model_member_strictness) = self
            .settings
            .lock()
            .map(|settings| {
                (
                    settings.base_ini_roots.clone(),
                    settings.model_member_strictness,
                )
            })
            .unwrap_or_default();
        // The blocking scan streams (done, total) over a channel; forward
        // each update as a progress report while waiting for the results.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(usize, usize)>();
        let scan_analyzer = analyzer.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let mut done = 0;
            let mut last_percent = u32::MAX;
            // Throttle to whole-percent changes (plus the final count) so a
            // 10k-file scan sends ~100 notifications, not 10k.
            let mut progress = |_done_in_batch: usize, total: usize| {
                done += 1;
                let percent = (done * 100 / total.max(1)) as u32;
                if percent != last_percent || done == total {
                    last_percent = percent;
                    let _ = tx.send((done, total));
                }
            };
            scan_with_cache(&scan_analyzer, &roots, &base_roots, &mut progress)
        });
        while let Some((done, total)) = rx.recv().await {
            self.report_scan_progress(&progress_token, done, total)
                .await;
        }
        let scanned = handle.await.unwrap_or_default();
        if replace_analyzer {
            if let Ok(mut current) = self.analyzer.write() {
                *current = analyzer.clone();
            }
            for mut document in self.docs.iter_mut() {
                document.parse = Arc::new(analyzer.parse(&document.text));
                document.diag_cache = DiagnosticsCache::new();
                document.last_semantic = None;
            }
        }

        let base_ini_count = scanned
            .iter()
            .filter(|(is_base, (_, _, _, _, _, _, models, assets, _))| {
                *is_base && models.is_empty() && assets.is_empty()
            })
            .count();
        self.base_indexed_count
            .store(base_ini_count, Ordering::Relaxed);
        self.scan_finished.store(true, Ordering::Relaxed);
        let ini_total = scanned
            .iter()
            .filter(|(_, (_, _, _, _, _, _, models, assets, _))| {
                models.is_empty() && assets.is_empty()
            })
            .count();
        let model_total: usize = scanned
            .iter()
            .map(|(_, (_, _, _, _, _, _, models, _, _))| models.len())
            .sum();
        let (audio_total, texture_total) = scanned
            .iter()
            .flat_map(|(_, (_, _, _, _, _, _, _, assets, _))| assets)
            .fold((0, 0), |(audio, texture), asset| match asset.kind {
                zerosyntax_analysis::index::AssetKind::Audio => (audio + 1, texture),
                zerosyntax_analysis::index::AssetKind::Texture => (audio, texture + 1),
            });
        // Build the replacement off to the side so removed roots cannot leave
        // stale definitions, assets, inheritance, models, or virtual files.
        let open: std::collections::HashSet<String> = self
            .docs
            .iter()
            .map(|e| e.key().as_str().to_string())
            .collect();
        let mut replacement = WorkspaceIndex::new();
        replacement.set_model_member_strictness(model_member_strictness);
        self.virtual_files.clear();
        for (_, (uri, defs, refs, tags, object_models, object_parents, models, assets, text)) in
            scanned
        {
            if let Some(text) = text {
                self.virtual_files.insert(uri.clone(), text);
            }
            if !open.contains(&uri) {
                replacement.set_file(&uri, defs);
                replacement.set_file_refs(&uri, refs);
                replacement.set_file_tags(&uri, tags);
                replacement.set_file_object_models(&uri, object_models);
                replacement.set_file_object_parents(&uri, object_parents);
                replacement.set_file_models(&uri, models);
                replacement.set_file_assets(&uri, assets);
            }
        }
        for document in self.docs.iter() {
            let uri = document.key();
            replacement.set_file(
                uri.as_str(),
                definitions_in(&analyzer, &document.parse, uri.as_str()),
            );
            replacement.set_file_refs(uri.as_str(), references_in(&analyzer, &document.parse));
            replacement.set_file_tags(uri.as_str(), module_tags_in(&analyzer, &document.parse));
            replacement
                .set_file_object_models(uri.as_str(), object_models_in(&analyzer, &document.parse));
            replacement.set_file_object_parents(uri.as_str(), object_parents_in(&document.parse));
            replacement.set_ini_string_keys(uri.as_str(), load_sibling_str_keys(uri));
        }
        if let Ok(mut index) = self.index.write() {
            *index = replacement;
        }
        self.clear_diagnostic_caches();
        self.end_scan_progress(
            progress_token,
            ini_total,
            model_total,
            audio_total,
            texture_total,
        )
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
                        message: Some("scanning workspace and configured game-data roots".into()),
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
        audio_total: usize,
        texture_total: usize,
    ) {
        let Some(token) = token else { return };
        self.client
            .send_notification::<notification::Progress>(ProgressParams {
                token,
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                    message: Some(format!(
                        "{ini_total} INI files, {model_total} W3D models, {audio_total} audio files, {texture_total} textures indexed"
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
        let uri = canonical_uri(uri.clone());
        if let Some(doc) = self.docs.get(&uri) {
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
        let Some(uri) = Url::parse(&params.uri).ok().map(canonical_uri) else {
            return Ok(None);
        };
        Ok(self
            .virtual_files
            .get(uri.as_str())
            .map(|text| text.to_string()))
    }

    pub async fn index_cache_path(&self) -> Result<String> {
        let roots = self
            .roots
            .lock()
            .map(|roots| roots.clone())
            .unwrap_or_default();
        let base_roots = self
            .settings
            .lock()
            .map(|settings| settings.base_ini_roots.clone())
            .unwrap_or_default();
        Ok(index_cache_path(&roots, &base_roots)
            .to_string_lossy()
            .into_owned())
    }

    /// The (kind, name, span) under the cursor — a reference-typed value token
    /// or a definition's name token. The shared entry point for
    /// find-references and rename, which work from either end of an edge.
    fn symbol_at(&self, uri: &Url, pos: Position) -> Option<SymbolAt> {
        let (rope, parse) = self.doc(uri)?;
        let offset = convert::position_to_offset(&rope, pos, self.enc());
        let analyzer = self.analyzer();
        reference_at(&analyzer, &parse, offset)
            .or_else(|| definition_at(&analyzer, &parse, offset))
            .map(SymbolAt::Reference)
            .or_else(|| {
                module_tag_reference_at(&parse, offset).map(|symbol| SymbolAt::ModuleTag {
                    before: Some(symbol.span.start),
                    symbol,
                })
            })
            .or_else(|| {
                module_tag_definition_at(&parse, offset).map(|symbol| SymbolAt::ModuleTag {
                    symbol,
                    before: None,
                })
            })
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

        // Editor-facing settings arrive as `initializationOptions`. Shape:
        // `{ "format": {"enable": bool}, "schemaPath": "schema.json",
        //    "analysis": {"modelMemberStrictness": "compatible",
        //                 "allowPercentagesWithoutSign": false,
        //                 "mapOrderingDiagnostics": true, "debounceMs": 250},
        //    "baseIniRoots": ["dir-or-big", ...],
        //    "clientBaseIniHint": bool }`.
        let settings = RuntimeSettings::from_value(params.initialization_options.as_ref());
        self.format_enabled
            .store(settings.format_enabled, Ordering::Relaxed);
        self.map_ordering_diagnostics
            .store(settings.map_ordering_diagnostics, Ordering::Relaxed);
        self.analysis_debounce_ms
            .store(settings.debounce_ms, Ordering::Relaxed);
        if let Ok(mut index) = self.index.write() {
            index.set_model_member_strictness(settings.model_member_strictness);
        }

        if !settings.schema_path.is_empty() {
            let (mut analyzer, error) = load_schema_or_embedded(&settings.schema_path);
            analyzer.set_allow_bare_percentages(settings.allow_bare_percentages);
            if let Ok(mut current) = self.analyzer.write() {
                *current = Arc::new(analyzer);
            }
            if let Ok(mut current) = self.schema_error.lock() {
                *current = error;
            }
        } else if settings.allow_bare_percentages {
            if let Ok(mut current) = self.analyzer.write() {
                Arc::get_mut(&mut current)
                    .expect("analyzer shared before initialization completed")
                    .set_allow_bare_percentages(true);
            }
        }
        if let Ok(mut current) = self.settings.lock() {
            *current = settings.clone();
        }

        let dynamic_formatting = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|text| text.formatting.as_ref())
            .and_then(|formatting| formatting.dynamic_registration)
            .unwrap_or(false);
        let _ = self.formatting_dynamic_registration.set(dynamic_formatting);
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
                document_formatting_provider: (!dynamic_formatting && settings.format_enabled)
                    .then_some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
                        ..Default::default()
                    },
                )),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        CLEAR_INDEX_CACHE_COMMAND.into(),
                        REBUILD_INDEX_CACHE_COMMAND.into(),
                    ],
                    work_done_progress_options: Default::default(),
                }),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        if let Some(error) = self.schema_error.lock().ok().and_then(|mut e| e.take()) {
            self.client.show_message(MessageType::WARNING, error).await;
        }
        if self.format_enabled() {
            self.set_formatting_enabled(true).await;
        }
        self.scan_workspace(self.analyzer(), false).await;
        // Re-publish diagnostics for any already-open docs now that the index
        // is populated (so cross-file references resolve). The cached parse is
        // still valid — only the index changed.
        self.refresh_all().await;
        let (ini, models, audio, textures) = {
            let idx = self.index.read().ok();
            let models = idx
                .as_ref()
                .map(|i| i.model_names().count())
                .unwrap_or_default();
            let audio = idx
                .as_ref()
                .map(|i| {
                    i.asset_names(zerosyntax_analysis::index::AssetKind::Audio)
                        .count()
                })
                .unwrap_or_default();
            let textures = idx
                .as_ref()
                .map(|i| {
                    i.asset_names(zerosyntax_analysis::index::AssetKind::Texture)
                        .count()
                })
                .unwrap_or_default();
            (
                self.base_indexed_count.load(Ordering::Relaxed),
                models,
                audio,
                textures,
            )
        };
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "zerosyntax language server ready ({ini} base INI files, {models} W3D models, {audio} audio files, {textures} textures indexed)"
                ),
            )
            .await;
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        if params.command != CLEAR_INDEX_CACHE_COMMAND
            && params.command != REBUILD_INDEX_CACHE_COMMAND
        {
            return Ok(None);
        }
        let roots = self
            .roots
            .lock()
            .map(|roots| roots.clone())
            .unwrap_or_default();
        let base_roots = self
            .settings
            .lock()
            .map(|settings| settings.base_ini_roots.clone())
            .unwrap_or_default();
        let cleared = clear_index_cache(&roots, &base_roots).map_err(|error| {
            tracing::warn!(%error, "could not clear asset index cache");
            tower_lsp::jsonrpc::Error::internal_error()
        })?;
        if params.command == REBUILD_INDEX_CACHE_COMMAND {
            self.scan_finished.store(false, Ordering::Relaxed);
            self.base_indexed_count.store(0, Ordering::Relaxed);
            self.scan_workspace(self.analyzer(), false).await;
            let open: Vec<Url> = self.docs.iter().map(|entry| entry.key().clone()).collect();
            for uri in open {
                self.refresh(&uri, None).await;
            }
            self.client
                .show_message(MessageType::INFO, "ZeroSyntax index cache rebuilt.")
                .await;
            return Ok(Some(
                serde_json::json!({ "rebuilt": true, "cleared": cleared }),
            ));
        }
        let message = if cleared {
            "ZeroSyntax index cache cleared. Restart the language server to rebuild it."
        } else {
            "ZeroSyntax index cache is already clear."
        };
        self.client.show_message(MessageType::INFO, message).await;
        Ok(Some(serde_json::json!({ "cleared": cleared })))
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        self.apply_settings(RuntimeSettings::from_value(Some(&params.settings)))
            .await;
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
        self.refresh(&uri, Some(version)).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = canonical_uri(params.text_document.uri);
        let version = params.text_document.version;
        let enc = self.enc();
        let analyzer = self.analyzer();
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
                entry.parse = Arc::new(analyzer.parse(&entry.text));
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
                                analyzer.reparse(&entry.parse, &entry.text, &new_text, edit);
                            entry.parse = Arc::new(parse);
                            entry.text = new_text;
                        }
                        None => {
                            // Full-document replacement.
                            entry.rope = Rope::from_str(&change.text);
                            entry.text = change.text.into();
                            entry.parse = Arc::new(analyzer.parse(&entry.text));
                        }
                    }
                }
                entry.version = version;
            }
            // Definition names power reference completions and are cheap to
            // extract. Commit them while the document guard preserves version
            // order; the expensive index passes wait for the debounce.
            let defs = definitions_in(&analyzer, &entry.parse, uri.as_str());
            if let Ok(mut idx) = self.index.write() {
                idx.set_file(uri.as_str(), defs);
            }
        }
        self.schedule_refresh(uri, version);
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
            let mut diags = diagnostics::diagnose_with_cache(
                &self.analyzer(),
                &parse,
                idx.as_deref(),
                Some(uri.as_str()),
                &mut cache,
            );
            filter_map_ordering_diagnostics(&mut diags, self.map_ordering_diagnostics_enabled());
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
        let locations: Vec<(String, zerosyntax_analysis::Span)> = {
            let Ok(idx) = self.index.read() else {
                return Ok(None);
            };
            if let Some(reference) = reference_at(&self.analyzer(), &parse, offset) {
                idx.locations(reference.kind, &reference.name)
                    .iter()
                    .map(|location| (location.file.clone(), location.span))
                    .collect()
            } else if let Some(reference) = module_tag_reference_at(&parse, offset) {
                idx.effective_module_tag_locations(
                    &reference.object,
                    &reference.name,
                    Some(uri.as_str()),
                    Some(reference.span.start),
                )
                .into_iter()
                .map(|location| (location.file.clone(), location.span))
                .collect()
            } else {
                return Ok(None);
            }
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
            match &sym {
                SymbolAt::Reference(sym) => {
                    let mut locations = idx
                        .reference_sites(sym.kind, &sym.name)
                        .iter()
                        .map(|l| (l.file.clone(), l.span))
                        .collect::<Vec<_>>();
                    if params.context.include_declaration {
                        locations.extend(
                            idx.locations(sym.kind, &sym.name)
                                .iter()
                                .map(|l| (l.file.clone(), l.span)),
                        );
                    }
                    locations
                }
                SymbolAt::ModuleTag { symbol, before } => {
                    let mut locations = idx
                        .module_tag_reference_locations(&symbol.object, &symbol.name)
                        .into_iter()
                        .map(|location| (location.file.clone(), location.span))
                        .collect::<Vec<_>>();
                    if params.context.include_declaration {
                        let definitions = if before.is_some() {
                            idx.effective_module_tag_locations(
                                &symbol.object,
                                &symbol.name,
                                Some(uri.as_str()),
                                *before,
                            )
                        } else {
                            idx.module_tag_locations(&symbol.object, &symbol.name)
                        };
                        locations.extend(
                            definitions
                                .into_iter()
                                .map(|location| (location.file.clone(), location.span)),
                        );
                    }
                    locations
                }
            }
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
            sym.span(),
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
            match &sym {
                SymbolAt::Reference(sym) => idx
                    .reference_sites(sym.kind, &sym.name)
                    .iter()
                    .chain(idx.locations(sym.kind, &sym.name).iter())
                    .map(|location| (location.file.clone(), location.span))
                    .collect(),
                SymbolAt::ModuleTag { symbol, before } => {
                    let definitions = if before.is_some() {
                        idx.effective_module_tag_locations(
                            &symbol.object,
                            &symbol.name,
                            Some(uri.as_str()),
                            *before,
                        )
                    } else {
                        idx.module_tag_locations(&symbol.object, &symbol.name)
                    };
                    idx.module_tag_reference_locations(&symbol.object, &symbol.name)
                        .into_iter()
                        .chain(definitions)
                        .map(|location| (location.file.clone(), location.span))
                        .collect()
                }
            }
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
    fn map_ordering_diagnostics_can_be_disabled() {
        assert!(RuntimeSettings::from_value(None).map_ordering_diagnostics);
        assert!(
            !RuntimeSettings::from_value(Some(&serde_json::json!({
                "zerosyntax": {"analysis": {"mapOrderingDiagnostics": false}}
            })))
            .map_ordering_diagnostics
        );

        let mut diagnostics = vec![
            zerosyntax_analysis::Diagnostic {
                span: zerosyntax_analysis::Span::new(0, 1),
                severity: zerosyntax_analysis::Severity::Warning,
                code: "map-forward-reference",
                message: String::new(),
            },
            zerosyntax_analysis::Diagnostic {
                span: zerosyntax_analysis::Span::new(0, 1),
                severity: zerosyntax_analysis::Severity::Warning,
                code: "map-projectile-object",
                message: String::new(),
            },
        ];
        filter_map_ordering_diagnostics(&mut diagnostics, false);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "map-projectile-object");
    }

    #[test]
    fn analysis_debounce_defaults_overrides_and_clamps() {
        assert_eq!(normalized_debounce_ms(None), 250);
        assert_eq!(normalized_debounce_ms(Some(&serde_json::json!(0))), 0);
        assert_eq!(normalized_debounce_ms(Some(&serde_json::json!(400))), 400);
        assert_eq!(normalized_debounce_ms(Some(&serde_json::json!(-1))), 0);
        assert_eq!(normalized_debounce_ms(Some(&serde_json::json!(9000))), 5000);
        assert_eq!(normalized_debounce_ms(Some(&serde_json::json!(12.5))), 250);
    }

    #[test]
    fn runtime_settings_accept_startup_and_vscode_shapes() {
        let startup = RuntimeSettings::from_value(Some(&serde_json::json!({
            "format": {"enable": true},
            "schemaPath": "schema.json",
            "baseIniRoots": ["base"],
            "analysis": {"modelMemberStrictness": "strict", "debounceMs": 9000}
        })));
        let notification = RuntimeSettings::from_value(Some(&serde_json::json!({
            "zerosyntax": {
                "format": {"enable": true},
                "schema": {"path": "schema.json"},
                "baseIniRoots": ["base"],
                "analysis": {"modelMemberStrictness": "strict", "debounceMs": 9000}
            }
        })));
        assert_eq!(startup, notification);
        assert_eq!(startup.debounce_ms, 5000);
    }

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
    fn canonical_uri_pass_through_other_schemes() {
        let u = Url::parse("untitled:///buffer").unwrap();
        assert_eq!(canonical_uri(u.clone()), u);
    }

    #[test]
    fn canonical_uri_normalises_big_uri_path() {
        let scanner =
            Url::parse("big:///C:/Game%20Folder/Base%23.big!/Data/INI/Object.ini").unwrap();
        for client in [
            "big:///C%3A/Game%20Folder/Base%23.big!/Data/INI/Object.ini",
            "big:/c%3A/Game%20Folder/Base%23.big%21/Data/INI/Object.ini",
        ] {
            assert_eq!(canonical_uri(Url::parse(client).unwrap()), scanner);
        }
    }

    #[test]
    fn canonical_uri_keeps_scanner_big_uri() {
        let scanner =
            Url::parse("big:///C:/Game%20Folder/Base%23.big!/Data/INI/Object.ini").unwrap();
        assert_eq!(canonical_uri(scanner.clone()), scanner);
    }

    #[test]
    fn malformed_or_unknown_big_uri_has_no_virtual_content() {
        let files = DashMap::new();
        files.insert(
            "big:///C:/Game%20Folder/Base.big!/Data/INI/Object.ini".to_string(),
            Arc::<str>::from("Object Known\nEnd\n"),
        );
        let malformed = Url::parse("big:///C%3A/Game%FF/Base.big!/Data/INI/Object.ini").unwrap();
        let unknown =
            Url::parse("big:///C%3A/Game%20Folder/Base.big!/Data/INI/Unknown.ini").unwrap();
        assert_eq!(canonical_uri(malformed.clone()), malformed);
        assert!(!files.contains_key(canonical_uri(malformed).as_str()));
        assert!(!files.contains_key(canonical_uri(unknown).as_str()));
    }

    #[test]
    fn detects_map_layer_filenames() {
        assert!(is_map_layer_file("file:///C:/Maps/Foo/map.ini"));
        assert!(is_map_layer_file("C:\\Maps\\Foo\\solo.ini"));
        assert!(!is_map_layer_file("C:/Data/INI/Object.ini"));
    }

    #[test]
    fn scans_ini_w3d_audio_and_texture_from_big_archive() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zerosyntax test #-{}.big", std::process::id()));
        let entries: Vec<(&str, &[u8])> = vec![
            ("Data\\INI\\Test.ini", b"Object BigArchiveObject\nEnd\n"),
            ("Art\\Good.w3d", b""),
            ("Audio\\Click.WAV", b"not read"),
            ("Textures\\Particle.DDS", b"not read"),
        ];
        let data_offset = 0x10
            + entries
                .iter()
                .map(|(name, _)| 8 + name.len() + 1)
                .sum::<usize>();
        let archive_size = data_offset + entries.iter().map(|(_, data)| data.len()).sum::<usize>();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"BIGF");
        bytes.extend_from_slice(&(archive_size as u32).to_be_bytes());
        bytes.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        let mut offset = data_offset;
        for (name, data) in &entries {
            bytes.extend_from_slice(&(offset as u32).to_be_bytes());
            bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
            offset += data.len();
        }
        for (_, data) in &entries {
            bytes.extend_from_slice(data);
        }
        std::fs::write(&path, bytes).unwrap();

        let analyzer = Analyzer::embedded();
        let scanned = scan_big(&analyzer, &path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(scanned.len(), 3, "INI, W3D, and one aggregated asset entry");
        let ini = scanned.iter().find(|entry| !entry.1.is_empty()).unwrap();
        assert!(Url::parse(&ini.0).is_ok());
        assert!(ini.1.iter().any(|d| d.name == "BigArchiveObject"));
        assert_eq!(ini.8.as_deref(), Some("Object BigArchiveObject\nEnd\n"));
        assert!(scanned
            .iter()
            .any(|entry| entry.6.iter().any(|model| model.name == "Good")));
        let assets = &scanned.iter().find(|entry| !entry.7.is_empty()).unwrap().7;
        assert_eq!(assets.len(), 2);
        assert!(assets.iter().any(|asset| asset.name == "Click.WAV"));
        assert!(assets.iter().any(|asset| asset.name == "Particle.DDS"));
    }

    #[test]
    fn loose_directory_scan_indexes_audio_and_texture_assets() {
        let dir = std::env::temp_dir().join(format!("zerosyntax-assets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Click.wav"), b"").unwrap();
        std::fs::write(dir.join("Particle.tga"), b"").unwrap();
        let scanned = scan_roots(&Analyzer::embedded(), std::slice::from_ref(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
        let assets = scanned
            .into_iter()
            .flat_map(|entry| entry.7)
            .collect::<Vec<_>>();
        assert_eq!(assets.len(), 2);
        assert!(assets.iter().any(|asset| asset.name == "Click.wav"));
        assert!(assets.iter().any(|asset| asset.name == "Particle.tga"));
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
        for (uri, defs, refs, tags, object_models, object_parents, models, assets, _) in scanned {
            idx.set_file(&uri, defs);
            idx.set_file_refs(&uri, refs);
            idx.set_file_tags(&uri, tags);
            idx.set_file_object_models(&uri, object_models);
            idx.set_file_object_parents(&uri, object_parents);
            idx.set_file_models(&uri, models);
            idx.set_file_assets(&uri, assets);
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
