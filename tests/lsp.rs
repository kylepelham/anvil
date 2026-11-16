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
        Client::start_with(&[])
    }

    fn start_with(variables: &[(&str, &str)]) -> Client {
        let mut child = Command::new(env!("CARGO_BIN_EXE_anvil"))
            .arg("lsp")
            .envs(variables.iter().copied())
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
        self.send_body(&message.to_string());
    }

    fn send_body(&mut self, body: &str) {
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

    /// `textDocument/diagnostic` for a document: its items.
    fn pull(&mut self, uri: &str) -> Value {
        let report = self.request("textDocument/diagnostic", json!({ "textDocument": { "uri": uri } }));
        report["result"]["items"].clone()
    }

    fn open(&mut self, uri: &str, version: i32, text: &str) {
        let document = json!({ "uri": uri, "languageId": "toml", "version": version, "text": text });
        self.notify("textDocument/didOpen", json!({ "textDocument": document }));
    }

    fn close(&mut self, uri: &str) {
        self.notify("textDocument/didClose", json!({ "textDocument": { "uri": uri } }));
    }

    /// shutdown, then exit: the exit code.
    fn stop(mut self) -> Option<i32> {
        self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        self.child.wait().unwrap().code()
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

/// A project in a fresh temp folder of its own (the tests run at once).
fn project(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join("anvil-it").join(format!("lsp-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("anvil.toml"), MANIFEST).unwrap();
    std::fs::write(root.join("src/core.cpp"), "int core() { return 1; }\n").unwrap();
    std::fs::write(root.join("src/main.cpp"), "int main() {}\n").unwrap();

    root
}

#[test]
fn language_server_session() {
    let root = project("session");
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

// The capabilities of the owner's editor: pull diagnostics without refresh, so its first answer must be right
fn pull_capabilities() -> Value {
    json!({ "textDocument": { "diagnostic": { "dynamicRegistration": true } } })
}

const BROKEN: &str = "[project]\nname = \"demo\"\n\n[target.app]\nkind = \"exx\"\nsources = [\"src/main.cpp\"]\n";

#[test]
fn a_document_closed_and_reopened_at_the_same_version_is_checked_again() {
    let root = project("reopen");
    let uri = file_uri(&root.join("anvil.toml"));
    let mut client = Client::start();
    client.request("initialize", json!({ "processId": null, "capabilities": pull_capabilities() }));
    client.notify("initialized", json!({}));

    // Broken, then closed and reopened fixed at version 1 again (as VS Code numbers a reopened document)
    client.open(&uri, 1, BROKEN);
    let items = client.pull(&uri);
    assert_eq!(items[0]["code"], "invalid-value", "{items}");
    client.close(&uri);
    client.open(&uri, 1, MANIFEST.replace("lnux", "linux").as_str());
    assert_eq!(client.pull(&uri), json!([]));

    // And back: the error returns
    client.close(&uri);
    client.open(&uri, 1, BROKEN);
    let items = client.pull(&uri);
    assert_eq!(items[0]["code"], "invalid-value", "{items}");

    assert_eq!(client.stop(), Some(0));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_pull_before_the_compiler_is_known_waits_for_the_whole_check() {
    let root = project("coldpull");
    let uri = file_uri(&root.join("anvil.toml"));
    let mut client = Client::start_with(&[("ANVIL_LSP_TEST_TOOLCHAIN_DELAY_MS", "1500")]);
    client.request("initialize", json!({ "processId": null, "capabilities": pull_capabilities() }));
    client.notify("initialized", json!({}));

    // An unknown dependency only the load finds (with no compiler on the machine, the load says that instead): the
    // files-only check before the compiler is known has nothing to say, and must not be the answer
    let text = MANIFEST
        .replace("lnux", "linux")
        .replace(r#"deps = ["core"]"#, r#"deps = ["core", "nopedep"]"#);
    client.open(&uri, 1, &text);
    let items = client.pull(&uri);
    assert!(!items.as_array().unwrap().is_empty(), "the first pull answered before the load: {items}");

    assert_eq!(client.stop(), Some(0));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_pull_waits_a_second_for_the_compiler_then_takes_the_files_errors() {
    let root = project("slowpull");
    let uri = file_uri(&root.join("anvil.toml"));
    let mut client = Client::start_with(&[("ANVIL_LSP_TEST_TOOLCHAIN_DELAY_MS", "4000")]);
    client.request("initialize", json!({ "processId": null, "capabilities": pull_capabilities() }));
    client.notify("initialized", json!({}));

    // A misspelled key the files show, and an unknown dependency only the load finds: the first pull is answered with
    // the files' error well before the compiler is known (debug builds delay it)
    let started = Instant::now();
    let text = MANIFEST
        .replace("lnux", "linux")
        .replace(r#"deps = ["core"]"#, "deps = [\"core\", \"nopedep\"]\ndefnies = [\"X\"]");
    client.open(&uri, 1, &text);
    let first = client.pull(&uri);
    assert_eq!(first[0]["code"], "unknown-key", "{first}");
    if cfg!(debug_assertions) {
        assert!(started.elapsed() < Duration::from_millis(3000), "answered after {:?}", started.elapsed());
        assert_eq!(first.as_array().unwrap().len(), 1, "{first}");
    }

    // A later pull, once the compiler is known, has the load's too
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let items = client.pull(&uri);
        if items.as_array().unwrap().len() > first.as_array().unwrap().len() {
            break;
        }
        assert!(Instant::now() < deadline, "no pull had the load's diagnostics: {items}");
        std::thread::sleep(Duration::from_millis(250));
    }

    assert_eq!(client.stop(), Some(0));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_client_told_to_pull_again_gets_the_files_only_check_first() {
    let root = project("refreshpull");
    let uri = file_uri(&root.join("anvil.toml"));
    let mut client = Client::start_with(&[("ANVIL_LSP_TEST_TOOLCHAIN_DELAY_MS", "4000")]);
    let capabilities = json!({
        "textDocument": { "diagnostic": { "dynamicRegistration": true } },
        "workspace": { "diagnostics": { "refreshSupport": true } },
    });
    client.request("initialize", json!({ "processId": null, "capabilities": capabilities }));
    client.notify("initialized", json!({}));

    // Nothing the files show: answered empty before the compiler is known, then told to pull again
    let started = Instant::now();
    let text = MANIFEST
        .replace("lnux", "linux")
        .replace(r#"deps = ["core"]"#, r#"deps = ["core", "nopedep"]"#);
    client.open(&uri, 1, &text);
    let first = client.pull(&uri);
    if cfg!(debug_assertions) {
        assert!(started.elapsed() < Duration::from_millis(3000), "answered after {:?}", started.elapsed());
        assert_eq!(first, json!([]));
        client.wait_for(|message| message["method"] == "workspace/diagnostic/refresh");
    }
    let items = client.pull(&uri);
    assert!(!items.as_array().unwrap().is_empty(), "the pull after the refresh: {items}");

    assert_eq!(client.stop(), Some(0));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn configuration_changes_only_the_settings_given() {
    let root = project("config");
    let uri = file_uri(&root.join("anvil.toml"));
    let text = MANIFEST.replace("lnux", "linux");
    let mut client = Client::start();
    let options = json!({ "anvil": { "profile": "nosuchprofile" } });
    client.request(
        "initialize",
        json!({ "processId": null, "capabilities": {}, "initializationOptions": options }),
    );
    client.notify("initialized", json!({}));

    client.open(&uri, 1, &text);
    let published = client.diagnostics(&uri, |params| params["diagnostics"].as_array().is_some_and(|list| !list.is_empty()));
    assert_eq!(published["diagnostics"][0]["code"], "unknown-profile", "{published}");

    // Neovim's `{ "settings": {} }` after `initialized`, and another tool's settings: the profile stays
    for settings in [json!({}), json!({ "other": { "profile": "release" } })] {
        client.notify("workspace/didChangeConfiguration", json!({ "settings": settings }));
    }
    let codes = |published: &Value| -> Vec<String> {
        published["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|diagnostic| diagnostic["code"].as_str().unwrap().to_string())
            .collect()
    };

    // A second mistake, so that the check publishes again: the profile's is still there
    let text = text.replace("[project]\n", "[project]\nnme = 1\n");
    let change = |version: i32| json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": [{ "text": text }] });
    client.notify("textDocument/didChange", change(2));
    let published = client.diagnostics(&uri, |params| params["version"] == 2);
    assert!(codes(&published).contains(&"unknown-profile".to_string()), "{published}");
    assert!(codes(&published).contains(&"unknown-key".to_string()), "{published}");

    // Anvil's settings: a null clears the profile, the toolchain (not given) stays as it was
    client.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "anvil": { "profile": null } } }),
    );
    client.notify("textDocument/didChange", change(3));
    let published = client.diagnostics(&uri, |params| params["version"] == 3);
    assert_eq!(codes(&published), vec!["unknown-key".to_string()], "{published}");

    assert_eq!(client.stop(), Some(0));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn workspace_folders_added_are_checked_and_removed_ones_cleared() {
    let root = project("folders");
    std::fs::write(root.join("anvil.toml"), BROKEN).unwrap();
    let uri = file_uri(&root.join("anvil.toml"));
    let folder = json!([{ "uri": file_uri(&root), "name": "folders" }]);
    let mut client = Client::start();
    client.request(
        "initialize",
        json!({ "processId": null, "capabilities": { "workspace": { "workspaceFolders": true } } }),
    );
    client.notify("initialized", json!({}));

    // Added: its project is checked without a document open
    client.notify(
        "workspace/didChangeWorkspaceFolders",
        json!({ "event": { "added": folder, "removed": [] } }),
    );
    let published = client.diagnostics(&uri, |params| params["diagnostics"].as_array().is_some_and(|list| !list.is_empty()));
    assert_eq!(published["diagnostics"][0]["code"], "invalid-value", "{published}");
    let symbols = client.request("workspace/symbol", json!({ "query": "" }));
    assert!(
        symbols["result"].as_array().unwrap().iter().any(|symbol| symbol["name"] == "app"),
        "{symbols}"
    );

    // Removed: its diagnostics and its targets go
    client.notify(
        "workspace/didChangeWorkspaceFolders",
        json!({ "event": { "added": [], "removed": folder } }),
    );
    client.diagnostics(&uri, |params| params["diagnostics"] == json!([]));
    let symbols = client.request("workspace/symbol", json!({ "query": "" }));
    assert_eq!(symbols["result"], json!([]));

    assert_eq!(client.stop(), Some(0));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_project_outside_the_folders_goes_with_its_last_document() {
    let root = project("outside");
    let uri = file_uri(&root.join("anvil.toml"));
    let mut client = Client::start();
    client.request("initialize", json!({ "processId": null, "capabilities": {} }));
    client.notify("initialized", json!({}));

    client.open(&uri, 1, BROKEN);
    client.diagnostics(&uri, |params| params["diagnostics"].as_array().is_some_and(|list| !list.is_empty()));
    client.close(&uri);
    client.diagnostics(&uri, |params| params["diagnostics"] == json!([]));

    assert_eq!(client.stop(), Some(0));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn messages_before_initialized_and_broken_ones_are_answered() {
    let mut client = Client::start();
    client.request("initialize", json!({ "processId": null, "capabilities": {} }));

    // Between the answer to initialize and `initialized`: notifications and requests are handled, the server lives
    client.notify("$/setTrace", json!({ "value": "off" }));
    client.notify("workspace/didChangeConfiguration", json!({ "settings": {} }));
    client.notify("$/cancelRequest", json!({ "id": 99 }));
    let symbols = client.request("workspace/symbol", json!({ "query": "" }));
    assert_eq!(symbols["result"], json!([]));
    let again = client.request("initialize", json!({ "processId": null, "capabilities": {} }));
    assert_eq!(again["error"]["code"], -32600, "{again}");
    client.notify("initialized", json!({}));

    // Not JSON: a parse error with a null id; not a message: an invalid request with its id
    client.send_body("{bad}");
    let parse_error = client.wait_for(|message| message["error"]["code"] == -32700);
    assert_eq!(parse_error["id"], Value::Null);
    client.send_body(r#"{"jsonrpc":"2.0","id":41,"method":7}"#);
    let invalid = client.wait_for(|message| message["id"] == 41);
    assert_eq!(invalid["error"]["code"], -32600, "{invalid}");

    assert_eq!(client.stop(), Some(0));
}
