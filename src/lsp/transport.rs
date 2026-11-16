//! The server's stdin and stdout, framed as LSP messages. lsp-server's own stdio transport ends the connection at the
//! first body that is not a JSON-RPC message; this one answers it with a JSON-RPC error and reads on.

use lsp_server::{Connection, ErrorCode, Message, RequestId, Response};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::thread::JoinHandle;

/// The reader and writer threads.
pub struct Threads {
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl Threads {
    /// Waits for the reader (it stops at `exit` or the end of stdin) and the writer (once every sender is gone).
    pub fn join(self) {
        let _ = self.reader.join();
        let _ = self.writer.join();
    }
}

/// A connection over stdin and stdout.
pub fn stdio() -> (Connection, Threads) {
    let (writer_sender, writer_receiver) = crossbeam_channel::unbounded::<Message>();
    let (reader_sender, reader_receiver) = crossbeam_channel::unbounded::<Message>();

    let writer = std::thread::spawn(move || {
        for message in writer_receiver {
            let stdout = std::io::stdout();
            if message.write(&mut stdout.lock()).is_err() {
                return;
            }
        }
    });

    let replies = writer_sender.clone();
    let reader = std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        while let Some(body) = read_body(&mut input) {
            let message = match parse(&body) {
                Ok(message) => message,
                Err((id, code, text)) => {
                    eprintln!("anvil lsp: {text}");
                    reply_error(&replies, id, code, &text);
                    continue;
                }
            };

            let exit = matches!(&message, Message::Notification(notification) if notification.method == "exit");
            if reader_sender.send(message).is_err() || exit {
                return;
            }
        }
    });

    let connection = Connection {
        sender: writer_sender,
        receiver: reader_receiver,
    };
    (connection, Threads { reader, writer })
}

/// The next message's body; None at the end of the input. Headers are read leniently (a bare `\n` ends a line too).
fn read_body(input: &mut impl BufRead) -> Option<Vec<u8>> {
    loop {
        let mut length = None;
        loop {
            let mut line = String::new();
            if input.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("Content-Length") {
                    length = value.trim().parse::<usize>().ok();
                }
            }
        }

        // A header block without a length: nothing to read, wait for the next one
        let Some(length) = length else {
            eprintln!("anvil lsp: a message without a Content-Length header was skipped");
            continue;
        };
        let mut body = vec![0; length];
        input.read_exact(&mut body).ok()?;
        return Some(body);
    }
}

/// A body as a message, or the error to answer it with: the request's id when one can be read, the code and text.
fn parse(body: &[u8]) -> Result<Message, (Option<RequestId>, i32, String)> {
    let value: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(error) => return Err((None, ErrorCode::ParseError as i32, format!("the message is not JSON: {error}"))),
    };

    let id = serde_json::from_value::<RequestId>(value["id"].clone()).ok();
    match serde_json::from_value::<Message>(value) {
        Ok(message) => Ok(message),
        Err(error) => Err((
            id,
            ErrorCode::InvalidRequest as i32,
            format!("the message is not a JSON-RPC request, response or notification: {error}"),
        )),
    }
}

/// Answers a message that could not be read: with its id, else with a null id (JSON-RPC's answer when the id is unknown).
fn reply_error(replies: &crossbeam_channel::Sender<Message>, id: Option<RequestId>, code: i32, text: &str) {
    if let Some(id) = id {
        let _ = replies.send(Message::Response(Response::new_err(id, code, text.to_string())));
        return;
    }

    // lsp-server's Response has no null id: written here, one whole frame under the stdout lock as the writer does
    let body = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": code, "message": text } }).to_string();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let _ = write!(output, "Content-Length: {}\r\n\r\n{body}", body.len());
    let _ = output.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_bodies_and_refuses_what_is_not_a_message() {
        let mut input = std::io::Cursor::new(b"Content-Length: 2\r\n\r\n{}Content-Length: 5\n\n{bad}".to_vec());
        assert_eq!(read_body(&mut input).unwrap(), b"{}");
        assert_eq!(read_body(&mut input).unwrap(), b"{bad}");
        assert!(read_body(&mut input).is_none());

        let (id, code, _) = parse(b"{bad}").unwrap_err();
        assert!(id.is_none());
        assert_eq!(code, -32700);
        let (id, code, _) = parse(br#"{"jsonrpc":"2.0","id":7,"method":5}"#).unwrap_err();
        assert_eq!(id, Some(RequestId::from(7)));
        assert_eq!(code, -32600);
        assert!(matches!(parse(br#"{"jsonrpc":"2.0","id":7,"method":"x"}"#), Ok(Message::Request(_))));
    }
}
