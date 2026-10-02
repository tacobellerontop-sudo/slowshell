//! Request/response protocol between `Shell.exe` and `shellctl.exe`.
//!
//! This module is transport-agnostic: it defines the message shapes and the line
//! framing. The named-pipe transport lives in `slowshell-win`, which keeps the core
//! free of platform dependencies and makes the protocol testable without Windows.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};

/// A request from the CLI to the running shell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
pub enum Request {
    /// Is a shell running, and what state is it in?
    Status,
    /// Re-read configuration and rebuild the scene.
    Reload,
    /// Ask the shell to exit cleanly.
    Stop,
    /// Toggle the developer error overlay.
    ToggleOverlay,
    /// Return recent log records.
    Logs { limit: usize, level: String },
    /// Return current diagnostics.
    Diagnostics,
    /// Return the property graph.
    Graph,
    /// Return the monitors as the shell sees them.
    Screens,
    /// Open a named surface, e.g. `{"name": "launcher"}`.
    Open { name: String },
}

impl Request {
    pub fn parse(line: &str) -> Result<Request, String> {
        serde_json::from_str(line).map_err(|e| format!("malformed request: {e}"))
    }

    /// The command name as typed, for logs and the help text.
    pub fn name(&self) -> &'static str {
        match self {
            Request::Status => "status",
            Request::Reload => "reload",
            Request::Stop => "stop",
            Request::ToggleOverlay => "overlay",
            Request::Logs { .. } => "logs",
            Request::Diagnostics => "diagnostics",
            Request::Graph => "graph",
            Request::Screens => "screens",
            Request::Open { .. } => "open",
        }
    }
}

/// The shell's reply. Every variant carries enough context for the CLI to print
/// something the user can act on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum Response {
    Ok { data: serde_json::Value },
    Err { message: String },
}

impl Response {
    pub fn ok(data: serde_json::Value) -> Response {
        Response::Ok { data }
    }

    pub fn err(message: impl Into<String>) -> Response {
        Response::Err { message: message.into() }
    }

    pub fn into_result(self) -> Result<serde_json::Value, String> {
        match self {
            Response::Ok { data } => Ok(data),
            Response::Err { message } => Err(message),
        }
    }
}

/// Encode one message as a single line.
pub fn write_line<W: Write>(w: &mut W, v: &impl Serialize) -> std::io::Result<()> {
    let s = serde_json::to_string(v).map_err(std::io::Error::other)?;
    // A newline inside the payload would desynchronize a line-based transport.
    debug_assert!(!s.contains('\n'), "IPC payload must not contain a newline");
    w.write_all(s.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()
}

/// Read one line, or `None` at end of stream.
pub fn read_line<R: BufRead>(r: &mut R) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(trimmed.to_string()))
}

/// Serve requests on a byte stream, one connection at a time.
///
/// The pipe server hands each connected client to `handler`. Serial handling is
/// intentional: `shellctl` is a debugging tool, not a hot path, and a single
/// client at a time means a reload cannot interleave with a status query.
pub fn serve<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    handler: impl FnOnce(Request) -> Response,
) -> std::io::Result<Response> {
    let Some(line) = read_line(reader)? else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "client disconnected without sending a request",
        ));
    };
    let req = Request::parse(&line).map_err(std::io::Error::other)?;
    let resp = handler(req);
    write_line(writer, &resp)?;
    Ok(resp)
}

/// Convenience for a blocking client: send one request, read one reply.
pub fn call<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    req: &Request,
) -> Result<Response, String> {
    write_line(writer, req).map_err(|e| format!("write failed: {e}"))?;
    let Some(line) = read_line(reader).map_err(|e| format!("read failed: {e}"))? else {
        return Err("shell closed the connection without replying".into());
    };
    serde_json::from_str(&line).map_err(|e| format!("malformed response: {e}"))
}

/// Reads many requests from one stream, so a caller can multiplex without
/// reconnecting. Used by the settings app.
pub struct Client<R: BufRead, W: Write> {
    reader: BufReader<R>,
    writer: W,
}

impl<R: BufRead, W: Write> Client<R, W> {
    pub fn new(reader: R, writer: W) -> Client<R, W> {
        Client { reader: BufReader::new(reader), writer }
    }

    pub fn send(&mut self, req: &Request) -> Result<Response, String> {
        call(&mut self.reader, &mut self.writer, req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn requests_round_trip_through_json() {
        let r = Request::Reload;
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(Request::parse(&s).unwrap(), Request::Reload);
    }

    #[test]
    fn tagged_enum_uses_compact_keys() {
        let s = serde_json::to_string(&Request::Open { name: "launcher".into() }).unwrap();
        assert!(s.contains("\"cmd\":\"open\""), "{s}");
        assert!(s.contains("\"name\":\"launcher\""), "{s}");
    }

    #[test]
    fn every_variant_round_trips() {
        let all = [
            Request::Status,
            Request::Reload,
            Request::Stop,
            Request::ToggleOverlay,
            Request::Logs { limit: 10, level: "info".into() },
            Request::Diagnostics,
            Request::Graph,
            Request::Screens,
            Request::Open { name: "x".into() },
        ];
        for r in all {
            let s = serde_json::to_string(&r).unwrap();
            assert!(!s.contains('\n'), "payload must be one line: {s}");
            assert_eq!(Request::parse(&s).unwrap(), r);
        }
    }

    #[test]
    fn responses_carry_a_status_tag() {
        let s = serde_json::to_string(&Response::ok(serde_json::json!({"pid": 1}))).unwrap();
        assert!(s.contains("\"status\":\"ok\""), "{s}");
        assert!(Response::err("nope").into_result().is_err());
    }

    #[test]
    fn malformed_input_is_rejected() {
        assert!(Request::parse("{not json").is_err());
    }

    #[test]
    fn serve_dispatches_and_replies() {
        let input = b"{\"cmd\":\"reload\"}\n";
        let mut r = Cursor::new(input.to_vec());
        let mut out: Vec<u8> = Vec::new();
        let resp = serve(&mut r, &mut out, |req| {
            assert_eq!(req, Request::Reload);
            Response::ok(serde_json::json!({"reloaded": true}))
        })
        .unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"reloaded\":true"), "{text}");
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn call_returns_an_error_when_the_shell_disconnects() {
        let mut r = Cursor::new(Vec::new());
        let mut w: Vec<u8> = Vec::new();
        assert!(call(&mut r, &mut w, &Request::Status).is_err());
    }
}
