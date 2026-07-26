//! Shared filesystem, BIG archive, and W3D workspace scanning.

use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

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

const INDEX_CACHE_VERSION: u32 = 5;
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

pub(crate) fn clear_index_cache(
    workspace_roots: &[PathBuf],
    base_roots: &[PathBuf],
) -> std::io::Result<bool> {
    match std::fs::remove_file(index_cache_path(workspace_roots, base_roots)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
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
    W3dFile::parse(bytes)
        .map(|file| {
            file.catalog(fallback_name)
                .into_iter()
                .map(|model| ModelAsset {
                    name: model.name,
                    members: model.members,
                })
                .collect()
        })
        .unwrap_or_default()
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

/// Best-effort discovery used by the interactive server.
pub(crate) fn collect_scan_paths(roots: &[PathBuf]) -> Vec<PathBuf> {
    collect_paths(roots, false).unwrap_or_default()
}

/// Checked discovery used by the CLI, where skipped inputs must fail visibly.
pub(crate) fn collect_scan_paths_checked(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    collect_paths(roots, true)
}

fn collect_paths(roots: &[PathBuf], checked: bool) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
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
                Err(_) => continue,
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
    Ok(out)
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
    progress: &mut impl FnMut(usize, usize),
) -> Vec<(bool, ScanEntry)> {
    let cache_path = index_cache_path(workspace_roots, base_roots);
    let expected_schema_hash = schema_hash();
    let mut cache = std::fs::read(&cache_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<IndexCache>(&bytes).ok())
        .filter(|cache| {
            cache.version == INDEX_CACHE_VERSION && cache.schema_hash == expected_schema_hash
        })
        .unwrap_or(IndexCache {
            version: INDEX_CACHE_VERSION,
            schema_hash: expected_schema_hash,
            files: HashMap::new(),
        });

    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for (roots, is_base) in [(base_roots, true), (workspace_roots, false)] {
        for path in collect_scan_paths(roots) {
            let key = path_key(&path);
            if seen.insert(key.clone()) {
                if let Some(fingerprint) = fingerprint(&path) {
                    paths.push((path, key, fingerprint, is_base));
                }
            }
        }
    }

    let mut next = HashMap::with_capacity(paths.len());
    let mut scanned = Vec::new();
    let total = paths.len();
    for (done, (path, key, fingerprint, is_base)) in paths.into_iter().enumerate() {
        let entries = match cache.files.remove(&key) {
            Some(cached) if cached.fingerprint == fingerprint => {
                cached.entries.into_iter().map(ScanEntry::from).collect()
            }
            _ => scan_path(analyzer, &path).unwrap_or_default(),
        };
        next.insert(
            key,
            CachedFile {
                fingerprint,
                entries: entries.iter().map(CachedEntry::from).collect(),
            },
        );
        scanned.extend(entries.into_iter().map(|entry| (is_base, entry)));
        progress(done + 1, total);
    }
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
            tracing::warn!(%error, path = %cache_path.display(), "could not write asset index cache");
        }
    }
    scanned
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
