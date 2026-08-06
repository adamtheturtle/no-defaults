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
fn fix_warns_about_callers_it_cannot_see() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(&path, "def f(value=1): pass\ndef g(other=2): pass\n")?;
    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("2 defaults removed"), "{stderr:?}");
    assert!(stderr.contains("callers outside them"), "{stderr:?}");
    Ok(())
}

#[test]
fn fix_updates_call_sites_across_files() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(
        &api,
        "from dataclasses import dataclass, field\n\n\
         def connect(host, timeout=30, *, retries=3):\n    return host\n\n\
         @dataclass\nclass Job:\n    name: str\n    tags: list = field(default_factory=list)\n\n\
         class Client:\n    def fetch(self, url, verify=True):\n        return url\n",
    )?;
    std::fs::write(
        &caller,
        "import api\n\n\
         api.connect(\"h\")\n\
         api.connect(\"h\", 5, retries=1)\n\
         api.Job(\"j\")\n\
         api.Client().fetch(\"u\")\n",
    )?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8(output.stdout)?.contains("Updated 3 call sites."),
        "the fully supplied call needs nothing added"
    );
    assert_eq!(
        std::fs::read_to_string(&caller)?,
        "import api\n\n\
         api.connect(\"h\", timeout=30, retries=3)\n\
         api.connect(\"h\", 5, retries=1)\n\
         api.Job(\"j\", tags=[])\n\
         api.Client().fetch(\"u\", verify=True)\n"
    );
    let output = Command::new(binary()).arg(directory.path()).output()?;
    assert_eq!(output.status.code(), Some(0), "the fix is complete");
    Ok(())
}

#[test]
fn fix_leaves_calls_it_cannot_resolve_alone() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("api.py"),
        "SENTINEL = object()\n\n\
         def keep(value=SENTINEL): return value\n\n\
         def shared(x=1): return x\n",
    )?;
    std::fs::write(
        directory.path().join("other.py"),
        "def shared(y=2): return y\n",
    )?;
    let caller = directory.path().join("caller.py");
    std::fs::write(
        &caller,
        "import api\nimport other\n\n\
         api.keep()\n\
         api.shared()\n\
         api.keep(**{})\n",
    )?;
    let before = std::fs::read_to_string(&caller)?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("is not a literal"), "{stderr:?}");
    assert!(
        stderr.contains("several fixed definitions share this name"),
        "{stderr:?}"
    );
    assert!(stderr.contains("unpacks `*` or `**`"), "{stderr:?}");
    assert_eq!(std::fs::read_to_string(&caller)?, before);
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
