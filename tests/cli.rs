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
         class Client:\n    def fetch(self, url, verify=True):\n        return url\n\n    \
         def twice(self, url):\n        return self.fetch(url)\n",
    )?;
    std::fs::write(
        &caller,
        "import api\n\n\
         api.connect(\"h\")\n\
         api.connect(\"h\", 5, retries=1)\n\
         api.Job(\"j\")\n",
    )?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8(output.stdout)?.contains("Updated 3 call sites."),
        "the fully supplied call needs nothing added, and `self.fetch` does"
    );
    assert_eq!(
        std::fs::read_to_string(&caller)?,
        "import api\n\n\
         api.connect(\"h\", timeout=30, retries=3)\n\
         api.connect(\"h\", 5, retries=1)\n\
         api.Job(\"j\", tags=[])\n"
    );
    assert!(
        std::fs::read_to_string(&api)?.contains("self.fetch(url, verify=True)"),
        "a method reached through `self` has a known receiver"
    );
    let output = Command::new(binary()).arg(directory.path()).output()?;
    assert_eq!(output.status.code(), Some(0), "the fix is complete");
    Ok(())
}

#[test]
fn an_exempt_file_keeps_its_defaults_but_has_its_calls_fixed(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("pyproject.toml"),
        "[tool.no_defaults]\n\n[tool.no_defaults.per_file_enforcement]\n\"exempt.py\" = \"none\"\n",
    )?;
    std::fs::write(
        directory.path().join("api.py"),
        "def connect(host, timeout=30): return (host, timeout)\n",
    )?;
    let exempt = directory.path().join("exempt.py");
    std::fs::write(
        &exempt,
        "from api import connect\n\ndef keeps_its_own(value=99): return value\n\nconnect(\"h\")\n",
    )?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&exempt)?,
        "from api import connect\n\ndef keeps_its_own(value=99): return value\n\nconnect(\"h\", timeout=30)\n",
        "exemption decides which definitions are checked, not whether the file's calls keep working"
    );
    Ok(())
}

#[test]
fn fix_leaves_calls_it_cannot_resolve_alone() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("api.py"),
        "SENTINEL = object()\n\n\
         def keep(value=SENTINEL): return value\n",
    )?;
    let caller = directory.path().join("caller.py");
    std::fs::write(
        &caller,
        "import api\n\n\
         api.keep()\n\
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
    assert!(stderr.contains("unpacks `*` or `**`"), "{stderr:?}");
    assert_eq!(std::fs::read_to_string(&caller)?, before);
    Ok(())
}

#[test]
fn the_updated_count_leaves_out_edits_that_were_dropped() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    // The call to `g` sits inside the default being deleted, so its rewrite is
    // dropped and must not be counted.
    std::fs::write(&path, "def g(a=1): pass\ndef f(x=g()): pass\n")?;
    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("Updated 0 call sites."), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(&path)?,
        "def g(a): pass\ndef f(x): pass\n"
    );
    Ok(())
}

/// A project's own `connect` must not lend its removed defaults to a same-named
/// method on an unrelated object. Guessing there breaks working code.
#[test]
fn a_same_named_call_on_another_object_is_left_alone() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let caller = directory.path().join("mine.py");
    std::fs::write(
        &caller,
        "import socket\n\n\
         def connect(host, timeout=30): return (host, timeout)\n\n\
         def go(): return socket.socket().connect((\"h\", 1))\n\n\
         connect(\"h\")\n",
    )?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    let fixed = std::fs::read_to_string(&caller)?;
    assert!(
        fixed.contains("socket.socket().connect((\"h\", 1))"),
        "the socket call is not this project's `connect`: {fixed}"
    );
    assert!(fixed.contains("connect(\"h\", timeout=30)"), "{fixed}");
    assert!(
        String::from_utf8(output.stderr)?.contains("cannot be tied to the definition"),
        "the call it cannot place is reported rather than guessed at"
    );
    Ok(())
}

/// A class reached through an imported module, or imported by name, is as
/// resolvable as one defined locally.
#[test]
fn methods_of_an_imported_class_are_updated() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("api.py"),
        "class Client:\n    @staticmethod\n    def build(kind=1): return kind\n\n    \
         @classmethod\n    def make(cls, mode=2): return mode\n\n    \
         def fetch(self, url, verify=3): return (url, verify)\n",
    )?;
    let caller = directory.path().join("caller.py");
    std::fs::write(
        &caller,
        "import api\nfrom api import Client\n\n\
         api.Client.build()\n\
         api.Client.make()\n\
         api.Client.fetch(api.Client(), \"u\")\n\
         Client.build()\n",
    )?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&caller)?,
        "import api\nfrom api import Client\n\n\
         api.Client.build(kind=1)\n\
         api.Client.make(mode=2)\n\
         api.Client.fetch(api.Client(), \"u\", verify=3)\n\
         Client.build(kind=1)\n"
    );
    Ok(())
}

/// Two modules may each define `helper`. Resolving through the calling file's
/// imports tells them apart, where matching on the bare name could not.
#[test]
fn same_named_functions_in_two_modules_resolve_separately() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("first.py"),
        "def helper(x=1): return x\n",
    )?;
    std::fs::write(
        directory.path().join("second.py"),
        "def helper(y=2): return y\n",
    )?;
    let caller = directory.path().join("caller.py");
    std::fs::write(
        &caller,
        "import first\nimport second\n\nfirst.helper()\nsecond.helper()\n",
    )?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&caller)?,
        "import first\nimport second\n\nfirst.helper(x=1)\nsecond.helper(y=2)\n"
    );
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

/// A project whose package root re-exports a helper defined in a private
/// module, so the helper's defaults are public API.
fn reexporting_project(respect_reexports: bool) -> Result<tempfile::TempDir, std::io::Error> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("package");
    std::fs::create_dir_all(&package)?;
    std::fs::write(
        directory.path().join("pyproject.toml"),
        format!(
            "[tool.no_defaults]\nprivate_only = true\nrespect_reexports = {respect_reexports}\n"
        ),
    )?;
    std::fs::write(
        package.join("__init__.py"),
        "from ._upload import upload\n\n__all__ = [\"upload\"]\n",
    )?;
    std::fs::write(
        package.join("_upload.py"),
        "def upload(timeout=30): pass\n\n\ndef _helper(retries=3): pass\n",
    )?;
    Ok(directory)
}

#[test]
fn respect_reexports_leaves_publicly_reexported_defaults_alone(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = reexporting_project(true)?;
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("function `_helper`"), "{stdout}");
    assert!(!stdout.contains("function `upload`"), "{stdout}");
    assert!(stdout.contains("Found 1 error."), "{stdout}");
    Ok(())
}

#[test]
fn without_respect_reexports_a_private_module_is_checked_whole(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = reexporting_project(false)?;
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("function `upload`"), "{stdout}");
    assert!(stdout.contains("Found 2 errors."), "{stdout}");
    // The flag turns it on for every checked file, whatever the file says.
    let output = Command::new(binary())
        .arg("--respect-reexports")
        .arg("--output-format")
        .arg("concise")
        .arg(directory.path())
        .output()?;
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("Found 1 error."), "{stdout}");
    Ok(())
}

#[test]
fn fix_leaves_a_reexported_default_in_place() -> Result<(), Box<dyn std::error::Error>> {
    let directory = reexporting_project(true)?;
    let upload = directory.path().join("package/_upload.py");
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&upload)?,
        "def upload(timeout=30): pass\n\n\ndef _helper(retries): pass\n"
    );
    Ok(())
}

#[test]
fn show_settings_reports_reexport_handling() -> Result<(), Box<dyn std::error::Error>> {
    let directory = reexporting_project(true)?;
    let output = Command::new(binary())
        .arg("--show-settings")
        .arg(directory.path().join("package/_upload.py"))
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("enforcement = private"), "{stdout}");
    assert!(stdout.contains("respect-reexports = true"), "{stdout}");
    Ok(())
}

#[test]
fn a_module_reexported_whole_keeps_its_public_defaults() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("package");
    std::fs::create_dir_all(&package)?;
    std::fs::write(
        directory.path().join("pyproject.toml"),
        "[tool.no_defaults]\nprivate_only = true\nrespect_reexports = true\n",
    )?;
    // The module itself is re-exported, so `package._upload.upload` is reachable.
    std::fs::write(package.join("__init__.py"), "from . import _upload\n")?;
    std::fs::write(
        package.join("_upload.py"),
        "def upload(timeout=30): pass\n\n\ndef _helper(retries=3): pass\n",
    )?;
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("function `_helper`"), "{stdout}");
    assert!(!stdout.contains("function `upload`"), "{stdout}");
    assert!(stdout.contains("Found 1 error."), "{stdout}");
    Ok(())
}

#[test]
fn a_namespace_package_still_reaches_the_package_root() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    // `data` holds no `__init__.py`, so it is a namespace package, and the
    // root's re-export still applies to what is inside it.
    let data = directory.path().join("package/data");
    std::fs::create_dir_all(&data)?;
    std::fs::write(
        directory.path().join("pyproject.toml"),
        "[tool.no_defaults]\nprivate_only = true\nrespect_reexports = true\n",
    )?;
    std::fs::write(
        directory.path().join("package/__init__.py"),
        "from .data._mod import upload\n",
    )?;
    std::fs::write(
        data.join("_mod.py"),
        "def upload(timeout=30): pass\n\n\ndef _helper(retries=3): pass\n",
    )?;
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("function `_helper`"), "{stdout}");
    assert!(!stdout.contains("function `upload`"), "{stdout}");
    assert!(stdout.contains("Found 1 error."), "{stdout}");
    Ok(())
}

#[test]
fn pydantic_models_are_checked_without_configuration() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("models.py"),
        "from pydantic import BaseModel\n\n\nclass Job(BaseModel):\n    retries: int = 3\n",
    )?;
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("class field `retries`"), "{stdout}");
    Ok(())
}

#[test]
fn field_base_classes_replaces_the_default_list() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("pyproject.toml"),
        "[tool.no_defaults]\nfield_base_classes = [\"msgspec.Struct\"]\n",
    )?;
    std::fs::write(
        directory.path().join("models.py"),
        "class Job(msgspec.Struct):\n    retries: int = 3\n\n\nclass Other(BaseModel):\n    tries: int = 1\n",
    )?;
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("class field `retries`"), "{stdout}");
    assert!(stdout.contains("Found 1 error."), "{stdout}");
    Ok(())
}

#[test]
fn an_empty_field_base_classes_checks_decorated_classes_only(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("pyproject.toml"),
        "[tool.no_defaults]\nfield_base_classes = []\n",
    )?;
    std::fs::write(
        directory.path().join("models.py"),
        "class Job(BaseModel):\n    retries: int = 3\n",
    )?;
    let output = Command::new(binary()).arg(directory.path()).output()?;
    assert_eq!(output.status.code(), Some(0));
    Ok(())
}

#[test]
fn show_settings_reports_the_field_base_classes() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("models.py");
    std::fs::write(&path, "class Job(BaseModel):\n    retries: int = 3\n")?;
    let output = Command::new(binary())
        .arg("--show-settings")
        .arg(&path)
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("field-base-classes = [\"pydantic.BaseModel\"]"),
        "{stdout}"
    );
    Ok(())
}

#[test]
fn a_named_file_that_is_not_python_is_an_error() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let readme = directory.path().join("README.md");
    std::fs::write(&readme, "not Python\n")?;
    let output = Command::new(binary()).arg(&readme).output()?;
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("not a Python file"), "{stderr}");
    assert!(stderr.contains("README.md"), "{stderr}");
    Ok(())
}

#[test]
fn a_directory_walk_still_skips_files_that_are_not_python() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("README.md"), "not Python\n")?;
    std::fs::write(
        directory.path().join("example.py"),
        "def f(value=1): pass\n",
    )?;
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("example.py"), "{stdout}");
    assert!(!stdout.contains("README.md"), "{stdout}");
    Ok(())
}

#[test]
fn one_file_named_twice_is_checked_once() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("d.py"), "def g(value=1): pass\n")?;
    let output = Command::new(binary())
        .current_dir(directory.path())
        .arg("--output-format")
        .arg("concise")
        .arg("d.py")
        .arg("./d.py")
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("Found 1 error."), "{stdout}");
    assert_eq!(stdout.matches("NOD001").count(), 1, "{stdout}");
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_symlink_and_its_target_are_one_file() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("real.py"), "def g(value=1): pass\n")?;
    std::os::unix::fs::symlink("real.py", directory.path().join("link.py"))?;
    let output = Command::new(binary())
        .current_dir(directory.path())
        .arg("--output-format")
        .arg("concise")
        .arg("link.py")
        .arg("real.py")
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("Found 1 error."), "{stdout}");
    Ok(())
}

#[cfg(unix)]
#[test]
fn fixing_a_symlink_writes_through_to_its_target() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let real = directory.path().join("real.py");
    let link = directory.path().join("link.py");
    std::fs::write(&real, "def f(value=1): pass\n")?;
    std::os::unix::fs::symlink("real.py", &link)?;
    let output = Command::new(binary()).arg("--fix").arg(&link).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(std::fs::read_to_string(&real)?, "def f(value): pass\n");
    assert!(
        std::fs::symlink_metadata(&link)?.file_type().is_symlink(),
        "the link must survive the fix"
    );
    Ok(())
}

#[test]
fn the_caret_lands_under_a_default_on_a_non_ascii_line() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("u.py");
    std::fs::write(&path, "def f(\u{e4}=1):\n    pass\n")?;
    let output = Command::new(binary()).arg(&path).output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains(":1:9: NOD001"), "{stdout}");
    let caret = stdout
        .lines()
        .find(|line| line.contains('^'))
        .ok_or("expected a caret line")?;
    let source = stdout
        .lines()
        .find(|line| line.contains("def f("))
        .ok_or("expected a source line")?;
    // Both lines carry the same `N | ` gutter, so counting characters into
    // each of them says which character the caret points at. It must be the
    // default, not the `)` a byte column would land on.
    let column = caret
        .chars()
        .position(|character| character == '^')
        .ok_or("expected a caret")?;
    assert_eq!(source.chars().nth(column), Some('1'), "{stdout}");
    Ok(())
}

#[test]
fn a_byte_order_mark_does_not_shift_the_first_line() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("bom.py");
    std::fs::write(&path, "\u{feff}def f(x=1):\n    pass\n")?;
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(&path)
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains(":1:9: NOD001"), "{stdout}");
    Ok(())
}

#[test]
fn fixing_a_file_with_a_byte_order_mark_keeps_it() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("bom.py");
    std::fs::write(&path, "\u{feff}def f(x=1):\n    pass\n\n\nf()\n")?;
    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    // The mark survives, and the edits landed where the source said they would
    // rather than three bytes off.
    assert_eq!(
        std::fs::read_to_string(&path)?,
        "\u{feff}def f(x):\n    pass\n\n\nf(x=1)\n"
    );
    Ok(())
}

#[test]
fn one_unparseable_file_does_not_hide_the_rest() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("good.py"), "def f(x=1): pass\n")?;
    std::fs::write(directory.path().join("bad.py"), "def broken(:\n")?;
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("bad.py:1:12: NOD000 syntax error"),
        "{stdout}"
    );
    assert!(stdout.contains("good.py:1:9: NOD001"), "{stdout}");
    assert!(stdout.contains("Found 2 errors."), "{stdout}");
    Ok(())
}

#[test]
fn a_syntax_error_does_not_stop_the_rest_being_fixed() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let good = directory.path().join("good.py");
    let bad = directory.path().join("bad.py");
    std::fs::write(&good, "def f(x=1): pass\n\nf()\n")?;
    std::fs::write(&bad, "def broken(:\n")?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    let stdout = String::from_utf8(output.stdout)?;
    // The syntax error carries no fix, so it is still there afterwards and the
    // summary and exit status say so.
    assert!(
        stdout.contains("Found 2 errors (1 fixed, 1 remaining)."),
        "{stdout}"
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        std::fs::read_to_string(&good)?,
        "def f(x): pass\n\nf(x=1)\n"
    );
    assert_eq!(std::fs::read_to_string(&bad)?, "def broken(:\n");
    Ok(())
}

#[test]
fn a_syntax_error_is_reported_in_json() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("bad.py"), "def broken(:\n")?;
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("json")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let diagnostics: serde_json::Value = serde_json::from_str(&String::from_utf8(output.stdout)?)?;
    let diagnostics = diagnostics.as_array().ok_or("expected JSON array")?;
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["code"], "NOD000");
    Ok(())
}

#[test]
fn fixing_honours_the_json_output_format() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(&path, "def f(value=1): pass\n\nf()\n")?;
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("json")
        .arg("--fix")
        .arg(&path)
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    // Nothing but the diagnostics goes to stdout, so a CI job can parse it.
    let diagnostics: serde_json::Value = serde_json::from_str(&String::from_utf8(output.stdout)?)?;
    let diagnostics = diagnostics.as_array().ok_or("expected JSON array")?;
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["code"], "NOD001");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("Updated 1 call site."), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(&path)?,
        "def f(value): pass\n\nf(value=1)\n"
    );
    Ok(())
}

#[test]
fn fixing_honours_the_github_output_format() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(&path, "def f(value=1): pass\n")?;
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("github")
        .arg("--fix")
        .arg(&path)
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.starts_with("::error file="), "{stdout}");
    assert!(stdout.contains("title=NOD001::"), "{stdout}");
    assert!(!stdout.contains("Found 1 error"), "{stdout}");
    Ok(())
}

#[test]
fn fixing_keeps_the_summary_in_the_text_formats() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(&path, "def f(value=1): pass\n")?;
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg("--fix")
        .arg(&path)
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("Found 1 error (1 fixed, 0 remaining)."),
        "{stdout}"
    );
    assert!(stdout.contains("Updated 0 call sites."), "{stdout}");
    Ok(())
}
