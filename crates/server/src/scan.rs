//! Shared filesystem, BIG archive, and W3D workspace scanning.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tower_lsp::lsp_types::Url;
use zerosyntax_analysis::index::{
    definitions_in, module_tags_in, references_in, Definition, ModelAsset, ReferenceSite,
};
use zerosyntax_analysis::Analyzer;

pub(crate) type ScanEntry = (
    String,
    Vec<Definition>,
    Vec<ReferenceSite>,
    Vec<(String, String)>,
    Vec<ModelAsset>,
    Option<Arc<str>>,
);

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

pub(crate) fn parse_w3d_models(bytes: &[u8], fallback_name: &str) -> Vec<ModelAsset> {
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
            push_name(&mut members, read_fixed_name(&payload[16..48]));
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

const MAX_W3D_CHUNK_DEPTH: usize = 16;

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
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
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
    let mut seen = std::collections::HashSet::new();
    values.retain(|value| seen.insert(value.to_ascii_lowercase()));
}

pub(crate) fn scan_big(analyzer: &Analyzer, path: &Path) -> Result<Vec<ScanEntry>> {
    let mut out = Vec::new();
    for entry in big_entries(path)? {
        let file = big_uri(path, &entry.name);
        if entry.name.ends_with(".ini") || entry.name.ends_with(".INI") {
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
                Vec::new(),
                Some(Arc::from(text)),
            ));
        } else if entry.name.ends_with(".w3d") || entry.name.ends_with(".W3D") {
            let bytes = read_big_entry_bytes(path, &entry).with_context(|| {
                format!("failed to read {} from {}", entry.name, path.display())
            })?;
            let models = parse_w3d_models(&bytes, &file_stem_str(&entry.name));
            if !models.is_empty() {
                out.push((file, Vec::new(), Vec::new(), Vec::new(), models, None));
            }
        }
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
            {
                out.push(path.to_path_buf());
            }
        }
    }
    Ok(out)
}

/// Best-effort indexing used by the interactive server.
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
                models,
                None,
            ))
            .into_iter()
            .collect())
    } else {
        Ok(Vec::new())
    }
}

#[cfg(test)]
pub(crate) fn scan_roots(analyzer: &Analyzer, roots: &[PathBuf]) -> Vec<ScanEntry> {
    scan_files(analyzer, &collect_scan_paths(roots), &mut |_, _| {})
}
