//! `anvil lsp`: a Language Server Protocol 3.17 server over stdin and stdout for anvil.toml and tests.toml.
//!
//! Diagnostics come from the real loader, run as `anvil targets` runs it (nothing fetched, installed, built or
//! written) on the editor's unsaved text (crate::lsp::session), so they are exactly what `anvil build` would say
//! when it loads the project; crate::lsp::locate places each one. Checks run on a worker thread, 200 ms after the
//! last edit (at once on save); completion, hover, navigation and symbols answer from the text and the last check.

mod actions;
mod analysis;
mod completion;
#[cfg(test)]
mod fuzz;
mod hover;
mod locate;
mod navigation;
mod scan;
mod schema_info;
pub mod session;
mod symbols;
mod text;
mod transport;
mod uri;
mod view;

use analysis::{Analysis, CheckSettings, Problem, Severity};
use crossbeam_channel::{Receiver, Sender};
use locate::SourceFile;
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use schema_info::FileKind;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use text::{Encoding, Text};

use crate::project::{MANIFEST_NAME, TESTS_FILE_NAME};
use crate::util;

// How long after an edit the project is checked again
const DEBOUNCE: Duration = Duration::from_millis(200);

// LSP error codes lsp-server does not name
const SERVER_NOT_INITIALIZED: i32 = -32002;
const REQUEST_CANCELLED: i32 = -32800;
const REQUEST_FAILED: i32 = -32803;

// Source files whose creation or deletion changes what globs match
const SOURCE_GLOB: &str = "**/*.{c,cc,cpp,cxx,c++,cu,h,hh,hpp,hxx,inl,ipp,rc,asm,s,S}";

/// Runs the server until the client says `exit`: 0 after a `shutdown`, 1 otherwise.
pub fn run(verbose: bool) -> anyhow::Result<i32> {
    let (connection, io_threads) = transport::stdio();

    let code = match Server::start(&connection, verbose) {
        Ok(Some(mut server)) => {
            let code = server.serve();
            drop(server);
            code
        }
        // `exit` or the end of the input before `initialize`
        Ok(None) => 1,
        Err(error) => {
            eprintln!("anvil lsp: {error}");
            return Ok(1);
        }
    };

    // The reader has stopped (exit, or the client went away): let the writer send what is left
    drop(connection);
    io_threads.join();
    Ok(code)
}

/// An open anvil.toml or tests.toml.
struct Document {
    uri: String,
    path: PathBuf,
    text: Text,
    version: i32,
    /// The project root it belongs to, by path key.
    root: String,
}

/// A project: a root package or workspace, checked as a whole.
struct Root {
    path: PathBuf,
    analysis: Option<Arc<Analysis>>,
    /// The open documents' versions the analysis saw.
    versions: HashMap<String, i32>,
    /// When to check it next.
    due: Option<Instant>,
    running: bool,
    /// From a workspace folder: kept while no document of it is open.
    from_folder: bool,
    /// Whether the next check walks folders again (else it reuses the last check's walks: only text changed).
    walk_again: bool,
    /// The analysis was made before the compiler was known (files only): a pull waits for the next one.
    provisional: bool,
}

/// What was last reported for one file: each project's diagnostics of it, merged.
struct Report {
    uri: String,
    by_root: HashMap<String, Vec<Value>>,
    sent: Vec<Value>,
    result_id: String,
}

/// A `textDocument/diagnostic` request waiting for its check.
struct Parked {
    id: RequestId,
    uri: String,
    previous: Option<String>,
}

struct Job {
    root: String,
    start: PathBuf,
    texts: HashMap<String, (PathBuf, String)>,
    own: Vec<PathBuf>,
    versions: HashMap<String, i32>,
    settings: CheckSettings,
    reuse_walks: bool,
}

enum Event {
    Analyzed {
        root: String,
        analysis: Box<Analysis>,
        versions: HashMap<String, i32>,
        with_compiler: bool,
    },
    ToolchainReady,
    Ports(Vec<String>),
}

struct Server<'a> {
    connection: &'a Connection,
    encoding: Encoding,
    verbose: bool,
    trace: bool,

    // What the client can do
    pull: bool,
    refresh_support: bool,
    hierarchical_symbols: bool,
    snippets: bool,
    hover_markdown: bool,
    documentation_markdown: bool,
    watch_files: bool,
    workspace_folders: Vec<PathBuf>,
    /// How the client writes drives in URIs, for the files it did not name.
    uri_style: Option<uri::Style>,

    settings: CheckSettings,
    documents: HashMap<String, Document>,
    roots: HashMap<String, Root>,
    reports: HashMap<String, Report>,
    parked: Vec<Parked>,
    queue: VecDeque<Message>,
    initialized: bool,
    shutdown: bool,
    toolchain_ready: bool,

    jobs: Sender<Job>,
    events: Receiver<Event>,
    ports: Arc<Vec<String>>,
    next_request: i32,
    next_result: u64,
}

impl<'a> Server<'a> {
    /// Waits for `initialize` and answers it; None when the client exits first. What comes between the answer and
    /// `initialized` is handled as usual by the message loop.
    fn start(connection: &'a Connection, verbose: bool) -> anyhow::Result<Option<Server<'a>>> {
        // Before `initialize`: requests are refused, notifications dropped, `exit` ends the server
        let (id, params) = loop {
            let Ok(message) = connection.receiver.recv() else { return Ok(None) };
            match message {
                Message::Request(request) if request.method == "initialize" => break (request.id, request.params),
                Message::Request(request) => {
                    let refused = Response::new_err(request.id, SERVER_NOT_INITIALIZED, "the server is not initialized yet".into());
                    let _ = connection.sender.send(Message::Response(refused));
                }
                Message::Notification(notification) if notification.method == "exit" => return Ok(None),
                _ => {}
            }
        };
        let capabilities = &params["capabilities"];

        // UTF-8 positions when the client offers them, else UTF-16 (the protocol's default)
        let offered = capabilities["general"]["positionEncodings"].as_array().cloned().unwrap_or_default();
        let encoding = match offered.iter().any(|kind| kind == "utf-8") {
            true => Encoding::Utf8,
            false => Encoding::Utf16,
        };

        let options = &params["initializationOptions"];
        let mut settings = CheckSettings::default();
        let trace = read_settings(options, true, &mut settings).unwrap_or(false) || params["trace"] == "verbose";

        // Workspace folders, else the root URI
        let mut folder_uris: Vec<&str> = params["workspaceFolders"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|folder| folder["uri"].as_str())
            .collect();
        if folder_uris.is_empty() {
            folder_uris.extend(params["rootUri"].as_str());
        }
        let workspace_folders: Vec<PathBuf> = folder_uris.iter().filter_map(|folder| uri::to_path(folder)).collect();
        let uri_style = folder_uris.iter().find_map(|folder| uri::Style::of(folder));

        let result = json!({
            "capabilities": capabilities_json(encoding),
            "serverInfo": { "name": "anvil", "version": env!("CARGO_PKG_VERSION") },
        });
        connection.sender.send(Message::Response(Response::new_ok(id, result)))?;

        let (jobs, job_receiver) = crossbeam_channel::unbounded::<Job>();
        let (events_sender, events) = crossbeam_channel::unbounded::<Event>();
        let toolchain_ready = Arc::new(AtomicBool::new(false));

        let offers_markdown = |formats: &Value| {
            formats
                .as_array()
                .is_some_and(|formats| formats.iter().any(|format| format == "markdown"))
        };
        let mut server = Server {
            connection,
            encoding,
            verbose,
            trace,
            pull: !capabilities["textDocument"]["diagnostic"].is_null(),
            refresh_support: capabilities["workspace"]["diagnostics"]["refreshSupport"] == true,
            hierarchical_symbols: capabilities["textDocument"]["documentSymbol"]["hierarchicalDocumentSymbolSupport"] == true,
            snippets: capabilities["textDocument"]["completion"]["completionItem"]["snippetSupport"] == true,
            hover_markdown: offers_markdown(&capabilities["textDocument"]["hover"]["contentFormat"]),
            documentation_markdown: offers_markdown(&capabilities["textDocument"]["completion"]["completionItem"]["documentationFormat"]),
            watch_files: capabilities["workspace"]["didChangeWatchedFiles"]["dynamicRegistration"] == true,
            workspace_folders: Vec::new(),
            uri_style,
            settings,
            documents: HashMap::new(),
            roots: HashMap::new(),
            reports: HashMap::new(),
            parked: Vec::new(),
            queue: VecDeque::new(),
            initialized: false,
            shutdown: false,
            toolchain_ready: false,
            jobs,
            events,
            ports: Arc::new(Vec::new()),
            next_request: 0,
            next_result: 0,
        };

        // The checks' thread, the compiler's detection (cached after the first time), vcpkg's ports
        let worker_events = events_sender.clone();
        let worker_ready = toolchain_ready.clone();
        std::thread::Builder::new()
            .name("anvil-lsp-check".into())
            .spawn(move || check_jobs(job_receiver, worker_events, worker_ready))?;
        let requested = server.settings.toolchain.clone();
        let ready_events = events_sender.clone();
        std::thread::Builder::new().name("anvil-lsp-toolchain".into()).spawn(move || {
            let requested = requested.or_else(|| crate::settings::Settings::load().toolchain);

            // For the tests: a slow first detection (a cold vcvarsall.bat)
            #[cfg(debug_assertions)]
            if let Some(delay) = std::env::var("ANVIL_LSP_TEST_TOOLCHAIN_DELAY_MS")
                .ok()
                .and_then(|delay| delay.parse().ok())
            {
                std::thread::sleep(Duration::from_millis(delay));
            }

            // Reported whatever happens: pulls wait for it
            let found = std::panic::catch_unwind(|| crate::toolchain::Toolchain::find(requested.as_deref(), &crate::platform::Platform::host()));
            match found {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => eprintln!("anvil lsp: no compiler found ({error:#}); checks that need one are skipped"),
                Err(panic) => eprintln!("anvil lsp: finding the compiler failed: {}", panic_message(&panic)),
            }
            toolchain_ready.store(true, Ordering::SeqCst);
            let _ = ready_events.send(Event::ToolchainReady);
        })?;
        std::thread::Builder::new().name("anvil-lsp-ports".into()).spawn(move || {
            let _ = events_sender.send(Event::Ports(vcpkg_ports()));
        })?;

        // The workspace folders' projects, checked right away (their diagnostics, their targets as symbols)
        server.add_folders(workspace_folders);

        server.log(&format!(
            "anvil lsp {} started ({} positions, {} diagnostics)",
            env!("CARGO_PKG_VERSION"),
            if encoding == Encoding::Utf8 { "UTF-8" } else { "UTF-16" },
            if server.pull { "pull" } else { "push" }
        ));
        Ok(Some(server))
    }

    /// The message loop; returns the exit code.
    fn serve(&mut self) -> i32 {
        loop {
            // Take what has arrived; cancellations apply before the requests they cancel run
            while let Ok(message) = self.connection.receiver.try_recv() {
                self.queue.push_back(message);
            }
            while let Ok(event) = self.events.try_recv() {
                self.on_event(event);
            }
            self.apply_cancellations();

            if let Some(message) = self.queue.pop_front() {
                if let Some(code) = self.handle(message) {
                    return code;
                }
                continue;
            }

            // Checks start once what has arrived is handled (a close and a reopen are one change, not two checks)
            self.schedule();

            // Wait for a message, a finished check, or the next check to be due
            let timeout = self
                .next_due()
                .map_or(Duration::from_secs(3600), |due| due.saturating_duration_since(Instant::now()));
            crossbeam_channel::select! {
                recv(self.connection.receiver) -> message => match message {
                    Ok(message) => self.queue.push_back(message),
                    Err(_) => return 1,
                },
                recv(self.events) -> event => {
                    if let Ok(event) = event {
                        self.on_event(event);
                    }
                }
                default(timeout) => {}
            }
        }
    }

    /// One message; Some(exit code) at `exit`.
    fn handle(&mut self, message: Message) -> Option<i32> {
        match message {
            Message::Request(request) => {
                if self.shutdown {
                    self.respond_error(request.id, ErrorCode::InvalidRequest as i32, "the server is shutting down".into());
                    return None;
                }
                if request.method == "shutdown" {
                    self.shutdown = true;
                    self.respond(request.id, Value::Null);
                    return None;
                }

                // A panic answers the request with an error; the server goes on
                let id = request.id.clone();
                let method = request.method.clone();
                let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| self.handle_request(&request)));
                match outcome {
                    Ok(Ok(Some(result))) => self.respond(id, result),
                    Ok(Ok(None)) => {}
                    Ok(Err((code, message))) => self.respond_error(id, code, message),
                    Err(panic) => {
                        let message = panic_message(&panic);
                        self.log(&format!("{method} failed: {message}"));
                        self.respond_error(id, ErrorCode::InternalError as i32, format!("anvil lsp: {method} failed: {message}"));
                    }
                }
            }
            Message::Notification(notification) => {
                if notification.method == "exit" {
                    return Some(if self.shutdown { 0 } else { 1 });
                }
                let method = notification.method.clone();
                if let Err(panic) = std::panic::catch_unwind(AssertUnwindSafe(|| self.handle_notification(notification))) {
                    self.log(&format!("{method} failed: {}", panic_message(&panic)));
                }
            }
            // Answers to our requests (registrations, refreshes): nothing to do
            Message::Response(_) => {}
        }

        None
    }

    /// Ok(None): answered later (a parked diagnostic request).
    fn handle_request(&mut self, request: &Request) -> Result<Option<Value>, (i32, String)> {
        let params = &request.params;
        let uri = params["textDocument"]["uri"].as_str().unwrap_or_default().to_string();

        match request.method.as_str() {
            "textDocument/completion" => Ok(Some(self.completion(&uri, &params["position"]))),
            "textDocument/hover" => Ok(Some(self.hover(&uri, &params["position"]))),
            "textDocument/definition" | "textDocument/declaration" => Ok(Some(self.definition(&uri, &params["position"]))),
            "textDocument/references" => {
                let declarations = params["context"]["includeDeclaration"] != false;
                Ok(Some(self.references(&uri, &params["position"], declarations)))
            }
            "textDocument/prepareRename" => Ok(Some(self.prepare_rename(&uri, &params["position"]))),
            "textDocument/rename" => {
                let new_name = params["newName"].as_str().unwrap_or_default();
                self.rename(&uri, &params["position"], new_name).map(Some)
            }
            "textDocument/documentSymbol" => Ok(Some(self.document_symbols(&uri))),
            "workspace/symbol" => Ok(Some(self.workspace_symbols(params["query"].as_str().unwrap_or_default()))),
            "textDocument/codeAction" => Ok(Some(self.code_actions(&uri, params))),
            "textDocument/semanticTokens/full" => Ok(Some(self.semantic_tokens(&uri))),
            "textDocument/diagnostic" => {
                let previous = params["previousResultId"].as_str().map(str::to_string);
                Ok(self.pull_document(request.id.clone(), &uri, previous))
            }
            "workspace/diagnostic" => Ok(Some(self.pull_workspace(&params["previousResultIds"]))),
            "initialize" => Err((ErrorCode::InvalidRequest as i32, "the server is initialized already".to_string())),
            // For the tests: a handler that panics must leave the server running
            #[cfg(debug_assertions)]
            "anvil/debugPanic" => panic!("a panic on request"),
            _ => Err((ErrorCode::MethodNotFound as i32, format!("unknown method {}", request.method))),
        }
    }

    fn handle_notification(&mut self, notification: Notification) {
        let params = &notification.params;

        match notification.method.as_str() {
            "initialized" => {
                if !std::mem::replace(&mut self.initialized, true) {
                    self.register_watchers();
                }
            }
            "textDocument/didOpen" => {
                let document = &params["textDocument"];
                let uri = document["uri"].as_str().unwrap_or_default().to_string();
                let text = document["text"].as_str().unwrap_or_default().to_string();
                let version = document["version"].as_i64().unwrap_or(0) as i32;
                self.open(uri, text, version);
            }
            "textDocument/didChange" => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or_default().to_string();
                let encoding = self.encoding;
                let Some(document) = self.documents.get_mut(&uri) else { return };
                for change in params["contentChanges"].as_array().into_iter().flatten() {
                    let range = serde_json::from_value::<lsp_types::Range>(change["range"].clone()).ok();
                    document.text.apply(range, change["text"].as_str().unwrap_or_default(), encoding);
                }
                document.version = params["textDocument"]["version"].as_i64().unwrap_or(document.version as i64 + 1) as i32;
                let root = document.root.clone();
                self.mark_due(&root, DEBOUNCE);
            }
            "textDocument/didSave" => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or_default();
                if let Some(text) = params["text"].as_str() {
                    if let Some(document) = self.documents.get_mut(uri) {
                        if document.text.text != text {
                            document.text = Text::new(text.to_string());
                        }
                    }
                }

                // A saved manifest may move documents to another project (a workspace's members changed)
                let saved_manifest = self.documents.get(uri).is_some_and(|document| is_manifest(&document.path));
                if saved_manifest {
                    self.relocate_documents();
                }
                if let Some(root) = self.documents.get(uri).map(|document| document.root.clone()) {
                    self.mark_due(&root, Duration::ZERO);
                }
            }
            "textDocument/didClose" => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or_default();
                if let Some(document) = self.documents.remove(uri) {
                    self.mark_due(&document.root, Duration::ZERO);
                }

                // A project outside the workspace folders goes with its last document, and its diagnostics with it
                self.drop_unused_roots();
            }
            "workspace/didChangeWatchedFiles" => {
                let paths: Vec<PathBuf> = params["changes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|change| change["uri"].as_str().and_then(uri::to_path))
                    .collect();
                self.files_changed(&paths);
            }
            "workspace/didChangeConfiguration" => {
                let settings = &params["settings"];
                if let Some(trace) = read_settings(settings, false, &mut self.settings) {
                    self.trace = trace;
                }
                let keys: Vec<String> = self.roots.keys().cloned().collect();
                for key in keys {
                    self.mark_due(&key, Duration::ZERO);
                }
            }
            "workspace/didChangeWorkspaceFolders" => {
                let folders = |list: &Value| -> Vec<PathBuf> {
                    list.as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|folder| folder["uri"].as_str().and_then(uri::to_path))
                        .collect()
                };
                self.remove_folders(&folders(&params["event"]["removed"]));
                self.add_folders(folders(&params["event"]["added"]));
            }
            "$/setTrace" => self.trace = params["value"] == "verbose" || params["value"] == "messages",
            // Cancellations were applied before; anything else is not ours to act on
            _ => {}
        }
    }

    // Documents and projects

    /// Watches manifests, tests.toml files and the sources globs match (once the client is initialized).
    fn register_watchers(&mut self) {
        if !self.watch_files {
            return;
        }

        let watchers = json!([
            { "globPattern": "**/anvil.toml" },
            { "globPattern": "**/tests.toml" },
            { "globPattern": SOURCE_GLOB, "kind": 5 },
        ]);
        self.request(
            "client/registerCapability",
            json!({ "registrations": [{
                "id": "anvil-watch",
                "method": "workspace/didChangeWatchedFiles",
                "registerOptions": { "watchers": watchers },
            }]}),
        );
    }

    /// Workspace folders opened: their projects are checked right away (their diagnostics, their targets as symbols).
    fn add_folders(&mut self, folders: Vec<PathBuf>) {
        for folder in folders {
            if self
                .workspace_folders
                .iter()
                .any(|known| util::path_key(known) == util::path_key(&folder))
            {
                continue;
            }
            self.workspace_folders.push(folder.clone());

            if folder.join(MANIFEST_NAME).is_file() {
                let root = self.find_root(&folder);
                let key = util::path_key(&root);
                self.root_entry(&key, &root).from_folder = true;
                self.mark_due(&key, Duration::ZERO);
            }
        }
    }

    /// Workspace folders closed: their projects go, with their diagnostics, unless an open document holds them.
    fn remove_folders(&mut self, folders: &[PathBuf]) {
        if folders.is_empty() {
            return;
        }
        let removed: HashSet<String> = folders.iter().map(|folder| util::path_key(folder)).collect();
        self.workspace_folders.retain(|folder| !removed.contains(&util::path_key(folder)));

        // The projects the remaining folders hold
        let mut kept = HashSet::new();
        for folder in self.workspace_folders.clone() {
            if folder.join(MANIFEST_NAME).is_file() {
                kept.insert(util::path_key(&self.find_root(&folder)));
            }
        }
        for (key, root) in self.roots.iter_mut() {
            root.from_folder = kept.contains(key);
        }

        self.relocate_documents();
    }

    fn open(&mut self, uri: String, text: String, version: i32) {
        if self.uri_style.is_none() {
            self.uri_style = uri::Style::of(&uri);
        }
        let Some(path) = uri::to_path(&uri) else { return };
        let Some(name) = path.file_name().map(|name| name.to_string_lossy().to_lowercase()) else {
            return;
        };
        if name != MANIFEST_NAME && name != TESTS_FILE_NAME {
            return;
        }

        self.documents.insert(
            uri.clone(),
            Document {
                uri: uri.clone(),
                path: path.clone(),
                text: Text::new(text),
                version,
                root: String::new(),
            },
        );

        // A tests.toml outside any Anvil package is someone else's file
        let folder = path.parent().unwrap_or(&path).to_path_buf();
        let in_package = folder
            .ancestors()
            .any(|ancestor| ancestor.join(MANIFEST_NAME).is_file() || self.is_open(&ancestor.join(MANIFEST_NAME)));
        if !in_package {
            return;
        }

        let root = self.find_root(&folder);
        let key = util::path_key(&root);
        self.root_entry(&key, &root);
        if let Some(document) = self.documents.get_mut(&uri) {
            document.root = key.clone();
        }

        // Checked again even at the version last checked: a close and a reopen at version 1 (as VS Code does) may
        // have been checked without it in between
        self.mark_due(&key, Duration::ZERO);
    }

    fn is_open(&self, path: &Path) -> bool {
        let key = util::path_key(path);
        self.documents.values().any(|document| util::path_key(&document.path) == key)
    }

    /// The project root of a folder, as the CLI finds it (with the editor's unsaved manifests).
    fn find_root(&self, folder: &Path) -> PathBuf {
        let texts = self.open_texts();
        let session = session::Session {
            texts: texts.into_iter().map(|(key, (_, text))| (key, text)).collect(),
            ..Default::default()
        };
        let (found, _) = session::run(session, || crate::project::locate(folder));

        match found {
            Ok((root, _)) => root,
            Err(_) => folder.to_path_buf(),
        }
    }

    fn root_entry(&mut self, key: &str, path: &Path) -> &mut Root {
        self.roots.entry(key.to_string()).or_insert_with(|| Root {
            path: path.to_path_buf(),
            analysis: None,
            versions: HashMap::new(),
            due: None,
            running: false,
            from_folder: false,
            walk_again: true,
            provisional: false,
        })
    }

    /// Finds every open document's project again (a manifest was saved or created).
    fn relocate_documents(&mut self) {
        let uris: Vec<String> = self.documents.keys().cloned().collect();
        for uri in uris {
            let Some(folder) = self
                .documents
                .get(&uri)
                .and_then(|document| document.path.parent().map(Path::to_path_buf))
            else {
                continue;
            };
            let root = self.find_root(&folder);
            let key = util::path_key(&root);
            let changed = self
                .documents
                .get(&uri)
                .is_some_and(|document| document.root != key && !document.root.is_empty());
            self.root_entry(&key, &root);
            if let Some(document) = self.documents.get_mut(&uri) {
                if document.root != key {
                    let old = std::mem::replace(&mut document.root, key.clone());
                    if changed {
                        self.mark_due(&old, Duration::ZERO);
                    }
                    self.mark_due(&key, Duration::ZERO);
                }
            }
        }

        self.drop_unused_roots();
    }

    /// Projects no document and no workspace folder holds any more: their diagnostics go (a running one's when its
    /// check ends).
    fn drop_unused_roots(&mut self) {
        let unused: Vec<String> = self
            .roots
            .iter()
            .filter(|(key, root)| !root.running && !self.holds(key, root))
            .map(|(key, _)| key.clone())
            .collect();
        for key in unused {
            self.roots.remove(&key);
            self.clear_root_reports(&key);
        }
        if !self.parked.is_empty() {
            self.answer_parked();
        }
    }

    /// Whether a workspace folder or an open document keeps a project.
    fn holds(&self, key: &str, root: &Root) -> bool {
        root.from_folder || self.documents.values().any(|document| document.root == key)
    }

    /// Files changed on disk: the projects that read them, or hold them, are checked again.
    fn files_changed(&mut self, paths: &[PathBuf]) {
        let mut manifest_changed = false;
        let mut due = Vec::new();

        for path in paths {
            let key = util::path_key(path);
            let name = path.file_name().map(|name| name.to_string_lossy().to_lowercase()).unwrap_or_default();
            manifest_changed |= name == MANIFEST_NAME;

            for (root_key, root) in &self.roots {
                let read = root
                    .analysis
                    .as_ref()
                    .is_some_and(|analysis| analysis.files.iter().any(|file| util::path_key(&file.path) == key));
                let inside = util::strip_prefix_ci(path, &root.path).is_some();
                if read || inside {
                    due.push(root_key.clone());
                }
            }
        }

        if manifest_changed {
            self.relocate_documents();
        }
        for key in due {
            if let Some(root) = self.roots.get_mut(&key) {
                root.walk_again = true;
            }
            self.mark_due(&key, DEBOUNCE);
        }
    }

    fn open_texts(&self) -> HashMap<String, (PathBuf, String)> {
        self.documents
            .values()
            .map(|document| (util::path_key(&document.path), (document.path.clone(), document.text.text.clone())))
            .collect()
    }

    // Checks

    fn mark_due(&mut self, key: &str, after: Duration) {
        if let Some(root) = self.roots.get_mut(key) {
            // Only an edit's check (debounced) may reuse folder walks
            root.walk_again |= after.is_zero();
            let when = Instant::now() + after;
            root.due = Some(match root.due {
                Some(earlier) if after.is_zero() => earlier.min(when),
                _ => when,
            });
        }
    }

    fn next_due(&self) -> Option<Instant> {
        self.roots.values().filter(|root| !root.running).filter_map(|root| root.due).min()
    }

    /// Starts the checks that are due (one project at a time on the worker; the others wait their turn).
    fn schedule(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self
            .roots
            .iter()
            .filter(|(_, root)| !root.running && root.due.is_some_and(|due| due <= now))
            .map(|(key, _)| key.clone())
            .collect();

        for key in due {
            let texts = self.open_texts();
            let own: Vec<PathBuf> = self
                .documents
                .values()
                .filter(|document| document.root == key)
                .map(|document| document.path.clone())
                .collect();
            let versions: HashMap<String, i32> = self.documents.values().map(|document| (document.uri.clone(), document.version)).collect();
            let Some(root) = self.roots.get_mut(&key) else { continue };
            root.due = None;
            root.running = true;
            let reuse_walks = !root.walk_again;
            root.walk_again = false;

            let job = Job {
                root: key.clone(),
                start: root.path.clone(),
                texts,
                own,
                versions,
                settings: self.settings.clone(),
                reuse_walks,
            };
            if self.jobs.send(job).is_err() {
                self.log("the check thread is gone");
            }
        }
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Analyzed {
                root,
                analysis,
                versions,
                with_compiler,
            } => self.on_analysis(root, *analysis, versions, with_compiler),
            Event::ToolchainReady => {
                // Projects checked without the compiler (a running one is checked again when it ends)
                self.toolchain_ready = true;
                let provisional: Vec<String> = self
                    .roots
                    .iter()
                    .filter(|(_, root)| root.provisional && !root.running)
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in provisional {
                    self.mark_due(&key, Duration::ZERO);
                }
            }
            Event::Ports(ports) => {
                self.log(&format!("{} vcpkg ports known", ports.len()));
                self.ports = Arc::new(ports);
            }
        }
    }

    fn on_analysis(&mut self, key: String, mut analysis: Analysis, versions: HashMap<String, i32>, with_compiler: bool) {
        // A project forgotten while it was checked has nothing to report
        let Some(root) = self.roots.get_mut(&key) else { return };
        root.running = false;
        if analysis.model.is_none() {
            analysis.model = root.analysis.as_ref().and_then(|previous| previous.model.clone());
        }
        let elapsed = analysis.elapsed;
        let analysis = Arc::new(analysis);
        root.analysis = Some(analysis.clone());
        root.versions = versions.clone();
        root.provisional = !with_compiler;

        // Nothing holds it any more (its folder closed, its last document too, while it ran)
        if let Some(root) = self.roots.get(&key) {
            if !self.holds(&key, root) {
                self.drop_unused_roots();
                return;
            }
        }

        // Checked again when the compiler became known meanwhile, or when an open document changed and nothing is due
        // to check it (a document reopened while its close was being checked)
        let stale = self
            .documents
            .values()
            .any(|document| document.root == key && versions.get(&document.uri) != Some(&document.version));
        let due = self.roots.get(&key).is_some_and(|root| root.due.is_some());
        if (!with_compiler && self.toolchain_ready) || (stale && !due) {
            self.mark_due(&key, Duration::ZERO);
        }

        self.log(&format!(
            "checked {} in {} ms: {} problems{}",
            util::slash(&analysis.root),
            elapsed.as_millis(),
            analysis.problems.len(),
            match (with_compiler, analysis.complete) {
                (false, _) => " (files only: no compiler yet)",
                (true, false) => " (files only: they do not parse)",
                (true, true) => "",
            }
        ));

        // Each file's diagnostics from this project, for files whose text is the one checked
        let mut by_file: HashMap<String, (PathBuf, Vec<Value>)> = HashMap::new();
        for file in &analysis.files {
            by_file.insert(util::path_key(&file.path), (file.path.clone(), Vec::new()));
        }
        let texts = self.analysis_texts(&analysis);
        for problem in &analysis.problems {
            let file_key = util::path_key(&problem.file);
            let Some(text) = texts.get(&file_key) else { continue };
            let diagnostic = self.diagnostic_json(problem, text, &texts);
            by_file
                .entry(file_key)
                .or_insert_with(|| (problem.file.clone(), Vec::new()))
                .1
                .push(diagnostic);
        }

        // Files reported before and not now are cleared
        for (file_key, report) in &self.reports {
            if report.by_root.contains_key(&key) && !by_file.contains_key(file_key) {
                if let Some(path) = uri::to_path(&report.uri) {
                    by_file.insert(file_key.clone(), (path, Vec::new()));
                }
            }
        }

        let mut changed = false;
        for (file_key, (path, diagnostics)) in by_file {
            // An open document edited since: its next check reports it
            let document = self.documents.values().find(|document| util::path_key(&document.path) == file_key);
            if let Some(document) = document {
                if versions.get(&document.uri) != Some(&document.version) {
                    continue;
                }
            }
            changed |= self.update_report(&file_key, &path, &key, diagnostics);
        }

        self.answer_parked();

        if changed && self.pull && self.refresh_support {
            self.request("workspace/diagnostic/refresh", Value::Null);
        }
    }

    /// Answers the parked pulls whose projects' checks are current now.
    fn answer_parked(&mut self) {
        let parked = std::mem::take(&mut self.parked);
        for waiting in parked {
            let answered = self.document_report(&waiting.uri, waiting.previous.as_deref());
            match answered {
                Some(report) => self.respond(waiting.id, report),
                None => self.parked.push(waiting),
            }
        }
    }

    /// Each file's text as the check read it, as line tables.
    fn analysis_texts(&self, analysis: &Analysis) -> HashMap<String, Text> {
        analysis
            .files
            .iter()
            .map(|file| (util::path_key(&file.path), Text::new(file.text.clone())))
            .collect()
    }

    /// Stores one project's diagnostics of a file; publishes the merged list when it changed (push clients).
    fn update_report(&mut self, file_key: &str, path: &Path, root: &str, diagnostics: Vec<Value>) -> bool {
        let uri = self.uri_of(path);
        let report = self.reports.entry(file_key.to_string()).or_insert_with(|| Report {
            uri: uri.clone(),
            by_root: HashMap::new(),
            sent: Vec::new(),
            result_id: "0".into(),
        });

        // Known to the client by another spelling now (the document opened as file:///c%3A/...): the old one clears
        let renamed = report.uri != uri;
        let old_uri = std::mem::replace(&mut report.uri, uri);
        let mut cleared = false;
        if renamed && !report.sent.is_empty() {
            cleared = true;
            report.sent.clear();
            if !self.pull {
                self.notify("textDocument/publishDiagnostics", json!({ "uri": old_uri, "diagnostics": [] }));
            }
        }
        let Some(report) = self.reports.get_mut(file_key) else { return cleared };
        report.by_root.insert(root.to_string(), diagnostics);

        // The same diagnostic from two projects (a shared dependency) once
        let mut merged: Vec<Value> = Vec::new();
        let mut roots: Vec<&String> = report.by_root.keys().collect();
        roots.sort();
        for root in roots {
            for diagnostic in &report.by_root[root] {
                if !merged.contains(diagnostic) {
                    merged.push(diagnostic.clone());
                }
            }
        }
        if merged == report.sent && report.result_id != "0" && !cleared {
            return false;
        }

        self.next_result += 1;
        report.result_id = self.next_result.to_string();
        report.sent = merged.clone();
        let uri = report.uri.clone();
        if !self.pull {
            let version = self.documents.get(&uri).map(|document| document.version);
            let mut params = json!({ "uri": uri, "diagnostics": merged });
            if let Some(version) = version {
                params["version"] = json!(version);
            }
            self.notify("textDocument/publishDiagnostics", params);
        }

        true
    }

    fn clear_root_reports(&mut self, root: &str) {
        let files: Vec<(String, PathBuf)> = self
            .reports
            .iter()
            .filter(|(_, report)| report.by_root.contains_key(root))
            .filter_map(|(key, report)| uri::to_path(&report.uri).map(|path| (key.clone(), path)))
            .collect();
        for (key, path) in files {
            self.update_report(&key, &path, root, Vec::new());
            if let Some(report) = self.reports.get_mut(&key) {
                report.by_root.remove(root);
            }
        }
    }

    /// The URI a file is known by: the client's for an open document, else its file URI.
    fn uri_of(&self, path: &Path) -> String {
        let key = util::path_key(path);
        self.documents
            .values()
            .find(|document| util::path_key(&document.path) == key)
            .map(|document| document.uri.clone())
            .or_else(|| self.reports.get(&key).map(|report| report.uri.clone()))
            .unwrap_or_else(|| uri::from_path(path, self.uri_style.unwrap_or_default()))
    }

    fn diagnostic_json(&self, problem: &Problem, text: &Text, texts: &HashMap<String, Text>) -> Value {
        let severity = match problem.severity {
            Severity::Error => 1,
            Severity::Warning => 2,
            Severity::Information => 3,
        };
        let mut diagnostic = json!({
            "range": text.range(&problem.span, self.encoding),
            "severity": severity,
            "code": problem.code,
            "source": "anvil",
            "message": presented_message(&problem.message, &problem.file),
        });

        let related: Vec<Value> = problem
            .related
            .iter()
            .filter_map(|(path, span, message)| {
                let other = texts.get(&util::path_key(path))?;
                Some(json!({
                    "location": { "uri": self.uri_of(path), "range": other.range(span, self.encoding) },
                    "message": message,
                }))
            })
            .collect();
        if !related.is_empty() {
            diagnostic["relatedInformation"] = json!(related);
        }
        if problem.code == "unused-dep" {
            diagnostic["tags"] = json!([1]);
        }
        if let Some((span, word)) = &problem.replacement {
            diagnostic["data"] = json!({ "replace": { "range": text.range(span, self.encoding), "text": word } });
        }

        diagnostic
    }

    // Requests

    /// Runs `work` with the view of a document: its current text, the project's other files, the last model.
    fn with_view<T>(&self, uri: &str, work: impl FnOnce(&View, &Document) -> T) -> Option<T> {
        let document = self.documents.get(uri)?;
        let analysis = self.roots.get(&document.root).and_then(|root| root.analysis.as_deref());
        let document_key = util::path_key(&document.path);

        let mut current = SourceFile::new(document.path.clone(), document.text.text.clone(), String::new());
        current.package = package_of(&current, analysis);

        let mut files = vec![&current];
        if let Some(analysis) = analysis {
            files.extend(analysis.files.iter().filter(|file| util::path_key(&file.path) != document_key));
        }
        let view = View {
            files,
            model: analysis.and_then(|analysis| analysis.model.as_ref()),
            ports: &self.ports,
        };

        Some(work(&view, document))
    }

    fn offset(&self, document: &Document, position: &Value) -> usize {
        let position = serde_json::from_value::<lsp_types::Position>(position.clone()).unwrap_or_default();
        document.text.offset(position, self.encoding)
    }

    fn completion(&self, uri: &str, position: &Value) -> Value {
        let encoding = self.encoding;
        let items = self.with_view(uri, |view, document| {
            let offset = self.offset(document, position);
            let cursor = document.text.position(offset, encoding);
            completion::complete(view, offset)
                .into_iter()
                .map(|item| {
                    // An edit must stay on the cursor's line and hold the cursor
                    let mut range = document.text.range(&item.edit.0, encoding);
                    if range.start.line != cursor.line || range.end.line != cursor.line || range.start > cursor || range.end < cursor {
                        range = lsp_types::Range::new(cursor, cursor);
                    }
                    // Snippets after the other items; as plain text for a client without snippets
                    let text = match item.snippet && !self.snippets {
                        true => completion::plain_text(&item.edit.1),
                        false => item.edit.1,
                    };
                    let mut json = json!({
                        "label": item.label,
                        "kind": item.kind,
                        "textEdit": { "range": range, "newText": text },
                    });
                    if item.snippet {
                        json["sortText"] = json!(format!("~{}", item.label));
                        if self.snippets {
                            json["insertTextFormat"] = json!(2);
                        }
                    }
                    if let Some(detail) = item.detail {
                        json["detail"] = json!(detail);
                    }
                    if let Some(documentation) = item.documentation {
                        json["documentation"] = match self.documentation_markdown {
                            true => json!({ "kind": "markdown", "value": documentation }),
                            false => json!(plain_text(&documentation)),
                        };
                    }
                    json
                })
                .collect::<Vec<Value>>()
        });

        json!({ "isIncomplete": false, "items": items.unwrap_or_default() })
    }

    fn hover(&self, uri: &str, position: &Value) -> Value {
        let found = self.with_view(uri, |view, document| {
            let offset = self.offset(document, position);
            hover::hover(view, offset).map(|(markdown, span)| {
                let contents = match self.hover_markdown {
                    true => json!({ "kind": "markdown", "value": markdown }),
                    false => json!({ "kind": "plaintext", "value": plain_text(&markdown) }),
                };
                json!({ "contents": contents, "range": document.text.range(&span, self.encoding) })
            })
        });

        found.flatten().unwrap_or(Value::Null)
    }

    fn definition(&self, uri: &str, position: &Value) -> Value {
        let found = self.with_view(uri, |view, document| {
            let offset = self.offset(document, position);
            match navigation::definition(view, offset)? {
                navigation::Target::Span(file, span) => Some(self.location(view, file, &span)),
                navigation::Target::File(path) => Some(json!({
                    "uri": self.uri_of(&path),
                    "range": lsp_types::Range::default(),
                })),
            }
        });

        found.flatten().unwrap_or(Value::Null)
    }

    fn references(&self, uri: &str, position: &Value, declarations: bool) -> Value {
        let found = self.with_view(uri, |view, document| {
            let offset = self.offset(document, position);
            let (symbol, _) = navigation::symbol_at(view, offset)?;
            let locations: Vec<Value> = navigation::occurrences(view, &symbol)
                .into_iter()
                .filter(|occurrence| declarations || !occurrence.definition)
                .map(|occurrence| self.location(view, occurrence.file, &occurrence.span))
                .collect();
            Some(json!(locations))
        });

        found.flatten().unwrap_or(json!([]))
    }

    fn prepare_rename(&self, uri: &str, position: &Value) -> Value {
        let found = self.with_view(uri, |view, document| {
            let offset = self.offset(document, position);
            let (symbol, span) = navigation::symbol_at(view, offset)?;
            if !navigation::can_rename(&symbol) {
                return None;
            }
            Some(json!({
                "range": document.text.range(&span, self.encoding),
                "placeholder": document.text.text[span].to_string(),
            }))
        });

        found.flatten().unwrap_or(Value::Null)
    }

    fn rename(&self, uri: &str, position: &Value, new_name: &str) -> Result<Value, (i32, String)> {
        let valid = !new_name.trim().is_empty() && !new_name.contains(['/', '\\', '"', '{', '}', '\n']);
        if !valid {
            return Err((REQUEST_FAILED, format!("`{new_name}` cannot be a name: no `/`, `\\`, quotes or braces")));
        }

        let found = self.with_view(uri, |view, document| {
            let offset = self.offset(document, position);
            let (symbol, _) = navigation::symbol_at(view, offset)?;
            if !navigation::can_rename(&symbol) {
                return None;
            }

            let mut changes: HashMap<String, Vec<Value>> = HashMap::new();
            for (file, span, text) in navigation::rename(view, &symbol, new_name) {
                let source = view.files[file];
                let range = Text::new(source.text.clone()).range(&span, self.encoding);
                changes
                    .entry(self.uri_of(&source.path))
                    .or_default()
                    .push(json!({ "range": range, "newText": text }));
            }
            Some(json!({ "changes": changes }))
        });

        found.flatten().ok_or((REQUEST_FAILED, "nothing to rename here".to_string()))
    }

    fn document_symbols(&self, uri: &str) -> Value {
        let found = self.with_view(uri, |view, document| {
            let symbols = symbols::document_symbols(view.document());
            match self.hierarchical_symbols {
                true => json!(symbols.iter().map(|symbol| self.symbol_json(&document.text, symbol)).collect::<Vec<_>>()),
                false => {
                    let mut flat = Vec::new();
                    flatten_symbols(&symbols, None, &mut |symbol, container| {
                        let mut json = json!({
                            "name": symbol.name,
                            "kind": symbol.kind,
                            "location": { "uri": document.uri, "range": document.text.range(&symbol.range, self.encoding) },
                        });
                        if let Some(container) = container {
                            json["containerName"] = json!(container);
                        }
                        flat.push(json);
                    });
                    json!(flat)
                }
            }
        });

        found.unwrap_or(json!([]))
    }

    fn symbol_json(&self, text: &Text, symbol: &symbols::Symbol) -> Value {
        let mut json = json!({
            "name": symbol.name,
            "kind": symbol.kind,
            "range": text.range(&symbol.range, self.encoding),
            "selectionRange": text.range(&symbol.selection, self.encoding),
            "children": symbol.children.iter().map(|child| self.symbol_json(text, child)).collect::<Vec<_>>(),
        });
        if let Some(detail) = &symbol.detail {
            json["detail"] = json!(detail);
        }

        json
    }

    fn workspace_symbols(&self, query: &str) -> Value {
        let mut seen = HashSet::new();
        let mut found = Vec::new();

        for root in self.roots.values() {
            let Some(analysis) = &root.analysis else { continue };
            let manifests = analysis.files.iter().filter(|file| file.kind == FileKind::Manifest);
            for (_, symbol, file) in symbols::target_symbols(manifests, query) {
                if !seen.insert((util::path_key(&file.path), symbol.name.clone())) {
                    continue;
                }
                let text = Text::new(file.text.clone());
                let mut json = json!({
                    "name": symbol.name,
                    "kind": symbol.kind,
                    "location": { "uri": self.uri_of(&file.path), "range": text.range(&symbol.selection, self.encoding) },
                });
                if let Some(container) = &symbol.detail {
                    json["containerName"] = json!(container);
                }
                found.push(json);
            }
        }

        json!(found)
    }

    fn code_actions(&self, uri: &str, params: &Value) -> Value {
        let found = self.with_view(uri, |view, document| {
            let diagnostics: Vec<&Value> = params["context"]["diagnostics"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|diagnostic| diagnostic["source"] == "anvil")
                .collect();
            let range = |value: &Value| -> Option<std::ops::Range<usize>> {
                let range = serde_json::from_value::<lsp_types::Range>(value.clone()).ok()?;
                Some(document.text.offset(range.start, self.encoding)..document.text.offset(range.end, self.encoding))
            };

            let mut actions_json = Vec::new();
            for diagnostic in diagnostics {
                let span = range(&diagnostic["range"]).unwrap_or(0..0);

                // The did-you-mean from the data sent with it; from its message and range for a client that drops data
                let sent = range(&diagnostic["data"]["replace"]["range"]).zip(diagnostic["data"]["replace"]["text"].as_str().map(str::to_string));
                let replacement = sent.or_else(|| {
                    let word = analysis::suggestion(diagnostic["message"].as_str().unwrap_or_default())?;
                    Some((misspelled_word(&document.text.text, &span)?, word))
                });
                let reported = actions::Reported {
                    span,
                    code: diagnostic["code"].as_str().unwrap_or_default().to_string(),
                    replacement,
                };
                for action in actions::actions(view.document(), std::slice::from_ref(&reported), view.ports) {
                    let edits: Vec<Value> = action
                        .edits
                        .iter()
                        .map(|(span, text)| json!({ "range": document.text.range(span, self.encoding), "newText": text }))
                        .collect();
                    actions_json.push(json!({
                        "title": action.title,
                        "kind": "quickfix",
                        "diagnostics": [diagnostic],
                        "isPreferred": action.preferred,
                        "edit": { "changes": { document.uri.clone(): edits } },
                    }));
                }
            }
            json!(actions_json)
        });

        found.unwrap_or(json!([]))
    }

    fn semantic_tokens(&self, uri: &str) -> Value {
        let found = self.with_view(uri, |view, document| {
            let mut data = Vec::new();
            let (mut line, mut character) = (0, 0);
            for (span, kind) in symbols::semantic_tokens(view) {
                let start = document.text.position(span.start, self.encoding);
                let end = document.text.position(span.end, self.encoding);
                if end.line != start.line {
                    continue;
                }
                let delta_line = start.line - line;
                let delta_start = if delta_line == 0 { start.character - character } else { start.character };
                data.extend([delta_line, delta_start, end.character - start.character, kind, 0]);
                line = start.line;
                character = start.character;
            }
            json!({ "data": data })
        });

        found.unwrap_or(json!({ "data": [] }))
    }

    /// `textDocument/diagnostic`: the document's report, or None (parked) while its project is being checked.
    fn pull_document(&mut self, id: RequestId, uri: &str, previous: Option<String>) -> Option<Value> {
        if let Some(report) = self.document_report(uri, previous.as_deref()) {
            return Some(report);
        }

        self.parked.push(Parked {
            id,
            uri: uri.to_string(),
            previous,
        });
        None
    }

    /// A document's diagnostic report when its project's check is current (and made with the compiler known).
    fn document_report(&self, uri: &str, previous: Option<&str>) -> Option<Value> {
        let Some(document) = self.documents.get(uri) else {
            return self.closed_report(uri, previous);
        };

        // A document of no project (a tests.toml of something else) has nothing to say
        let Some(root) = self.roots.get(&document.root) else {
            return Some(json!({ "kind": "full", "items": [] }));
        };
        let current =
            root.analysis.is_some() && !root.running && root.due.is_none() && !root.provisional && root.versions.get(uri) == Some(&document.version);
        if !current {
            return None;
        }

        let key = util::path_key(&document.path);
        let report = self.reports.get(&key);
        let result_id = report.map_or("0".to_string(), |report| report.result_id.clone());
        if previous == Some(result_id.as_str()) {
            return Some(json!({ "kind": "unchanged", "resultId": result_id }));
        }

        // Other files of the project with diagnostics come along
        let mut related = serde_json::Map::new();
        for (other_key, other) in &self.reports {
            if *other_key != key && other.by_root.contains_key(&document.root) && !other.sent.is_empty() {
                related.insert(
                    other.uri.clone(),
                    json!({ "kind": "full", "resultId": other.result_id, "items": other.sent }),
                );
            }
        }
        let mut json = json!({
            "kind": "full",
            "resultId": result_id,
            "items": report.map(|report| report.sent.clone()).unwrap_or_default(),
        });
        if !related.is_empty() {
            json["relatedDocuments"] = Value::Object(related);
        }

        Some(json)
    }

    /// The report of a file the client has not opened (a member's manifest a check read), once the checks that read
    /// it are current.
    fn closed_report(&self, uri: &str, previous: Option<&str>) -> Option<Value> {
        let Some(path) = uri::to_path(uri) else {
            return Some(json!({ "kind": "full", "items": [] }));
        };

        // Waits while a project that read it, or holds it, is being checked
        let key = util::path_key(&path);
        let report = self.reports.get(&key);
        let pending = self.roots.iter().any(|(root_key, root)| {
            let concerned = report.is_some_and(|report| report.by_root.contains_key(root_key)) || util::strip_prefix_ci(&path, &root.path).is_some();
            concerned && (root.running || root.due.is_some() || root.provisional)
        });
        if pending {
            return None;
        }
        let Some(report) = report else {
            return Some(json!({ "kind": "full", "items": [] }));
        };
        if previous == Some(report.result_id.as_str()) {
            return Some(json!({ "kind": "unchanged", "resultId": report.result_id }));
        }

        Some(json!({ "kind": "full", "resultId": report.result_id, "items": report.sent }))
    }

    /// `workspace/diagnostic`: every file's report as it stands.
    fn pull_workspace(&self, previous: &Value) -> Value {
        let previous: HashMap<String, String> = previous
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| Some((entry["uri"].as_str()?.to_string(), entry["value"].as_str()?.to_string())))
            .collect();

        let items: Vec<Value> = self
            .reports
            .values()
            .map(|report| {
                let version = self
                    .documents
                    .get(&report.uri)
                    .map(|document| json!(document.version))
                    .unwrap_or(Value::Null);
                if previous.get(&report.uri) == Some(&report.result_id) {
                    return json!({ "kind": "unchanged", "uri": report.uri, "version": version, "resultId": report.result_id });
                }
                json!({ "kind": "full", "uri": report.uri, "version": version, "resultId": report.result_id, "items": report.sent })
            })
            .collect();

        json!({ "items": items })
    }

    fn location(&self, view: &View, file: usize, span: &std::ops::Range<usize>) -> Value {
        let source = view.files[file];
        let range = match self
            .documents
            .values()
            .find(|document| util::path_key(&document.path) == util::path_key(&source.path))
        {
            Some(document) => document.text.range(span, self.encoding),
            None => Text::new(source.text.clone()).range(span, self.encoding),
        };

        json!({ "uri": self.uri_of(&source.path), "range": range })
    }

    // Messages

    /// `$/cancelRequest`: a queued request is answered as cancelled instead of run, a parked one at once.
    fn apply_cancellations(&mut self) {
        let mut cancelled: Vec<RequestId> = Vec::new();
        self.queue.retain(|message| match message {
            Message::Notification(notification) if notification.method == "$/cancelRequest" => {
                if let Ok(id) = serde_json::from_value::<RequestId>(notification.params["id"].clone()) {
                    cancelled.push(id);
                }
                false
            }
            _ => true,
        });

        for id in cancelled {
            let queued = self
                .queue
                .iter()
                .position(|message| matches!(message, Message::Request(request) if request.id == id));
            let parked = self.parked.iter().position(|waiting| waiting.id == id);
            if let Some(index) = queued {
                self.queue.remove(index);
            } else if let Some(index) = parked {
                self.parked.remove(index);
            } else {
                continue;
            }
            self.respond_error(id, REQUEST_CANCELLED, "cancelled".into());
        }
    }

    fn respond(&self, id: RequestId, result: Value) {
        let _ = self.connection.sender.send(Message::Response(Response::new_ok(id, result)));
    }

    fn respond_error(&self, id: RequestId, code: i32, message: String) {
        let _ = self.connection.sender.send(Message::Response(Response::new_err(id, code, message)));
    }

    fn notify(&self, method: &str, params: Value) {
        let _ = self
            .connection
            .sender
            .send(Message::Notification(Notification::new(method.to_string(), params)));
    }

    fn request(&mut self, method: &str, params: Value) {
        self.next_request += 1;
        let id = RequestId::from(format!("anvil-{}", self.next_request));
        let _ = self
            .connection
            .sender
            .send(Message::Request(Request::new(id, method.to_string(), params)));
    }

    /// stderr always; the client's log too with `--verbose` or tracing on.
    fn log(&self, message: &str) {
        if self.verbose || self.trace {
            eprintln!("anvil lsp: {message}");
            self.notify("window/logMessage", json!({ "type": 4, "message": message }));
        }
    }
}

use view::View;

/// The worker: checks projects one at a time; a panic in a check becomes a diagnostic, not a dead server.
fn check_jobs(jobs: Receiver<Job>, events: Sender<Event>, toolchain_ready: Arc<AtomicBool>) {
    let mut walks = session::Walks::default();
    for job in jobs {
        let load_model = toolchain_ready.load(Ordering::SeqCst);
        walks.reuse = job.reuse_walks;
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            analysis::analyze(&job.start, &job.texts, &job.own, &job.settings, load_model, &mut walks)
        }));
        let analysis = match outcome {
            Ok(analysis) => analysis,
            Err(panic) => analysis::failed(&job.start, &format!("the check failed: {} (please report this)", panic_message(&panic))),
        };

        let event = Event::Analyzed {
            root: job.root,
            analysis: Box::new(analysis),
            versions: job.versions,
            with_compiler: load_model,
        };
        if events.send(event).is_err() {
            return;
        }
    }
}

/// The ports of the vcpkg Anvil would use, for completion: the chosen one, VCPKG_ROOT's, else the first found.
fn vcpkg_ports() -> Vec<String> {
    let chosen = crate::settings::Settings::load()
        .vcpkg_root
        .or_else(|| std::env::var_os("VCPKG_ROOT").map(PathBuf::from))
        .filter(|root| root.join("ports").is_dir());
    let root = chosen.or_else(|| {
        crate::vcpkg::Instance::discover()
            .into_iter()
            .find(|instance| instance.has_ports)
            .map(|instance| instance.root)
    });

    let Some(root) = root else { return Vec::new() };
    let mut ports: Vec<String> = std::fs::read_dir(root.join("ports"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    ports.sort();

    ports
}

/// The server's capabilities.
fn capabilities_json(encoding: Encoding) -> Value {
    json!({
        "positionEncoding": if encoding == Encoding::Utf8 { "utf-8" } else { "utf-16" },
        "textDocumentSync": { "openClose": true, "change": 2, "save": { "includeText": true } },
        "completionProvider": { "triggerCharacters": [".", "\"", "[", "=", "{", ":"] },
        "hoverProvider": true,
        "definitionProvider": true,
        "declarationProvider": true,
        "referencesProvider": true,
        "renameProvider": { "prepareProvider": true },
        "documentSymbolProvider": true,
        "workspaceSymbolProvider": true,
        "codeActionProvider": { "codeActionKinds": ["quickfix"] },
        "semanticTokensProvider": {
            "legend": { "tokenTypes": symbols::TOKEN_TYPES, "tokenModifiers": [] },
            "full": true,
        },
        "diagnosticProvider": { "identifier": "anvil", "interFileDependencies": true, "workspaceDiagnostics": true },
        "workspace": { "workspaceFolders": { "supported": true, "changeNotifications": true } },
    })
}

/// Reads `profile`, `toolchain` and `trace` from settings: `{ "anvil": { ... } }`, or (`bare`, the initialization
/// options) the object itself. Only the keys given change (null or "" clears one); no `anvil` object changes nothing
/// (Neovim sends `{ "settings": {} }` after `initialized`). Returns the trace setting when given.
fn read_settings(value: &Value, bare: bool, settings: &mut CheckSettings) -> Option<bool> {
    let value = match value["anvil"].is_object() {
        true => &value["anvil"],
        false if bare => value,
        false => return None,
    };
    let object = value.as_object()?;

    let text = |key: &str| value[key].as_str().filter(|text| !text.is_empty()).map(str::to_string);
    if object.contains_key("profile") {
        settings.profile = text("profile");
    }
    if object.contains_key("toolchain") {
        settings.toolchain = text("toolchain");
    }

    value["trace"].as_bool()
}

/// A message as the editor shows it, beside the file: without the `package `x` (<this file>): ` the CLI puts first,
/// and without trailing line breaks.
fn presented_message(message: &str, file: &Path) -> String {
    static PREFIX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let prefix = PREFIX.get_or_init(|| regex::Regex::new(r"^package `[^`]*` \((.+?)\): ").unwrap());

    let message = message.trim_end();
    if let Some(captures) = prefix.captures(message) {
        if util::path_key(Path::new(&captures[1])) == util::path_key(file) {
            return message[captures[0].len()..].to_string();
        }
    }

    message.to_string()
}

/// Markdown as plain text, for a client that shows no markdown: no `**`, backticks or `---` rules.
fn plain_text(markdown: &str) -> String {
    markdown
        .lines()
        .filter(|line| line.trim() != "---")
        .map(|line| line.replace("**", "").replace('`', ""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The misspelled word a diagnostic's range covers (a key, or a quoted value without its quotes); None when the range
/// holds more than one word.
fn misspelled_word(text: &str, span: &std::ops::Range<usize>) -> Option<std::ops::Range<usize>> {
    let written = text.get(span.clone())?;
    let quoted = written.len() >= 2 && (written.starts_with('"') || written.starts_with('\''));
    let word = if quoted { span.start + 1..span.end - 1 } else { span.clone() };

    let one_word = text[word.clone()]
        .chars()
        .all(|character| character.is_alphanumeric() || "_-+".contains(character));
    (!word.is_empty() && one_word).then_some(word)
}

/// A file's package: a manifest's own, a tests.toml's nearest manifest's.
fn package_of(file: &SourceFile, analysis: Option<&Analysis>) -> String {
    if let Some(known) = analysis.and_then(|analysis| {
        analysis
            .files
            .iter()
            .find(|other| util::path_key(&other.path) == util::path_key(&file.path))
    }) {
        if !known.package.is_empty() && file.kind == FileKind::Tests {
            return known.package.clone();
        }
    }
    if file.kind == FileKind::Manifest {
        return analysis::package_name(file);
    }

    analysis
        .into_iter()
        .flat_map(|analysis| &analysis.files)
        .filter(|other| other.kind == FileKind::Manifest)
        .filter_map(|other| other.path.parent().map(|folder| (other, folder)))
        .filter(|(_, folder)| util::strip_prefix_ci(&file.path, folder).is_some())
        .max_by_key(|(_, folder)| folder.as_os_str().len())
        .map(|(other, _)| other.package.clone())
        .unwrap_or_default()
}

fn is_manifest(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(MANIFEST_NAME))
}

fn flatten_symbols(symbols: &[symbols::Symbol], container: Option<&str>, visit: &mut dyn FnMut(&symbols::Symbol, Option<&str>)) {
    for symbol in symbols {
        visit(symbol, container);
        flatten_symbols(&symbol.children, Some(&symbol.name), visit);
    }
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|text| text.to_string()))
        .unwrap_or_else(|| "a panic".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_change_only_where_given() {
        let mut settings = CheckSettings {
            profile: Some("release".into()),
            toolchain: Some("gcc".into()),
        };

        // No `anvil` object: nothing changes (the initialization options may be the object itself)
        read_settings(&json!({}), false, &mut settings);
        read_settings(&json!({ "profile": "debug" }), false, &mut settings);
        assert_eq!(settings.profile.as_deref(), Some("release"));
        read_settings(&json!({ "toolchain": "msvc" }), true, &mut settings);
        assert_eq!(settings.toolchain.as_deref(), Some("msvc"));

        // The keys given: a null or an empty one clears
        let trace = read_settings(&json!({ "anvil": { "profile": null, "trace": true } }), false, &mut settings);
        assert_eq!(trace, Some(true));
        assert_eq!(settings.profile, None);
        assert_eq!(settings.toolchain.as_deref(), Some("msvc"));
        read_settings(&json!({ "anvil": { "toolchain": "" } }), false, &mut settings);
        assert_eq!(settings.toolchain, None);
    }

    #[test]
    fn messages_as_the_editor_shows_them() {
        let file = Path::new("C:/ws/lib/anvil.toml");
        assert_eq!(
            presented_message("package `lib` (C:/ws/lib/anvil.toml): in [target.lib]: kind = \"lbi\"\n", file),
            "in [target.lib]: kind = \"lbi\""
        );
        let elsewhere = "package `lib` (C:/ws/other/anvil.toml): in [target.lib]: oops";
        assert_eq!(presented_message(elsewhere, file), elsewhere);

        assert_eq!(
            plain_text("**app** — exe target\n\n---\nfile: `out/app.exe`"),
            "app — exe target\n\nfile: out/app.exe"
        );
    }

    #[test]
    fn misspelled_words_from_a_range() {
        let text = "verison = \"1\"\nkind = \"exx\"\nwhen.lnux.x = 1\n";
        assert_eq!(misspelled_word(text, &(0..7)), Some(0..7));
        assert_eq!(misspelled_word(text, &(21..26)), Some(22..25));
        assert_eq!(misspelled_word(text, &(28..37)), None);
    }
}
