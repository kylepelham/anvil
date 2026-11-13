//! Positions in a document: byte offsets to and from LSP line/character positions (UTF-8 or UTF-16 characters, as
//! negotiated), and incremental edits.

use lsp_types::{Position, Range};

/// How the client counts characters in a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
}

/// A document's text with the byte offset of each line's start.
#[derive(Debug, Clone)]
pub struct Text {
    pub text: String,
    line_starts: Vec<usize>,
}

impl Text {
    pub fn new(text: String) -> Text {
        let line_starts = line_starts(&text);
        Text { text, line_starts }
    }

    /// The position of a byte offset (clamped to the text and to a character boundary).
    pub fn position(&self, offset: usize, encoding: Encoding) -> Position {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }

        let line = self.line_starts.partition_point(|start| *start <= offset).saturating_sub(1);
        let line_text = &self.text[self.line_starts[line]..offset];
        let character = match encoding {
            Encoding::Utf8 => line_text.len(),
            Encoding::Utf16 => line_text.encode_utf16().count(),
        };

        Position::new(line as u32, character as u32)
    }

    pub fn range(&self, span: &std::ops::Range<usize>, encoding: Encoding) -> Range {
        Range::new(self.position(span.start, encoding), self.position(span.end, encoding))
    }

    /// The byte offset of a position. A character past the line's end means the line's end, a line past the text's
    /// end the text's end; a position inside a character that UTF-16 splits in two means that character's start.
    pub fn offset(&self, position: Position, encoding: Encoding) -> usize {
        let Some(start) = self.line_starts.get(position.line as usize).copied() else {
            return self.text.len();
        };

        // The line without its line break
        let end = self.line_starts.get(position.line as usize + 1).copied().unwrap_or(self.text.len());
        let line = self.text[start..end].trim_end_matches(['\n', '\r']);

        let wanted = position.character as usize;
        let mut counted = 0;
        for (index, character) in line.char_indices() {
            if counted >= wanted {
                return start + index;
            }
            counted += match encoding {
                Encoding::Utf8 => character.len_utf8(),
                Encoding::Utf16 => character.len_utf16(),
            };
            if counted > wanted {
                return start + index;
            }
        }

        start + line.len()
    }

    /// Applies one change of `textDocument/didChange`: a range replaced, or (no range) the whole text.
    pub fn apply(&mut self, range: Option<Range>, new_text: &str, encoding: Encoding) {
        match range {
            Some(range) => {
                let start = self.offset(range.start, encoding);
                let end = self.offset(range.end, encoding).max(start);
                self.text.replace_range(start..end, new_text);
            }
            None => self.text = new_text.to_string(),
        }

        self.line_starts = line_starts(&self.text);
    }
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));

    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_columns_count_surrogate_pairs() {
        // "é" is 2 bytes and 1 UTF-16 unit; "😀" is 4 bytes and 2 units
        let text = Text::new("a = \"é😀x\"\r\nb = 1\n".to_string());
        let x = text.text.find('x').unwrap();

        assert_eq!(text.position(x, Encoding::Utf16), Position::new(0, 8));
        assert_eq!(text.position(x, Encoding::Utf8), Position::new(0, 11));
        assert_eq!(text.offset(Position::new(0, 8), Encoding::Utf16), x);
        assert_eq!(text.offset(Position::new(0, 11), Encoding::Utf8), x);

        // Inside the surrogate pair: the emoji's start; past the line's end: before the line break
        let emoji = text.text.find('😀').unwrap();
        assert_eq!(text.offset(Position::new(0, 7), Encoding::Utf16), emoji);
        assert_eq!(text.offset(Position::new(0, 99), Encoding::Utf16), text.text.find('\r').unwrap());
        assert_eq!(text.offset(Position::new(1, 2), Encoding::Utf16), text.text.find("b =").unwrap() + 2);
        assert_eq!(text.offset(Position::new(9, 0), Encoding::Utf16), text.text.len());

        // A byte offset inside a character counts from its start
        assert_eq!(text.position(emoji + 2, Encoding::Utf16), Position::new(0, 6));
    }

    #[test]
    fn incremental_edits() {
        let mut text = Text::new("[project]\nname = \"é\"\n".to_string());
        text.apply(Some(Range::new(Position::new(1, 8), Position::new(1, 9))), "ab", Encoding::Utf16);
        assert_eq!(text.text, "[project]\nname = \"ab\"\n");

        text.apply(Some(Range::new(Position::new(2, 0), Position::new(2, 0))), "[deps]\n", Encoding::Utf16);
        assert_eq!(text.text, "[project]\nname = \"ab\"\n[deps]\n");
        assert_eq!(text.position(text.text.len(), Encoding::Utf16), Position::new(3, 0));

        text.apply(None, "x", Encoding::Utf16);
        assert_eq!(text.text, "x");
    }
}
