use std::process::{Command, Stdio};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_no-defaults")
}

#[test]
fn a_closed_stdout_pipe_is_a_normal_termination() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    let violations = "def f(value=1): pass\n".repeat(10_000);
    std::fs::write(&path, violations)?;
    let mut child = Command::new(binary())
        .arg(&path)
        .stdout(Stdio::piped())
        .spawn()?;
    drop(child.stdout.take());
    assert_eq!(child.wait()?.code(), Some(0));
    Ok(())
}

#[test]
fn bare_carriage_returns_are_line_endings() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "def first(value=1): pass\rdef second(value=2): pass\r",
    )?;

    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(&path)
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("example.py:1:17: NOD001"), "{stdout}");
    assert!(stdout.contains("example.py:2:18: NOD001"), "{stdout}");

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "def first(value): pass\rdef second(value): pass\r"
    );
    Ok(())
}

#[test]
fn dataclass_aliases_imported_in_module_loops_are_recognized(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "for _ in [0]:\n    from dataclasses import dataclass as dc\n\n@dc\nclass C:\n    value: int = 1\n\nC()\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "for _ in [0]:\n    from dataclasses import dataclass as dc\n\n@dc\nclass C:\n    value: int\n\nC(value=1)\n"
    );
    Ok(())
}

#[test]
fn show_settings_rejects_mutating_modes() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(&path, "def target(value=1): pass\n")?;

    for mode in ["--fix", "--diff"] {
        let output = Command::new(binary())
            .arg("--show-settings")
            .arg(mode)
            .arg(&path)
            .output()?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stderr.contains("cannot be used with"), "{stderr}");
    }
    assert_eq!(
        std::fs::read_to_string(path)?,
        "def target(value=1): pass\n"
    );
    Ok(())
}

#[test]
fn later_dataclass_alias_imports_do_not_change_earlier_classes(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "@dc\nclass C:\n    value: int = 1\n\nfrom dataclasses import dataclass as dc\n",
    )?;

    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(&path)
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn defaults_on_implicit_method_receivers_are_not_reinserted(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "class C:\n    @classmethod\n    def make(cls=None):\n        return cls\n\nC.make()\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "class C:\n    @classmethod\n    def make(cls):\n        return cls\n\nC.make()\n"
    );
    Ok(())
}

#[test]
fn aliased_classmethod_decorators_receive_the_class_implicitly(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from builtins import classmethod as cm\n\nclass C:\n    @cm\n    def parse(cls, value=1):\n        return value\n\nC.parse(5)\n",
    )?;
    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "from builtins import classmethod as cm\n\nclass C:\n    @cm\n    def parse(cls, value):\n        return value\n\nC.parse(5)\n"
    );
    Ok(())
}

#[test]
fn try_and_handler_imports_remain_conditional() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("api.py"),
        "def target(value=1): return value\n",
    )?;
    std::fs::write(
        directory.path().join("fallback.py"),
        "def target(): return 5\n",
    )?;
    let caller = directory.path().join("case.py");
    let source = "try:\n    import api as module\nexcept ImportError:\n    import fallback as module\n\nmodule.target()\n";
    std::fs::write(&caller, source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(std::fs::read_to_string(caller)?, source);
    assert!(String::from_utf8(output.stderr)?.contains("cannot be tied to the definition"));
    Ok(())
}

#[test]
fn diff_rejects_machine_readable_output_formats() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(&path, "def target(value=1): pass\n")?;

    for format in ["json", "github"] {
        let output = Command::new(binary())
            .arg("--diff")
            .arg("--output-format")
            .arg(format)
            .arg(&path)
            .output()?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr)?;
        assert!(
            stderr.contains("--diff cannot be combined with a machine-readable --output-format"),
            "{stderr}"
        );
    }
    Ok(())
}

#[test]
fn unpacked_field_options_keep_defaults_out_of_constructor_fixes(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    let source = "from dataclasses import dataclass, field\n\n@dataclass\nclass C:\n    value: int = field(default=1, **{\"init\": False})\n\nC()\n";
    std::fs::write(&path, source)?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(path)?, source);
    Ok(())
}

#[test]
fn field_defaults_needed_by_later_deletes_are_retained() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    let source = "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    value: int = 1\n    del value\n\nC(1)\n";
    std::fs::write(&path, source)?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(path)?, source);
    Ok(())
}

#[test]
fn unpacked_dataclass_options_keep_field_defaults() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    let source = "from dataclasses import dataclass\n\nOPTIONS = {\"init\": False}\n\n@dataclass(**OPTIONS)\nclass C:\n    value: int = 1\n\nC()\n";
    std::fs::write(&path, source)?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(path)?, source);
    Ok(())
}

#[test]
fn function_defaults_resolve_before_body_bindings() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "def target(value=1):\n    return 5\n\ndef decorated(item=target()):  # noqa: NOD001\n    target = 5\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "def target(value):\n    return 5\n\ndef decorated(item=target(value=1)):  # noqa: NOD001\n    target = 5\n"
    );
    Ok(())
}

#[test]
fn unpacked_kw_only_field_options_do_not_produce_constructor_arguments(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass, field\n\n@dataclass\nclass C:\n    first: int = 0\n    value: int = field(default=1, **{\"kw_only\": True})\n\nC(5)\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "from dataclasses import dataclass, field\n\n@dataclass\nclass C:\n    first: int\n    value: int = field(default=1, **{\"kw_only\": True})\n\nC(5)\n"
    );
    Ok(())
}

#[test]
fn a_shadowed_object_base_is_not_treated_as_builtin() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass\n\nclass Base:\n    inherited: int = 1\n\nobject = Base\n\n@dataclass\nclass C(object):\n    own: int = 2\n\nC()\n",
    )?;
    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let fixed = std::fs::read_to_string(path)?;
    assert!(fixed.contains("own: int\n"), "{fixed}");
    assert!(fixed.ends_with("C()\n"), "{fixed}");
    assert!(String::from_utf8(output.stderr)?.contains("inherits fields"));
    Ok(())
}

#[test]
fn double_leading_underscore_names_are_private_but_dunders_are_not(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        directory.path().join("pyproject.toml"),
        "[tool.no_defaults]\nprivate_only = true\n",
    )?;
    std::fs::write(
        &path,
        "def __secret(value=1): pass\ndef __protocol__(value=2): pass\n",
    )?;

    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(&path)
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("function `__secret`"), "{stdout}");
    assert!(!stdout.contains("function `__protocol__`"), "{stdout}");
    Ok(())
}

#[test]
fn class_base_calls_resolve_before_class_body_bindings() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "def target(value=1):\n    return object\n\nclass C(target()):\n    target = 5\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "def target(value):\n    return object\n\nclass C(target(value=1)):\n    target = 5\n"
    );
    Ok(())
}

#[test]
fn metaclass_keyword_calls_resolve_before_class_body_bindings(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "def target(value=1):\n    return type\n\nclass C(metaclass=target()):\n    target = 5\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "def target(value):\n    return type\n\nclass C(metaclass=target(value=1)):\n    target = 5\n"
    );
    Ok(())
}

#[test]
fn full_diagnostics_escape_terminal_control_characters() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(&path, "def target(value=1): pass  # \u{1b}[31mred\n")?;

    let output = Command::new(binary()).arg(&path).output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(!stdout.contains('\u{1b}'), "{stdout:?}");
    assert!(stdout.contains(r"\u{1b}[31mred"), "{stdout:?}");
    Ok(())
}

#[test]
fn custom_field_calls_are_removed_whole_instead_of_surgically_edited(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass\n\ndef field(**kwargs):\n    return 7\n\n@dataclass\nclass C:\n    value: int = field(default=1, metadata=2)\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let fixed = std::fs::read_to_string(path)?;
    assert!(fixed.contains("value: int\n"), "{fixed}");
    assert!(!fixed.contains("field(metadata=2)"), "{fixed}");
    Ok(())
}

#[test]
fn assigning_over_dataclasses_field_retains_the_replacement_call(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass, field\ndef replacement(**kwargs): return kwargs.get('default', 0)\nfield = replacement\n@dataclass\nclass C:\n    value: int = field(default=1, compare=False)\nassert C().value == 1\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(1));
    let fixed = std::fs::read_to_string(path)?;
    assert!(
        fixed.contains("value: int = field(default=1, compare=False)"),
        "{fixed}"
    );
    Ok(())
}

#[test]
fn custom_field_helpers_do_not_use_pydantic_argument_surgery(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from pydantic import BaseModel\n\ndef Field(**kwargs):\n    return 9\n\nclass C(BaseModel):\n    value: int = Field(default=1, note=2)\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let fixed = std::fs::read_to_string(path)?;
    assert!(fixed.contains("value: int\n"), "{fixed}");
    assert!(!fixed.contains("Field(note=2)"), "{fixed}");
    Ok(())
}

#[test]
fn a_user_defined_private_attr_is_a_configured_model_field(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("pyproject.toml"),
        "[tool.no_defaults]\nfield_base_classes = ['Model']\n",
    )?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "def PrivateAttr(default): return default\nclass Model: pass\nclass C(Model):\n    value: int = PrivateAttr(1)\nassert C.value == 1\n",
    )?;

    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(&path)
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stdout)?.contains("NOD001"));
    Ok(())
}

#[test]
fn assigning_over_pydantic_field_retains_the_replacement_call(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("pydantic.py"),
        "class BaseModel: pass\ndef Field(**kwargs): return kwargs.get('default', 0)\n",
    )?;
    let path = directory.path().join("example.py");
    let source = "from pydantic import BaseModel, Field\ndef replacement(**kwargs): return kwargs.get('default', 0)\nField = replacement\nclass C(BaseModel):\n    value: int = Field(default=1, description='kept')\nassert C.value == 1\n";
    std::fs::write(&path, source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(path)?, source);
    Ok(())
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
fn non_utf8_source_does_not_abort_other_files() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let latin1 = directory.path().join("latin1.py");
    let utf8 = directory.path().join("utf8.py");
    std::fs::write(
        &latin1,
        b"# coding: latin-1\ndef target(caf\xe9=1): return caf\xe9\n",
    )?;
    std::fs::write(&utf8, "def visible(value=1): pass\n")?;

    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("latin1.py:1:1: NOD000"), "{stdout}");
    assert!(stdout.contains("utf8.py:1:19: NOD001"), "{stdout}");
    assert!(String::from_utf8(output.stderr)?.is_empty());
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
    assert!(!stdout.contains("--- a//"), "{stdout}");
    assert!(!stdout.contains("+++ b//"), "{stdout}");
    assert!(stdout.contains("-def f(value=1): pass"));
    assert!(stdout.contains("+def f(value): pass"));
    assert_eq!(std::fs::read_to_string(path)?, "def f(value=1): pass\n");
    Ok(())
}

#[test]
fn diff_reports_diagnostics_without_edits() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let broken = directory.path().join("bad.py");
    let stub = directory.path().join("api.pyi");
    std::fs::write(&broken, "def broken(:\n")?;
    std::fs::write(&stub, "def f(x: int = ...) -> None: ...\n")?;
    let output = Command::new(binary())
        .arg("--diff")
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
    assert!(stdout.contains("api.pyi:1:16: NOD001"), "{stdout}");
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
fn imports_in_false_branches_are_not_reexports() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("package");
    std::fs::create_dir_all(&package)?;
    std::fs::write(
        directory.path().join("pyproject.toml"),
        "[tool.no_defaults]\nprivate_only = true\nrespect_reexports = true\n",
    )?;
    std::fs::write(
        package.join("__init__.py"),
        "if False:\n    from ._api import target\n",
    )?;
    std::fs::write(package.join("_api.py"), "def target(value=1): pass\n")?;

    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stdout)?.contains("function `target`"));
    Ok(())
}

#[test]
fn assignment_aliases_are_public_reexports() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("package");
    std::fs::create_dir_all(&package)?;
    std::fs::write(
        directory.path().join("pyproject.toml"),
        "[tool.no_defaults]\nprivate_only = true\nrespect_reexports = true\n",
    )?;
    std::fs::write(
        package.join("__init__.py"),
        "from . import _api\ntarget = _api.target\n__all__ = [\"target\"]\n",
    )?;
    std::fs::write(package.join("_api.py"), "def target(value=1): pass\n")?;

    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn overwritten_imports_are_not_public_reexports() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("package");
    std::fs::create_dir_all(&package)?;
    std::fs::write(
        directory.path().join("pyproject.toml"),
        "[tool.no_defaults]\nprivate_only = true\nrespect_reexports = true\n",
    )?;
    std::fs::write(
        package.join("__init__.py"),
        "from ._api import target\ntarget = 5\n",
    )?;
    std::fs::write(package.join("_api.py"), "def target(value=1): pass\n")?;

    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stdout)?.contains("function `target`"));
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

#[cfg(unix)]
#[test]
fn a_directory_walk_error_is_an_operational_failure() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir()?;
    let locked = directory.path().join("locked");
    std::fs::create_dir(&locked)?;
    std::fs::write(locked.join("hidden.py"), "def f(value=1): pass\n")?;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))?;
    let output = Command::new(binary()).arg(directory.path()).output();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))?;
    let output = output?;
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("could not walk"), "{stderr}");
    assert!(stderr.contains("locked"), "{stderr}");
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
fn a_directory_walk_checks_symlinked_python_files() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let tree = directory.path().join("tree");
    std::fs::create_dir(&tree)?;
    std::fs::write(
        directory.path().join("real.py"),
        "def target(value=1): pass\n",
    )?;
    std::os::unix::fs::symlink("../real.py", tree.join("linked.py"))?;

    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(&tree)
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("linked.py:1:18: NOD001"), "{stdout}");
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
fn full_output_carets_account_for_tabs_and_wide_characters(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("display.py");
    std::fs::write(
        &path,
        "class C:\n\tdef target(value=1): ...\n\ndef target(界=1): ...\n",
    )?;
    let output = Command::new(binary()).arg(&path).output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    let caret_lines = stdout
        .lines()
        .filter(|line| line.contains('^'))
        .collect::<Vec<_>>();
    assert_eq!(caret_lines.len(), 2, "{stdout}");
    assert_eq!(caret_lines[0], format!("  | {}^", " ".repeat(21)));
    assert_eq!(caret_lines[1], format!("  | {}^", " ".repeat(14)));
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
    assert!(
        stdout.contains("bad.py:1:12: NOD000 syntax error"),
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
fn text_fix_reports_an_unfixable_stub_default() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let stub = directory.path().join("api.pyi");
    std::fs::write(&stub, "def f(x: int = ...) -> None: ...\n")?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg("--output-format")
        .arg("concise")
        .arg(&stub)
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("api.pyi:1:16: NOD001"), "{stdout}");
    assert!(
        stdout.contains("Found 1 error (0 fixed, 1 remaining)."),
        "{stdout}"
    );
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

/// One call needing both a positional and a keyword insertion is one call
/// site, not two.
#[test]
fn one_call_needing_two_insertions_counts_once() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    // `b` is positional-only, so it goes in ahead of the existing keyword,
    // while `d` is appended after it.
    std::fs::write(&path, "def f(a, b=1, /, *, c=2, d=3): pass\n\nf(1, c=9)\n")?;
    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("Updated 1 call site."), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(&path)?,
        "def f(a, b, /, *, c, d): pass\n\nf(1, 1, c=9, d=3)\n"
    );
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

#[test]
fn an_ellipsis_default_in_a_stub_is_reported_but_not_fixed(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let stub = directory.path().join("stub.pyi");
    let source = "from typing import overload\n\n\
                  @overload\ndef f(x: int = ...) -> int: ...\n\
                  @overload\ndef f(x: str = ...) -> str: ...\n\
                  def g(y: int = 5) -> int: ...\n";
    std::fs::write(&stub, source)?;
    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(&stub)
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout)?;
    assert_eq!(stdout.matches("NOD001").count(), 3, "{stdout}");

    let output = Command::new(binary()).arg("--fix").arg(&stub).output()?;
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains("Found 3 errors (0 fixed, 3 remaining)."),
        "{stdout}"
    );
    assert_eq!(output.status.code(), Some(1));
    // Every stub default describes an optional runtime parameter. None can be
    // removed without changing the implementation the stub advertises.
    assert_eq!(std::fs::read_to_string(&stub)?, source);
    Ok(())
}

#[test]
fn an_ellipsis_default_outside_a_stub_is_still_fixed() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("module.py");
    std::fs::write(&path, "def f(x: int = ...) -> int: ...\n")?;
    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&path)?,
        "def f(x: int) -> int: ...\n"
    );
    Ok(())
}

#[test]
fn fix_updates_calls_reached_through_a_relative_import() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("pkg");
    let nested = package.join("sub");
    std::fs::create_dir_all(&nested)?;
    std::fs::write(package.join("__init__.py"), "")?;
    std::fs::write(nested.join("__init__.py"), "")?;
    std::fs::write(
        package.join("api.py"),
        "def connect(host, timeout=30): return host\n",
    )?;
    std::fs::write(
        nested.join("tool.py"),
        "def helper(a, size=8192): return a\n",
    )?;
    // Three ways of naming a sibling module. Only the middle one used to
    // resolve; `from . import api` bound `api` as a symbol of the package.
    let relative = package.join("use.py");
    let absolute = package.join("use2.py");
    let descending = package.join("use3.py");
    std::fs::write(&relative, "from . import api\napi.connect(\"h\")\n")?;
    std::fs::write(&absolute, "from pkg import api\napi.connect(\"h\")\n")?;
    std::fs::write(&descending, "from .sub import tool\ntool.helper(1)\n")?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(!stderr.contains("left the call"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(&relative)?,
        "from . import api\napi.connect(\"h\", timeout=30)\n"
    );
    assert_eq!(
        std::fs::read_to_string(&absolute)?,
        "from pkg import api\napi.connect(\"h\", timeout=30)\n"
    );
    assert_eq!(
        std::fs::read_to_string(&descending)?,
        "from .sub import tool\ntool.helper(1, size=8192)\n"
    );
    Ok(())
}

#[test]
fn an_unresolved_later_import_invalidates_an_earlier_binding(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(&api, "def target(value=1): return value\n")?;
    std::fs::write(
        &caller,
        "import api\nimport external as api\n\napi.target()\n",
    )?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        std::fs::read_to_string(&api)?,
        "def target(value=1): return value\n"
    );
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "import api\nimport external as api\n\napi.target()\n"
    );
    assert!(
        String::from_utf8(output.stderr)?.contains("cannot be tied to the definition"),
        "the unsafe call is surfaced rather than silently rewritten"
    );
    Ok(())
}

#[test]
fn fix_updates_calls_reached_through_an_unaliased_dotted_import(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("pkg");
    std::fs::create_dir(&package)?;
    std::fs::write(package.join("__init__.py"), "")?;
    let api = package.join("api.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(&api, "def target(value=1): return value\n")?;
    std::fs::write(&caller, "import pkg.api\n\npkg.api.target()\n")?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "import pkg.api\n\npkg.api.target(value=1)\n"
    );
    Ok(())
}

/// `import pkg.api` binds only `pkg`, so replacing that name replaces what
/// every `pkg.…` expression reaches.
#[test]
fn rebinding_a_package_name_drops_its_dotted_import() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("pkg");
    std::fs::create_dir(&package)?;
    std::fs::write(package.join("__init__.py"), "")?;
    std::fs::write(
        package.join("api.py"),
        "def target(value=1): return value\n",
    )?;
    let caller = directory.path().join("caller.py");
    let source = "import pkg.api\n\npkg = object()\n\npkg.api.target()\n";
    std::fs::write(&caller, source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(caller)?, source);
    assert_eq!(
        std::fs::read_to_string(package.join("api.py"))?,
        "def target(value=1): return value\n"
    );
    Ok(())
}

/// A loop or context-manager target at module scope replaces what an import
/// bound under that name. An `except … as` target does not: the name is deleted
/// when the handler ends, and if the handler never runs the import still binds.
#[test]
fn module_level_targets_replace_an_imported_name() -> Result<(), Box<dyn std::error::Error>> {
    for rebinding in [
        "for mod in [1]:\n    pass\n",
        "with open(\"f\") as mod:\n    pass\n",
    ] {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("mod.py"),
            "def target(value=1): return value\n",
        )?;
        let caller = directory.path().join("caller.py");
        let source = format!("import mod\n\n{rebinding}\nmod.target()\n");
        std::fs::write(&caller, &source)?;

        let output = Command::new(binary())
            .arg("--fix")
            .arg(directory.path())
            .output()?;
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(std::fs::read_to_string(&caller)?, source, "{rebinding}");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("mod.py"))?,
            "def target(value=1): return value\n",
            "{rebinding}"
        );
    }
    Ok(())
}

/// `except … as` does not replace a module-level import for later statements.
#[test]
fn except_as_does_not_replace_an_imported_name() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("mod.py"),
        "def target(value=1): return value\n",
    )?;
    let caller = directory.path().join("caller.py");
    std::fs::write(
        &caller,
        "import mod\n\ntry:\n    pass\nexcept Exception as mod:\n    pass\n\nmod.target()\n",
    )?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "import mod\n\ntry:\n    pass\nexcept Exception as mod:\n    pass\n\nmod.target(value=1)\n"
    );
    Ok(())
}

#[test]
fn from_import_resolves_a_namespace_package_module() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let namespace = directory.path().join("ns");
    std::fs::create_dir(&namespace)?;
    let api = namespace.join("api.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(&api, "def target(value=1): return value\n")?;
    std::fs::write(&caller, "from ns import api\n\napi.target()\n")?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "from ns import api\n\napi.target(value=1)\n"
    );
    Ok(())
}

#[test]
fn reassigning_an_imported_module_invalidates_its_binding() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(&api, "def target(value=1): return value\n")?;
    std::fs::write(
        &caller,
        "import api\n\nclass Other:\n    @staticmethod\n    def target(): return 5\n\napi = Other()\napi.target()\n",
    )?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(
        std::fs::read_to_string(caller)?.ends_with("api = Other()\napi.target()\n"),
        "the call through the reassigned receiver must stay unchanged"
    );
    assert!(String::from_utf8(output.stderr)?.contains("cannot be tied to the definition"));
    assert_eq!(
        std::fs::read_to_string(api)?,
        "def target(value=1): return value\n"
    );
    Ok(())
}

#[test]
fn reassigning_an_imported_symbol_invalidates_its_binding() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(&api, "def target(value=1): return value\n")?;
    std::fs::write(
        &caller,
        "from api import target\n\ntarget = lambda: 5\ntarget()\n",
    )?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(
        std::fs::read_to_string(caller)?.ends_with("target = lambda: 5\ntarget()\n"),
        "the call through the reassigned symbol must stay unchanged"
    );
    assert!(String::from_utf8(output.stderr)?.contains("cannot be tied to the definition"));
    assert_eq!(
        std::fs::read_to_string(api)?,
        "def target(value=1): return value\n"
    );
    Ok(())
}

#[test]
fn module_definitions_invalidate_imported_callable_bindings(
) -> Result<(), Box<dyn std::error::Error>> {
    for definition in ["def target(): return 9", "class target: pass"] {
        let directory = tempfile::tempdir()?;
        let api = directory.path().join("api.py");
        let caller = directory.path().join("caller.py");
        let api_source = "def target(value=1): return value\n";
        let caller_source = format!("from api import target\n{definition}\ntarget()\n");
        std::fs::write(&api, api_source)?;
        std::fs::write(&caller, &caller_source)?;

        let output = Command::new(binary())
            .arg("--fix")
            .arg(directory.path())
            .output()?;
        assert_eq!(output.status.code(), Some(1), "{definition}");
        assert_eq!(std::fs::read_to_string(&api)?, api_source, "{definition}");
        assert_eq!(
            std::fs::read_to_string(&caller)?,
            caller_source,
            "{definition}"
        );
    }
    Ok(())
}

#[test]
fn module_named_expressions_invalidate_imported_callable_bindings(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    let api_source = "def target(value=1): return value\n";
    let caller_source = "from api import target\n(target := lambda: 9)\nassert target() == 9\n";
    std::fs::write(&api, api_source)?;
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&api)?, api_source);
    assert_eq!(std::fs::read_to_string(&caller)?, caller_source);
    assert!(String::from_utf8(output.stderr)?.contains("cannot be tied to the definition"));
    Ok(())
}

#[test]
fn module_for_targets_invalidate_imported_bindings_inside_the_body(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    let api_source = "def target(value=1): return value\n";
    let caller_source =
        "from api import target\nfor target in [lambda: 9]:\n    assert target() == 9\n";
    std::fs::write(&api, api_source)?;
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&api)?, api_source);
    assert_eq!(std::fs::read_to_string(&caller)?, caller_source);
    Ok(())
}

#[test]
fn module_with_targets_invalidate_imported_bindings_inside_the_body(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    let api_source = "def target(value=1): return value\n";
    let caller_source = "from api import target\nclass Context:\n    def __enter__(self): return lambda: 9\n    def __exit__(self, *args): pass\nwith Context() as target:\n    assert target() == 9\n";
    std::fs::write(&api, api_source)?;
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&api)?, api_source);
    assert_eq!(std::fs::read_to_string(&caller)?, caller_source);
    Ok(())
}

#[test]
fn module_except_targets_invalidate_imported_bindings_inside_the_handler(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    let api_source = "def target(value=1): return value\n";
    let caller_source = "from api import target\nclass Error(Exception):\n    def __call__(self): return 9\ntry:\n    raise Error()\nexcept Error as target:\n    assert target() == 9\n";
    std::fs::write(&api, api_source)?;
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&api)?, api_source);
    assert_eq!(std::fs::read_to_string(&caller)?, caller_source);
    Ok(())
}

#[test]
fn unreachable_if_imports_do_not_replace_live_bindings() -> Result<(), Box<dyn std::error::Error>> {
    for conditional in [
        "if False:\n    from other import target",
        "if True:\n    pass\nelse:\n    from other import target",
    ] {
        let directory = tempfile::tempdir()?;
        let api = directory.path().join("api.py");
        let other = directory.path().join("other.py");
        let caller = directory.path().join("caller.py");
        std::fs::write(&api, "def target(value=1): return value\n")?;
        std::fs::write(&other, "def target(value=2): return value\n")?;
        let caller_source =
            format!("from api import target\n{conditional}\nassert target() == 1\n");
        std::fs::write(&caller, &caller_source)?;

        let output = Command::new(binary())
            .arg("--fix")
            .arg(directory.path())
            .output()?;
        assert_eq!(output.status.code(), Some(0), "{conditional}");
        assert_eq!(
            std::fs::read_to_string(caller)?,
            format!("from api import target\n{conditional}\nassert target(value=1) == 1\n"),
            "{conditional}"
        );
    }
    Ok(())
}

#[test]
fn imports_in_unentered_exception_handlers_do_not_replace_live_bindings(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let other = directory.path().join("other.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(&api, "def target(value=1): return value\n")?;
    std::fs::write(&other, "def target(value=2): return value\n")?;
    let caller_source = "from api import target\ntry:\n    pass\nexcept Exception:\n    from other import target\nassert target() == 1\n";
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "from api import target\ntry:\n    pass\nexcept Exception:\n    from other import target\nassert target(value=1) == 1\n"
    );
    Ok(())
}

#[test]
fn imports_in_empty_for_bodies_do_not_replace_live_bindings(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let other = directory.path().join("other.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(&api, "def target(value=1): return value\n")?;
    std::fs::write(&other, "def target(value=2): return value\n")?;
    let caller_source = "from api import target\nfor _ in []:\n    from other import target\nassert target() == 1\n";
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "from api import target\nfor _ in []:\n    from other import target\nassert target(value=1) == 1\n"
    );
    Ok(())
}

#[test]
fn imports_in_for_else_skipped_by_break_do_not_replace_live_bindings(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let other = directory.path().join("other.py");
    let third = directory.path().join("third.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(&api, "def target(value=1): return value\n")?;
    std::fs::write(&other, "def target(value=2): return value\n")?;
    std::fs::write(&third, "def target(value=3): return value\n")?;
    let caller_source = "from api import target\nfor _ in [1]:\n    from other import target\n    break\nelse:\n    from third import target\nassert target() == 2\n";
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "from api import target\nfor _ in [1]:\n    from other import target\n    break\nelse:\n    from third import target\nassert target(value=2) == 2\n"
    );
    Ok(())
}

#[test]
fn imports_in_while_false_bodies_do_not_replace_live_bindings(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let other = directory.path().join("other.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(&api, "def target(value=1): return value\n")?;
    std::fs::write(&other, "def target(value=2): return value\n")?;
    let caller_source = "from api import target\nwhile False:\n    from other import target\nassert target() == 1\n";
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "from api import target\nwhile False:\n    from other import target\nassert target(value=1) == 1\n"
    );
    Ok(())
}

#[test]
fn imports_in_while_else_skipped_by_break_do_not_replace_live_bindings(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let other = directory.path().join("other.py");
    let third = directory.path().join("third.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(&api, "def target(value=1): return value\n")?;
    std::fs::write(&other, "def target(value=2): return value\n")?;
    std::fs::write(&third, "def target(value=3): return value\n")?;
    let caller_source = "from api import target\nwhile True:\n    from other import target\n    break\nelse:\n    from third import target\nassert target() == 2\n";
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "from api import target\nwhile True:\n    from other import target\n    break\nelse:\n    from third import target\nassert target(value=2) == 2\n"
    );
    Ok(())
}

#[test]
fn imports_in_unselected_match_cases_do_not_replace_live_bindings(
) -> Result<(), Box<dyn std::error::Error>> {
    for cases in [
        "    case 1:\n        pass\n    case _:\n        from other import target",
        "    case 2:\n        from other import target",
    ] {
        let directory = tempfile::tempdir()?;
        let api = directory.path().join("api.py");
        let other = directory.path().join("other.py");
        let caller = directory.path().join("caller.py");
        std::fs::write(&api, "def target(value=1): return value\n")?;
        std::fs::write(&other, "def target(value=2): return value\n")?;
        let caller_source =
            format!("from api import target\nmatch 1:\n{cases}\nassert target() == 1\n");
        std::fs::write(&caller, &caller_source)?;

        let output = Command::new(binary())
            .arg("--fix")
            .arg(directory.path())
            .output()?;
        assert_eq!(output.status.code(), Some(0), "{cases}");
        assert_eq!(
            std::fs::read_to_string(caller)?,
            format!("from api import target\nmatch 1:\n{cases}\nassert target(value=1) == 1\n"),
            "{cases}"
        );
    }
    Ok(())
}

#[test]
fn module_match_captures_invalidate_imported_callable_bindings(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    let api_source = "def target(value=1): return value\n";
    let caller_source = "from api import target\nmatch (lambda: 9):\n    case target:\n        assert target() == 9\n";
    std::fs::write(&api, api_source)?;
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&api)?, api_source);
    assert_eq!(std::fs::read_to_string(&caller)?, caller_source);
    Ok(())
}

#[test]
fn unresolved_from_imports_invalidate_earlier_checked_bindings(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let external = directory.path().join("external.py");
    let caller = directory.path().join("caller.py");
    let api_source = "def target(value=1): return value\n";
    let caller_source =
        "from api import target\nfrom external import target\nassert target() == 9\n";
    std::fs::write(&api, api_source)?;
    std::fs::write(&external, "def target(): return 9\n")?;
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(&api)
        .arg(&caller)
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&api)?, api_source);
    assert_eq!(std::fs::read_to_string(&caller)?, caller_source);
    Ok(())
}

#[test]
fn unresolved_dotted_imports_invalidate_the_top_level_binding(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let external = directory.path().join("external");
    let caller = directory.path().join("caller.py");
    std::fs::create_dir(&external)?;
    let api_source = "def target(value=1): return value\n";
    let caller_source =
        "import api as external\nimport external.sub\nassert external.target() == 9\n";
    std::fs::write(&api, api_source)?;
    std::fs::write(external.join("__init__.py"), "def target(): return 9\n")?;
    std::fs::write(external.join("sub.py"), "")?;
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(&api)
        .arg(&caller)
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&api)?, api_source);
    assert_eq!(std::fs::read_to_string(&caller)?, caller_source);
    Ok(())
}

#[test]
fn class_for_targets_shadow_imported_callables_inside_the_body(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    let api_source = "def target(value=1): return value\n";
    let caller_source = "from api import target\nclass C:\n    for target in [lambda: 9]:\n        result = target()\nassert C.result == 9\n";
    std::fs::write(&api, api_source)?;
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&api)?, api_source);
    assert_eq!(std::fs::read_to_string(&caller)?, caller_source);
    Ok(())
}

#[test]
fn class_with_targets_shadow_imported_callables_inside_the_body(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    let api_source = "def target(value=1): return value\n";
    let caller_source = "from api import target\nclass Manager:\n    def __enter__(self): return lambda: 9\n    def __exit__(self, *args): pass\nclass C:\n    with Manager() as target:\n        result = target()\nassert C.result == 9\n";
    std::fs::write(&api, api_source)?;
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&api)?, api_source);
    assert_eq!(std::fs::read_to_string(&caller)?, caller_source);
    Ok(())
}

#[test]
fn class_except_targets_shadow_imported_callables_inside_the_handler(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    let api_source = "def target(value=1): return value\n";
    let caller_source = "from api import target\nclass Error(Exception):\n    def __call__(self): return 9\nclass C:\n    try:\n        raise Error()\n    except Error as target:\n        result = target()\nassert C.result == 9\n";
    std::fs::write(&api, api_source)?;
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&api)?, api_source);
    assert_eq!(std::fs::read_to_string(&caller)?, caller_source);
    Ok(())
}

#[test]
fn class_named_expressions_shadow_imported_callables_after_binding(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    let api_source = "def target(value=1): return value\n";
    let caller_source = "from api import target\nclass C:\n    (target := lambda: 9)\n    result = target()\nassert C.result == 9\n";
    std::fs::write(&api, api_source)?;
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&api)?, api_source);
    assert_eq!(std::fs::read_to_string(&caller)?, caller_source);
    Ok(())
}

#[test]
fn class_match_captures_shadow_imported_callables_inside_the_case(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    let api_source = "def target(value=1): return value\n";
    let caller_source = "from api import target\nclass C:\n    match (lambda: 9):\n        case target:\n            result = target()\nassert C.result == 9\n";
    std::fs::write(&api, api_source)?;
    std::fs::write(&caller, caller_source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&api)?, api_source);
    assert_eq!(std::fs::read_to_string(&caller)?, caller_source);
    Ok(())
}

#[test]
fn deleting_a_class_local_shadow_restores_the_global_callable(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(&api, "def target(value=1): return value\n")?;
    std::fs::write(
        &caller,
        "from api import target\nclass C:\n    target = lambda: 9\n    del target\n    result = target()\nassert C.result == 1\n",
    )?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(api)?,
        "def target(value): return value\n"
    );
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "from api import target\nclass C:\n    target = lambda: 9\n    del target\n    result = target(value=1)\nassert C.result == 1\n"
    );
    Ok(())
}

#[test]
fn class_local_import_calls_receive_removed_defaults() -> Result<(), Box<dyn std::error::Error>> {
    for body in [
        "    from api import target\n    result = target()",
        "    import api\n    result = api.target()",
    ] {
        let directory = tempfile::tempdir()?;
        let api = directory.path().join("api.py");
        let caller = directory.path().join("caller.py");
        std::fs::write(&api, "def target(value=1): return value\n")?;
        let caller_source = format!("class C:\n{body}\nassert C.result == 1\n");
        std::fs::write(&caller, &caller_source)?;

        let output = Command::new(binary())
            .arg("--fix")
            .arg(directory.path())
            .output()?;
        assert_eq!(output.status.code(), Some(0), "{body}");
        assert_eq!(
            std::fs::read_to_string(&api)?,
            "def target(value): return value\n",
            "{body}"
        );
        assert_eq!(
            std::fs::read_to_string(caller)?,
            format!(
                "class C:\n{}\nassert C.result == 1\n",
                body.replace("target()", "target(value=1)")
            ),
            "{body}"
        );
    }
    Ok(())
}

#[test]
fn function_local_imports_do_not_replace_module_bindings() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let other = directory.path().join("other.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(&api, "def target(value=1): return value\n")?;
    std::fs::write(&other, "def target(): return 5\n")?;
    std::fs::write(
        &caller,
        "import other as api\n\ndef load():\n    import api\n    return api.target()\n\napi.target()\n",
    )?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "import other as api\n\ndef load():\n    import api\n    return api.target(value=1)\n\napi.target()\n"
    );
    Ok(())
}

#[test]
fn a_single_component_import_resolves_only_at_the_importers_root(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let nested = directory.path().join("deep").join("nested");
    std::fs::create_dir_all(&nested)?;
    // `import utils` does not reach `deep/nested/utils.py` under any sys.path
    // this tree implies, so the call must be left alone rather than given a
    // keyword the callable it really reaches does not accept.
    std::fs::write(
        nested.join("utils.py"),
        "def helper(a, size=8192): return a\n",
    )?;
    let main = directory.path().join("main.py");
    std::fs::write(&main, "from utils import helper\nhelper(1)\n")?;
    let before = std::fs::read_to_string(&main)?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("cannot be tied to the definition that was fixed"),
        "{stderr}"
    );
    assert_eq!(std::fs::read_to_string(&main)?, before);
    assert_eq!(
        std::fs::read_to_string(nested.join("utils.py"))?,
        "def helper(a, size=8192): return a\n"
    );
    Ok(())
}

#[test]
fn a_single_component_import_at_the_importers_root_still_resolves(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("utils.py"),
        "def helper(a, size=8192): return a\n",
    )?;
    let main = directory.path().join("main.py");
    std::fs::write(&main, "from utils import helper\nhelper(1)\n")?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&main)?,
        "from utils import helper\nhelper(1, size=8192)\n"
    );
    Ok(())
}

#[test]
fn pythonpath_resolves_a_single_component_import_from_a_package_directory(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("pkg");
    std::fs::create_dir(&package)?;
    let api = package.join("api.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(&api, "def target(value=1): return value\n")?;
    std::fs::write(&caller, "import api\napi.target()\n")?;

    let output = Command::new(binary())
        .env("PYTHONPATH", &package)
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "import api\napi.target(value=1)\n"
    );
    Ok(())
}

#[test]
fn an_import_two_roots_disagree_about_is_left_alone() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let elsewhere = directory.path().join("elsewhere");
    std::fs::create_dir(&elsewhere)?;
    // Both roots hold an `api`: the importer's own directory, and the one
    // named on PYTHONPATH. Which of them Python would import depends on the
    // entry script, so the call has to be left alone.
    std::fs::write(
        directory.path().join("api.py"),
        "def target(value=1): return value\n",
    )?;
    std::fs::write(
        elsewhere.join("api.py"),
        "def target(other=2): return other\n",
    )?;
    let caller = directory.path().join("caller.py");
    std::fs::write(&caller, "import api\napi.target()\n")?;

    let output = Command::new(binary())
        .env("PYTHONPATH", &elsewhere)
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        std::fs::read_to_string(&caller)?,
        "import api\napi.target()\n",
        "neither definition can be shown to be the one that runs"
    );
    assert!(
        String::from_utf8(output.stderr)?.contains("cannot be tied to the definition"),
        "the call is reported rather than silently skipped"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("api.py"))?,
        "def target(value=1): return value\n"
    );
    assert_eq!(
        std::fs::read_to_string(elsewhere.join("api.py"))?,
        "def target(other=2): return other\n"
    );
    Ok(())
}

#[test]
fn a_dotted_import_still_reaches_another_source_root() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("src").join("pkg");
    std::fs::create_dir_all(&package)?;
    std::fs::create_dir_all(directory.path().join("tests"))?;
    std::fs::write(package.join("__init__.py"), "")?;
    std::fs::write(
        package.join("api.py"),
        "def connect(host, timeout=30): return host\n",
    )?;
    // A src layout: the test's own import root is `tests/`, so this only
    // resolves through the suffix match that two components still allow.
    let test = directory.path().join("tests").join("test_api.py");
    std::fs::write(&test, "from pkg.api import connect\nconnect(\"h\")\n")?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&test)?,
        "from pkg.api import connect\nconnect(\"h\", timeout=30)\n"
    );
    Ok(())
}

#[test]
fn a_top_level_import_does_not_resolve_inside_its_own_package(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("pkg");
    std::fs::create_dir_all(&package)?;
    std::fs::write(package.join("__init__.py"), "")?;
    // Python has no implicit relative imports, so `import utils` inside `pkg`
    // reaches the root `utils`, not `pkg/utils.py`.
    std::fs::write(package.join("utils.py"), "def other(): pass\n")?;
    std::fs::write(
        directory.path().join("utils.py"),
        "def helper(a, size=8192): return a\n",
    )?;
    let module = package.join("mod.py");
    std::fs::write(&module, "from utils import helper\nhelper(1)\n")?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&module)?,
        "from utils import helper\nhelper(1, size=8192)\n"
    );
    Ok(())
}

#[test]
fn an_import_two_roots_could_answer_is_left_alone() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("pkg");
    std::fs::create_dir_all(&package)?;
    // Neither directory is a package, so either could be the one on sys.path.
    std::fs::write(
        directory.path().join("utils.py"),
        "def helper(a, size=8192): return a\n",
    )?;
    std::fs::write(
        package.join("utils.py"),
        "def helper(a, size=4096): return a\n",
    )?;
    let module = package.join("mod.py");
    std::fs::write(&module, "from utils import helper\nhelper(1)\n")?;
    let before = std::fs::read_to_string(&module)?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("cannot be tied to the definition that was fixed"),
        "{stderr}"
    );
    assert_eq!(std::fs::read_to_string(&module)?, before);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("utils.py"))?,
        "def helper(a, size=8192): return a\n"
    );
    assert_eq!(
        std::fs::read_to_string(package.join("utils.py"))?,
        "def helper(a, size=4096): return a\n"
    );
    Ok(())
}

#[test]
fn a_namespace_package_still_sees_a_module_at_the_root() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("pkg");
    std::fs::create_dir_all(&package)?;
    std::fs::write(
        directory.path().join("utils.py"),
        "def helper(a, size=8192): return a\n",
    )?;
    let module = package.join("mod.py");
    std::fs::write(&module, "from utils import helper\nhelper(1)\n")?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&module)?,
        "from utils import helper\nhelper(1, size=8192)\n"
    );
    Ok(())
}

#[test]
fn skipped_calls_are_reported_even_with_nothing_removed() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    // The default is not a literal, so it is removed from the signature but
    // the call cannot be completed. The warning about that call must not
    // depend on how the removed-defaults count came out.
    std::fs::write(
        &path,
        "SENTINEL = object()\n\n\ndef keep(value=SENTINEL): return value\n\n\nkeep()\n",
    )?;
    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("is not a literal"), "{stderr}");
    Ok(())
}

#[test]
fn conditional_imports_are_not_resolved_by_traversal_order(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("api.py"),
        "def target(value=1): return value\n",
    )?;
    std::fs::write(
        directory.path().join("other.py"),
        "def target(): return 5\n",
    )?;
    let caller = directory.path().join("caller.py");
    let source = "flag = True\nif flag:\n    import api as module\nelse:\n    import other as module\n\nmodule.target()\n";
    std::fs::write(&caller, source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(std::fs::read_to_string(caller)?, source);
    assert!(String::from_utf8(output.stderr)?.contains("cannot be tied to the definition"));
    Ok(())
}

#[test]
fn imports_inside_module_loops_resolve_calls() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("api.py"),
        "def target(value=1): return value\n",
    )?;
    let caller = directory.path().join("caller.py");
    std::fs::write(&caller, "for _ in [0]:\n    import api\n\napi.target()\n")?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "for _ in [0]:\n    import api\n\napi.target(value=1)\n"
    );
    Ok(())
}

#[test]
fn from_package_reexports_resolve_to_the_defining_module() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("pkg");
    std::fs::create_dir(&package)?;
    std::fs::write(package.join("__init__.py"), "from .api import target\n")?;
    std::fs::write(
        package.join("api.py"),
        "def target(value=1): return value\n",
    )?;
    let caller = directory.path().join("caller.py");
    std::fs::write(&caller, "from pkg import target\ntarget()\n")?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "from pkg import target\ntarget(value=1)\n"
    );
    Ok(())
}

#[test]
fn package_attributes_resolve_to_the_reexported_definition(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("pkg");
    std::fs::create_dir(&package)?;
    std::fs::write(package.join("__init__.py"), "from .api import target\n")?;
    std::fs::write(
        package.join("api.py"),
        "def target(value=1): return value\n",
    )?;
    let caller = directory.path().join("caller.py");
    std::fs::write(&caller, "import pkg\npkg.target()\n")?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "import pkg\npkg.target(value=1)\n"
    );
    Ok(())
}

#[test]
fn packages_take_precedence_over_same_named_modules() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("dupe");
    std::fs::create_dir(&package)?;
    std::fs::write(
        directory.path().join("dupe.py"),
        "def target(value=1): return value\n",
    )?;
    std::fs::write(
        package.join("__init__.py"),
        "def target(value=5): return value\n",
    )?;
    let caller = directory.path().join("caller.py");
    std::fs::write(&caller, "import dupe\ndupe.target()\n")?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "import dupe\ndupe.target(value=5)\n"
    );
    Ok(())
}

#[test]
fn star_imported_functions_have_their_calls_updated() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("api.py"),
        "def target(value=1): return value\n",
    )?;
    let caller = directory.path().join("caller.py");
    std::fs::write(&caller, "from api import *\ntarget()\n")?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "from api import *\ntarget(value=1)\n"
    );
    Ok(())
}

#[test]
fn later_imports_do_not_change_earlier_calls() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("api.py"),
        "def target(value=1): return value\n",
    )?;
    std::fs::write(
        directory.path().join("other.py"),
        "def target(): return 5\n",
    )?;
    let caller = directory.path().join("caller.py");
    std::fs::write(
        &caller,
        "import api as module\nmodule.target()\nimport other as module\nmodule.target()\n",
    )?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "import api as module\nmodule.target(value=1)\nimport other as module\nmodule.target()\n"
    );
    Ok(())
}
#[cfg(unix)]
#[test]
fn symlinked_definitions_use_the_real_module_path() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let link = directory.path().join("api_link.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(&api, "def target(value=1): return value\n")?;
    symlink(&api, &link)?;
    std::fs::write(&caller, "import api\napi.target()\n")?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(&link)
        .arg(&caller)
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(api)?,
        "def target(value): return value\n"
    );
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "import api\napi.target(value=1)\n"
    );
    Ok(())
}

#[test]
fn absolute_and_relative_inputs_share_module_roots() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let api = directory.path().join("api.py");
    let caller = directory.path().join("caller.py");
    std::fs::write(&api, "def target(value=1): return value\n")?;
    std::fs::write(&caller, "import api\napi.target()\n")?;

    let output = Command::new(binary())
        .current_dir(directory.path())
        .arg("--fix")
        .arg(&api)
        .arg("caller.py")
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(api)?,
        "def target(value): return value\n"
    );
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "import api\napi.target(value=1)\n"
    );
    Ok(())
}

#[test]
fn imports_inside_module_while_loops_resolve_calls() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("api.py"),
        "def target(value=1): return value\n",
    )?;
    let caller = directory.path().join("caller.py");
    std::fs::write(
        &caller,
        "while ready():\n    import api\n    break\n\napi.target()\n",
    )?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "while ready():\n    import api\n    break\n\napi.target(value=1)\n"
    );
    Ok(())
}

#[test]
fn imports_inside_module_with_statements_resolve_calls() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("api.py"),
        "def target(value=1): return value\n",
    )?;
    let caller = directory.path().join("caller.py");
    std::fs::write(&caller, "with context():\n    import api\n\napi.target()\n")?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "with context():\n    import api\n\napi.target(value=1)\n"
    );
    Ok(())
}

#[test]
fn imports_inside_exhaustive_match_cases_resolve_calls() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("api.py"),
        "def target(value=1): return value\n",
    )?;
    let caller = directory.path().join("caller.py");
    std::fs::write(
        &caller,
        "match value:\n    case _:\n        import api\n\napi.target()\n",
    )?;
    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "match value:\n    case _:\n        import api\n\napi.target(value=1)\n"
    );
    Ok(())
}

#[test]
fn nested_dataclass_names_do_not_pollute_module_inheritance(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass\n\ndef factory():\n    @dataclass\n    class Base:\n        nested: int = 1\n\n@dataclass\nclass Base:\n    pass\n\n@dataclass\nclass Child(Base):\n    own: int = 2\n\nChild()\n",
    )?;
    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let fixed = std::fs::read_to_string(path)?;
    assert!(fixed.contains("nested: int\n"), "{fixed}");
    assert!(fixed.contains("own: int\n"), "{fixed}");
    assert!(fixed.ends_with("Child(own=2)\n"), "{fixed}");
    Ok(())
}

#[test]
fn custom_qualified_protocol_bases_are_not_treated_as_structural(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass\n\nclass helpers:\n    class Protocol:\n        inherited: int = 1\n\n@dataclass\nclass C(helpers.Protocol):\n    own: int = 2\n\nC()\n",
    )?;
    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let fixed = std::fs::read_to_string(path)?;
    assert!(fixed.contains("own: int\n"), "{fixed}");
    assert!(fixed.ends_with("C()\n"), "{fixed}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("inherits fields"), "{stderr}");
    Ok(())
}

#[test]
fn custom_field_functions_are_not_treated_as_dataclasses_field(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass\n\ndef field(*, default):\n    return default\n\n@dataclass\nclass C:\n    value: int = field(default=1)\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let fixed = std::fs::read_to_string(path)?;
    assert!(fixed.contains("def field(*, default):"), "{fixed}");
    assert!(fixed.contains("value: int\n"), "{fixed}");
    assert!(!fixed.contains("value: int = field()"), "{fixed}");
    Ok(())
}

#[test]
fn custom_dataclass_decorators_do_not_enable_field_diagnostics(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "def dataclass(cls):\n    return cls\n\n@dataclass\nclass C:\n    value: int = 1\n",
    )?;

    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(&path)
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn a_local_basemodel_does_not_activate_the_pydantic_default(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "class BaseModel:\n    pass\n\nclass C(BaseModel):\n    value: int = 1\n",
    )?;

    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(&path)
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn rebinding_a_dataclass_alias_invalidates_the_import() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass as dc\n\ndef dc(cls):\n    return cls\n\n@dc\nclass C:\n    value: int = 1\n",
    )?;

    let output = Command::new(binary())
        .arg("--output-format")
        .arg("concise")
        .arg(&path)
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn assigning_over_a_generic_alias_restores_inherited_dataclass_fields(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass\nfrom typing import Generic as G\n@dataclass\nclass Base:\n    first: int = 1\nG = Base\n@dataclass\nclass Child(G):\n    second: int = 2\nassert Child() == Child(first=1, second=2)\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let fixed = std::fs::read_to_string(path)?;
    assert!(
        fixed.contains("Child(first=1, second=2) == Child(first=1, second=2)"),
        "{fixed}"
    );
    Ok(())
}

#[test]
fn assigning_over_the_abc_module_alias_restores_inherited_dataclass_fields(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "import abc\nfrom dataclasses import dataclass\n@dataclass\nclass Base:\n    first: int = 1\nclass Namespace:\n    ABC = Base\nabc = Namespace()\n@dataclass\nclass Child(abc.ABC):\n    second: int = 2\nassert Child() == Child(first=1, second=2)\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let fixed = std::fs::read_to_string(path)?;
    assert!(
        fixed.contains("Child(first=1, second=2) == Child(first=1, second=2)"),
        "{fixed}"
    );
    Ok(())
}

#[test]
fn assigning_over_a_dataclass_decorator_invalidates_the_import(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    let source = "from dataclasses import dataclass\ndataclass = lambda cls: cls\n\n@dataclass\nclass C:\n    value: int = 1\n\nassert C().value == 1\n";
    std::fs::write(&path, source)?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert_eq!(std::fs::read_to_string(path)?, source);
    Ok(())
}

#[test]
fn later_class_assignments_do_not_shadow_earlier_calls() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "def target(value=1):\n    return value\n\nclass C:\n    before = target()\n    target = 5\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "def target(value):\n    return value\n\nclass C:\n    before = target(value=1)\n    target = 5\n"
    );
    Ok(())
}

#[test]
fn later_class_methods_do_not_shadow_earlier_calls() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "def target(value=1):\n    return value\n\nclass C:\n    before = target()\n\n    def target():\n        return 5\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "def target(value):\n    return value\n\nclass C:\n    before = target(value=1)\n\n    def target():\n        return 5\n"
    );
    Ok(())
}

#[test]
fn later_class_imports_do_not_shadow_earlier_calls() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("api.py"), "def target(): return 5\n")?;
    let path = directory.path().join("case.py");
    std::fs::write(
        &path,
        "def target(value=1):\n    return value\n\nclass C:\n    before = target()\n    from api import target\n",
    )?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "def target(value):\n    return value\n\nclass C:\n    before = target(value=1)\n    from api import target\n"
    );
    Ok(())
}

#[test]
fn instance_receivers_need_not_be_named_self() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "class C:\n    def fetch(self, value=1):\n        return value\n\n    def run(this):\n        return this.fetch()\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "class C:\n    def fetch(self, value):\n        return value\n\n    def run(this):\n        return this.fetch(value=1)\n"
    );
    Ok(())
}

#[test]
fn methods_on_freshly_constructed_instances_are_resolved() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "class C:\n    def fetch(self, value=1):\n        return value\n\nC().fetch()\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "class C:\n    def fetch(self, value):\n        return value\n\nC().fetch(value=1)\n"
    );
    Ok(())
}

#[test]
fn lambda_parameters_shadow_enclosing_method_receivers() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "class C:\n    def fetch(self, value=1):\n        return value\n\n    def run(self, other):\n        return (lambda self: self.fetch())(other)\n\nclass Other:\n    def fetch(self):\n        return 5\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let fixed = std::fs::read_to_string(path)?;
    assert!(fixed.contains("def fetch(self, value):"), "{fixed}");
    assert!(fixed.contains("lambda self: self.fetch()"), "{fixed}");
    assert!(
        !fixed.contains("lambda self: self.fetch(value=1)"),
        "{fixed}"
    );
    Ok(())
}

#[test]
fn aliased_staticmethod_decorators_have_no_implicit_receiver(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from builtins import staticmethod as sm\n\nclass C:\n    @sm\n    def parse(value=1):\n        return value\n\n    def run(self):\n        return self.parse(5)\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "from builtins import staticmethod as sm\n\nclass C:\n    @sm\n    def parse(value):\n        return value\n\n    def run(self):\n        return self.parse(5)\n"
    );
    Ok(())
}

#[test]
fn nested_classes_have_qualified_method_identities() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "class Outer:\n    class C:\n        def fetch(self, value=1):\n            return value\n\n        def run(self):\n            return self.fetch()\n\nclass C:\n    def fetch(self, value=2):\n        return value\n\nC().fetch()\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let fixed = std::fs::read_to_string(path)?;
    assert!(fixed.contains("return self.fetch(value=1)"), "{fixed}");
    assert!(fixed.ends_with("C().fetch(value=2)\n"), "{fixed}");
    Ok(())
}

#[test]
fn package_attributes_take_precedence_over_same_named_submodules(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = directory.path().join("pkg");
    std::fs::create_dir(&package)?;
    std::fs::write(
        package.join("__init__.py"),
        "def api(value=1): return value\n",
    )?;
    std::fs::write(package.join("api.py"), "def other(): return 5\n")?;
    let caller = directory.path().join("case.py");
    std::fs::write(&caller, "from pkg import api\napi()\n")?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(caller)?,
        "from pkg import api\napi(value=1)\n"
    );
    Ok(())
}

#[test]
fn dataclass_fields_in_false_branches_are_not_constructor_parameters(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    if False:\n        ghost: int = 1\n    value: int = 2\n\nC()\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    if False:\n        ghost: int = 1\n    value: int\n\nC(value=2)\n"
    );
    Ok(())
}

#[test]
fn mutually_exclusive_dataclass_fields_are_not_combined() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    let source = "from dataclasses import dataclass\n\nflag = True\n@dataclass\nclass C:\n    if flag:\n        first: int = 1\n    else:\n        second: int = 2\n\nC()\n";
    std::fs::write(&path, source)?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(path)?, source);
    Ok(())
}

#[test]
fn try_and_except_dataclass_fields_are_not_combined() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    let source = "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    try:\n        first: int = 1\n    except Exception:\n        second: int = 2\n\nC()\n";
    std::fs::write(&path, source)?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(path)?, source);
    Ok(())
}

#[test]
fn dataclass_fields_in_empty_loops_are_not_constructor_parameters(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    for _ in ():\n        ghost: int = 1\n    value: int = 2\n\nC()\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    for _ in ():\n        ghost: int = 1\n    value: int\n\nC(value=2)\n"
    );
    Ok(())
}

#[test]
fn dataclass_fields_in_false_while_loops_are_not_constructor_parameters(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    while False:\n        ghost: int = 1\n    value: int = 2\n\nC()\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    while False:\n        ghost: int = 1\n    value: int\n\nC(value=2)\n"
    );
    Ok(())
}

#[test]
fn match_case_dataclass_fields_are_not_combined() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    let source = "from dataclasses import dataclass\n\nchoice = 1\n@dataclass\nclass C:\n    match choice:\n        case 1:\n            first: int = 1\n        case _:\n            second: int = 2\n\nC()\n";
    std::fs::write(&path, source)?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(path)?, source);
    Ok(())
}

#[test]
fn field_defaults_overwritten_later_are_not_copied_to_calls(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    let source = "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    value: int = 1\n    value = 2\n\nC()\n";
    std::fs::write(&path, source)?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(path)?, source);
    Ok(())
}

#[test]
fn repeated_dataclass_fields_use_the_final_default_once() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    value: int = 1\n    value: int = 2\n\nC()\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "from dataclasses import dataclass\n\n@dataclass\nclass C:\n    value: int\n    value: int\n\nC(value=2)\n"
    );
    Ok(())
}

#[test]
fn overriding_inherited_fields_use_the_subclass_default_once(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    value: int = 1\n\n@dataclass\nclass C(Base):\n    value: int = 2\n\nC()\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "from dataclasses import dataclass\n\n@dataclass\nclass Base:\n    value: int\n\n@dataclass\nclass C(Base):\n    value: int\n\nC(value=2)\n"
    );
    Ok(())
}

#[test]
fn a_sole_field_default_with_a_trailing_comma_is_fixed() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass, field\n\n@dataclass\nclass C:\n    value: int = field(default=1,)\n\nC()\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "from dataclasses import dataclass, field\n\n@dataclass\nclass C:\n    value: int = field()\n\nC(value=1)\n"
    );
    Ok(())
}

#[test]
fn a_sole_pydantic_field_default_with_a_trailing_comma_is_fixed(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from pydantic import BaseModel, Field\n\nclass C(BaseModel):\n    value: int = Field(1,)\n\nC()\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "from pydantic import BaseModel, Field\n\nclass C(BaseModel):\n    value: int = Field()\n\nC(value=1)\n"
    );
    Ok(())
}

#[test]
fn a_try_else_import_is_exclusive_to_the_success_path() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("after.py"),
        "def target(): return 9\n",
    )?;
    std::fs::write(
        directory.path().join("fallback.py"),
        "def target(value=1): return value\n",
    )?;
    let caller = directory.path().join("case.py");
    let source = "try:\n    import missing_module\nexcept ImportError:\n    import fallback as module\nelse:\n    import after as module\n\nmodule.target()\n";
    std::fs::write(&caller, source)?;

    let output = Command::new(binary())
        .arg("--fix")
        .arg(directory.path())
        .output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(std::fs::read_to_string(caller)?, source);
    assert!(String::from_utf8(output.stderr)?.contains("cannot be tied to the definition"));
    Ok(())
}

#[test]
fn type_checking_only_dataclass_fields_are_not_constructor_parameters(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass\nfrom typing import TYPE_CHECKING\n\n@dataclass\nclass C:\n    if TYPE_CHECKING:\n        ghost: int = 1\n    value: int = 2\n\nC()\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(path)?,
        "from dataclasses import dataclass\nfrom typing import TYPE_CHECKING\n\n@dataclass\nclass C:\n    if TYPE_CHECKING:\n        ghost: int = 1\n    value: int\n\nC(value=2)\n"
    );
    Ok(())
}

#[test]
fn assigning_over_type_checking_makes_the_live_branch_fixable(
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    std::fs::write(
        &path,
        "from dataclasses import dataclass\nfrom typing import TYPE_CHECKING as checking\nchecking = True\nif checking:\n    @dataclass\n    class C:\n        value: int = 1\nassert C().value == 1\n",
    )?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    let fixed = std::fs::read_to_string(path)?;
    assert!(fixed.contains("value: int\n"), "{fixed}");
    assert!(fixed.contains("C(value=1).value"), "{fixed}");
    Ok(())
}

#[test]
fn for_targets_invalidate_imported_dataclass_aliases() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    let source = "from dataclasses import dataclass as dc\nfor dc in [lambda cls: cls]:\n    pass\n@dc\nclass C:\n    value: int = 1\nassert C().value == 1\n";
    std::fs::write(&path, source)?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert_eq!(std::fs::read_to_string(path)?, source);
    Ok(())
}

#[test]
fn with_targets_invalidate_imported_dataclass_aliases() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("example.py");
    let source = "from dataclasses import dataclass as dc\nclass Manager:\n    def __enter__(self): return lambda cls: cls\n    def __exit__(self, *args): pass\nwith Manager() as dc:\n    pass\n@dc\nclass C:\n    value: int = 1\nassert C().value == 1\n";
    std::fs::write(&path, source)?;

    let output = Command::new(binary()).arg("--fix").arg(&path).output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert_eq!(std::fs::read_to_string(path)?, source);
    Ok(())
}
