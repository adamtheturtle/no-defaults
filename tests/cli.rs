use std::process::Command;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_no-defaults")
}

#[test]
fn real_project_uses_per_file_configuration() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = format!("{}/tests/fixtures/real_project", env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("json")
        .arg(&fixture)
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    let diagnostics: serde_json::Value = serde_json::from_str(&stdout)?;
    let diagnostics = diagnostics.as_array().ok_or("expected JSON array")?;
    assert_eq!(diagnostics.len(), 2);
    assert!(stdout.contains("_private"));
    assert!(stdout.contains("helper"));
    assert!(!stdout.contains("function `public`"));
    Ok(())
}

#[test]
fn unused_noqa_is_reported_and_fixed() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "def f(value): pass  # noqa: NOD001\ndef g(value=1): pass  # noqa: NOD001\n",
    )?;
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(&path)
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("1:21: NOD002 unused `noqa` directive for `NOD001`"),
        "{stdout}"
    );
    assert!(!stdout.contains("2:"), "{stdout}");
    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&path)?,
        "def f(value): pass\ndef g(value=1): pass  # noqa: NOD001\n"
    );
    Ok(())
}

#[test]
fn diff_previews_without_writing() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(&path, "def f(value=1): pass\n")?;
    let output = Command::new(binary()).arg("--diff").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("-def f(value=1): pass"));
    assert!(stdout.contains("+def f(value): pass"));
    assert_eq!(std::fs::read_to_string(path)?, "def f(value=1): pass\n");
    Ok(())
}

#[test]
fn fix_warns_that_call_sites_are_not_updated() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(&path, "def f(value=1): pass\ndef g(other=2): pass\n")?;
    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("2 defaults removed"), "{stderr:?}");
    assert!(stderr.contains("call sites are not updated"), "{stderr:?}");
    Ok(())
}

#[test]
fn fix_does_not_warn_when_only_unused_directives_are_removed(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(&path, "def f(value):  # noqa: NOD001\n    pass\n")?;
    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(!stderr.contains("call sites"), "{stderr:?}");
    Ok(())
}
