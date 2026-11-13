//! What a load under the language server does differently: manifests and tests.toml files come from the editor's
//! unsaved text, warnings are collected instead of printed, [[write]] files are not written, and every file read is
//! remembered (an edit to one of them checks the project again). Outside the language server nothing changes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a remembered folder walk may be reused while only manifests are edited.
const WALK_LIFETIME: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Session {
    /// The editor's text of open files, by `util::path_key`.
    pub texts: HashMap<String, String>,

    /// What `ui::warn` would have printed.
    pub warnings: Vec<String>,

    /// Manifests and tests.toml files the load read.
    pub read: Vec<PathBuf>,

    /// Folder walks (source globs, tests.toml files) kept from one check to the next.
    pub walks: Walks,
}

/// Folder walks remembered between checks: typing in a manifest creates no files, so a check after an edit reuses
/// them; a check after a save, an open or a file event walks again.
#[derive(Default)]
pub struct Walks {
    found: HashMap<String, (Instant, Vec<PathBuf>)>,
    pub reuse: bool,
}

thread_local! {
    static CURRENT: RefCell<Option<Session>> = const { RefCell::new(None) };
}

/// Runs `work` with `session` in place on this thread, and gives the session back with what it collected.
pub fn run<T>(session: Session, work: impl FnOnce() -> T) -> (T, Session) {
    CURRENT.with(|current| *current.borrow_mut() = Some(session));

    // Taken back on a panic too, so the next load on this thread starts clean
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.with(|current| current.borrow_mut().take());
        }
    }

    let restore = Restore;
    let result = work();
    let session = CURRENT.with(|current| current.borrow_mut().take()).unwrap_or_default();
    drop(restore);

    (result, session)
}

/// Whether a language server load runs on this thread (nothing is written then).
pub fn active() -> bool {
    CURRENT.with(|current| current.borrow().is_some())
}

/// A manifest's or tests.toml's text: the editor's when it has the file open, else the file's.
pub fn read_text(path: &Path) -> std::io::Result<String> {
    let key = crate::util::path_key(path);
    let open = CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        let session = current.as_mut()?;
        session.read.push(path.to_path_buf());
        Some(session.texts.get(&key).cloned())
    });

    match open.flatten() {
        Some(text) => Ok(text),
        None => std::fs::read_to_string(path),
    }
}

/// Gives the load another text for a file (a manifest with its unknown keys blanked out, so the rest is checked).
pub fn replace_text(path: &Path, text: String) {
    CURRENT.with(|current| {
        if let Some(session) = current.borrow_mut().as_mut() {
            session.texts.insert(crate::util::path_key(path), text);
        }
    });
}

/// The files a folder walk finds, remembered under the language server (`kind` and `parts` say which walk it is).
pub fn walk<E>(kind: &str, parts: &[&Path], walk: impl FnOnce() -> Result<Vec<PathBuf>, E>) -> Result<Vec<PathBuf>, E> {
    let mut key = kind.to_string();
    for part in parts {
        key.push('\n');
        key.push_str(&crate::util::path_key(part));
    }

    // A walk this check may reuse
    let remembered = CURRENT.with(|current| {
        let current = current.borrow();
        let walks = &current.as_ref()?.walks;
        let (when, files) = walks.found.get(&key)?;
        (walks.reuse && when.elapsed() < WALK_LIFETIME).then(|| files.clone())
    });
    if let Some(files) = remembered {
        return Ok(files);
    }

    // Else walk, and remember it when a check runs
    let files = walk()?;
    CURRENT.with(|current| {
        if let Some(session) = current.borrow_mut().as_mut() {
            session.walks.found.insert(key, (Instant::now(), files.clone()));
        }
    });

    Ok(files)
}

/// Keeps a warning for the language server instead of printing it; false outside one.
pub fn capture_warning(message: &str) -> bool {
    CURRENT.with(|current| match current.borrow_mut().as_mut() {
        Some(session) => {
            session.warnings.push(message.to_string());
            true
        }
        None => false,
    })
}
