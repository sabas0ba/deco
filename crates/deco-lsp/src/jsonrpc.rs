//! JSON-RPC 2.0 as the Language Server Protocol uses it, and its framing.
//!
//! Deliberately separate from `deco-remote`'s wire format even though both are
//! length-prefixed JSON. That one is a protocol deco defines on both ends and
//! can tag however it likes; this one is fixed by a specification and spoken by
//! programs written by other people, so it has to match exactly — `"jsonrpc":
//! "2.0"` on every message, ids that may be a number *or* a string, and an
//! `error` object with a numeric code.
//!
//! The one piece of real subtlety is telling the three message kinds apart.
//! They are distinguished by which fields are present, not by a tag:
//!
//! | `id` | `method` | kind |
//! | --- | --- | --- |
//! | yes | yes | request |
//! | no | yes | notification |
//! | yes | no | response |
//!
//! `#[serde(untagged)]` is the obvious way to express that and the wrong one:
//! it tries each variant in order and takes the first that deserialises, so a
//! request whose `params` happen to fit a response shape decodes as the wrong
//! thing and the failure appears far from its cause. This module decodes into
//! one permissive struct and then classifies, so an ambiguous message is a
//! named error rather than a silent mis-parse.

use std::io::{BufRead, Write};

use serde::{Deserialize, Serialize};

/// A request identifier.
///
/// LSP permits either a number or a string, and a server that was given a
/// string id must be answered with the same string. deco only ever allocates
/// numbers, but it receives both, because servers send requests too.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// A numeric id. What deco allocates.
    Number(i64),
    /// A string id. Only ever seen on requests from a server.
    String(String),
}

impl From<i64> for RequestId {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => write!(f, "{s:?}"),
        }
    }
}

/// A call that expects a reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Correlation id. Must appear on the response.
    pub id: RequestId,
    /// The LSP method, e.g. `textDocument/hover`.
    pub method: String,
    /// Method arguments. Absent is distinct from `null` for some servers, so an
    /// absent value stays absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A one-way message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    /// The LSP method, e.g. `textDocument/didChange`.
    pub method: String,
    /// Method arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A reply to a request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// The id being answered.
    pub id: RequestId,
    /// The result, on success. `null` is a valid successful result — several
    /// methods return it — so this is `Some(Value::Null)` rather than `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// The failure, on error. Exactly one of `result` and `error` is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

impl Response {
    /// A successful reply.
    pub fn ok(id: RequestId, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    /// A failed reply.
    pub fn err(id: RequestId, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(ResponseError {
                code: code as i64,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// The `error` member of a response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseError {
    /// A code from [`ErrorCode`], or a server-specific one.
    pub code: i64,
    /// Human-readable explanation.
    pub message: String,
    /// Optional structured detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl std::fmt::Display for ResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match ErrorCode::from_code(self.code) {
            Some(known) => write!(f, "{} ({})", self.message, known.name()),
            None => write!(f, "{} (code {})", self.message, self.code),
        }
    }
}

/// The error codes LSP defines, on top of JSON-RPC's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum ErrorCode {
    /// Invalid JSON.
    ParseError = -32700,
    /// Not a valid request object.
    InvalidRequest = -32600,
    /// No such method.
    MethodNotFound = -32601,
    /// The parameters did not fit the method.
    InvalidParams = -32602,
    /// The peer failed internally.
    InternalError = -32603,
    /// A request arrived before `initialize` completed.
    ServerNotInitialized = -32002,
    /// The request was cancelled via `$/cancelRequest`.
    RequestCancelled = -32800,
    /// The document changed while the request was in flight, so the answer
    /// would have referred to text that no longer exists.
    ContentModified = -32801,
}

impl ErrorCode {
    /// Recognises a numeric code, if it is one of the defined ones.
    pub fn from_code(code: i64) -> Option<Self> {
        Some(match code {
            -32700 => Self::ParseError,
            -32600 => Self::InvalidRequest,
            -32601 => Self::MethodNotFound,
            -32602 => Self::InvalidParams,
            -32603 => Self::InternalError,
            -32002 => Self::ServerNotInitialized,
            -32800 => Self::RequestCancelled,
            -32801 => Self::ContentModified,
            _ => return None,
        })
    }

    /// The specification's name for this code.
    pub fn name(self) -> &'static str {
        match self {
            Self::ParseError => "ParseError",
            Self::InvalidRequest => "InvalidRequest",
            Self::MethodNotFound => "MethodNotFound",
            Self::InvalidParams => "InvalidParams",
            Self::InternalError => "InternalError",
            Self::ServerNotInitialized => "ServerNotInitialized",
            Self::RequestCancelled => "RequestCancelled",
            Self::ContentModified => "ContentModified",
        }
    }

    /// Whether a failed request is worth reporting to the user.
    ///
    /// A cancellation is something the editor asked for, and a
    /// `ContentModified` means the user typed while the server was thinking.
    /// Both are routine; surfacing them would fill the screen with noise during
    /// ordinary editing.
    pub fn is_expected(self) -> bool {
        matches!(self, Self::RequestCancelled | Self::ContentModified)
    }
}

/// One message in either direction.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// A call expecting a reply.
    Request(Request),
    /// A one-way message.
    Notification(Notification),
    /// A reply.
    Response(Response),
}

impl Message {
    /// The method name, for a request or notification.
    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Request(r) => Some(&r.method),
            Self::Notification(n) => Some(&n.method),
            Self::Response(_) => None,
        }
    }
}

/// The permissive shape every message is decoded into before being classified.
#[derive(Deserialize)]
struct RawMessage {
    #[serde(default)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<serde_json::Value>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<ResponseError>,
}

/// Why a message could not be read or classified.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// The stream failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A header line was not `Name: value`.
    #[error("malformed header line: {line:?}")]
    MalformedHeader {
        /// The offending line.
        line: String,
    },
    /// The headers ended without a `Content-Length`.
    #[error("frame has no Content-Length header")]
    MissingContentLength,
    /// A frame claimed to be larger than deco will accept.
    #[error("frame of {size} bytes exceeds the {limit} byte limit")]
    TooLarge {
        /// The claimed size.
        size: usize,
        /// The limit.
        limit: usize,
    },
    /// The body was not valid JSON.
    #[error("frame body is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The JSON was valid but was not a request, notification or response.
    #[error("message is neither a request, a notification nor a response: {reason}")]
    NotAMessage {
        /// Which rule it broke.
        reason: &'static str,
    },
    /// `jsonrpc` was present and was not `"2.0"`.
    #[error("unsupported jsonrpc version {version:?}")]
    UnsupportedVersion {
        /// What the peer claimed.
        version: String,
    },
}

/// The largest frame that will be read.
///
/// A language server is a subprocess, not a trusted part of the editor: a
/// runaway or hostile one announcing `Content-Length: 999999999999` should get
/// an error rather than an allocation the size of the machine's memory.
pub const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;

/// Classifies a decoded JSON value.
pub fn classify(value: serde_json::Value) -> Result<Message, ProtocolError> {
    // Whether `result` is *present* has to be read from the object itself.
    // Deserialising it as `Option<Value>` collapses `"result": null` and an
    // absent `result` into the same `None`, and the difference is the whole
    // meaning of the message: `null` is how `textDocument/definition` says
    // "nothing here", while absent means this is not a response at all.
    let has_result = value
        .as_object()
        .is_some_and(|object| object.contains_key("result"));

    let raw: RawMessage = serde_json::from_value(value)?;
    let result = if has_result {
        Some(raw.result.unwrap_or(serde_json::Value::Null))
    } else {
        None
    };

    // Absent is tolerated: a handful of servers omit it. A *wrong* version is
    // not, because it means the peer is speaking something else entirely.
    if let Some(version) = &raw.jsonrpc {
        if version != "2.0" {
            return Err(ProtocolError::UnsupportedVersion {
                version: version.clone(),
            });
        }
    }

    let id = match raw.id {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Number(n)) => Some(RequestId::Number(n.as_i64().ok_or(
            ProtocolError::NotAMessage {
                reason: "id is a number that is not an integer",
            },
        )?)),
        Some(serde_json::Value::String(s)) => Some(RequestId::String(s)),
        Some(_) => {
            return Err(ProtocolError::NotAMessage {
                reason: "id is neither a number nor a string",
            })
        }
    };

    match (id, raw.method) {
        (Some(id), Some(method)) => Ok(Message::Request(Request {
            id,
            method,
            params: raw.params,
        })),
        (None, Some(method)) => Ok(Message::Notification(Notification {
            method,
            params: raw.params,
        })),
        (Some(id), None) => {
            if result.is_none() && raw.error.is_none() {
                return Err(ProtocolError::NotAMessage {
                    reason: "a response needs a result or an error",
                });
            }
            if result.is_some() && raw.error.is_some() {
                return Err(ProtocolError::NotAMessage {
                    reason: "a response cannot carry both a result and an error",
                });
            }
            Ok(Message::Response(Response {
                id,
                result,
                error: raw.error,
            }))
        }
        (None, None) => Err(ProtocolError::NotAMessage {
            reason: "no method and no id",
        }),
    }
}

/// Renders a message as the JSON object that goes on the wire.
pub fn to_value(message: &Message) -> serde_json::Value {
    let mut value = match message {
        Message::Request(r) => serde_json::to_value(r),
        Message::Notification(n) => serde_json::to_value(n),
        Message::Response(r) => serde_json::to_value(r),
    }
    .expect("a message is always representable as JSON");

    // `jsonrpc` lives here rather than as a struct field so that a caller
    // cannot construct a message without it, and so that it cannot be set to
    // anything but "2.0".
    if let Some(object) = value.as_object_mut() {
        object.insert("jsonrpc".into(), serde_json::Value::String("2.0".into()));
        // A successful response with a `null` result must still carry the
        // member; serde skips `None` but `Some(Null)` survives, and the
        // distinction matters to servers that check for its presence.
        if let Message::Response(response) = message {
            if response.result.is_none() && response.error.is_none() {
                object.insert("result".into(), serde_json::Value::Null);
            }
        }
    }
    value
}

/// Writes one message, framed.
pub fn write(out: &mut impl Write, message: &Message) -> Result<(), ProtocolError> {
    let body = serde_json::to_vec(&to_value(message))?;
    // No `Content-Type`: the specification's default is the only one anybody
    // implements, and some servers reject the header outright.
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()?;
    Ok(())
}

/// Reads one message, or `None` at a clean end of stream.
pub fn read(input: &mut impl BufRead) -> Result<Option<Message>, ProtocolError> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    let mut saw_header = false;

    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            // End of stream between frames is how a server exits normally.
            return if saw_header {
                Err(ProtocolError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "stream ended mid-header",
                )))
            } else {
                Ok(None)
            };
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        saw_header = true;
        let Some((name, value)) = trimmed.split_once(':') else {
            return Err(ProtocolError::MalformedHeader {
                line: trimmed.to_owned(),
            });
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse::<usize>().map(Some).map_err(|_| {
                ProtocolError::MalformedHeader {
                    line: trimmed.to_owned(),
                }
            })?;
        }
        // Every other header is ignored so that a `Content-Type` from a newer
        // peer does not break the connection.
    }

    let size = content_length.ok_or(ProtocolError::MissingContentLength)?;
    if size > MAX_FRAME_BYTES {
        return Err(ProtocolError::TooLarge {
            size,
            limit: MAX_FRAME_BYTES,
        });
    }

    let mut body = vec![0u8; size];
    input.read_exact(&mut body)?;
    classify(serde_json::from_slice(&body)?).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decode(value: serde_json::Value) -> Message {
        classify(value).expect("should classify")
    }

    #[test]
    fn a_request_has_an_id_and_a_method() {
        let message = decode(json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "textDocument/hover", "params": {"x": 1}
        }));
        assert_eq!(
            message,
            Message::Request(Request {
                id: RequestId::Number(1),
                method: "textDocument/hover".into(),
                params: Some(json!({"x": 1})),
            })
        );
    }

    #[test]
    fn a_notification_has_a_method_and_no_id() {
        let message = decode(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
        assert!(matches!(message, Message::Notification(_)));
    }

    #[test]
    fn a_response_has_an_id_and_no_method() {
        let message = decode(json!({"jsonrpc": "2.0", "id": 7, "result": {"ok": true}}));
        assert!(matches!(message, Message::Response(_)));
    }

    #[test]
    fn a_request_is_not_mistaken_for_an_empty_response() {
        // The failure an `#[serde(untagged)]` enum produces: both carry `id`,
        // and every field of a response is optional, so a request decodes as a
        // response with nothing in it and the method is silently lost.
        let message = decode(json!({"jsonrpc": "2.0", "id": 1, "method": "shutdown"}));
        assert_eq!(message.method(), Some("shutdown"));
    }

    #[test]
    fn a_null_result_is_a_successful_response_not_an_absent_one() {
        // `textDocument/definition` answers `null` for "no definition here",
        // which is a successful answer and must not be read as an error.
        let Message::Response(response) = decode(json!({"id": 1, "result": null})) else {
            panic!("expected a response");
        };
        assert_eq!(response.result, Some(serde_json::Value::Null));
        assert!(response.error.is_none());
    }

    #[test]
    fn a_null_id_is_treated_as_absent() {
        // JSON-RPC uses a null id on an error that could not be correlated.
        assert!(matches!(
            decode(json!({"method": "exit", "id": null})),
            Message::Notification(_)
        ));
    }

    #[test]
    fn a_string_id_is_preserved_exactly() {
        // A server that sends `"id": "3"` must be answered with the string, not
        // the number. Coercing here would leave its request unanswered forever.
        let Message::Request(request) =
            decode(json!({"id": "3", "method": "window/showMessageRequest"}))
        else {
            panic!("expected a request");
        };
        assert_eq!(request.id, RequestId::String("3".into()));
        assert_ne!(request.id, RequestId::Number(3));
    }

    #[test]
    fn a_message_with_neither_id_nor_method_is_refused() {
        assert!(matches!(
            classify(json!({"jsonrpc": "2.0"})),
            Err(ProtocolError::NotAMessage { .. })
        ));
    }

    #[test]
    fn a_response_with_both_a_result_and_an_error_is_refused() {
        // Ambiguous by the specification. Picking one would mean sometimes
        // reporting success for a call that failed.
        assert!(matches!(
            classify(json!({"id": 1, "result": 1, "error": {"code": -1, "message": "x"}})),
            Err(ProtocolError::NotAMessage { .. })
        ));
    }

    #[test]
    fn a_response_with_neither_a_result_nor_an_error_is_refused() {
        assert!(matches!(
            classify(json!({"id": 1})),
            Err(ProtocolError::NotAMessage { .. })
        ));
    }

    #[test]
    fn a_wrong_jsonrpc_version_is_refused_but_an_absent_one_is_not() {
        assert!(matches!(
            classify(json!({"jsonrpc": "1.0", "id": 1, "method": "x"})),
            Err(ProtocolError::UnsupportedVersion { .. })
        ));
        assert!(classify(json!({"id": 1, "method": "x"})).is_ok());
    }

    #[test]
    fn a_non_integer_id_is_refused() {
        for id in [json!(1.5), json!([1]), json!({})] {
            assert!(
                matches!(
                    classify(json!({"id": id, "method": "x"})),
                    Err(ProtocolError::NotAMessage { .. })
                ),
                "{id} should be refused"
            );
        }
    }

    #[test]
    fn every_outgoing_message_carries_the_version() {
        for message in [
            Message::Request(Request {
                id: 1.into(),
                method: "initialize".into(),
                params: None,
            }),
            Message::Notification(Notification {
                method: "exit".into(),
                params: None,
            }),
            Message::Response(Response::ok(1.into(), json!(null))),
        ] {
            assert_eq!(to_value(&message)["jsonrpc"], json!("2.0"));
        }
    }

    #[test]
    fn absent_params_stay_absent() {
        // `"params": null` is rejected by stricter servers, so an absent value
        // must not be materialised as null.
        let value = to_value(&Message::Notification(Notification {
            method: "exit".into(),
            params: None,
        }));
        assert!(!value.as_object().unwrap().contains_key("params"));
    }

    #[test]
    fn messages_round_trip_through_the_framing() {
        let messages = vec![
            Message::Request(Request {
                id: RequestId::String("abc".into()),
                method: "textDocument/completion".into(),
                params: Some(json!({"position": {"line": 1, "character": 0}})),
            }),
            Message::Notification(Notification {
                method: "textDocument/didChange".into(),
                params: Some(json!({"contentChanges": []})),
            }),
            Message::Response(Response::ok(2.into(), json!({"contents": "doc"}))),
            Message::Response(Response::err(
                3.into(),
                ErrorCode::RequestCancelled,
                "cancelled",
            )),
        ];

        let mut stream: Vec<u8> = Vec::new();
        for message in &messages {
            write(&mut stream, message).unwrap();
        }

        let mut input = std::io::Cursor::new(stream);
        for expected in &messages {
            assert_eq!(read(&mut input).unwrap().as_ref(), Some(expected));
        }
        assert_eq!(read(&mut input).unwrap(), None, "clean end of stream");
    }

    #[test]
    fn the_body_length_is_counted_in_bytes_not_characters() {
        // A header counting characters truncates the body mid-way through a
        // multi-byte sequence, and every subsequent frame is misaligned.
        let message = Message::Notification(Notification {
            method: "window/logMessage".into(),
            params: Some(json!({"message": "日本語のログ"})),
        });
        let mut stream = Vec::new();
        write(&mut stream, &message).unwrap();

        let text = String::from_utf8(stream.clone()).unwrap();
        let (header, body) = text.split_once("\r\n\r\n").unwrap();
        let declared: usize = header
            .strip_prefix("Content-Length: ")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(declared, body.len());
        assert_ne!(declared, body.chars().count());

        assert_eq!(
            read(&mut std::io::Cursor::new(stream)).unwrap(),
            Some(message)
        );
    }

    #[test]
    fn an_unknown_header_is_ignored() {
        // A `Content-Type` from a newer peer must not break the connection.
        let body = r#"{"id":1,"method":"shutdown"}"#;
        let frame = format!(
            "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc\r\n\r\n{body}",
            body.len()
        );
        let mut input = std::io::Cursor::new(frame.as_bytes());
        assert_eq!(
            read(&mut input).unwrap().unwrap().method(),
            Some("shutdown")
        );
    }

    #[test]
    fn an_oversized_frame_is_refused_before_allocating() {
        let frame = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1);
        assert!(matches!(
            read(&mut std::io::Cursor::new(frame.as_bytes())),
            Err(ProtocolError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_frame_without_a_length_is_refused() {
        assert!(matches!(
            read(&mut std::io::Cursor::new(b"X-Thing: 1\r\n\r\n{}".as_ref())),
            Err(ProtocolError::MissingContentLength)
        ));
    }

    #[test]
    fn a_truncated_header_is_an_error_not_a_clean_end() {
        // Distinguishing this from a clean exit is what tells the editor the
        // server crashed rather than shut down.
        assert!(matches!(
            read(&mut std::io::Cursor::new(b"Content-Length: 5".as_ref())),
            Err(ProtocolError::Io(_))
        ));
    }

    #[test]
    fn a_malformed_header_line_names_itself() {
        let Err(ProtocolError::MalformedHeader { line }) =
            read(&mut std::io::Cursor::new(b"not a header\r\n\r\n".as_ref()))
        else {
            panic!("expected a malformed header");
        };
        assert_eq!(line, "not a header");
    }

    #[test]
    fn cancellation_and_content_modified_are_expected_failures() {
        // These arrive during ordinary typing; reporting them would bury the
        // errors that do matter.
        assert!(ErrorCode::RequestCancelled.is_expected());
        assert!(ErrorCode::ContentModified.is_expected());
        assert!(!ErrorCode::InternalError.is_expected());
        assert!(!ErrorCode::MethodNotFound.is_expected());
    }

    #[test]
    fn every_defined_code_round_trips_and_has_a_name() {
        for code in [
            ErrorCode::ParseError,
            ErrorCode::InvalidRequest,
            ErrorCode::MethodNotFound,
            ErrorCode::InvalidParams,
            ErrorCode::InternalError,
            ErrorCode::ServerNotInitialized,
            ErrorCode::RequestCancelled,
            ErrorCode::ContentModified,
        ] {
            assert_eq!(ErrorCode::from_code(code as i64), Some(code));
            assert!(!code.name().is_empty());
        }
        assert_eq!(ErrorCode::from_code(1), None, "server-specific codes pass");
    }

    #[test]
    fn an_unknown_error_code_still_renders_readably() {
        let error = ResponseError {
            code: 42,
            message: "server said no".into(),
            data: None,
        };
        assert_eq!(error.to_string(), "server said no (code 42)");
    }
}
