//! CMake's language, parsed: commands with bracket, quoted and unquoted arguments, grouped into blocks
//! (if/foreach/while/function/macro) for the interpreter.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    Unquoted,
    Quoted,
    Bracket,
}

#[derive(Debug, Clone)]
pub struct Arg {
    pub text: String,
    pub kind: ArgKind,
}

#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub args: Vec<Arg>,
    pub line: usize,
    pub file: Rc<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum Node {
    Command(Command),
    If {
        branches: Vec<(Vec<Arg>, Vec<Node>)>,
        otherwise: Option<Vec<Node>>,
    },
    Foreach {
        args: Vec<Arg>,
        body: Vec<Node>,
    },
    While {
        args: Vec<Arg>,
        body: Vec<Node>,
    },
    Function {
        name: String,
        params: Vec<String>,
        body: Rc<Vec<Node>>,
        is_macro: bool,
    },
}

struct Lexer<'a> {
    characters: Vec<char>,
    position: usize,
    line: usize,
    file: &'a Rc<PathBuf>,
}

pub fn parse_file(path: &Path) -> Result<Vec<Node>> {
    let text = std::fs::read_to_string(path).map_err(|error| anyhow::anyhow!("reading {}: {error}", path.display()))?;
    let file = Rc::new(path.to_path_buf());

    let commands = Lexer {
        characters: text.chars().collect(),
        position: 0,
        line: 1,
        file: &file,
    }
    .commands()?;
    let mut iterator = commands.into_iter().peekable();

    build_blocks(&mut iterator, &[])
}

impl Lexer<'_> {
    fn peek(&self) -> Option<char> {
        self.characters.get(self.position).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.characters.get(self.position + offset).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.position += 1;
        if character == '\n' {
            self.line += 1;
        }
        Some(character)
    }

    fn commands(mut self) -> Result<Vec<Command>> {
        let mut commands = Vec::new();

        while let Some(character) = self.peek() {
            if character.is_whitespace() {
                self.advance();
            } else if character == '#' {
                self.skip_comment();
            } else if character.is_ascii_alphabetic() || character == '_' {
                commands.push(self.command()?);
            } else {
                bail!("{}:{}: unexpected `{character}`", self.file.display(), self.line);
            }
        }

        Ok(commands)
    }

    fn skip_comment(&mut self) {
        self.advance();

        // #[[ bracket comment ]]
        if self.peek() == Some('[') {
            if let Some(level) = self.bracket_open_level() {
                self.read_bracket_body(level);
                return;
            }
        }

        while let Some(character) = self.peek() {
            if character == '\n' {
                break;
            }
            self.advance();
        }
    }

    fn command(&mut self) -> Result<Command> {
        let line = self.line;
        let mut name = String::new();
        while let Some(character) = self.peek() {
            if !(character.is_ascii_alphanumeric() || character == '_') {
                break;
            }
            name.push(character);
            self.advance();
        }

        while self.peek().is_some_and(|character| character == ' ' || character == '\t') {
            self.advance();
        }
        if self.advance() != Some('(') {
            bail!("{}:{line}: expected `(` after `{name}`", self.file.display());
        }

        let args = self.arguments(line)?;
        Ok(Command {
            name: name.to_lowercase(),
            args,
            line,
            file: self.file.clone(),
        })
    }

    /// Arguments up to the matching `)`; nested parentheses become `(` and `)` arguments.
    fn arguments(&mut self, line: usize) -> Result<Vec<Arg>> {
        let mut args = Vec::new();
        let mut depth = 0;

        loop {
            let Some(character) = self.peek() else {
                bail!("{}:{line}: missing `)`", self.file.display());
            };

            match character {
                c if c.is_whitespace() => {
                    self.advance();
                }
                '#' => self.skip_comment(),
                '(' => {
                    self.advance();
                    depth += 1;
                    args.push(Arg {
                        text: "(".into(),
                        kind: ArgKind::Unquoted,
                    });
                }
                ')' => {
                    self.advance();
                    if depth == 0 {
                        return Ok(args);
                    }
                    depth -= 1;
                    args.push(Arg {
                        text: ")".into(),
                        kind: ArgKind::Unquoted,
                    });
                }
                '"' => args.push(self.quoted()?),
                '[' if self.bracket_open_level().is_some() => {
                    let level = self.bracket_open_level().unwrap();
                    let text = self.read_bracket_body(level);
                    args.push(Arg {
                        text,
                        kind: ArgKind::Bracket,
                    });
                }
                _ => args.push(self.unquoted()),
            }
        }
    }

    fn quoted(&mut self) -> Result<Arg> {
        let line = self.line;
        self.advance();

        let mut text = String::new();
        loop {
            match self.advance() {
                None => bail!("{}:{line}: unterminated string", self.file.display()),
                Some('"') => break,
                Some('\\') => match self.advance() {
                    // Line continuation
                    Some('\n') => {}
                    Some(escaped) => {
                        text.push('\\');
                        text.push(escaped);
                    }
                    None => break,
                },
                Some(character) => text.push(character),
            }
        }

        Ok(Arg { text, kind: ArgKind::Quoted })
    }

    fn unquoted(&mut self) -> Arg {
        let mut text = String::new();
        let mut genex_depth = 0;

        while let Some(character) = self.peek() {
            // Inside $<...> anything but a newline belongs to the argument
            if character == '\n' || (genex_depth == 0 && (character.is_whitespace() || character == '(' || character == ')' || character == '#')) {
                break;
            }
            if genex_depth == 0 && character == '"' && text.is_empty() {
                break;
            }

            if character == '\\' {
                text.push(character);
                self.advance();
                if let Some(escaped) = self.advance() {
                    text.push(escaped);
                }
                continue;
            }

            // A legacy unquoted argument may contain a quoted part: a"b c"
            if character == '"' {
                self.advance();
                text.push('"');
                while let Some(inner) = self.advance() {
                    text.push(inner);
                    if inner == '"' {
                        break;
                    }
                }
                continue;
            }

            if character == '$' && self.peek_at(1) == Some('<') {
                genex_depth += 1;
            } else if character == '>' && genex_depth > 0 {
                genex_depth -= 1;
            }

            text.push(character);
            self.advance();
        }

        Arg {
            text,
            kind: ArgKind::Unquoted,
        }
    }

    /// At `[`: the number of `=` in `[==[`, or None when it is not a bracket opener.
    fn bracket_open_level(&self) -> Option<usize> {
        let mut offset = 1;
        while self.peek_at(offset) == Some('=') {
            offset += 1;
        }
        (self.peek_at(offset) == Some('[')).then_some(offset - 1)
    }

    fn read_bracket_body(&mut self, level: usize) -> String {
        for _ in 0..level + 2 {
            self.advance();
        }
        // A newline right after the opener is not part of the content
        if self.peek() == Some('\n') {
            self.advance();
        }

        let closer: String = format!("]{}]", "=".repeat(level));
        let closer: Vec<char> = closer.chars().collect();
        let mut text = String::new();

        while self.peek().is_some() {
            if self.characters[self.position..].starts_with(&closer) {
                for _ in 0..closer.len() {
                    self.advance();
                }
                break;
            }
            text.push(self.advance().unwrap());
        }

        text
    }
}

/// Groups flat commands into blocks until one of `terminators` (returned to the caller unconsumed).
fn build_blocks(commands: &mut std::iter::Peekable<std::vec::IntoIter<Command>>, terminators: &[&str]) -> Result<Vec<Node>> {
    let mut nodes = Vec::new();

    while let Some(command) = commands.peek() {
        if terminators.contains(&command.name.as_str()) {
            return Ok(nodes);
        }
        let command = commands.next().unwrap();

        match command.name.as_str() {
            "if" => {
                let mut branches = vec![(command.args.clone(), build_blocks(commands, &["elseif", "else", "endif"])?)];
                let mut otherwise = None;

                loop {
                    let Some(next) = commands.next() else {
                        bail!("{}: if() at line {} has no endif()", command.file.display(), command.line)
                    };
                    match next.name.as_str() {
                        "elseif" => branches.push((next.args, build_blocks(commands, &["elseif", "else", "endif"])?)),
                        "else" => otherwise = Some(build_blocks(commands, &["endif"])?),
                        _ => break,
                    }
                }

                nodes.push(Node::If { branches, otherwise });
            }
            "foreach" | "while" => {
                let end = if command.name == "foreach" { "endforeach" } else { "endwhile" };
                let body = build_blocks(commands, &[end])?;
                if commands.next().is_none() {
                    bail!("{}: {}() at line {} has no {end}()", command.file.display(), command.name, command.line);
                }

                nodes.push(if command.name == "foreach" {
                    Node::Foreach { args: command.args, body }
                } else {
                    Node::While { args: command.args, body }
                });
            }
            "function" | "macro" => {
                let end = if command.name == "function" { "endfunction" } else { "endmacro" };
                let body = build_blocks(commands, &[end])?;
                if commands.next().is_none() {
                    bail!("{}: {}() at line {} has no {end}()", command.file.display(), command.name, command.line);
                }

                let mut words = command.args.iter().map(|arg| arg.text.clone());
                let name = words.next().unwrap_or_default().to_lowercase();
                nodes.push(Node::Function {
                    name,
                    params: words.collect(),
                    body: Rc::new(body),
                    is_macro: command.name == "macro",
                });
            }
            _ => nodes.push(Node::Command(command)),
        }
    }

    if !terminators.is_empty() {
        bail!("missing {}()", terminators.last().unwrap());
    }
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Vec<Node> {
        let path = std::env::temp_dir().join(format!("anvil-cmake-{}-{}.txt", std::process::id(), text.len()));
        std::fs::write(&path, text).unwrap();
        let nodes = parse_file(&path).unwrap();
        std::fs::remove_file(&path).ok();
        nodes
    }

    #[test]
    fn arguments() {
        let nodes = parse("add_library(x STATIC \"a b.cpp\" [[raw ${x}]] $<$<CONFIG:Debug>:y z> # comment\n c.cpp)\n");
        let Node::Command(command) = &nodes[0] else { panic!() };

        let texts: Vec<&str> = command.args.iter().map(|arg| arg.text.as_str()).collect();
        assert_eq!(texts, vec!["x", "STATIC", "a b.cpp", "raw ${x}", "$<$<CONFIG:Debug>:y z>", "c.cpp"]);
        assert_eq!(command.args[2].kind, ArgKind::Quoted);
    }

    #[test]
    fn blocks() {
        let nodes = parse("if(WIN32)\n a()\nelseif(UNIX)\n b()\nelse()\n c()\nendif()\nfunction(f x)\n d()\nendfunction()\n");
        assert!(matches!(&nodes[0], Node::If { branches, otherwise: Some(_) } if branches.len() == 2));
        assert!(matches!(&nodes[1], Node::Function { name, params, .. } if name == "f" && params == &vec!["x".to_string()]));
    }
}
