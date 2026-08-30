//! Resolve Python definitions through the supported `ty server` LSP interface.

use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DefinitionLocation {
    pub path: PathBuf,
    pub line: u32,
}

pub struct TyResolver {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: i64,
    opened: HashSet<PathBuf>,
    pending: HashMap<i64, Value>,
}

fn ty_command() -> Command {
    let program = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(Path::to_path_buf))
        .map(|directory| directory.join(if cfg!(windows) { "ty.exe" } else { "ty" }))
        .filter(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from("ty"));
    Command::new(program)
}

pub fn require_ty() -> Result<(), String> {
    ty_command()
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .filter(std::process::ExitStatus::success)
        .map(|_| ())
        .ok_or_else(|| {
            "the `ty` type-inference backend is required but was not found; install it with `uv tool install ty`".to_owned()
        })
}

impl TyResolver {
    pub fn start(project_root: &Path) -> Result<Self, String> {
        let mut child = ty_command()
            .arg("server")
            .current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("could not start `ty server`: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "`ty server` supplied no stdin".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "`ty server` supplied no stdout".to_owned())?;
        let (sender, incoming) = std::sync::mpsc::channel();
        std::thread::spawn(move || read_messages(stdout, &sender));
        let mut resolver = Self {
            child,
            stdin,
            incoming,
            next_id: 1,
            opened: HashSet::new(),
            pending: HashMap::new(),
        };
        let id = resolver.request(
            "initialize",
            &json!({
                "processId": std::process::id(),
                "rootUri": absolute_uri(project_root),
                "capabilities": { "textDocument": { "diagnostic": {} } },
            }),
        )?;
        resolver.collect(id, INITIALIZE_TIMEOUT)?;
        resolver.notify("initialized", &json!({}))?;
        Ok(resolver)
    }

    pub fn definitions(
        &mut self,
        path: &Path,
        source: &str,
        offset: usize,
    ) -> Result<Vec<DefinitionLocation>, String> {
        self.open(path, source)?;
        let (line, character) = lsp_position(source, offset);
        let id = self.request(
            "textDocument/definition",
            &json!({
                "textDocument": { "uri": absolute_uri(path) },
                "position": { "line": line, "character": character },
            }),
        )?;
        let result = self.collect(id, REQUEST_TIMEOUT)?;
        Ok(locations_from_value(&result))
    }

    pub fn open(&mut self, path: &Path, source: &str) -> Result<(), String> {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        if self.opened.insert(path.clone()) {
            self.notify(
                "textDocument/didOpen",
                &json!({
                    "textDocument": {
                        "uri": absolute_uri(&path),
                        "languageId": "python",
                        "version": 1,
                        "text": source,
                    }
                }),
            )?;
        }
        Ok(())
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<i64, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        }))?;
        Ok(id)
    }

    fn notify(&mut self, method: &str, params: &Value) -> Result<(), String> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        let body = serde_json::to_vec(message).map_err(|error| error.to_string())?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())
            .and_then(|()| self.stdin.write_all(&body))
            .and_then(|()| self.stdin.flush())
            .map_err(|error| format!("could not communicate with `ty server`: {error}"))
    }

    fn collect(&mut self, id: i64, timeout: Duration) -> Result<Value, String> {
        if let Some(value) = self.pending.remove(&id) {
            return Ok(value);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| "`ty server` request timed out".to_owned())?;
            let message = self
                .incoming
                .recv_timeout(remaining)
                .map_err(|error| match error {
                    RecvTimeoutError::Timeout => "`ty server` request timed out".to_owned(),
                    RecvTimeoutError::Disconnected => "`ty server` disconnected".to_owned(),
                })?;
            if message.get("method").is_none()
                && message.get("id").and_then(Value::as_i64) == Some(id)
            {
                if let Some(error) = message.get("error") {
                    return Err(format!("`ty server` returned an error: {error}"));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            if let Some(other_id) = message.get("id").and_then(Value::as_i64) {
                if message.get("method").is_some() {
                    self.send(&json!({ "jsonrpc": "2.0", "id": other_id, "result": null }))?;
                } else if let Some(error) = message.get("error") {
                    return Err(format!("`ty server` returned an error: {error}"));
                } else {
                    self.pending.insert(
                        other_id,
                        message.get("result").cloned().unwrap_or(Value::Null),
                    );
                }
            }
        }
    }
}

impl Drop for TyResolver {
    fn drop(&mut self) {
        let _ = self.send(&json!({
            "jsonrpc": "2.0", "id": -1, "method": "shutdown", "params": null,
        }));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_messages(stdout: impl Read, sender: &std::sync::mpsc::Sender<Value>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut header = Vec::new();
        let mut byte = [0_u8; 1];
        while !header.ends_with(b"\r\n\r\n") {
            if reader.read_exact(&mut byte).is_err() {
                return;
            }
            header.push(byte[0]);
        }
        let content_length = String::from_utf8_lossy(&header)
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length:"))
            .and_then(|length| length.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0_u8; content_length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        if let Ok(value) = serde_json::from_slice(&body) {
            if sender.send(value).is_err() {
                return;
            }
        }
    }
}

fn lsp_position(source: &str, offset: usize) -> (u32, u32) {
    let prefix = source.get(..offset).unwrap_or(source);
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let line_start = prefix.rfind(['\n', '\r']).map_or(0, |index| index + 1);
    let character = prefix[line_start..].encode_utf16().count();
    (
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(character).unwrap_or(u32::MAX),
    )
}

fn locations_from_value(result: &Value) -> Vec<DefinitionLocation> {
    let values: Vec<&Value> = match result {
        Value::Array(values) => values.iter().collect(),
        Value::Object(_) => vec![result],
        _ => Vec::new(),
    };
    values
        .into_iter()
        .filter_map(|location| {
            let uri = location
                .get("uri")
                .or_else(|| location.get("targetUri"))?
                .as_str()?;
            let range = location
                .get("range")
                .or_else(|| location.get("targetRange"))?;
            let line = u32::try_from(range.get("start")?.get("line")?.as_u64()?).ok()?;
            Some(DefinitionLocation {
                path: uri_to_path(uri)?,
                line,
            })
        })
        .collect()
}

fn absolute_uri(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let path = absolute.to_string_lossy().replace('\\', "/");
    let encoded = path
        .replace('%', "%25")
        .replace(' ', "%20")
        .replace('#', "%23");
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let mut path = percent_decode(uri.strip_prefix("file://")?);
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        path.remove(0);
    }
    Some(PathBuf::from(path))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let parsed = std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok());
            if let Some(byte) = parsed {
                output.push(byte);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{lsp_position, percent_decode};

    #[test]
    fn lsp_columns_count_utf16_code_units() {
        assert_eq!(lsp_position("a😀b", "a😀".len()), (0, 3));
    }

    #[test]
    fn percent_encoded_paths_are_decoded() {
        assert_eq!(percent_decode("/tmp/a%20b%23c.py"), "/tmp/a b#c.py");
    }
}
