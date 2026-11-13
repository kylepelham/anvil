//! A forgiving reader of TOML's structure: every key, table header and value with its byte span and its full key
//! path (`target.app.when."windows".deps[2]`), and the place a position is at. Unlike a TOML parser it never stops at
//! an error, so it also works on a half-typed line; values are not checked, only located.

use std::collections::HashMap;
use std::ops::Range;

/// One step of a key path: a key, or an item of an array (also an array of tables, `[[test]]`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Part {
    Key(String),
    Index(usize),
}

pub type KeyPath = Vec<Part>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// A key of a table header (`[target.app]`: `target`, then `app`).
    Header,
    Key,
    String,
    /// A number, boolean or date (or a word written without its quotes).
    Bare,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Range<usize>,
    /// Inside the quotes, for strings and quoted keys; the span otherwise.
    pub content: Range<usize>,
    /// Keys: the path to the key itself. Values: the value's path (an array item's ends with its index).
    pub path: KeyPath,
    /// Escapes decoded.
    pub text: String,
}

/// `key = value`, in a table or an inline table.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: KeyPath,
    pub key_span: Range<usize>,
    pub value_span: Range<usize>,
}

/// `[table]` or `[[array]]`; its path ends with the index of the array's item.
#[derive(Debug, Clone)]
pub struct Header {
    pub path: KeyPath,
    pub span: Range<usize>,
    /// Where its keys end: the next header's line.
    pub end: usize,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub tokens: Vec<Token>,
    pub entries: Vec<Entry>,
    pub headers: Vec<Header>,
}

/// What a position is at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Header,
    Key,
    Value,
    /// After a value, or in a comment: nothing to complete.
    Other,
}

#[derive(Debug, Clone)]
pub struct Cursor {
    pub place: Place,
    /// Header and Key: the table the key goes in, with the dotted keys before the cursor. Value: the value's path.
    pub path: KeyPath,
    /// The token the position is in, or right after.
    pub token: Option<Token>,
    pub array_header: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Key,
    Value,
    After,
}

/// An array or inline table being read.
struct Frame {
    path: KeyPath,
    array: bool,
    index: usize,
}

/// A `key =` whose value is being read.
struct OpenEntry {
    path: KeyPath,
    key_span: Range<usize>,
    value_start: Option<usize>,
    depth: usize,
}

struct Scanner<'a> {
    text: &'a str,
    bytes: &'a [u8],
    position: usize,
    stop: usize,
    scan: Scan,
    cursor: Option<Cursor>,

    mode: Mode,
    table: KeyPath,
    keys: KeyPath,
    key_start: usize,
    value_path: KeyPath,
    frames: Vec<Frame>,
    open: Vec<OpenEntry>,

    // Arrays of tables: how many items each has so far, by path
    array_items: HashMap<KeyPath, usize>,
}

/// Every token, entry and header of a document.
pub fn scan(text: &str) -> Scan {
    let mut scanner = Scanner::new(text, usize::MAX);
    scanner.run();
    scanner.finish();

    scanner.scan
}

/// What the position at `offset` is in.
pub fn cursor(text: &str, offset: usize) -> Cursor {
    let mut scanner = Scanner::new(text, offset);
    scanner.run();

    if let Some(cursor) = scanner.cursor.take() {
        return cursor;
    }
    Cursor {
        place: if scanner.mode == Mode::Key && scanner.frames.is_empty() {
            Place::Key
        } else {
            Place::Other
        },
        path: scanner.base(),
        token: None,
        array_header: false,
    }
}

/// A path written as anvil's messages and TOML write it: `target.app.when."windows & msvc".deps[0]`.
pub fn display(path: &[Part]) -> String {
    let mut text = String::new();

    for part in path {
        match part {
            Part::Key(key) => {
                if !text.is_empty() {
                    text.push('.');
                }
                let bare = !key.is_empty() && key.chars().all(|character| character.is_ascii_alphanumeric() || "-_".contains(character));
                match bare {
                    true => text.push_str(key),
                    false => text.push_str(&format!("{key:?}")),
                }
            }
            Part::Index(index) => text.push_str(&format!("[{index}]")),
        }
    }

    text
}

/// Keys from strings: `key(&["target", "app"])`.
pub fn keys(names: &[&str]) -> KeyPath {
    names.iter().map(|name| Part::Key(name.to_string())).collect()
}

impl Scan {
    /// The header or entry of exactly this path: its header span, else its key's span.
    pub fn key_span(&self, path: &[Part]) -> Option<Range<usize>> {
        if let Some(header) = self.headers.iter().find(|header| header.path == path) {
            return Some(header.span.clone());
        }
        if let Some(entry) = self.entries.iter().find(|entry| entry.path == path) {
            return Some(entry.key_span.clone());
        }

        // A table only named on the way to another (`[target.app.when.windows]` names `target.app`)
        self.tokens
            .iter()
            .find(|token| matches!(token.kind, TokenKind::Header | TokenKind::Key) && token.path == path)
            .map(|token| token.span.clone())
    }

    /// The entry of this path (a value's key and the value).
    pub fn entry(&self, path: &[Part]) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.path == path)
    }

    /// Value tokens at this path or under it.
    pub fn values_under<'a>(&'a self, path: &'a [Part]) -> impl Iterator<Item = &'a Token> + 'a {
        self.tokens
            .iter()
            .filter(move |token| matches!(token.kind, TokenKind::String | TokenKind::Bare) && token.path.starts_with(path))
    }

    /// Keys and headers at this path or under it.
    pub fn keys_under<'a>(&'a self, path: &'a [Part]) -> impl Iterator<Item = &'a Token> + 'a {
        self.tokens
            .iter()
            .filter(move |token| matches!(token.kind, TokenKind::Header | TokenKind::Key) && token.path.starts_with(path))
    }

    /// The token at a byte offset (or ending right there).
    pub fn token_at(&self, offset: usize) -> Option<&Token> {
        let inside = self.tokens.iter().find(|token| token.span.start <= offset && offset < token.span.end);
        inside.or_else(|| self.tokens.iter().find(|token| token.span.end == offset))
    }

    /// The direct children of a table: (key, its path), each once, in order.
    pub fn children(&self, path: &[Part]) -> Vec<String> {
        let mut children: Vec<String> = Vec::new();

        for token in &self.tokens {
            if !matches!(token.kind, TokenKind::Header | TokenKind::Key) || token.path.len() != path.len() + 1 || !token.path.starts_with(path) {
                continue;
            }
            if let Some(Part::Key(key)) = token.path.last() {
                if !children.contains(key) {
                    children.push(key.clone());
                }
            }
        }

        children
    }

    /// The items of an array of tables (`[[test]]`): each item's path.
    pub fn array_items(&self, path: &[Part]) -> Vec<KeyPath> {
        let mut items: Vec<KeyPath> = Vec::new();

        for token in &self.tokens {
            if token.path.len() > path.len() && token.path.starts_with(path) {
                if let Part::Index(_) = token.path[path.len()] {
                    let item = token.path[..=path.len()].to_vec();
                    if !items.contains(&item) {
                        items.push(item);
                    }
                }
            }
        }

        items
    }

    /// The string value at exactly this path.
    pub fn string(&self, path: &[Part]) -> Option<&Token> {
        self.tokens.iter().find(|token| token.kind == TokenKind::String && token.path == path)
    }
}

impl<'a> Scanner<'a> {
    fn new(text: &'a str, stop: usize) -> Scanner<'a> {
        Scanner {
            text,
            bytes: text.as_bytes(),
            position: 0,
            stop,
            scan: Scan::default(),
            cursor: None,
            mode: Mode::Key,
            table: Vec::new(),
            keys: Vec::new(),
            key_start: 0,
            value_path: Vec::new(),
            frames: Vec::new(),
            open: Vec::new(),
            array_items: HashMap::new(),
        }
    }

    fn run(&mut self) {
        while self.position < self.bytes.len() {
            if self.cursor.is_some() {
                return;
            }
            if self.position >= self.stop {
                self.capture(None);
                return;
            }

            let byte = self.bytes[self.position];
            match byte {
                b' ' | b'\t' | b'\r' => self.position += 1,
                b'\n' => {
                    self.position += 1;
                    if self.frames.is_empty() {
                        self.open.clear();
                        self.keys.clear();
                        self.mode = Mode::Key;
                    }
                }
                b'#' => self.skip_comment(),
                _ => match self.mode {
                    Mode::Key => self.read_key_part(byte),
                    Mode::Value => self.read_value(byte),
                    Mode::After => self.read_after(byte),
                },
            }
        }

        if self.cursor.is_none() && self.stop <= self.bytes.len() {
            self.capture(None);
        }
    }

    /// Header ends: each runs to the next.
    fn finish(&mut self) {
        let count = self.scan.headers.len();
        for index in 0..count {
            let end = match self.scan.headers.get(index + 1) {
                Some(next) => line_start(self.text, next.span.start),
                None => self.text.len(),
            };
            self.scan.headers[index].end = end;
        }
    }

    /// The table keys go in now: an inline table's, else the last header's.
    fn base(&self) -> KeyPath {
        match self.frames.last() {
            Some(frame) if !frame.array => frame.path.clone(),
            _ => self.table.clone(),
        }
    }

    /// The path of the value starting here: the next item of an array, else the key's.
    fn current_value_path(&self) -> KeyPath {
        match self.frames.last() {
            Some(frame) if frame.array => {
                let mut path = frame.path.clone();
                path.push(Part::Index(frame.index));
                path
            }
            _ => self.value_path.clone(),
        }
    }

    fn capture(&mut self, token: Option<Token>) {
        if self.cursor.is_some() {
            return;
        }

        let (place, path) = match self.mode {
            Mode::Key => {
                let mut path = self.base();
                path.extend(self.keys.iter().cloned());
                (Place::Key, path)
            }
            Mode::Value => (Place::Value, self.current_value_path()),
            Mode::After => (Place::Other, self.current_value_path()),
        };
        self.cursor = Some(Cursor {
            place,
            path,
            token,
            array_header: false,
        });
    }

    fn skip_comment(&mut self) {
        let end = self.line_end(self.position);
        if self.position < self.stop && self.stop <= end {
            self.cursor = Some(Cursor {
                place: Place::Other,
                path: self.base(),
                token: None,
                array_header: false,
            });
        }
        self.position = end;
    }

    fn line_end(&self, from: usize) -> usize {
        self.text[from..].find('\n').map_or(self.text.len(), |offset| from + offset)
    }

    fn at_line_start(&self) -> bool {
        let start = line_start(self.text, self.position);
        self.text[start..self.position].trim().is_empty()
    }

    fn read_key_part(&mut self, byte: u8) {
        // A table header
        if byte == b'[' && self.frames.is_empty() && self.at_line_start() {
            self.read_header();
            return;
        }

        // The end of an inline table (`{}` or `{ a = 1, }`)
        if byte == b'}' {
            if self.frames.last().is_some_and(|frame| !frame.array) {
                self.position += 1;
                self.frames.pop();
                self.value_done();
            } else {
                self.position += 1;
            }
            return;
        }
        if byte == b',' {
            self.position += 1;
            return;
        }

        let Some((text, span, content)) = self.read_key_text() else {
            // Not a key: skip the rest of the line (or one character inside an inline table)
            self.position = match self.frames.is_empty() {
                true => self.line_end(self.position),
                false => self.position + utf8_length(byte),
            };
            return;
        };

        if self.keys.is_empty() {
            self.key_start = span.start;
        }
        let mut path = self.base();
        path.extend(self.keys.iter().cloned());
        path.push(Part::Key(text.clone()));
        let token = Token {
            kind: TokenKind::Key,
            span: span.clone(),
            content,
            path,
            text: text.clone(),
        };
        if span.start < self.stop && self.stop <= span.end {
            self.capture(Some(token.clone()));
        }
        self.scan.tokens.push(token);
        self.keys.push(Part::Key(text));

        // A dotted key goes on; `=` starts the value
        self.skip_spaces();
        match self.bytes.get(self.position) {
            Some(b'.') => self.position += 1,
            Some(b'=') => {
                self.position += 1;
                let mut path = self.base();
                path.append(&mut self.keys);
                self.open.push(OpenEntry {
                    path: path.clone(),
                    key_span: self.key_start..span.end,
                    value_start: None,
                    depth: self.frames.len(),
                });
                self.value_path = path;
                self.mode = Mode::Value;
            }
            _ => {}
        }
    }

    /// A bare or quoted key: its text, span and the span inside its quotes.
    fn read_key_text(&mut self) -> Option<(String, Range<usize>, Range<usize>)> {
        let start = self.position;
        let byte = *self.bytes.get(start)?;

        if byte == b'"' || byte == b'\'' {
            let (text, content, end) = self.read_quoted(byte, false);
            self.position = end;
            return Some((text, start..end, content));
        }
        if !is_bare_key_byte(byte) {
            return None;
        }

        let mut end = start;
        while end < self.bytes.len() && is_bare_key_byte(self.bytes[end]) {
            end += 1;
        }
        self.position = end;

        Some((self.text[start..end].to_string(), start..end, start..end))
    }

    fn skip_spaces(&mut self) {
        while matches!(self.bytes.get(self.position), Some(b' ' | b'\t')) {
            self.position += 1;
        }
    }

    fn read_header(&mut self) {
        let start = self.position;
        let array = self.bytes.get(start + 1) == Some(&b'[');
        self.position += if array { 2 } else { 1 };
        self.open.clear();
        self.keys.clear();

        let mut path: KeyPath = Vec::new();
        loop {
            self.skip_spaces();
            let Some((text, span, content)) = self.read_key_text() else { break };
            path.push(Part::Key(text.clone()));

            // An array of tables named on the way means its last item (`[test.matrix]` after `[[test]]`)
            let token = Token {
                kind: TokenKind::Header,
                span: span.clone(),
                content,
                path: path.clone(),
                text,
            };
            if span.start < self.stop && self.stop <= span.end {
                self.capture_header(Some(token.clone()), &path[..path.len() - 1], array);
            }
            self.scan.tokens.push(token);

            self.skip_spaces();
            if self.bytes.get(self.position) == Some(&b'.') {
                self.position += 1;
                if let Some(count) = self.array_items.get(&path) {
                    path.push(Part::Index(count - 1));
                }
                continue;
            }
            break;
        }

        // The cursor on an empty header part (`[target.|`)
        if self.position >= self.stop && self.cursor.is_none() && start < self.stop {
            self.capture_header(None, &path.clone(), array);
        }

        // The closing brackets, then the rest of the line is a comment at most
        let line_end = self.line_end(self.position);
        let close = self.text[self.position..line_end].find(']').map(|offset| self.position + offset);
        let end = match close {
            Some(close) if array && self.bytes.get(close + 1) == Some(&b']') => close + 2,
            Some(close) => close + 1,
            None => line_end,
        };

        if array {
            let count = self.array_items.entry(path.clone()).or_insert(0);
            path.push(Part::Index(*count));
            *count += 1;
        }
        self.scan.headers.push(Header {
            path: path.clone(),
            span: start..end,
            end: self.text.len(),
        });
        self.table = path;
        self.mode = Mode::After;
        self.position = end;
    }

    fn capture_header(&mut self, token: Option<Token>, path: &[Part], array: bool) {
        if self.cursor.is_none() {
            self.cursor = Some(Cursor {
                place: Place::Header,
                path: path.to_vec(),
                token,
                array_header: array,
            });
        }
    }

    fn read_value(&mut self, byte: u8) {
        if let Some(open) = self.open.last_mut() {
            if open.value_start.is_none() && open.depth == self.frames.len() {
                open.value_start = Some(self.position);
            }
        }

        match byte {
            b'"' | b'\'' => {
                let start = self.position;
                let (text, content, end) = self.read_quoted(byte, true);
                self.position = end;
                self.push_value(TokenKind::String, start..end, content, text);
            }
            b'[' => {
                self.position += 1;
                let path = self.current_value_path();
                self.frames.push(Frame { path, array: true, index: 0 });
            }
            b'{' => {
                self.position += 1;
                let path = self.current_value_path();
                self.frames.push(Frame {
                    path,
                    array: false,
                    index: 0,
                });
                self.mode = Mode::Key;
            }
            b']' | b'}' | b',' => self.read_after(byte),
            _ => {
                let start = self.position;
                let mut end = start;
                while end < self.bytes.len() && !b" \t\r\n,]}#".contains(&self.bytes[end]) {
                    end += 1;
                }
                if end == start {
                    end += utf8_length(byte);
                }
                self.position = end;
                let text = self.text[start..end].to_string();
                self.push_value(TokenKind::Bare, start..end, start..end, text);
            }
        }
    }

    fn push_value(&mut self, kind: TokenKind, span: Range<usize>, content: Range<usize>, text: String) {
        let token = Token {
            kind,
            span: span.clone(),
            content,
            path: self.current_value_path(),
            text,
        };
        if span.start < self.stop && self.stop <= span.end {
            self.capture(Some(token.clone()));
        }
        self.scan.tokens.push(token);
        self.value_done();
    }

    /// A value is complete: the entry it belongs to ends here.
    fn value_done(&mut self) {
        let depth = self.frames.len();
        if let Some(open) = self.open.pop_if(|open| open.depth == depth) {
            let value_start = open.value_start.unwrap_or(self.position);
            self.scan.entries.push(Entry {
                path: open.path,
                key_span: open.key_span,
                value_span: value_start..self.position,
            });
        }

        self.mode = Mode::After;
    }

    fn read_after(&mut self, byte: u8) {
        self.position += 1;

        match byte {
            b',' => match self.frames.last_mut() {
                Some(frame) if frame.array => {
                    frame.index += 1;
                    self.mode = Mode::Value;
                }
                Some(_) => {
                    self.mode = Mode::Key;
                    self.keys.clear();
                }
                None => {}
            },
            b']' => {
                if self.frames.last().is_some_and(|frame| frame.array) {
                    self.frames.pop();
                    self.value_done();
                }
            }
            b'}' => {
                if self.frames.last().is_some_and(|frame| !frame.array) {
                    self.frames.pop();
                    self.value_done();
                }
            }
            _ => {
                // Something after a value at the top level: skip to the line's end
                if self.frames.is_empty() {
                    self.position = self.line_end(self.position - 1);
                }
            }
        }
    }

    /// A quoted string from its opening quote: (decoded text, span of the content, end). An unclosed single-line
    /// string ends at the line's end.
    fn read_quoted(&self, quote: u8, allow_multiline: bool) -> (String, Range<usize>, usize) {
        let start = self.position;
        let literal = quote == b'\'';
        let triple = allow_multiline && self.bytes.get(start + 1) == Some(&quote) && self.bytes.get(start + 2) == Some(&quote);

        // A line break right after a multi-line string's opening quotes is not part of it
        let mut content_start = start + if triple { 3 } else { 1 };
        if triple {
            if self.text[content_start..].starts_with("\r\n") {
                content_start += 2;
            } else if self.text[content_start..].starts_with('\n') {
                content_start += 1;
            }
        }
        let mut text = String::new();
        let mut index = content_start;
        while index < self.bytes.len() {
            let byte = self.bytes[index];

            // The closing quote (a multi-line one may have up to two quotes of the content before it)
            if byte == quote {
                if !triple {
                    return (text, content_start..index, index + 1);
                }
                if self.bytes.get(index + 1) == Some(&quote) && self.bytes.get(index + 2) == Some(&quote) {
                    let mut close = index;
                    while self.bytes.get(close + 3) == Some(&quote) && close < index + 2 {
                        text.push(quote as char);
                        close += 1;
                    }
                    return (text, content_start..close, close + 3);
                }
            }
            if byte == b'\n' && !triple {
                return (text, content_start..index, index);
            }

            // Escapes in basic strings
            if byte == b'\\' && !literal {
                let (decoded, length) = decode_escape(&self.text[index..]);
                text.push_str(&decoded);
                index += length;
                continue;
            }

            let character = self.text[index..].chars().next().unwrap_or(' ');
            text.push(character);
            index += character.len_utf8();
        }

        (text, content_start..self.bytes.len(), self.bytes.len())
    }
}

/// One escape sequence from its backslash: (what it stands for, its length).
fn decode_escape(text: &str) -> (String, usize) {
    let mut characters = text.chars();
    characters.next();
    let Some(next) = characters.next() else { return ("\\".into(), 1) };

    let simple = match next {
        'n' => Some('\n'),
        't' => Some('\t'),
        'r' => Some('\r'),
        'b' => Some('\u{8}'),
        'f' => Some('\u{c}'),
        'e' => Some('\u{1b}'),
        '"' => Some('"'),
        '\\' => Some('\\'),
        _ => None,
    };
    if let Some(simple) = simple {
        return (simple.to_string(), 2);
    }

    let digits = match next {
        'u' => 4,
        'U' => 8,
        'x' => 2,
        // A line-ending backslash: the line break and the white space after it disappear
        '\n' | '\r' | ' ' | '\t' => {
            let skipped = text[1..].len() - text[1..].trim_start().len();
            return (String::new(), 1 + skipped);
        }
        other => return (other.to_string(), 1 + other.len_utf8()),
    };
    let hex = text.get(2..2 + digits).unwrap_or_default();
    match u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
        Some(character) if hex.len() == digits => (character.to_string(), 2 + digits),
        _ => (String::new(), 2),
    }
}

fn is_bare_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn utf8_length(first: u8) -> usize {
    match first {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

pub fn line_start(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].rfind('\n').map_or(0, |index| index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"[project]
name = "game"   # the name

[target.app]
deps = ["core", 'fmt']
when."windows & msvc".libs = ["user32"]
public = { defines = ["A=1"], deps = ["x"] }

[[test]]
name = "one"
command = """
run
"""

[[test]]
name = "two"
[test.env]
X = "\u00e9"
"#;

    fn path(text: &str) -> KeyPath {
        text.split('/')
            .map(|part| match part.parse::<usize>() {
                Ok(index) => Part::Index(index),
                Err(_) => Part::Key(part.to_string()),
            })
            .collect()
    }

    #[test]
    fn paths_and_spans() {
        let scan = scan(MANIFEST);

        let fmt = scan.tokens.iter().find(|token| token.text == "fmt").unwrap();
        assert_eq!(fmt.path, path("target/app/deps/1"));
        assert_eq!(&MANIFEST[fmt.span.clone()], "'fmt'");
        assert_eq!(&MANIFEST[fmt.content.clone()], "fmt");

        let user32 = scan.tokens.iter().find(|token| token.text == "user32").unwrap();
        assert_eq!(user32.path, path("target/app/when/windows & msvc/libs/0"));
        let x = scan.tokens.iter().find(|token| token.text == "x").unwrap();
        assert_eq!(x.path, path("target/app/public/deps/0"));

        // Arrays of tables count their items, and a table under one belongs to its last item
        assert_eq!(scan.string(&path("test/1/name")).unwrap().text, "two");
        assert_eq!(scan.string(&path("test/0/command")).unwrap().text, "run\n");
        assert_eq!(scan.string(&path("test/1/env/X")).unwrap().text, "é");
        assert_eq!(scan.array_items(&path("test")).len(), 2);

        // Spans of headers and entries
        let header = scan.key_span(&path("target/app")).unwrap();
        assert_eq!(&MANIFEST[header], "[target.app]");
        let entry = scan.entry(&path("target/app/deps")).unwrap();
        assert_eq!(&MANIFEST[entry.key_span.clone()], "deps");
        assert_eq!(&MANIFEST[entry.value_span.clone()], "[\"core\", 'fmt']");
        let dotted = scan.entry(&path("target/app/when/windows & msvc/libs")).unwrap();
        assert_eq!(&MANIFEST[dotted.key_span.clone()], "when.\"windows & msvc\".libs");
        assert_eq!(scan.children(&path("target/app")), vec!["deps", "when", "public"]);
    }

    #[test]
    fn cursor_places() {
        let at = |text: &str| {
            let offset = text.find('|').unwrap();
            let text = text.replace('|', "");
            cursor(&text, offset)
        };

        let key = at("[target.app]\nde|");
        assert_eq!(key.place, Place::Key);
        assert_eq!(key.path, path("target/app"));
        assert_eq!(key.token.unwrap().text, "de");

        let dotted = at("[target.app]\nwhen.windows.|");
        assert_eq!(dotted.place, Place::Key);
        assert_eq!(dotted.path, path("target/app/when/windows"));

        let value = at("[target.app]\ndeps = [\"core\", \"|");
        assert_eq!(value.place, Place::Value);
        assert_eq!(value.path, path("target/app/deps/1"));
        assert_eq!(value.token.unwrap().kind, TokenKind::String);

        let multiline = at("[target.app]\ndeps = [\n  \"a\",\n  |\n]\n");
        assert_eq!(multiline.place, Place::Value);
        assert_eq!(multiline.path, path("target/app/deps/1"));

        let inline = at("[deps]\nfmt = { vcpkg = \"fmt\", fe|");
        assert_eq!(inline.place, Place::Key);
        assert_eq!(inline.path, path("deps/fmt"));

        let header = at("[target.|");
        assert_eq!(header.place, Place::Header);
        assert_eq!(header.path, path("target"));

        let after_equals = at("[project]\ntoolchain = |\n");
        assert_eq!(after_equals.place, Place::Value);
        assert_eq!(after_equals.path, path("project/toolchain"));

        let comment = at("[project]\n# a |comment\n");
        assert_eq!(comment.place, Place::Other);
    }

    #[test]
    fn never_panics_on_broken_text() {
        for text in [
            "[",
            "[[",
            "a = [",
            "a = {",
            "a = \"",
            "a = '''",
            "\"",
            "a.\"b",
            "x = \"\\u12\"",
            "[a.\"é\"]\nb = 1",
            "é = 1",
        ] {
            let scan = scan(text);
            assert!(scan.tokens.iter().all(|token| token.span.end <= text.len()));
            for offset in 0..=text.len() {
                if text.is_char_boundary(offset) {
                    cursor(text, offset);
                }
            }
        }
    }
}
