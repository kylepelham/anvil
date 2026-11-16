//! Terminal output: Cargo-style status lines, warnings and errors, and one live progress line that tool output
//! never interleaves with.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static COLOR: AtomicBool = AtomicBool::new(false);
static INTERACTIVE: AtomicBool = AtomicBool::new(false);
static QUIET: AtomicBool = AtomicBool::new(false);

// Width of the live progress line we last drew, so it can be erased
static PROGRESS_WIDTH: Mutex<usize> = Mutex::new(0);

const GREEN: &str = "\x1b[1;32m";
const YELLOW: &str = "\x1b[1;33m";
const RED: &str = "\x1b[1;31m";
const CYAN: &str = "\x1b[1;36m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Decides once whether output is colored and interactive.
pub fn init(quiet: bool) {
    let terminal = std::io::stderr().is_terminal();
    let color = terminal && std::env::var_os("NO_COLOR").is_none() && enable_virtual_terminal();

    COLOR.store(color || std::env::var_os("CLICOLOR_FORCE").is_some(), Ordering::Relaxed);
    INTERACTIVE.store(terminal, Ordering::Relaxed);
    QUIET.store(quiet, Ordering::Relaxed);
}

pub fn color() -> bool {
    COLOR.load(Ordering::Relaxed)
}

/// `-q`: print less.
pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

pub fn interactive() -> bool {
    INTERACTIVE.load(Ordering::Relaxed)
}

/// "   Compiling foo": a right-aligned green verb, then the message.
pub fn status(verb: &str, message: &str) {
    if QUIET.load(Ordering::Relaxed) {
        return;
    }
    print_line(&format!("{} {message}", paint(GREEN, &format!("{verb:>12}"))));
}

pub fn warn(message: &str) {
    if crate::lsp::session::capture_warning(message, None) {
        return;
    }
    print_line(&format!("{} {message}", paint(YELLOW, "warning:")));
}

/// A warning about a place in a manifest: printed as `warn` prints it, marked there by the language server.
pub fn warn_at(place: crate::located::Place, code: &'static str, message: &str) {
    if crate::lsp::session::capture_warning(message, Some((place, code))) {
        return;
    }
    print_line(&format!("{} {message}", paint(YELLOW, "warning:")));
}

/// A warning said once per run (`warned` is its own flag); under the language server, in every check.
pub fn warn_once(warned: &AtomicBool, message: &str) {
    if crate::lsp::session::capture_warning(message, None) || warned.swap(true, Ordering::Relaxed) {
        return;
    }
    print_line(&format!("{} {message}", paint(YELLOW, "warning:")));
}

pub fn error(message: &str) {
    print_line(&format!("{} {message}", paint(RED, "error:")));
}

/// "note: ...": something worth knowing, left out with `-q`.
pub fn note(message: &str) {
    if QUIET.load(Ordering::Relaxed) {
        return;
    }
    print_line(&format!("{} {message}", paint(CYAN, "note:")));
}

pub fn dim(text: &str) -> String {
    paint(DIM, text)
}

pub fn bold_green(text: &str) -> String {
    paint(GREEN, text)
}

pub fn bold_red(text: &str) -> String {
    paint(RED, text)
}

/// Prints a block (tool output) above the progress line.
pub fn print_block(text: &str) {
    let mut width = PROGRESS_WIDTH.lock().unwrap();
    let mut stderr = std::io::stderr().lock();

    erase(&mut stderr, &mut width);
    let _ = stderr.write_all(text.as_bytes());
    if !text.ends_with('\n') {
        let _ = stderr.write_all(b"\n");
    }
    let _ = stderr.flush();
}

pub fn print_line(text: &str) {
    print_block(&format!("{text}\n"));
}

/// Redraws the live progress line (interactive terminals only).
pub fn progress(done: usize, total: usize, text: &str) {
    if !interactive() || QUIET.load(Ordering::Relaxed) {
        return;
    }

    let mut width = PROGRESS_WIDTH.lock().unwrap();
    let mut stderr = std::io::stderr().lock();

    // Fit the terminal: never wrap, or the erase would leave garbage
    let counter = format!("[{done:>width$}/{total}] ", width = total.to_string().len());
    let room = terminal_width().saturating_sub(counter.len() + 1);
    let text: String = if text.chars().count() > room {
        text.chars().take(room).collect()
    } else {
        text.to_string()
    };

    erase(&mut stderr, &mut width);
    let _ = write!(stderr, "{}{text}", paint(GREEN, &counter));
    let _ = stderr.flush();
    *width = counter.len() + text.chars().count();
}

pub fn clear_progress() {
    let mut width = PROGRESS_WIDTH.lock().unwrap();
    erase(&mut std::io::stderr().lock(), &mut width);
}

fn paint(code: &str, text: &str) -> String {
    if color() {
        format!("{code}{text}{RESET}")
    } else {
        text.to_string()
    }
}

fn erase(stderr: &mut impl Write, width: &mut usize) {
    if *width > 0 {
        let _ = write!(stderr, "\r{}\r", " ".repeat(*width));
        *width = 0;
    }
}

fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|columns| columns.parse().ok())
        .unwrap_or_else(console_width)
}

#[cfg(windows)]
fn console_width() -> usize {
    use windows_sys::Win32::System::Console::{GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO, STD_ERROR_HANDLE};

    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    let succeeded = unsafe { GetConsoleScreenBufferInfo(GetStdHandle(STD_ERROR_HANDLE), &mut info) };
    if succeeded == 0 {
        return 100;
    }

    (info.srWindow.Right - info.srWindow.Left + 1).max(20) as usize
}

#[cfg(not(windows))]
fn console_width() -> usize {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let succeeded = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) };
    if succeeded != 0 || size.ws_col == 0 {
        return 100;
    }

    size.ws_col as usize
}

/// Windows consoles need VT processing switched on for escape codes.
#[cfg(windows)]
fn enable_virtual_terminal() -> bool {
    use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_ERROR_HANDLE};

    unsafe {
        let handle = GetStdHandle(STD_ERROR_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            // Not a console (mintty, a pipe): escape codes pass through as is
            return true;
        }

        SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

#[cfg(not(windows))]
fn enable_virtual_terminal() -> bool {
    true
}
