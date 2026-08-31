//! Shared filesystem, BIG archive, and W3D workspace scanning.

use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::Url;
use zerosyntax_analysis::index::{
    definitions_in, module_tags_in, object_models_in, object_parents_in, references_in, AssetKind,
    Definition, FileAsset, ModelAsset, ModuleTagDefinition, ReferenceSite,
};
use zerosyntax_analysis::Analyzer;
use zerosyntax_w3d::W3dFile;

pub(crate) type ScanEntry = (
    String,
    Vec<Definition>,
    Vec<ReferenceSite>,
    Vec<ModuleTagDefinition>,
    Vec<(String, Vec<String>)>,
    Vec<(String, String)>,
    Vec<ModelAsset>,
    Vec<FileAsset>,
    Option<Arc<str>>,
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanProgress {
    Discovering,
    InputsDiscovered {
        total: usize,
        skipped: usize,
    },
    Indexing {
        done: usize,
        total: usize,
        cache_hits: usize,
        cache_misses: usize,
    },
    WritingCache,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScanStats {
    pub(crate) discovered_inputs: usize,
    pub(crate) discovery_failures: usize,
    pub(crate) cache_hits: usize,
    pub(crate) cache_misses: usize,
    pub(crate) fingerprint_failures: usize,
    pub(crate) scan_failures: usize,
    /// A usable persistent cache exists after the scan. This is also true
    /// when an unchanged cache did not need to be written again.
    pub(crate) cache_written: bool,
    /// The persistent cache was created or replaced during this scan.
    pub(crate) cache_updated: bool,
}

impl ScanStats {
    pub(crate) fn skipped_inputs(self) -> usize {
        self.discovery_failures + self.fingerprint_failures + self.scan_failures
    }
}

pub(crate) struct ScanOutcome {
    pub(crate) entries: Vec<(bool, ScanEntry)>,
    pub(crate) stats: ScanStats,
}

const INDEX_CACHE_VERSION: u32 = 5;
/// How many current-version caches `prune_index_caches` keeps, newest first.
const INDEX_CACHE_RETAINED: usize = 4;
/// How long a cache may sit unused before `prune_index_caches` drops it.
const INDEX_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const MAX_PREVIEW_ASSET_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Fingerprint {
    len: u64,
    modified_secs: u64,
    modified_nanos: u32,
}

#[derive(Serialize, Deserialize)]
struct CachedEntry {
    file: String,
    definitions: Vec<Definition>,
    references: Vec<ReferenceSite>,
    tags: Vec<ModuleTagDefinition>,
    object_models: Vec<(String, Vec<String>)>,
    object_parents: Vec<(String, String)>,
    models: Vec<ModelAsset>,
    assets: Vec<FileAsset>,
    text: Option<String>,
}

impl From<&ScanEntry> for CachedEntry {
    fn from(entry: &ScanEntry) -> Self {
        Self {
            file: entry.0.clone(),
            definitions: entry.1.clone(),
            references: entry.2.clone(),
            tags: entry.3.clone(),
            object_models: entry.4.clone(),
            object_parents: entry.5.clone(),
            models: entry.6.clone(),
            assets: entry.7.clone(),
            text: entry.8.as_deref().map(str::to_owned),
        }
    }
}

impl From<CachedEntry> for ScanEntry {
    fn from(entry: CachedEntry) -> Self {
        (
            entry.file,
            entry.definitions,
            entry.references,
            entry.tags,
            entry.object_models,
            entry.object_parents,
            entry.models,
            entry.assets,
            entry.text.map(Arc::from),
        )
    }
}

#[derive(Serialize, Deserialize)]
struct CachedFile {
    fingerprint: Fingerprint,
    entries: Vec<CachedEntry>,
}

#[derive(Serialize, Deserialize)]
struct IndexCache {
    version: u32,
    schema_hash: u64,
    files: HashMap<String, CachedFile>,
}

fn cache_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(path).join("zerosyntax");
    }
    #[cfg(not(windows))]
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(path).join("zerosyntax");
    }
    std::env::temp_dir().join("zerosyntax")
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

fn schema_hash() -> u64 {
    let mut hasher = DefaultHasher::new();
    zerosyntax_schema::EMBEDDED_SCHEMA_JSON.hash(&mut hasher);
    hasher.finish()
}

fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(Fingerprint {
        len: metadata.len(),
        modified_secs: modified.as_secs(),
        modified_nanos: modified.subsec_nanos(),
    })
}

/// Refresh the retention timestamp without rewriting a valid cache.
fn refresh_index_cache_last_used(path: &Path) -> bool {
    match std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::now()))
    {
        Ok(()) => true,
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "index cache last-used time could not be updated; falling back to cache rewrite");
            false
        }
    }
}

pub(crate) fn index_cache_path(workspace_roots: &[PathBuf], base_roots: &[PathBuf]) -> PathBuf {
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

/// The cache version encoded in an `index-v<version>-<hash>.json` file name,
/// or `None` for anything the server did not write as an index cache.
fn cache_file_version(name: &str) -> Option<u32> {
    let (version, hash) = name
        .strip_prefix("index-v")?
        .strip_suffix(".json")?
        .split_once('-')?;
    (hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| version.parse().ok())
        .flatten()
}

/// Delete index caches the server can no longer use — earlier cache
/// versions, and current-version caches unused for a while — down to
/// `INDEX_CACHE_RETAINED` files. `keep`, when given, is exempt and reserves a
/// retention slot; pass `None` if the caller didn't just write it
/// successfully, so a failed or partial write can't shield a stale file at
/// the expense of evicting a newer one. Unrecognized files are never
/// touched, and a failed delete only costs disk space.
fn prune_index_caches_in(dir: &Path, keep: Option<&Path>) -> usize {
    let Ok(dir) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut pruned = 0;
    let mut remove = |path: &Path| match std::fs::remove_file(path) {
        Ok(()) => pruned += 1,
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "stale index cache could not be removed")
        }
    };
    let mut current = Vec::new();
    for entry in dir.flatten() {
        let path = entry.path();
        let Some(version) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(cache_file_version)
        else {
            continue;
        };
        if Some(path.as_path()) == keep {
            continue;
        }
        let modified = entry.metadata().and_then(|data| data.modified()).ok();
        let expired = modified
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > INDEX_CACHE_MAX_AGE);
        if version != INDEX_CACHE_VERSION || expired {
            remove(&path);
        } else {
            current.push((modified, path));
        }
    }
    current.sort_unstable_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    let retained = INDEX_CACHE_RETAINED.saturating_sub(usize::from(keep.is_some()));
    for (_, path) in current.into_iter().skip(retained) {
        remove(&path);
    }
    pruned
}

fn prune_index_caches(keep: Option<&Path>) -> usize {
    prune_index_caches_in(&cache_dir(), keep)
}

pub(crate) fn clear_index_cache(
    workspace_roots: &[PathBuf],
    base_roots: &[PathBuf],
) -> std::io::Result<bool> {
    let path = index_cache_path(workspace_roots, base_roots);
    let cleared = match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    prune_index_caches(None);
    Ok(cleared)
}

struct BigEntry {
    name: String,
    offset: u64,
    size: usize,
}

/// Read a file leniently: real INIs predate UTF-8 (Windows-1252 comments).
pub(crate) fn read_lossy(path: &Path) -> Result<String> {
    std::fs::read(path)
        .with_context(|| format!("failed to read {}", path.display()))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Load key names from an INI file's optional sibling Generals `.str` file.
pub(crate) fn load_sibling_str_keys(ini_url: &Url) -> Vec<String> {
    let Ok(path) = ini_url.to_file_path() else {
        return Vec::new();
    };
    for path in [path.with_extension("str"), path.with_extension("STR")] {
        if let Ok(text) = read_lossy(&path) {
            return parse_str_keys(&text);
        }
    }
    Vec::new()
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

fn big_entries(path: &Path) -> Result<Vec<BigEntry>> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to open BIG archive {}", path.display()))?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)
        .with_context(|| format!("failed to read BIG archive {}", path.display()))?;
    if &magic != b"BIGF" {
        anyhow::bail!("{} is not a BIGF archive", path.display());
    }

    let _archive_size = read_u32_be(&mut file)?;
    let count = read_u32_be(&mut file)?;
    file.seek(SeekFrom::Start(0x10))?;

    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let offset = read_u32_be(&mut file)? as u64;
        let size = read_u32_be(&mut file)? as usize;
        let name = read_c_string(&mut file)?.replace('\\', "/");
        entries.push(BigEntry { name, offset, size });
    }
    Ok(entries)
}

fn read_big_entry_bytes(path: &Path, entry: &BigEntry) -> Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(entry.offset))?;
    let mut bytes = vec![0; entry.size];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn big_uri(path: &Path, entry: &str) -> String {
    let archive = path.to_string_lossy().replace('\\', "/");
    let mut uri = Url::parse("big:///").expect("static BIG URI is valid");
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

fn raw_asset(path: &str, uri: &str) -> Option<FileAsset> {
    let name = path.rsplit(['/', '\\']).next()?;
    let (_, extension) = name.rsplit_once('.')?;
    let kind = if extension.eq_ignore_ascii_case("wav") || extension.eq_ignore_ascii_case("mp3") {
        AssetKind::Audio
    } else if extension.eq_ignore_ascii_case("tga") || extension.eq_ignore_ascii_case("dds") {
        AssetKind::Texture
    } else {
        return None;
    };
    Some(FileAsset {
        kind,
        name: name.to_string(),
        uri: uri.to_string(),
    })
}

pub(crate) fn parse_w3d_models(bytes: &[u8], fallback_name: &str) -> Vec<ModelAsset> {
    match W3dFile::parse(bytes) {
        Ok(file) => file
            .catalog(fallback_name)
            .into_iter()
            .map(|model| ModelAsset {
                name: model.name,
                members: model.members,
            })
            .collect(),
        Err(_) if !fallback_name.trim().is_empty() => vec![ModelAsset {
            name: fallback_name.trim().to_string(),
            members: Vec::new(),
        }],
        Err(_) => Vec::new(),
    }
}

pub(crate) fn scan_big(analyzer: &Analyzer, path: &Path) -> Result<Vec<ScanEntry>> {
    let mut out = Vec::new();
    let mut assets = Vec::new();
    for entry in big_entries(path)? {
        let file = big_uri(path, &entry.name);
        let extension = Path::new(&entry.name)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if extension.eq_ignore_ascii_case("ini") {
            let bytes = read_big_entry_bytes(path, &entry).with_context(|| {
                format!("failed to read {} from {}", entry.name, path.display())
            })?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let parse = analyzer.parse(&text);
            out.push((
                file.clone(),
                definitions_in(analyzer, &parse, &file),
                references_in(analyzer, &parse),
                module_tags_in(analyzer, &parse),
                object_models_in(analyzer, &parse),
                object_parents_in(&parse),
                Vec::new(),
                Vec::new(),
                Some(Arc::from(text)),
            ));
        } else if extension.eq_ignore_ascii_case("w3d") {
            let bytes = read_big_entry_bytes(path, &entry).with_context(|| {
                format!("failed to read {} from {}", entry.name, path.display())
            })?;
            let models = parse_w3d_models(&bytes, &file_stem_str(&entry.name));
            if !models.is_empty() {
                out.push((
                    file,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    models,
                    Vec::new(),
                    None,
                ));
            }
        } else if let Some(asset) = raw_asset(&entry.name, &file) {
            assets.push(asset);
        }
    }
    assets.sort_by(|left, right| {
        file_stem_str(&left.name)
            .to_ascii_lowercase()
            .cmp(&file_stem_str(&right.name).to_ascii_lowercase())
            .then_with(|| {
                let rank = |name: &str| {
                    if name
                        .rsplit_once('.')
                        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("dds"))
                    {
                        1
                    } else {
                        0
                    }
                };
                rank(&left.name).cmp(&rank(&right.name))
            })
    });
    if !assets.is_empty() {
        out.push((
            big_uri(path, "__assets__"),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            assets,
            None,
        ));
    }
    Ok(out)
}

struct DiscoveryOutcome {
    paths: Vec<PathBuf>,
    skipped: usize,
}

/// Best-effort discovery used by tests and helpers that do not need the
/// skipped-entry count. Interactive indexing consumes `collect_paths`
/// directly so inaccessible configured inputs remain visible in ScanStats.
#[cfg(test)]
pub(crate) fn collect_scan_paths(roots: &[PathBuf]) -> Vec<PathBuf> {
    collect_paths(roots, false)
        .map(|outcome| outcome.paths)
        .unwrap_or_default()
}

/// Checked discovery used by the CLI, where skipped inputs must fail visibly.
pub(crate) fn collect_scan_paths_checked(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    collect_paths(roots, true).map(|outcome| outcome.paths)
}

fn collect_paths(roots: &[PathBuf], checked: bool) -> Result<DiscoveryOutcome> {
    let mut out = Vec::new();
    let mut skipped = 0;
    for root in roots {
        let root_start = out.len();
        if root.is_file()
            && root
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("big"))
        {
            out.push(root.clone());
            continue;
        }
        for entry in walkdir::WalkDir::new(root) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if checked => return Err(error.into()),
                Err(error) => {
                    skipped += 1;
                    tracing::debug!(root = %root.display(), %error, "workspace walk entry skipped");
                    continue;
                }
            };
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext.eq_ignore_ascii_case("big")
                || ext.eq_ignore_ascii_case("ini")
                || ext.eq_ignore_ascii_case("w3d")
                || ext.eq_ignore_ascii_case("wav")
                || ext.eq_ignore_ascii_case("mp3")
                || ext.eq_ignore_ascii_case("tga")
                || ext.eq_ignore_ascii_case("dds")
            {
                out.push(path.to_path_buf());
            }
        }
        out[root_start..].sort_by(|left, right| {
            let left_stem = left
                .with_extension("")
                .to_string_lossy()
                .to_ascii_lowercase();
            let right_stem = right
                .with_extension("")
                .to_string_lossy()
                .to_ascii_lowercase();
            left_stem.cmp(&right_stem).then_with(|| {
                let rank = |path: &Path| {
                    path.extension()
                        .and_then(|value| value.to_str())
                        .map_or(0, |extension| {
                            if extension.eq_ignore_ascii_case("dds") {
                                1
                            } else {
                                0
                            }
                        })
                };
                rank(left).cmp(&rank(right))
            })
        });
    }
    if skipped > 0 {
        tracing::warn!(
            skipped_count = skipped,
            "workspace walk skipped inaccessible entries"
        );
    }
    Ok(DiscoveryOutcome {
        paths: out,
        skipped,
    })
}

/// Best-effort indexing used by the interactive server.
#[cfg(test)]
pub(crate) fn scan_files(
    analyzer: &Analyzer,
    paths: &[PathBuf],
    progress: &mut impl FnMut(usize, usize),
) -> Vec<ScanEntry> {
    let mut out = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        if let Ok(mut entries) = scan_path(analyzer, path) {
            out.append(&mut entries);
        }
        progress(i + 1, paths.len());
    }
    out
}

/// Scan workspace and base roots, reusing unchanged files from the persistent
/// asset index cache. Base entries stay first so workspace definitions retain
/// their existing override order.
pub(crate) fn scan_with_cache(
    analyzer: &Analyzer,
    workspace_roots: &[PathBuf],
    base_roots: &[PathBuf],
    progress: &mut impl FnMut(ScanProgress),
) -> ScanOutcome {
    let started = Instant::now();
    progress(ScanProgress::Discovering);
    let cache_path = index_cache_path(workspace_roots, base_roots);
    let expected_schema_hash = schema_hash();
    let empty_cache = || IndexCache {
        version: INDEX_CACHE_VERSION,
        schema_hash: expected_schema_hash,
        files: HashMap::new(),
    };
    let (mut cache, cache_state) = match std::fs::read(&cache_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (empty_cache(), "absent"),
        Err(error) => {
            tracing::debug!(path = %cache_path.display(), %error, "index cache could not be read");
            (empty_cache(), "corrupt")
        }
        Ok(bytes) => match serde_json::from_slice::<IndexCache>(&bytes) {
            Err(error) => {
                tracing::debug!(path = %cache_path.display(), %error, "index cache could not be parsed");
                (empty_cache(), "corrupt")
            }
            Ok(cache)
                if cache.version != INDEX_CACHE_VERSION
                    || cache.schema_hash != expected_schema_hash =>
            {
                tracing::debug!(
                    path = %cache_path.display(),
                    cache_version = cache.version,
                    expected_version = INDEX_CACHE_VERSION,
                    "index cache is stale"
                );
                (empty_cache(), "stale")
            }
            Ok(cache) => (cache, "valid"),
        },
    };

    let mut discovered_inputs = 0;
    let mut discovery_failures = 0;
    let mut fingerprint_failures = 0;
    let mut cache_hits = 0;
    let mut cache_misses = 0;
    let mut scan_failures = 0;
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for (roots, is_base) in [(base_roots, true), (workspace_roots, false)] {
        let discovery = collect_paths(roots, false)
            .expect("best-effort discovery converts walk errors into skipped entries");
        discovery_failures += discovery.skipped;
        for path in discovery.paths {
            let key = path_key(&path);
            if seen.insert(key.clone()) {
                discovered_inputs += 1;
                if let Some(fingerprint) = fingerprint(&path) {
                    paths.push((path, key, fingerprint, is_base));
                } else {
                    fingerprint_failures += 1;
                    tracing::debug!(
                        path = %path.display(),
                        "workspace input skipped because it could not be fingerprinted"
                    );
                }
            }
        }
    }
    progress(ScanProgress::InputsDiscovered {
        total: paths.len(),
        skipped: discovery_failures + fingerprint_failures,
    });

    let cache_manifest_unchanged = cache_state == "valid"
        && discovery_failures == 0
        && fingerprint_failures == 0
        && paths.len() == cache.files.len()
        && paths.iter().all(|(_, key, fingerprint, _)| {
            cache
                .files
                .get(key)
                .is_some_and(|cached| cached.fingerprint == *fingerprint)
        });
    let cache_unchanged = cache_manifest_unchanged && refresh_index_cache_last_used(&cache_path);

    let mut next = if cache_unchanged {
        HashMap::new()
    } else {
        HashMap::with_capacity(paths.len())
    };
    let mut scanned = Vec::new();
    let total = paths.len();
    for (done, (path, key, fingerprint, is_base)) in paths.into_iter().enumerate() {
        let entries = match cache.files.remove(&key) {
            Some(cached) if cached.fingerprint == fingerprint => {
                cache_hits += 1;
                cached.entries.into_iter().map(ScanEntry::from).collect()
            }
            _ => {
                cache_misses += 1;
                match scan_path(analyzer, &path) {
                    Ok(entries) => entries,
                    Err(error) => {
                        scan_failures += 1;
                        tracing::debug!(path = %path.display(), %error, "workspace input could not be indexed");
                        Vec::new()
                    }
                }
            }
        };
        if !cache_unchanged {
            next.insert(
                key,
                CachedFile {
                    fingerprint,
                    entries: entries.iter().map(CachedEntry::from).collect(),
                },
            );
        }
        scanned.extend(entries.into_iter().map(|entry| (is_base, entry)));
        progress(ScanProgress::Indexing {
            done: done + 1,
            total,
            cache_hits,
            cache_misses,
        });
    }
    let mut cache_written = cache_unchanged;
    let mut cache_updated = false;
    if !cache_unchanged {
        progress(ScanProgress::WritingCache);
        let cache = IndexCache {
            version: INDEX_CACHE_VERSION,
            schema_hash: expected_schema_hash,
            files: next,
        };
        if let Some(parent) = cache_path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent).and_then(|()| {
                serde_json::to_vec(&cache)
                    .map_err(std::io::Error::other)
                    .and_then(|bytes| std::fs::write(&cache_path, bytes))
            }) {
                tracing::debug!(path = %cache_path.display(), %error, "asset index cache write failed");
                tracing::warn!(%error, "could not write asset index cache");
            } else {
                cache_written = true;
                cache_updated = true;
            }
        }
    }
    let pruned_caches = prune_index_caches(cache_written.then_some(cache_path.as_path()));
    let skipped_count = fingerprint_failures + scan_failures;
    if skipped_count > 0 {
        tracing::warn!(
            skipped_count,
            fingerprint_failures,
            scan_failures,
            "workspace indexing skipped inputs"
        );
    }
    tracing::debug!(
        path = %cache_path.display(),
        cache_state,
        discovered_inputs,
        discovery_failures,
        cache_hits,
        cache_misses,
        reparsed_files = cache_misses,
        fingerprint_failures,
        scan_failures,
        produced_entries = scanned.len(),
        cache_written,
        pruned_caches,
        cache_updated,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "workspace scan completed"
    );
    ScanOutcome {
        entries: scanned,
        stats: ScanStats {
            discovered_inputs,
            discovery_failures,
            cache_hits,
            cache_misses,
            fingerprint_failures,
            scan_failures,
            cache_written,
            cache_updated,
        },
    }
}

pub(crate) fn scan_files_checked(analyzer: &Analyzer, paths: &[PathBuf]) -> Result<Vec<ScanEntry>> {
    let mut out = Vec::new();
    for path in paths {
        out.extend(scan_path(analyzer, path)?);
    }
    Ok(out)
}

fn scan_path(analyzer: &Analyzer, path: &Path) -> Result<Vec<ScanEntry>> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext.eq_ignore_ascii_case("big") {
        return scan_big(analyzer, path);
    }

    let uri = Url::from_file_path(path)
        .map_err(|_| anyhow::anyhow!("cannot convert {} to a file URI", path.display()))?;
    if ext.eq_ignore_ascii_case("ini") {
        let text = read_lossy(path)?;
        let parse = analyzer.parse(&text);
        Ok(vec![(
            uri.to_string(),
            definitions_in(analyzer, &parse, uri.as_str()),
            references_in(analyzer, &parse),
            module_tags_in(analyzer, &parse),
            object_models_in(analyzer, &parse),
            object_parents_in(&parse),
            Vec::new(),
            Vec::new(),
            None,
        )])
    } else if ext.eq_ignore_ascii_case("w3d") {
        let bytes =
            std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let models = parse_w3d_models(&bytes, stem);
        Ok((!models.is_empty())
            .then_some((
                uri.to_string(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                models,
                Vec::new(),
                None,
            ))
            .into_iter()
            .collect())
    } else if let Some(asset) = raw_asset(&path.to_string_lossy(), uri.as_str()) {
        Ok(vec![(
            uri.to_string(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![asset],
            None,
        )])
    } else {
        Ok(Vec::new())
    }
}

pub(crate) fn read_asset_uri(uri: &str) -> Result<Vec<u8>> {
    let url = Url::parse(uri).with_context(|| format!("invalid asset URI `{uri}`"))?;
    if url.scheme() == "file" {
        let path = url
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("invalid file asset URI `{uri}`"))?;
        let len = std::fs::metadata(&path)?.len();
        if len > MAX_PREVIEW_ASSET_BYTES {
            anyhow::bail!("asset exceeds 128 MiB");
        }
        return std::fs::read(&path)
            .with_context(|| format!("failed to read asset {}", path.display()));
    }
    if url.scheme() != "big" {
        anyhow::bail!("unsupported asset URI scheme `{}`", url.scheme());
    }
    let decoded = percent_decode_str(url.path()).decode_utf8_lossy();
    let (archive, entry_name) = decoded
        .split_once("!/")
        .ok_or_else(|| anyhow::anyhow!("invalid BIG asset URI `{uri}`"))?;
    let archive =
        if cfg!(windows) && archive.as_bytes().get(2) == Some(&b':') && archive.starts_with('/') {
            &archive[1..]
        } else {
            archive
        };
    let path = Path::new(archive);
    let entry = big_entries(path)?
        .into_iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(entry_name))
        .ok_or_else(|| anyhow::anyhow!("asset `{entry_name}` not found in {}", path.display()))?;
    if entry.size as u64 > MAX_PREVIEW_ASSET_BYTES {
        anyhow::bail!("asset exceeds 128 MiB");
    }
    read_big_entry_bytes(path, &entry)
}

#[cfg(test)]
pub(crate) fn scan_roots(analyzer: &Analyzer, roots: &[PathBuf]) -> Vec<ScanEntry> {
    scan_files(analyzer, &collect_scan_paths(roots), &mut |_, _| {})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "zerosyntax-{label}-{}-{}",
            std::process::id(),
            UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ))
    }

    #[test]
    fn unchanged_warm_cache_is_not_rewritten() {
        let root = unique_temp_dir("warm-cache");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Weapon.ini"), "Weapon TestWeapon\nEnd\n").unwrap();
        let workspace_roots = vec![root.clone()];

        let cold = scan_with_cache(&Analyzer::embedded(), &workspace_roots, &[], &mut |_| {});
        assert_eq!(cold.stats.cache_misses, 1);
        assert!(cold.stats.cache_updated);
        let cache_path = index_cache_path(&workspace_roots, &[]);
        let old_last_used = SystemTime::now() - Duration::from_secs(24 * 60 * 60);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&cache_path)
            .unwrap()
            .set_modified(old_last_used)
            .unwrap();

        let mut warm_events = Vec::new();
        let warm = scan_with_cache(&Analyzer::embedded(), &workspace_roots, &[], &mut |event| {
            warm_events.push(event)
        });
        assert_eq!(warm.stats.cache_hits, 1);
        assert_eq!(warm.stats.cache_misses, 0);
        assert!(warm.stats.cache_written);
        assert!(!warm.stats.cache_updated);
        assert!(!warm_events.contains(&ScanProgress::WritingCache));
        assert!(
            std::fs::metadata(&cache_path).unwrap().modified().unwrap() > old_last_used,
            "using an unchanged cache refreshes its retention timestamp"
        );

        std::fs::write(root.join("Weapon.ini"), "Weapon UpdatedWeapon\nEnd\n").unwrap();
        let changed = scan_with_cache(&Analyzer::embedded(), &workspace_roots, &[], &mut |_| {});
        assert_eq!(changed.stats.cache_misses, 1);
        assert!(changed.stats.cache_updated);

        let _ = clear_index_cache(&workspace_roots, &[]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_retention_refresh_rejects_the_no_rewrite_fast_path() {
        let missing = unique_temp_dir("missing-cache").join("index.json");
        assert!(!missing.exists());
        assert!(!refresh_index_cache_last_used(&missing));
    }

    #[test]
    fn discovery_failures_reach_progress_and_scan_stats() {
        let missing = std::env::temp_dir().join(format!(
            "zerosyntax-missing-{}-{}",
            std::process::id(),
            UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        assert!(!missing.exists());
        let workspace_roots = vec![missing];
        let mut events = Vec::new();
        let outcome = scan_with_cache(&Analyzer::embedded(), &workspace_roots, &[], &mut |event| {
            events.push(event)
        });

        assert_eq!(outcome.stats.discovered_inputs, 0);
        assert_eq!(outcome.stats.discovery_failures, 1);
        assert_eq!(outcome.stats.skipped_inputs(), 1);
        assert!(events.iter().any(|event| matches!(
            event,
            ScanProgress::InputsDiscovered {
                total: 0,
                skipped: 1
            }
        )));

        let _ = clear_index_cache(&workspace_roots, &[]);
    }

    fn temp_cache_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zerosyntax-prune-{name}-{}-{}",
            std::process::id(),
            UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a cache-shaped file `age` old, so retention order is deterministic
    /// instead of dependent on filesystem timestamp granularity.
    fn write_cache_file(dir: &Path, name: &str, age: Duration) -> PathBuf {
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
        path
    }

    #[test]
    fn cache_file_version_accepts_only_generated_names() {
        assert_eq!(
            cache_file_version("index-v5-0123456789abcdef.json"),
            Some(5)
        );
        assert_eq!(
            cache_file_version("index-v12-0123456789abcdef.json"),
            Some(12)
        );
        for name in [
            "index-v5-0123456789abcde.json",   // hash too short
            "index-v5-0123456789abcdefg.json", // not hexadecimal
            "index-vX-0123456789abcdef.json",
            "index-v5-0123456789abcdef.json.bak",
            "notes.txt",
        ] {
            assert_eq!(cache_file_version(name), None, "{name}");
        }
    }

    #[test]
    fn pruning_drops_earlier_versions_and_keeps_recent_caches() {
        let dir = temp_cache_dir("versions");
        let unrelated = write_cache_file(&dir, "notes.txt", Duration::ZERO);
        let old_version = write_cache_file(&dir, "index-v1-00000000000000ff.json", Duration::ZERO);
        let keep = write_cache_file(
            &dir,
            &format!("index-v{INDEX_CACHE_VERSION}-0000000000000000.json"),
            Duration::ZERO,
        );
        let others: Vec<_> = (1..=INDEX_CACHE_RETAINED as u64 + 2)
            .map(|index| {
                write_cache_file(
                    &dir,
                    &format!("index-v{INDEX_CACHE_VERSION}-{index:016x}.json"),
                    Duration::from_secs(index * 60),
                )
            })
            .collect();

        let pruned = prune_index_caches_in(&dir, Some(&keep));

        assert!(keep.exists(), "the cache just written survives");
        assert!(unrelated.exists(), "unrelated files are never touched");
        assert!(!old_version.exists(), "earlier cache versions are dropped");
        let surviving = others.iter().filter(|path| path.exists()).count();
        assert_eq!(surviving, INDEX_CACHE_RETAINED - 1, "newest others survive");
        assert!(others[0].exists() && !others[others.len() - 1].exists());
        assert_eq!(pruned, 1 + others.len() - surviving);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pruning_drops_caches_unused_past_the_age_limit() {
        let dir = temp_cache_dir("age");
        let fresh = write_cache_file(
            &dir,
            &format!("index-v{INDEX_CACHE_VERSION}-000000000000000a.json"),
            Duration::ZERO,
        );
        let expired = write_cache_file(
            &dir,
            &format!("index-v{INDEX_CACHE_VERSION}-000000000000000b.json"),
            INDEX_CACHE_MAX_AGE + Duration::from_secs(60),
        );

        // No `keep` (the cache write failed) still prunes.
        let pruned = prune_index_caches_in(&dir, None);

        assert!(fresh.exists());
        assert!(!expired.exists());
        assert_eq!(pruned, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_write_does_not_reserve_a_retention_slot_for_the_stale_file() {
        // A write failure leaves the previous file at `cache_path` in place.
        // It must compete for a retention slot like any other cache, not
        // reserve one and evict a newer file in its place.
        let dir = temp_cache_dir("stale-keep");
        let stale = write_cache_file(
            &dir,
            &format!("index-v{INDEX_CACHE_VERSION}-0000000000000001.json"),
            Duration::from_secs(600),
        );
        let others: Vec<_> = (2..=INDEX_CACHE_RETAINED as u64 + 1)
            .map(|index| {
                write_cache_file(
                    &dir,
                    &format!("index-v{INDEX_CACHE_VERSION}-{index:016x}.json"),
                    Duration::from_secs(600 - index * 60),
                )
            })
            .collect();

        prune_index_caches_in(&dir, None);

        assert!(!stale.exists(), "the oldest file is evicted, not reserved");
        assert!(
            others.iter().all(|path| path.exists()),
            "newer files are not evicted to make room for the stale one"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
