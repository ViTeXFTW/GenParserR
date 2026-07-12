//! Open-document state and source lookup for the language server.

use std::sync::Arc;

use dashmap::DashMap;
use ropey::Rope;
use tower_lsp::lsp_types::{SemanticToken, TextDocumentContentChangeEvent, Url};
use zerosyntax_analysis::diagnostics::DiagnosticsCache;
use zerosyntax_analysis::Analyzer;
use zerosyntax_syntax::{Edit, Parse};

use crate::convert::{self, PositionEnc};

const BULK_CHANGE_THRESHOLD: usize = 8;
const BULK_TEXT_BYTES: usize = 32 * 1024;

/// An immutable, internally consistent view of an open document.
#[derive(Clone)]
pub(crate) struct DocumentSnapshot {
    pub(crate) rope: Rope,
    pub(crate) text: Arc<str>,
    pub(crate) parse: Arc<Parse>,
    pub(crate) version: i32,
}

/// A snapshot plus exclusive ownership of its diagnostics cache.
pub(crate) struct DiagnosticSnapshot {
    pub(crate) document: DocumentSnapshot,
    pub(crate) cache: DiagnosticsCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeStrategy {
    Incremental,
    Full,
    Bulk,
}

struct DocumentState {
    rope: Rope,
    text: Arc<str>,
    parse: Arc<Parse>,
    version: i32,
    diag_cache: DiagnosticsCache,
    last_semantic: Option<(u64, Vec<SemanticToken>)>,
}

impl DocumentState {
    fn snapshot(&self) -> DocumentSnapshot {
        DocumentSnapshot {
            rope: self.rope.clone(),
            text: Arc::clone(&self.text),
            parse: Arc::clone(&self.parse),
            version: self.version,
        }
    }
}

/// Owns live editor buffers and read-only files synthesized from BIG archives.
pub(crate) struct DocumentStore {
    open: DashMap<Url, DocumentState>,
    virtual_files: DashMap<String, Arc<str>>,
}

impl DocumentStore {
    pub(crate) fn new() -> Self {
        Self {
            open: DashMap::new(),
            virtual_files: DashMap::new(),
        }
    }

    pub(crate) fn open(&self, analyzer: &Analyzer, uri: Url, text: String, version: i32) {
        let text: Arc<str> = text.into();
        let state = DocumentState {
            rope: Rope::from_str(&text),
            parse: Arc::new(analyzer.parse(&text)),
            text,
            version,
            diag_cache: DiagnosticsCache::new(),
            last_semantic: None,
        };
        self.open.insert(uri, state);
    }

    pub(crate) fn change(
        &self,
        analyzer: &Analyzer,
        uri: &Url,
        changes: Vec<TextDocumentContentChangeEvent>,
        version: i32,
        encoding: PositionEnc,
    ) -> Option<ChangeStrategy> {
        let mut state = self.open.get_mut(uri)?;
        let bulk = changes.len() > BULK_CHANGE_THRESHOLD
            || (changes.len() > 1
                && changes
                    .iter()
                    .map(|change| change.text.len())
                    .sum::<usize>()
                    > BULK_TEXT_BYTES);

        if bulk && changes.iter().all(|change| change.range.is_some()) {
            for change in changes {
                convert::apply_change(&mut state.rope, change.range, &change.text, encoding);
            }
            state.text = state.rope.to_string().into();
            state.parse = Arc::new(analyzer.parse(&state.text));
            state.version = version;
            return Some(ChangeStrategy::Bulk);
        }

        let mut strategy = ChangeStrategy::Incremental;
        for change in changes {
            match change.range {
                Some(range) => {
                    let start = convert::position_to_offset(&state.rope, range.start, encoding);
                    let old_end = convert::position_to_offset(&state.rope, range.end, encoding);
                    convert::apply_change(&mut state.rope, Some(range), &change.text, encoding);
                    let new_text: Arc<str> = state.rope.to_string().into();
                    let edit = Edit {
                        start: start as usize,
                        old_end: old_end as usize,
                        new_len: change.text.len(),
                    };
                    let (parse, _) = analyzer.reparse(&state.parse, &state.text, &new_text, edit);
                    state.parse = Arc::new(parse);
                    state.text = new_text;
                }
                None => {
                    state.rope = Rope::from_str(&change.text);
                    state.text = change.text.into();
                    state.parse = Arc::new(analyzer.parse(&state.text));
                    strategy = ChangeStrategy::Full;
                }
            }
        }
        state.version = version;
        Some(strategy)
    }

    pub(crate) fn close(&self, uri: &Url) {
        self.open.remove(uri);
    }

    pub(crate) fn snapshot(&self, uri: &Url) -> Option<DocumentSnapshot> {
        self.open.get(uri).map(|state| state.snapshot())
    }

    pub(crate) fn checkout_diagnostics(&self, uri: &Url) -> Option<DiagnosticSnapshot> {
        self.open.get_mut(uri).map(|mut state| DiagnosticSnapshot {
            document: state.snapshot(),
            cache: std::mem::take(&mut state.diag_cache),
        })
    }

    /// Restore a warmed cache only when the document version is still current.
    pub(crate) fn restore_diagnostics(
        &self,
        uri: &Url,
        version: i32,
        cache: DiagnosticsCache,
    ) -> bool {
        let Some(mut state) = self.open.get_mut(uri) else {
            return false;
        };
        if state.version != version {
            return false;
        }
        state.diag_cache = cache;
        true
    }

    pub(crate) fn clear_diagnostic_caches(&self) {
        for mut state in self.open.iter_mut() {
            state.diag_cache = DiagnosticsCache::new();
        }
    }

    pub(crate) fn replace_semantic_history(
        &self,
        uri: &Url,
        history: (u64, Vec<SemanticToken>),
    ) -> Option<(u64, Vec<SemanticToken>)> {
        self.open
            .get_mut(uri)
            .and_then(|mut state| state.last_semantic.replace(history))
    }

    pub(crate) fn open_uris(&self) -> Vec<Url> {
        self.open.iter().map(|state| state.key().clone()).collect()
    }

    pub(crate) fn open_uri_strings(&self) -> std::collections::HashSet<String> {
        self.open
            .iter()
            .map(|state| state.key().as_str().to_string())
            .collect()
    }

    pub(crate) fn replace_virtual_files(
        &self,
        files: impl IntoIterator<Item = (String, Arc<str>)>,
    ) {
        self.virtual_files.clear();
        for (uri, text) in files {
            self.virtual_files.insert(uri, text);
        }
    }

    pub(crate) fn clear_virtual_files(&self) {
        self.virtual_files.clear();
    }

    pub(crate) fn read_virtual_file(&self, uri: &str) -> Option<String> {
        self.virtual_files.get(uri).map(|text| text.to_string())
    }

    /// Resolve source with live editor buffers taking precedence over virtual
    /// BIG entries, which in turn take precedence over disk.
    pub(crate) fn rope_for(&self, uri: &Url) -> Option<Rope> {
        if let Some(document) = self.open.get(uri) {
            return Some(document.rope.clone());
        }
        if let Some(text) = self.virtual_files.get(uri.as_str()) {
            return Some(Rope::from_str(&text));
        }
        let path = uri.to_file_path().ok()?;
        std::fs::read(path)
            .ok()
            .map(|bytes| Rope::from_str(&String::from_utf8_lossy(&bytes)))
    }

    pub(crate) fn source_text(&self, uri: &str) -> Option<String> {
        self.rope_for(&Url::parse(uri).ok()?)
            .map(|rope| rope.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use tower_lsp::lsp_types::{Position, Range};

    use super::*;

    fn uri(name: &str) -> Url {
        Url::parse(&format!("file:///C:/zerosyntax-tests/{name}")).unwrap()
    }

    fn ranged(start: Position, end: Position, text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(Range { start, end }),
            range_length: None,
            text: text.into(),
        }
    }

    #[test]
    fn ranged_incremental_change_keeps_snapshot_synchronized() {
        let analyzer = Analyzer::embedded();
        let store = DocumentStore::new();
        let uri = uri("incremental.ini");
        store.open(&analyzer, uri.clone(), "Object Old\nEnd\n".into(), 1);

        let strategy = store.change(
            &analyzer,
            &uri,
            vec![ranged(Position::new(0, 7), Position::new(0, 10), "New")],
            2,
            PositionEnc::Utf16,
        );

        assert_eq!(strategy, Some(ChangeStrategy::Incremental));
        let snapshot = store.snapshot(&uri).unwrap();
        assert_eq!(&*snapshot.text, "Object New\nEnd\n");
        assert_eq!(snapshot.rope.to_string(), &*snapshot.text);
        assert_eq!(snapshot.parse.syntax().text().to_string(), &*snapshot.text);
        assert_eq!(snapshot.version, 2);
    }

    #[test]
    fn full_replacement_rebuilds_all_representations() {
        let analyzer = Analyzer::embedded();
        let store = DocumentStore::new();
        let uri = uri("full.ini");
        store.open(&analyzer, uri.clone(), "Object Old\nEnd\n".into(), 1);
        let strategy = store.change(
            &analyzer,
            &uri,
            vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "Object New\nEnd\n".into(),
            }],
            2,
            PositionEnc::Utf16,
        );
        assert_eq!(strategy, Some(ChangeStrategy::Full));
        let snapshot = store.snapshot(&uri).unwrap();
        assert_eq!(snapshot.rope.to_string(), &*snapshot.text);
        assert_eq!(snapshot.parse.syntax().text().to_string(), &*snapshot.text);
    }

    #[test]
    fn many_ranged_changes_use_bulk_fallback() {
        let analyzer = Analyzer::embedded();
        let store = DocumentStore::new();
        let uri = uri("bulk.ini");
        store.open(&analyzer, uri.clone(), "123456789\n".into(), 1);
        let changes = (0..9)
            .map(|column| ranged(Position::new(0, column), Position::new(0, column + 1), "x"))
            .collect();
        assert_eq!(
            store.change(&analyzer, &uri, changes, 2, PositionEnc::Utf16),
            Some(ChangeStrategy::Bulk)
        );
        let snapshot = store.snapshot(&uri).unwrap();
        assert_eq!(snapshot.rope.to_string(), &*snapshot.text);
        assert_eq!(snapshot.parse.syntax().text().to_string(), &*snapshot.text);
    }

    #[test]
    fn crlf_changes_honor_utf8_and_utf16_positions() {
        for (encoding, character) in [(PositionEnc::Utf8, 4), (PositionEnc::Utf16, 2)] {
            let analyzer = Analyzer::embedded();
            let store = DocumentStore::new();
            let uri = uri(if encoding == PositionEnc::Utf8 {
                "utf8.ini"
            } else {
                "utf16.ini"
            });
            store.open(&analyzer, uri.clone(), "😀x\r\nEnd\r\n".into(), 1);
            store.change(
                &analyzer,
                &uri,
                vec![ranged(
                    Position::new(0, character),
                    Position::new(0, character + 1),
                    "y",
                )],
                2,
                encoding,
            );
            assert_eq!(&*store.snapshot(&uri).unwrap().text, "😀y\r\nEnd\r\n");
        }
    }

    #[test]
    fn rejects_stale_diagnostic_cache_restoration() {
        let analyzer = Analyzer::embedded();
        let store = DocumentStore::new();
        let uri = uri("diagnostics.ini");
        store.open(&analyzer, uri.clone(), "Object Old\nEnd\n".into(), 1);
        let checked_out = store.checkout_diagnostics(&uri).unwrap();
        store.change(
            &analyzer,
            &uri,
            vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "Object New\nEnd\n".into(),
            }],
            2,
            PositionEnc::Utf16,
        );
        assert!(!store.restore_diagnostics(&uri, checked_out.document.version, checked_out.cache));
    }

    #[test]
    fn semantic_history_is_replaced() {
        let analyzer = Analyzer::embedded();
        let store = DocumentStore::new();
        let uri = uri("semantic.ini");
        store.open(&analyzer, uri.clone(), String::new(), 1);
        assert!(store
            .replace_semantic_history(&uri, (1, Vec::new()))
            .is_none());
        assert_eq!(
            store.replace_semantic_history(&uri, (2, Vec::new())),
            Some((1, Vec::new()))
        );
    }

    #[test]
    fn source_precedence_is_open_then_virtual_then_disk() {
        let analyzer = Analyzer::embedded();
        let store = DocumentStore::new();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("zerosyntax-document-{stamp}.ini"));
        fs::write(&path, "disk").unwrap();
        let disk_uri = Url::from_file_path(&path).unwrap();
        assert_eq!(
            store.source_text(disk_uri.as_str()).as_deref(),
            Some("disk")
        );
        store.open(&analyzer, disk_uri.clone(), "open".into(), 1);
        assert_eq!(
            store.source_text(disk_uri.as_str()).as_deref(),
            Some("open")
        );

        let big_uri = Url::parse("big:///archive.big!/entry.ini").unwrap();
        store.replace_virtual_files([(big_uri.to_string(), Arc::<str>::from("virtual"))]);
        assert_eq!(
            store.source_text(big_uri.as_str()).as_deref(),
            Some("virtual")
        );
        store.open(&analyzer, big_uri.clone(), "open-big".into(), 1);
        assert_eq!(
            store.source_text(big_uri.as_str()).as_deref(),
            Some("open-big")
        );
        fs::remove_file(path).unwrap();
    }
}
