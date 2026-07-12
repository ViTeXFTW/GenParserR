//! Asset ingestion for workspace and configured game roots.
//!
//! Owns discovery, INI/W3D/BIG parsing, persistent cache compatibility, and
//! bounded parallel scanning. The LSP backend supplies roots and progress,
//! then applies the ordered result to its live workspace index.

use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::Url;
use zerosyntax_analysis::index::{
    definitions_in, module_tags_in, references_in, Definition, ModelAsset, ReferenceSite,
};
use zerosyntax_analysis::Analyzer;

const INDEX_CACHE_VERSION: u32 = 1;
const MAX_SCAN_WORKERS: usize = 4;
const MAX_W3D_CHUNK_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanEntry {
    pub(crate) file: String,
    pub(crate) definitions: Vec<Definition>,
    pub(crate) references: Vec<ReferenceSite>,
    pub(crate) tags: Vec<(String, String)>,
    pub(crate) models: Vec<ModelAsset>,
    pub(crate) text: Option<Arc<str>>,
    pub(crate) is_base: bool,
}

#[derive(Debug, Default)]
pub(crate) struct ScanStats {
    pub(crate) discovery: Duration,
    pub(crate) cache_load: Duration,
    pub(crate) parse: Duration,
    pub(crate) cache_write: Duration,
    pub(crate) hits: usize,
    pub(crate) misses: usize,
}

#[derive(Debug, Default)]
pub(crate) struct ScanResult {
    pub(crate) entries: Vec<ScanEntry>,
    pub(crate) stats: ScanStats,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CachedScanEntry {
    file: String,
    definitions: Vec<Definition>,
    references: Vec<ReferenceSite>,
    tags: Vec<(String, String)>,
    models: Vec<ModelAsset>,
    text: Option<String>,
}

impl CachedScanEntry {
    fn from_scan(entry: &ScanEntry) -> Self {
        Self {
            file: entry.file.clone(),
            definitions: entry.definitions.clone(),
            references: entry.references.clone(),
            tags: entry.tags.clone(),
            models: entry.models.clone(),
            text: entry.text.as_deref().map(str::to_string),
        }
    }

    fn into_scan(self, is_base: bool) -> ScanEntry {
        ScanEntry {
            file: self.file,
            definitions: self.definitions,
            references: self.references,
            tags: self.tags,
            models: self.models,
            text: self.text.map(Arc::from),
            is_base,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileFingerprint {
    len: u64,
    modified_secs: u64,
    modified_nanos: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedFile {
    fingerprint: FileFingerprint,
    entries: Vec<CachedScanEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexCache {
    version: u32,
    schema_hash: u64,
    files: HashMap<String, CachedFile>,
}

impl IndexCache {
    fn empty(schema_hash: u64) -> Self {
        Self {
            version: INDEX_CACHE_VERSION,
            schema_hash,
            files: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct ScanPath {
    path: PathBuf,
    key: String,
    is_base: bool,
    fingerprint: FileFingerprint,
}

struct BigEntry {
    name: String,
    offset: u64,
    size: usize,
}

fn read_lossy(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

pub(crate) fn load_sibling_str_keys(ini_url: &Url) -> Vec<String> {
    let path = match ini_url.to_file_path() {
        Ok(path) => path,
        Err(_) => return Vec::new(),
    };
    if let Some(text) = read_lossy(&path.with_extension("str")) {
        return parse_str_keys(&text);
    }
    read_lossy(&path.with_extension("STR"))
        .map(|text| parse_str_keys(&text))
        .unwrap_or_default()
}

fn parse_str_keys(content: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut expect_key = true;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.eq_ignore_ascii_case("END") {
            expect_key = true;
        } else if expect_key {
            keys.push(trimmed.to_string());
            expect_key = false;
        }
    }
    keys
}

fn schema_hash() -> u64 {
    let mut hasher = DefaultHasher::new();
    zerosyntax_schema::EMBEDDED_SCHEMA_JSON.hash(&mut hasher);
    hasher.finish()
}

fn path_key(path: &Path) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let key = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        key.to_ascii_lowercase()
    } else {
        key
    }
}

fn file_fingerprint(path: &Path) -> Option<FileFingerprint> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .unwrap_or_default();
    Some(FileFingerprint {
        len: metadata.len(),
        modified_secs: modified.as_secs(),
        modified_nanos: modified.subsec_nanos(),
    })
}

fn cache_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(path).join("zerosyntax");
    }
    #[cfg(not(windows))]
    {
        if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
            return PathBuf::from(path).join("zerosyntax");
        }
        if let Some(path) = std::env::var_os("HOME") {
            return PathBuf::from(path).join(".cache/zerosyntax");
        }
    }
    std::env::temp_dir().join("zerosyntax")
}

fn cache_path(workspace_roots: &[PathBuf], base_roots: &[PathBuf]) -> PathBuf {
    let mut roots: Vec<_> = workspace_roots
        .iter()
        .map(|root| format!("workspace:{}", path_key(root)))
        .chain(
            base_roots
                .iter()
                .map(|root| format!("base:{}", path_key(root))),
        )
        .collect();
    roots.sort_unstable();
    let mut hasher = DefaultHasher::new();
    roots.hash(&mut hasher);
    cache_dir().join(format!(
        "index-v{INDEX_CACHE_VERSION}-{:016x}.json",
        hasher.finish()
    ))
}

fn load_index_cache(path: &Path, expected_schema_hash: u64) -> IndexCache {
    let Some(bytes) = std::fs::read(path).ok() else {
        return IndexCache::empty(expected_schema_hash);
    };
    let Ok(cache) = serde_json::from_slice::<IndexCache>(&bytes) else {
        return IndexCache::empty(expected_schema_hash);
    };
    if cache.version != INDEX_CACHE_VERSION || cache.schema_hash != expected_schema_hash {
        return IndexCache::empty(expected_schema_hash);
    }
    cache
}

fn write_index_cache(path: &Path, cache: &IndexCache) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec(cache).map_err(std::io::Error::other)?;
    std::fs::write(path, bytes)
}

pub(crate) fn clear_cache(
    workspace_roots: &[PathBuf],
    base_roots: &[PathBuf],
) -> std::io::Result<bool> {
    match std::fs::remove_file(cache_path(workspace_roots, base_roots)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn read_u32_be<R: Read>(reader: &mut R) -> std::io::Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_be_bytes(buf))
}

fn read_c_string<R: Read>(reader: &mut R) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        reader.read_exact(&mut byte)?;
        if byte[0] == 0 {
            break;
        }
        buf.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn big_entries(file: &mut std::fs::File) -> std::io::Result<Vec<BigEntry>> {
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)?;
    if &magic != b"BIGF" {
        return Ok(Vec::new());
    }
    let _archive_size = read_u32_be(&mut *file)?;
    let count = read_u32_be(&mut *file)?;
    file.seek(SeekFrom::Start(0x10))?;
    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        entries.push(BigEntry {
            offset: read_u32_be(&mut *file)? as u64,
            size: read_u32_be(&mut *file)? as usize,
            name: read_c_string(&mut *file)?.replace('\\', "/"),
        });
    }
    Ok(entries)
}

fn read_big_entry_bytes(file: &mut std::fs::File, entry: &BigEntry) -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(entry.offset)).ok()?;
    let mut bytes = vec![0; entry.size];
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

fn big_uri(path: &Path, entry: &str) -> String {
    let archive = path.to_string_lossy().replace('\\', "/");
    let mut uri = Url::parse("big:///").expect("static big URI is valid");
    uri.set_path(&format!("{archive}!/{entry}"));
    uri.to_string()
}

fn file_stem_str(path: &str) -> String {
    let file_name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    file_name
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(file_name)
        .to_string()
}

fn parse_w3d_models(bytes: &[u8], fallback_name: &str) -> Vec<ModelAsset> {
    let mut names = Vec::new();
    let mut members = Vec::new();
    if !fallback_name.is_empty() {
        names.push(fallback_name.to_string());
    }
    walk_w3d_chunks(bytes, 0, bytes.len(), 0, &mut |kind, payload| match kind {
        0x0000_001F if payload.len() >= 40 => {
            push_name(&mut members, read_fixed_name(&payload[8..24]));
            push_name(&mut names, read_fixed_name(&payload[24..40]));
        }
        0x0000_0101 | 0x0000_0501 | 0x0000_0601 if payload.len() >= 20 => {
            push_name(&mut names, read_fixed_name(&payload[4..20]));
        }
        0x0000_0102 => {
            for pivot in payload.chunks_exact(60) {
                push_name(&mut members, read_fixed_name(&pivot[..16]));
            }
        }
        0x0000_0701 if payload.len() >= 40 => {
            push_name(&mut names, read_fixed_name(&payload[8..24]));
            push_name(&mut names, read_fixed_name(&payload[24..40]));
        }
        0x0000_0704 if payload.len() >= 36 => {
            push_name(&mut members, read_fixed_name(&payload[4..36]));
        }
        0x0000_0740 if payload.len() >= 40 => {
            push_name(&mut members, read_fixed_name(&payload[8..40]));
        }
        0x0000_0750 if payload.len() >= 48 => {
            push_name(&mut members, read_fixed_name(&payload[16..48]))
        }
        _ => {}
    });
    dedup_case_insensitive(&mut names);
    dedup_case_insensitive(&mut members);
    names
        .into_iter()
        .filter(|name| !name.is_empty())
        .map(|name| ModelAsset {
            name,
            members: members.clone(),
        })
        .collect()
}

fn walk_w3d_chunks(
    bytes: &[u8],
    mut pos: usize,
    end: usize,
    depth: usize,
    f: &mut impl FnMut(u32, &[u8]),
) {
    if depth > MAX_W3D_CHUNK_DEPTH {
        return;
    }
    while pos + 8 <= end && pos + 8 <= bytes.len() {
        let kind = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
        let size_raw = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
        let has_children = (size_raw & 0x8000_0000) != 0 || is_w3d_container(kind);
        let size = (size_raw & 0x7fff_ffff) as usize;
        let payload_start = pos + 8;
        let Some(payload_end) = payload_start.checked_add(size) else {
            break;
        };
        if payload_end > end || payload_end > bytes.len() {
            break;
        }
        let payload = &bytes[payload_start..payload_end];
        f(kind, payload);
        if has_children {
            walk_w3d_chunks(bytes, payload_start, payload_end, depth + 1, f);
        }
        pos = payload_end;
    }
}

fn is_w3d_container(kind: u32) -> bool {
    matches!(
        kind,
        0x0000_0000
            | 0x0000_0100
            | 0x0000_0500
            | 0x0000_0600
            | 0x0000_0700
            | 0x0000_0702
            | 0x0000_0705
    )
}

fn read_fixed_name(bytes: &[u8]) -> &str {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("").trim()
}

fn push_name(out: &mut Vec<String>, name: &str) {
    if name.is_empty() {
        return;
    }
    out.push(name.to_string());
    if let Some((_, short)) = name.rsplit_once('.') {
        if !short.is_empty() {
            out.push(short.to_string());
        }
    }
}

fn dedup_case_insensitive(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.to_ascii_lowercase()));
}

fn scan_big(analyzer: &Analyzer, path: &Path, is_base: bool) -> Vec<ScanEntry> {
    let mut out = Vec::new();
    let Ok(mut file) = std::fs::File::open(path) else {
        return out;
    };
    let Ok(entries) = big_entries(&mut file) else {
        return out;
    };
    for entry in entries {
        let uri = big_uri(path, &entry.name);
        if entry.name.ends_with(".ini") || entry.name.ends_with(".INI") {
            let Some(bytes) = read_big_entry_bytes(&mut file, &entry) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let parse = analyzer.parse(&text);
            out.push(ScanEntry {
                definitions: definitions_in(analyzer, &parse, &uri),
                references: references_in(analyzer, &parse),
                tags: module_tags_in(analyzer, &parse),
                file: uri,
                models: Vec::new(),
                text: Some(Arc::from(text)),
                is_base,
            });
        } else if entry.name.ends_with(".w3d") || entry.name.ends_with(".W3D") {
            let Some(bytes) = read_big_entry_bytes(&mut file, &entry) else {
                continue;
            };
            let models = parse_w3d_models(&bytes, &file_stem_str(&entry.name));
            if !models.is_empty() {
                out.push(ScanEntry {
                    file: uri,
                    definitions: Vec::new(),
                    references: Vec::new(),
                    tags: Vec::new(),
                    models,
                    text: None,
                    is_base,
                });
            }
        }
    }
    out
}

fn collect_scan_paths(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        if root
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("big"))
        {
            out.push(root.clone());
            continue;
        }
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            if extension.eq_ignore_ascii_case("big")
                || extension.eq_ignore_ascii_case("ini")
                || extension.eq_ignore_ascii_case("w3d")
            {
                out.push(path.to_path_buf());
            }
        }
    }
    out
}

fn collect_scan_plan(workspace_roots: &[PathBuf], base_roots: &[PathBuf]) -> Vec<ScanPath> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (roots, is_base) in [(base_roots, true), (workspace_roots, false)] {
        for path in collect_scan_paths(roots) {
            let key = path_key(&path);
            let Some(fingerprint) = file_fingerprint(&path) else {
                continue;
            };
            if seen.insert(key.clone()) {
                out.push(ScanPath {
                    path,
                    key,
                    is_base,
                    fingerprint,
                });
            }
        }
    }
    out
}

fn scan_path(analyzer: &Analyzer, path: &Path, is_base: bool) -> Vec<ScanEntry> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if extension.eq_ignore_ascii_case("big") {
        return scan_big(analyzer, path, is_base);
    }
    if extension.eq_ignore_ascii_case("ini") {
        if let (Some(text), Ok(uri)) = (read_lossy(path), Url::from_file_path(path)) {
            let parse = analyzer.parse(&text);
            return vec![ScanEntry {
                definitions: definitions_in(analyzer, &parse, uri.as_str()),
                references: references_in(analyzer, &parse),
                tags: module_tags_in(analyzer, &parse),
                file: uri.to_string(),
                models: Vec::new(),
                text: None,
                is_base,
            }];
        }
    } else if extension.eq_ignore_ascii_case("w3d") {
        if let (Ok(bytes), Ok(uri)) = (std::fs::read(path), Url::from_file_path(path)) {
            let stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            let models = parse_w3d_models(&bytes, stem);
            if !models.is_empty() {
                return vec![ScanEntry {
                    file: uri.to_string(),
                    definitions: Vec::new(),
                    references: Vec::new(),
                    tags: Vec::new(),
                    models,
                    text: None,
                    is_base,
                }];
            }
        }
    }
    Vec::new()
}

pub(crate) fn scan(
    analyzer: &Analyzer,
    workspace_roots: &[PathBuf],
    base_roots: &[PathBuf],
    progress: &mut impl FnMut(usize, usize),
) -> ScanResult {
    let discovery_started = Instant::now();
    let paths = collect_scan_plan(workspace_roots, base_roots);
    let discovery = discovery_started.elapsed();
    let workers = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(MAX_SCAN_WORKERS);
    let (entries, mut stats) = scan_with_cache(
        analyzer,
        &paths,
        &cache_path(workspace_roots, base_roots),
        workers,
        progress,
    );
    stats.discovery = discovery;
    ScanResult { entries, stats }
}

fn scan_with_cache(
    analyzer: &Analyzer,
    paths: &[ScanPath],
    cache_path: &Path,
    workers: usize,
    progress: &mut impl FnMut(usize, usize),
) -> (Vec<ScanEntry>, ScanStats) {
    let expected_schema_hash = schema_hash();
    let load_started = Instant::now();
    let mut old_cache = load_index_cache(cache_path, expected_schema_hash);
    let cache_load = load_started.elapsed();
    let old_file_count = old_cache.files.len();
    let mut results: Vec<Option<Vec<ScanEntry>>> = (0..paths.len()).map(|_| None).collect();
    let mut misses = Vec::new();
    let mut hits = 0;
    let mut done = 0;

    for (index, path) in paths.iter().enumerate() {
        match old_cache.files.remove(&path.key) {
            Some(cached) if cached.fingerprint == path.fingerprint => {
                results[index] = Some(
                    cached
                        .entries
                        .into_iter()
                        .map(|entry| entry.into_scan(path.is_base))
                        .collect(),
                );
                hits += 1;
                done += 1;
                progress(done, paths.len());
            }
            _ => misses.push(index),
        }
    }

    let parse_started = Instant::now();
    if !misses.is_empty() {
        let worker_count = workers.max(1).min(misses.len());
        let next = AtomicUsize::new(0);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                let tx = tx.clone();
                let misses = &misses;
                let next = &next;
                scope.spawn(move || loop {
                    let work = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&index) = misses.get(work) else {
                        break;
                    };
                    let path = &paths[index];
                    if tx
                        .send((index, scan_path(analyzer, &path.path, path.is_base)))
                        .is_err()
                    {
                        break;
                    }
                });
            }
            drop(tx);
            for (index, entries) in rx {
                results[index] = Some(entries);
                done += 1;
                progress(done, paths.len());
            }
        });
    }
    let parse = parse_started.elapsed();

    let mut cache = IndexCache::empty(expected_schema_hash);
    let mut scanned = Vec::new();
    for (path, entries) in paths.iter().zip(results) {
        let entries = entries.unwrap_or_default();
        cache.files.insert(
            path.key.clone(),
            CachedFile {
                fingerprint: path.fingerprint.clone(),
                entries: entries.iter().map(CachedScanEntry::from_scan).collect(),
            },
        );
        scanned.extend(entries);
    }

    let write_started = Instant::now();
    if (!misses.is_empty() || old_file_count != paths.len())
        && write_index_cache(cache_path, &cache).is_err()
    {
        tracing::warn!(path = %cache_path.display(), "could not write asset index cache");
    }
    let cache_write = write_started.elapsed();
    (
        scanned,
        ScanStats {
            cache_load,
            parse,
            cache_write,
            hits,
            misses: misses.len(),
            ..ScanStats::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerosyntax_analysis::{completion, WorkspaceIndex};

    fn test_dir(label: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "zerosyntax-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn w3d_with_member(member: &str) -> Vec<u8> {
        let mut pivot = vec![0; 60];
        let name = member.as_bytes();
        pivot[..name.len().min(16)].copy_from_slice(&name[..name.len().min(16)]);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x0000_0102u32.to_le_bytes());
        bytes.extend_from_slice(&(pivot.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&pivot);
        bytes
    }

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

    fn write_big(path: &Path, entries: &[(&str, Vec<u8>)]) {
        let directory_len: usize = entries.iter().map(|(name, _)| 8 + name.len() + 1).sum();
        let mut offset = 0x10 + directory_len;
        let archive_size = offset + entries.iter().map(|(_, bytes)| bytes.len()).sum::<usize>();
        let mut out = Vec::with_capacity(archive_size);
        out.extend_from_slice(b"BIGF");
        out.extend_from_slice(&(archive_size as u32).to_be_bytes());
        out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        for (name, bytes) in entries {
            out.extend_from_slice(&(offset as u32).to_be_bytes());
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(name.as_bytes());
            out.push(0);
            offset += bytes.len();
        }
        for (_, bytes) in entries {
            out.extend_from_slice(bytes);
        }
        std::fs::write(path, out).unwrap();
    }

    fn cached_scan(
        analyzer: &Analyzer,
        workspace_roots: &[PathBuf],
        base_roots: &[PathBuf],
        cache: &Path,
        workers: usize,
    ) -> (Vec<ScanEntry>, ScanStats) {
        let paths = collect_scan_plan(workspace_roots, base_roots);
        scan_with_cache(analyzer, &paths, cache, workers, &mut |_, _| {})
    }

    #[test]
    fn removes_only_the_requested_index_cache() {
        let dir = test_dir("clear-cache");
        std::fs::write(dir.join("Object.ini"), "Object CachedTank\nEnd\n").unwrap();
        let roots = std::slice::from_ref(&dir);
        let _ = scan(&Analyzer::embedded(), &[], roots, &mut |_, _| {});
        assert!(clear_cache(&[], roots).unwrap());
        assert!(!clear_cache(&[], roots).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_reuses_complete_scan_and_invalidates_changed_loose_file() {
        let dir = test_dir("cache-loose");
        let cache = dir.join("cache.json");
        std::fs::write(dir.join("Object.ini"), "Object CachedTank\nEnd\n").unwrap();
        let model = dir.join("Tank.w3d");
        std::fs::write(&model, w3d_with_member("Muzzle01")).unwrap();
        let analyzer = Analyzer::embedded();
        let roots = std::slice::from_ref(&dir);
        let (cold, cold_stats) = cached_scan(&analyzer, &[], roots, &cache, 1);
        assert_eq!((cold_stats.hits, cold_stats.misses), (0, 2));
        let (warm, warm_stats) = cached_scan(&analyzer, &[], roots, &cache, 4);
        assert_eq!((warm_stats.hits, warm_stats.misses), (2, 0));
        assert_eq!(cold, warm);
        let mut changed = w3d_with_member("Muzzle02");
        changed.push(0);
        std::fs::write(&model, changed).unwrap();
        let (_, changed_stats) = cached_scan(&analyzer, &[], roots, &cache, 4);
        assert_eq!((changed_stats.hits, changed_stats.misses), (1, 1));

        let mut incompatible = load_index_cache(&cache, schema_hash());
        incompatible.schema_hash ^= 1;
        write_index_cache(&cache, &incompatible).unwrap();
        let (_, schema_stats) = cached_scan(&analyzer, &[], roots, &cache, 1);
        assert_eq!((schema_stats.hits, schema_stats.misses), (0, 2));

        std::fs::write(&cache, b"not json").unwrap();
        let (_, corrupt_stats) = cached_scan(&analyzer, &[], roots, &cache, 1);
        assert_eq!((corrupt_stats.hits, corrupt_stats.misses), (0, 2));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_invalidates_changed_big_archive_as_one_file() {
        let dir = test_dir("cache-big");
        let archive = dir.join("Models.big");
        let cache = dir.join("cache.json");
        write_big(
            &archive,
            &[
                ("Data/INI/Object.ini", b"Object BigTank\nEnd\n".to_vec()),
                ("Art/Tank.w3d", w3d_with_member("Muzzle01")),
            ],
        );
        let analyzer = Analyzer::embedded();
        let roots = std::slice::from_ref(&archive);
        let (cold, cold_stats) = cached_scan(&analyzer, &[], roots, &cache, 1);
        assert_eq!((cold_stats.hits, cold_stats.misses), (0, 1));
        assert_eq!(cold.len(), 2);
        let (warm, warm_stats) = cached_scan(&analyzer, &[], roots, &cache, 4);
        assert_eq!((warm_stats.hits, warm_stats.misses), (1, 0));
        assert_eq!(cold, warm);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parallel_scan_preserves_order_and_overlapping_roots_are_deduped() {
        let dir = test_dir("parallel");
        for index in 0..32 {
            std::fs::write(
                dir.join(format!("Model{index:02}.w3d")),
                w3d_with_member(&format!("Bone{index:02}")),
            )
            .unwrap();
        }
        let roots = std::slice::from_ref(&dir);
        let plan = collect_scan_plan(roots, roots);
        assert_eq!(plan.len(), 32);
        assert!(plan.iter().all(|path| path.is_base));
        let analyzer = Analyzer::embedded();
        let (serial, _) = cached_scan(&analyzer, &[], roots, &dir.join("serial.json"), 1);
        let (parallel, _) = cached_scan(&analyzer, &[], roots, &dir.join("parallel.json"), 4);
        assert_eq!(serial, parallel);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    #[ignore = "3,000-file synthetic timing harness; run with --ignored --nocapture"]
    fn synthetic_3000_asset_cold_and_warm_timings() {
        let dir = test_dir("benchmark");
        for index in 0..3000 {
            std::fs::write(
                dir.join(format!("Model{index:04}.w3d")),
                w3d_with_member(&format!("Bone{:02}", index % 100)),
            )
            .unwrap();
        }
        let analyzer = Analyzer::embedded();
        let roots = std::slice::from_ref(&dir);
        let mut serial_times = Vec::new();
        let mut parallel_times = Vec::new();
        let mut expected = None;
        for run in 0..3 {
            let started = Instant::now();
            let (out, _) = cached_scan(
                &analyzer,
                &[],
                roots,
                &dir.join(format!("serial-{run}.json")),
                1,
            );
            serial_times.push(started.elapsed());
            expected.get_or_insert(out);
            let started = Instant::now();
            let (out, _) = cached_scan(
                &analyzer,
                &[],
                roots,
                &dir.join(format!("parallel-{run}.json")),
                4,
            );
            parallel_times.push(started.elapsed());
            assert_eq!(expected.as_ref(), Some(&out));
        }
        serial_times.sort_unstable();
        parallel_times.sort_unstable();
        let warm_started = Instant::now();
        let (_, warm_stats) = cached_scan(&analyzer, &[], roots, &dir.join("parallel-2.json"), 4);
        assert_eq!((warm_stats.hits, warm_stats.misses), (3000, 0));
        eprintln!(
            "3,000 W3Ds: serial median {:?}, parallel median {:?}, warm {:?}",
            serial_times[1],
            parallel_times[1],
            warm_started.elapsed()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scans_ini_from_big_archive() {
        let dir = test_dir("big");
        let path = dir.join("test.big");
        write_big(
            &path,
            &[(
                "Data/INI/Test.ini",
                b"Object BigArchiveObject\nEnd\n".to_vec(),
            )],
        );
        let scanned = scan_big(&Analyzer::embedded(), &path, true);
        assert_eq!(scanned.len(), 1);
        assert!(scanned[0]
            .definitions
            .iter()
            .any(|definition| definition.name == "BigArchiveObject"));
        assert_eq!(
            scanned[0].text.as_deref(),
            Some("Object BigArchiveObject\nEnd\n")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn w3d_root_scan_powers_model_and_bone_completions() {
        let dir = test_dir("w3d");
        std::fs::write(dir.join("Good.w3d"), w3d_with_member("Tire01")).unwrap();
        let analyzer = Analyzer::embedded();
        let result = scan(&analyzer, &[], std::slice::from_ref(&dir), &mut |_, _| {});
        let mut index = WorkspaceIndex::new();
        for entry in result.entries {
            index.set_file(&entry.file, entry.definitions);
            index.set_file_refs(&entry.file, entry.references);
            index.set_file_tags(&entry.file, entry.tags);
            index.set_file_models(&entry.file, entry.models);
        }
        let source = "Object Tank\n  Draw = W3DTankDraw ModuleTag_01\n    DefaultConditionState\n      Model = Good\n      HideSubObject = \n    End\n  End\nEnd\n";
        let parse = analyzer.parse(source);
        let offset = source.find("HideSubObject = ").unwrap() + "HideSubObject = ".len();
        let labels: Vec<_> =
            completion::complete(&analyzer, &parse, offset as u32, Some(&index), None)
                .into_iter()
                .map(|completion| completion.label)
                .collect();
        assert!(labels.contains(&"Tire".to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_w3d_model_names_and_members() {
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
        let good = models.iter().find(|model| model.name == "Good").unwrap();
        assert!(good.members.iter().any(|member| member == "Tire01"));
        assert!(good.members.iter().any(|member| member == "Cargo01"));
    }

    #[test]
    fn parses_w3d_aggregate_and_emitter_names() {
        let mut header = Vec::new();
        header.extend_from_slice(&1u32.to_le_bytes());
        header.extend_from_slice(&fixed::<16>("Aggro"));
        let mut bytes = chunk(0x0000_0600, chunk(0x0000_0601, header));
        let mut header = Vec::new();
        header.extend_from_slice(&1u32.to_le_bytes());
        header.extend_from_slice(&fixed::<16>("Smoke"));
        bytes.extend(chunk(0x0000_0500, chunk(0x0000_0501, header)));
        let models = parse_w3d_models(&bytes, "");
        assert!(models.iter().any(|model| model.name == "Aggro"));
        assert!(models.iter().any(|model| model.name == "Smoke"));
    }

    #[test]
    fn w3d_chunk_walker_survives_hostile_deep_nesting() {
        let total = 64 * 1024;
        let mut bytes = Vec::with_capacity(total);
        let mut remaining = total;
        while remaining >= 8 {
            bytes.extend_from_slice(&0x0000_0700u32.to_le_bytes());
            bytes.extend_from_slice(&((remaining - 8) as u32).to_le_bytes());
            remaining -= 8;
        }
        let _ = parse_w3d_models(&bytes, "Fallback");
    }
}
