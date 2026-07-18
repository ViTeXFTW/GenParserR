use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "zerosyntax-cli-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(args: &[&str], stdin: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zerosyntax-lsp"));
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().unwrap();
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn json(output: &Output) -> Vec<serde_json::Value> {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON: {error}; stdout={}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn write_big(path: &Path, entry: &str, content: &[u8]) {
    let mut entry = entry.replace('/', "\\").into_bytes();
    entry.push(0);
    let data_offset = 0x10 + 8 + entry.len();
    let archive_size = data_offset + content.len();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"BIGF");
    bytes.extend_from_slice(&(archive_size as u32).to_be_bytes());
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(&(data_offset as u32).to_be_bytes());
    bytes.extend_from_slice(&(content.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&entry);
    bytes.extend_from_slice(content);
    fs::write(path, bytes).unwrap();
}

#[test]
fn stdin_json_positions_and_failure_thresholds() {
    let error = run(
        &["check", "-", "--json"],
        Some("Weapon X\n  ClipSize = é\nEnd\n"),
    );
    assert_eq!(error.status.code(), Some(1));
    let diagnostics = json(&error);
    let bad_number = diagnostics
        .iter()
        .find(|item| item["code"] == "bad-number")
        .unwrap();
    assert_eq!(bad_number["file"], "<stdin>");
    assert_eq!(bad_number["range"]["start"]["line"], 2);
    assert_eq!(bad_number["range"]["start"]["column"], 14);
    assert_eq!(bad_number["range"]["end"]["column"], 15);

    let warning_src = "Weapon X\n  PrimaryDamg = 1\nEnd\n";
    assert_eq!(
        run(&["check", "--json", "-"], Some(warning_src))
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        run(
            &["check", "--json", "--fail-on", "warning", "-"],
            Some(warning_src)
        )
        .status
        .code(),
        Some(1)
    );

    let hint_src = "; zerosyntax-disable: made-up-code\nWeapon X\nEnd\n";
    assert_eq!(
        run(
            &["check", "--json", "--fail-on", "hint", "-"],
            Some(hint_src)
        )
        .status
        .code(),
        Some(1)
    );
}

#[test]
fn directory_targets_are_indexed_together_and_deduplicated() {
    let dir = TempDir::new("workspace");
    let definitions = dir.path().join("definitions.ini");
    let object = dir.path().join("object.ini");
    fs::write(&definitions, "Upgrade Upgrade_Test\n  Bogus = 1\nEnd\n").unwrap();
    fs::write(
        &object,
        "Object Tank\n  Behavior = WeaponSetUpgrade ModuleTag_01\n    TriggeredBy = Upgrade_Test\n    Bogus = 1\n  End\nEnd\n",
    )
    .unwrap();

    let dir_arg = dir.path().to_str().unwrap();
    let object_arg = object.to_str().unwrap();
    let output = run(&["check", "--json", dir_arg, object_arg], None);
    assert_eq!(output.status.code(), Some(0));
    let diagnostics = json(&output);
    assert!(!diagnostics
        .iter()
        .any(|item| item["code"] == "unresolved-reference"));
    let unknown_fields: Vec<_> = diagnostics
        .iter()
        .filter(|item| item["code"] == "unknown-field")
        .collect();
    assert_eq!(
        unknown_fields.len(),
        2,
        "overlapping targets must only be diagnosed once"
    );
    assert!(unknown_fields[0]["file"].as_str() < unknown_fields[1]["file"].as_str());
}

#[test]
fn directory_and_big_base_roots_resolve_without_reporting_base_diagnostics() {
    let base_dir = TempDir::new("base-dir");
    fs::write(
        base_dir.path().join("base.ini"),
        "Object Tank\n  Bogus = 1\nEnd\n",
    )
    .unwrap();
    let archive_dir = TempDir::new("base-big");
    let archive = archive_dir.path().join("base.big");
    write_big(
        &archive,
        "Data/INI/base.ini",
        b"Object Tank\n  Bogus = 1\nEnd\n",
    );

    for base in [base_dir.path(), archive.as_path()] {
        let output = run(
            &[
                "check",
                "--json",
                "--base-root",
                base.to_str().unwrap(),
                "--stdin-filename",
                "map.ini",
                "-",
            ],
            Some("Object Tank\nEnd\n"),
        );
        assert_eq!(output.status.code(), Some(0));
        let diagnostics = json(&output);
        assert!(diagnostics.iter().any(|item| item["code"] == "overrides"));
        assert!(!diagnostics
            .iter()
            .any(|item| item["code"] == "duplicate-definition"));
        assert!(diagnostics.iter().all(|item| item["file"] == "map.ini"));
    }
}

#[test]
fn human_output_and_usage_failures_are_stable() {
    let output = run(&["check", "-"], Some("Weapon X\n  ClipSize = lots\nEnd\n"));
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("<stdin>:2:14: error[bad-number]:"),
        "{stdout}"
    );
    assert!(stdout.contains("1 error(s), 0 warning(s), 0 hint(s)"));

    assert_eq!(run(&["check"], None).status.code(), Some(2));
    assert_eq!(
        run(&["check", "definitely-does-not-exist.ini"], None)
            .status
            .code(),
        Some(2)
    );
}
