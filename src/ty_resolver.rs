//! Resolve Python definitions through the supported `ty server` LSP interface.

use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
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
    outgoing: Option<Sender<OutgoingMessage>>,
    incoming: Receiver<Value>,
    next_id: i64,
    opened: HashSet<PathBuf>,
    pending: HashMap<i64, Value>,
}

struct OutgoingMessage {
    body: Vec<u8>,
    completed: Sender<std::io::Result<()>>,
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
    pub fn start(project_root: &Path, extra_paths: &[PathBuf]) -> Result<Self, String> {
        let child = ty_command()
            .arg("server")
            .current_dir(project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("could not start `ty server`: {error}"))?;
        let mut resolver = Self::from_child(child)?;
        let deadline = Instant::now() + INITIALIZE_TIMEOUT;
        let id = resolver.request(
            "initialize",
            &json!({
                "processId": std::process::id(),
                "rootUri": absolute_uri(project_root),
                "capabilities": { "textDocument": { "diagnostic": {} } },
                "initializationOptions": {
                    "configuration": {
                        "environment": { "extra-paths": extra_paths },
                    },
                },
            }),
            deadline,
        )?;
        resolver.collect(id, deadline)?;
        resolver.notify("initialized", &json!({}), deadline)?;
        Ok(resolver)
    }

    fn from_child(mut child: Child) -> Result<Self, String> {
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
        let (outgoing, messages) = std::sync::mpsc::channel();
        std::thread::spawn(move || write_messages(stdin, messages));
        Ok(Self {
            child,
            outgoing: Some(outgoing),
            incoming,
            next_id: 1,
            opened: HashSet::new(),
            pending: HashMap::new(),
        })
    }

    pub fn definitions(
        &mut self,
        path: &Path,
        source: &str,
        offset: usize,
    ) -> Result<Vec<DefinitionLocation>, String> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        self.open_until(path, source, deadline)?;
        let (line, character) = lsp_position(source, offset);
        let id = self.request(
            "textDocument/definition",
            &json!({
                "textDocument": { "uri": absolute_uri(path) },
                "position": { "line": line, "character": character },
            }),
            deadline,
        )?;
        let result = self.collect(id, deadline)?;
        Ok(locations_from_value(&result))
    }

    pub fn open(&mut self, path: &Path, source: &str) -> Result<(), String> {
        self.open_until(path, source, Instant::now() + REQUEST_TIMEOUT)
    }

    fn open_until(&mut self, path: &Path, source: &str, deadline: Instant) -> Result<(), String> {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        if !self.opened.contains(&path) {
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
                deadline,
            )?;
            self.opened.insert(path);
        }
        Ok(())
    }

    fn request(&mut self, method: &str, params: &Value, deadline: Instant) -> Result<i64, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(
            &json!({
                "jsonrpc": "2.0", "id": id, "method": method, "params": params,
            }),
            deadline,
        )?;
        Ok(id)
    }

    fn notify(&mut self, method: &str, params: &Value, deadline: Instant) -> Result<(), String> {
        self.send(
            &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
            deadline,
        )
    }

    fn send(&mut self, message: &Value, deadline: Instant) -> Result<(), String> {
        let body = serde_json::to_vec(message).map_err(|error| error.to_string())?;
        let (completed, written) = std::sync::mpsc::channel();
        let result = (|| {
            remaining_time(deadline)?;
            self.outgoing
                .as_ref()
                .ok_or_else(|| "`ty server` disconnected".to_owned())?
                .send(OutgoingMessage { body, completed })
                .map_err(|_| "`ty server` disconnected".to_owned())?;
            written
                .recv_timeout(remaining_time(deadline)?)
                .map_err(communication_wait_error)?
                .map_err(|error| format!("could not communicate with `ty server`: {error}"))
        })();
        if result.is_err() {
            // A timed-out write may have sent only part of a frame. Never reuse
            // that transport, and unblock the writer by terminating the child.
            self.outgoing.take();
            let _ = self.child.kill();
        }
        result
    }

    fn collect(&mut self, id: i64, deadline: Instant) -> Result<Value, String> {
        if let Some(value) = self.pending.remove(&id) {
            return Ok(value);
        }
        loop {
            let remaining = remaining_time(deadline)?;
            let message = self
                .incoming
                .recv_timeout(remaining)
                .map_err(communication_wait_error)?;
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
                    self.send(
                        &json!({ "jsonrpc": "2.0", "id": other_id, "result": null }),
                        deadline,
                    )?;
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
        // Cleanup must not write to a pipe that may already be full.
        self.outgoing.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn remaining_time(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| "`ty server` request timed out".to_owned())
}

fn communication_wait_error(error: RecvTimeoutError) -> String {
    match error {
        RecvTimeoutError::Timeout => "`ty server` request timed out".to_owned(),
        RecvTimeoutError::Disconnected => "`ty server` disconnected".to_owned(),
    }
}

fn write_messages(mut stdin: ChildStdin, messages: Receiver<OutgoingMessage>) {
    for message in messages {
        let result = write!(stdin, "Content-Length: {}\r\n\r\n", message.body.len())
            .and_then(|()| stdin.write_all(&message.body))
            .and_then(|()| stdin.flush());
        let failed = result.is_err();
        if message.completed.send(result).is_err() || failed {
            return;
        }
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
    let path = uri_path(&path);
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

fn uri_path(path: &str) -> &str {
    path.strip_prefix("//?/").unwrap_or(path)
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
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::{Duration, Instant};

    use serde_json::{json, Value};

    use super::{
        lsp_position, percent_decode, read_messages, uri_path, OutgoingMessage, TyResolver,
    };

    const TEST_TIMEOUT: Duration = Duration::from_millis(100);
    const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(3);

    fn large_params() -> Value {
        json!({ "text": "x".repeat(8 * 1024 * 1024) })
    }

    fn transport(mode: &str) -> Result<TyResolver, Box<dyn std::error::Error>> {
        // Run a Rust test as the peer so real pipe backpressure is covered on
        // every supported platform, without requiring a shell or Python.
        let child = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "ty_resolver::tests::transport_peer",
                "--ignored",
                "--nocapture",
            ])
            .env("NO_DEFAULTS_TRANSPORT_PEER", mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let resolver = TyResolver::from_child(child)?;
        assert_eq!(
            resolver.incoming.recv_timeout(WATCHDOG_TIMEOUT)?,
            json!({ "method": "ready" })
        );
        Ok(resolver)
    }

    fn peer_message(message: &Value) -> std::io::Result<()> {
        let body = serde_json::to_vec(message)?;
        let mut stdout = std::io::stdout().lock();
        // The newline separates the first frame from the test harness output.
        write!(stdout, "\nContent-Length: {}\r\n\r\n", body.len())?;
        stdout.write_all(&body)?;
        stdout.flush()
    }

    #[test]
    #[ignore = "subprocess peer for transport regression tests"]
    fn transport_peer() -> Result<(), Box<dyn std::error::Error>> {
        let mode = std::env::var("NO_DEFAULTS_TRANSPORT_PEER")?;
        peer_message(&json!({ "method": "ready" }))?;
        if mode == "server-request" || mode == "echo-server-request" {
            peer_message(&json!({ "id": 42, "method": "workspace/configuration" }))?;
        }
        if mode == "exit" {
            return Ok(());
        }
        if mode == "blocked" || mode == "server-request" {
            // Finite lifetime also prevents a broken implementation from
            // leaving the regression test stuck forever.
            std::thread::sleep(Duration::from_secs(10));
            return Ok(());
        }
        let (sender, incoming) = mpsc::channel();
        std::thread::spawn(move || read_messages(std::io::stdin().lock(), &sender));
        for message in incoming {
            peer_message(&json!({ "id": message["id"], "result": message }))?;
        }
        Ok(())
    }

    #[test]
    fn requests_time_out_when_the_peer_does_not_read() -> Result<(), Box<dyn std::error::Error>> {
        let mut resolver = transport("blocked")?;
        let params = large_params();
        let started = Instant::now();
        assert_eq!(
            resolver.request("initialize", &params, started + TEST_TIMEOUT),
            Err("`ty server` request timed out".to_owned())
        );
        assert!(started.elapsed() < WATCHDOG_TIMEOUT);
        assert!(resolver.outgoing.is_none());
        // A partially written frame cannot be retried on the same pipe.
        assert_eq!(
            resolver.send(&json!({}), Instant::now() + TEST_TIMEOUT),
            Err("`ty server` disconnected".to_owned())
        );
        assert!(!resolver.child.wait()?.success());
        Ok(())
    }

    #[test]
    fn opening_a_document_times_out_without_marking_it_open(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut resolver = transport("blocked")?;
        let source = "x".repeat(8 * 1024 * 1024);
        let started = Instant::now();
        assert_eq!(
            resolver.open_until(
                std::path::Path::new("example.py"),
                &source,
                started + TEST_TIMEOUT
            ),
            Err("`ty server` request timed out".to_owned())
        );
        assert!(started.elapsed() < WATCHDOG_TIMEOUT);
        assert!(resolver.opened.is_empty());
        Ok(())
    }

    #[test]
    fn server_request_replies_use_the_response_deadline() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut resolver = transport("server-request")?;
        let (completed, written) = mpsc::channel();
        resolver
            .outgoing
            .as_ref()
            .ok_or("missing writer")?
            .send(OutgoingMessage {
                body: serde_json::to_vec(&large_params())?,
                completed,
            })?;
        assert!(matches!(
            written.recv_timeout(TEST_TIMEOUT),
            Err(RecvTimeoutError::Timeout)
        ));
        let started = Instant::now();
        assert_eq!(
            resolver.collect(1, started + TEST_TIMEOUT),
            Err("`ty server` request timed out".to_owned())
        );
        assert!(started.elapsed() < WATCHDOG_TIMEOUT);
        assert!(resolver.outgoing.is_none());
        Ok(())
    }

    #[test]
    fn cleanup_terminates_a_child_with_a_blocked_writer() -> Result<(), Box<dyn std::error::Error>>
    {
        let resolver = transport("blocked")?;
        let (completed, written) = mpsc::channel();
        resolver
            .outgoing
            .as_ref()
            .ok_or("missing writer")?
            .send(OutgoingMessage {
                body: serde_json::to_vec(&large_params())?,
                completed,
            })?;
        assert!(matches!(
            written.recv_timeout(TEST_TIMEOUT),
            Err(RecvTimeoutError::Timeout)
        ));
        let (finished, dropped) = mpsc::channel();
        std::thread::spawn(move || {
            drop(resolver);
            let _ = finished.send(());
        });
        dropped.recv_timeout(WATCHDOG_TIMEOUT)?;
        // Killing the child releases the real blocked pipe write too.
        assert!(written.recv_timeout(WATCHDOG_TIMEOUT)?.is_err());
        Ok(())
    }

    #[test]
    fn sending_and_receiving_share_one_deadline() -> Result<(), Box<dyn std::error::Error>> {
        let mut resolver = transport("echo")?;
        let deadline = Instant::now() + Duration::from_secs(1);
        let id = resolver.request("initialize", &json!({}), deadline)?;
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()) + TEST_TIMEOUT);
        // Receiving must not start a fresh timeout after sending has used up
        // the operation's budget, even if a response is already buffered.
        assert_eq!(
            resolver.collect(id, deadline),
            Err("`ty server` request timed out".to_owned())
        );
        assert_eq!(
            resolver.collect(id, Instant::now() + WATCHDOG_TIMEOUT)?,
            json!({ "jsonrpc": "2.0", "id": id, "method": "initialize", "params": {} })
        );
        Ok(())
    }

    #[test]
    fn messages_remain_framed_and_ordered() -> Result<(), Box<dyn std::error::Error>> {
        let mut resolver = transport("echo")?;
        let deadline = Instant::now() + WATCHDOG_TIMEOUT;
        resolver.notify("initialized", &json!({}), deadline)?;
        let params = large_params();
        let first = resolver.request("first", &params, deadline)?;
        let second = resolver.request("second", &json!({}), deadline)?;
        // Collect out of order to exercise pending responses as well.
        assert_eq!(
            resolver.collect(second, deadline)?,
            json!({ "jsonrpc": "2.0", "id": second, "method": "second", "params": {} })
        );
        assert_eq!(
            resolver.collect(first, deadline)?,
            json!({ "jsonrpc": "2.0", "id": first, "method": "first", "params": params })
        );
        Ok(())
    }

    #[test]
    fn server_requests_receive_a_framed_reply() -> Result<(), Box<dyn std::error::Error>> {
        let mut resolver = transport("echo-server-request")?;
        let deadline = Instant::now() + WATCHDOG_TIMEOUT;
        let id = resolver.request("initialize", &json!({}), deadline)?;
        assert_eq!(
            resolver.collect(id, deadline)?,
            json!({ "jsonrpc": "2.0", "id": id, "method": "initialize", "params": {} })
        );
        assert_eq!(
            resolver.collect(42, deadline)?,
            json!({ "jsonrpc": "2.0", "id": 42, "result": null })
        );
        Ok(())
    }

    #[test]
    fn pipe_write_errors_are_reported() -> Result<(), Box<dyn std::error::Error>> {
        let mut resolver = transport("exit")?;
        let error = resolver
            .request(
                "initialize",
                &large_params(),
                Instant::now() + WATCHDOG_TIMEOUT,
            )
            .err()
            .ok_or("expected a pipe write error")?;
        assert!(
            error.starts_with("could not communicate with `ty server`: "),
            "{error}"
        );
        assert!(resolver.outgoing.is_none());
        Ok(())
    }

    #[test]
    fn closed_response_pipes_report_disconnection() -> Result<(), Box<dyn std::error::Error>> {
        let mut resolver = transport("exit")?;
        assert_eq!(
            resolver.collect(1, Instant::now() + WATCHDOG_TIMEOUT),
            Err("`ty server` disconnected".to_owned())
        );
        Ok(())
    }

    #[test]
    fn lsp_columns_count_utf16_code_units() {
        assert_eq!(lsp_position("a😀b", "a😀".len()), (0, 3));
    }

    #[test]
    fn percent_encoded_paths_are_decoded() {
        assert_eq!(percent_decode("/tmp/a%20b%23c.py"), "/tmp/a b#c.py");
    }

    #[test]
    fn windows_verbatim_paths_become_regular_lsp_paths() {
        assert_eq!(uri_path("//?/C:/work/file.py"), "C:/work/file.py");
    }
}
