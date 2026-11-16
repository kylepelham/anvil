//! `anvil lsp` end to end: a tiny JSON-RPC client starts the server, opens a manifest with a mistake, sees the
//! diagnostic, fixes it, and asks for completion, definition and rename before shutting the server down.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const MANIFEST: &str = r#"[project]
name = "demo"

[target.core]
kind = "lib"
sources = ["src/core.cpp"]

[target.app]
sources = ["src/main.cpp"]
deps = ["core"]
when.lnux.defines = ["X"]
"#;

/// The server, spoken to over its stdin and stdout.
struct Client {
    child: Child,
    input: ChildStdin,
    messages: Receiver<Value>,
    next_id: i64,
}

impl Client {
    fn start() -> Client {
        let mut child = Command::new(env!("CARGO_BIN_EXE_anvil"))
            .arg("lsp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("starting anvil lsp");
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();

        // Every message the server writes; anything but Content-Length framing fails the test
        let (sender, messages) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut output = BufReader::new(output);
            loop {
                let mut length = None;
                loop {
                    let mut line = String::new();
                    if output.read_line(&mut line).unwrap() == 0 {
                        return;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    let (name, value) = line.split_once(": ").unwrap_or_else(|| panic!("not a header: {line:?}"));
                    if name.eq_ignore_ascii_case("Content-Length") {
                        length = Some(value.parse::<usize>().unwrap());
                    }
                }
                let mut body = vec![0; length.expect("a Content-Length header")];
                output.read_exact(&mut body).unwrap();
                if sender.send(serde_json::from_slice(&body).expect("a JSON body")).is_err() {
                    return;
                }
            }
        });

        Client {
            child,
            input,
            messages,
            next_id: 0,
        }
    }

    fn send(&mut self, message: Value) {
        let body = message.to_string();
        write!(self.input, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        self.input.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Sends a request and waits for its response (answering the server's own requests meanwhile).
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));

        self.wait_for(|message| message["id"] == id && message.get("method").is_none())
    }

    /// The next message `wanted` accepts, within 60 seconds.
    fn wait_for(&mut self, wanted: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = match self.messages.recv_timeout(left) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => panic!("no awaited message from the server within 60 seconds"),
                Err(RecvTimeoutError::Disconnected) => panic!("the server closed its output"),
            };
            if message.get("method").is_some() && message.get("id").is_some() {
                self.send(json!({ "jsonrpc": "2.0", "id": message["id"], "result": null }));
            }
            if wanted(&message) {
                return message;
            }
        }
    }

    /// The next diagnostics published for `uri` that `wanted` accepts.
    fn diagnostics(&mut self, uri: &str, wanted: impl Fn(&Value) -> bool) -> Value {
        self.wait_for(|message| {
            message["method"] == "textDocument/publishDiagnostics" && message["params"]["uri"] == uri && wanted(&message["params"])
        })["params"]
            .clone()
    }
}

fn file_uri(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    format!("file:///{}", path.trim_start_matches('/'))
}

fn position(line: u32, character: u32) -> Value {
    json!({ "line": line, "character": character })
}

fn range(line: u32, start: u32, end: u32) -> Value {
    json!({ "start": position(line, start), "end": position(line, end) })
}

/// A project in a fresh temp folder.
fn project() -> PathBuf {
    let root = std::env::temp_dir().join("anvil-it").join(format!("lsp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("anvil.toml"), MANIFEST).unwrap();
    std::fs::write(root.join("src/core.cpp"), "int core() { return 1; }\n").unwrap();
    std::fs::write(root.join("src/main.cpp"), "int main() {}\n").unwrap();

    root
}

#[test]
fn language_server_session() {
    let root = project();
    let uri = file_uri(&root.join("anvil.toml"));
    let mut client = Client::start();

    // initialize: UTF-16 positions, push diagnostics
    let initialized = client.request(
        "initialize",
        json!({
            "processId": null,
            "rootUri": file_uri(&root),
            "capabilities": { "general": { "positionEncodings": ["utf-16"] } },
        }),
    );
    let result = &initialized["result"];
    assert_eq!(result["serverInfo"]["name"], "anvil");
    assert_eq!(result["capabilities"]["positionEncoding"], "utf-16");
    assert_eq!(result["capabilities"]["textDocumentSync"]["change"], 2);
    client.notify("initialized", json!({}));

    // A misspelled condition word: an error on the word, with its fix
    client.notify(
        "textDocument/didOpen",
        json!({ "textDocument": { "uri": uri, "languageId": "toml", "version": 1, "text": MANIFEST } }),
    );
    let published = client.diagnostics(&uri, |params| params["diagnostics"].as_array().is_some_and(|list| !list.is_empty()));
    let diagnostic = &published["diagnostics"][0];
    assert_eq!(diagnostic["code"], "unknown-condition-word", "{published}");
    assert_eq!(diagnostic["severity"], 1);
    assert_eq!(diagnostic["source"], "anvil");
    assert_eq!(diagnostic["range"], range(10, 5, 9));
    assert_eq!(diagnostic["data"]["replace"]["text"], "linux");

    // Fixed by an incremental change: the diagnostics clear
    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "range": range(10, 5, 9), "text": "linux" }],
        }),
    );
    let cleared = client.diagnostics(&uri, |params| params["version"] == 2);
    assert_eq!(cleared["diagnostics"], json!([]));

    // Completion in `deps = ["core"]`: the other target
    let completion = client.request(
        "textDocument/completion",
        json!({ "textDocument": { "uri": uri }, "position": position(9, 9) }),
    );
    let items = completion["result"]["items"].as_array().cloned().unwrap_or_default();
    assert!(items.iter().any(|item| item["label"] == "core"), "{completion}");
    assert!(!items.iter().any(|item| item["label"] == "app"), "{completion}");

    // On an empty line: table snippets, as plain text for a client without snippet support
    let completion = client.request(
        "textDocument/completion",
        json!({ "textDocument": { "uri": uri }, "position": position(11, 0) }),
    );
    let items = completion["result"]["items"].as_array().cloned().unwrap_or_default();
    let snippet = items
        .iter()
        .find(|item| item["label"] == "[target.name] (program)")
        .expect("a table snippet");
    assert_eq!(snippet["kind"], 15);
    assert_eq!(snippet["insertTextFormat"], Value::Null);
    assert_eq!(snippet["textEdit"]["newText"], "[target.app]\nkind = \"exe\"\ndir = \"app\"\ndeps = []\n");

    // Definition: `core` leads to its [target.core] header
    let definition = client.request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri }, "position": position(9, 10) }),
    );
    assert_eq!(definition["result"]["uri"], uri.as_str());
    assert_eq!(definition["result"]["range"], range(3, 8, 12));

    // Rename: the header and the use
    let renamed = client.request(
        "textDocument/rename",
        json!({ "textDocument": { "uri": uri }, "position": position(3, 9), "newName": "base" }),
    );
    let edits = renamed["result"]["changes"][uri.as_str()].as_array().cloned().unwrap_or_default();
    let ranges: Vec<&Value> = edits.iter().map(|edit| &edit["range"]).collect();
    assert_eq!(ranges, vec![&range(3, 8, 12), &range(9, 9, 13)], "{renamed}");
    assert!(edits.iter().all(|edit| edit["newText"] == "base"));

    // An unknown request is an error; a handler that panics answers with one and the server goes on
    let unknown = client.request("anvil/noSuchThing", json!({}));
    assert_eq!(unknown["error"]["code"], -32601);
    if cfg!(debug_assertions) {
        let panicked = client.request("anvil/debugPanic", json!({}));
        assert_eq!(panicked["error"]["code"], -32603);
    }

    // shutdown, then exit: exit code 0
    let shutdown = client.request("shutdown", Value::Null);
    assert_eq!(shutdown["result"], Value::Null);
    client.notify("exit", Value::Null);
    let status = client.child.wait().unwrap();
    assert_eq!(status.code(), Some(0));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn exit_without_shutdown_fails() {
    let mut client = Client::start();
    client.request("initialize", json!({ "processId": null, "capabilities": {} }));
    client.notify("initialized", json!({}));
    client.notify("exit", Value::Null);

    assert_eq!(client.child.wait().unwrap().code(), Some(1));
}
