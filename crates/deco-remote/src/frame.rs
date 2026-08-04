//! The framing deco uses between a local frontend and a remote server.
//!
//! Length-prefixed JSON: `Content-Length: N\r\n\r\n` followed by exactly `N`
//! bytes. This is the Language Server Protocol's base framing, chosen because
//! it survives a stream that also carries a remote shell's stray output, and
//! because anything that can already speak to a language server can speak this.

use std::io::{BufRead, Write};

use serde::{Deserialize, Serialize};

/// One message on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Message {
    /// A call expecting a reply.
    Request {
        /// Correlation id.
        id: u64,
        /// The method being called.
        method: String,
        /// Arguments.
        #[serde(default)]
        params: serde_json::Value,
    },
    /// A reply.
    Response {
        /// The id being answered.
        id: u64,
        /// The result, when the call succeeded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        /// The failure, when it did not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// A one-way message.
    Notification {
        /// The method being signalled.
        method: String,
        /// Arguments.
        #[serde(default)]
        params: serde_json::Value,
    },
}

/// Failure to read or write a frame.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
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
    /// The body was not the expected JSON.
    #[error("frame body is not a valid message: {0}")]
    Malformed(#[from] serde_json::Error),
}

/// The largest frame that will be read.
///
/// A remote is not automatically trusted with the local machine's memory: a
/// hostile or broken peer sending `Content-Length: 999999999999` should get an
/// error rather than an allocation.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Writes one message.
pub fn write(out: &mut impl Write, message: &Message) -> Result<(), FrameError> {
    let body = serde_json::to_vec(message)?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()?;
    Ok(())
}

/// Reads one message, or `None` at a clean end of stream.
pub fn read(input: &mut impl BufRead) -> Result<Option<Message>, FrameError> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();

    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            // End of stream between frames is how a session ends normally.
            return if content_length.is_none() {
                Ok(None)
            } else {
                Err(FrameError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "stream ended mid-header",
                )))
            };
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            return Err(FrameError::MalformedHeader {
                line: trimmed.to_owned(),
            });
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse::<usize>().map(Some).map_err(|_| {
                FrameError::MalformedHeader {
                    line: trimmed.to_owned(),
                }
            })?;
        }
        // Any other header is ignored, which keeps a future Content-Type from
        // breaking an older peer.
    }

    let size = content_length.ok_or(FrameError::MissingContentLength)?;
    if size > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            size,
            limit: MAX_FRAME_BYTES,
        });
    }

    let mut body = vec![0u8; size];
    input.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> Message {
        Message::Request {
            id: 1,
            method: "fs.read".into(),
            params: json!({"path": "/a"}),
        }
    }

    #[test]
    fn messages_round_trip() {
        for message in [
            request(),
            Message::Response {
                id: 1,
                result: Some(json!("ok")),
                error: None,
            },
            Message::Response {
                id: 2,
                result: None,
                error: Some("nope".into()),
            },
            Message::Notification {
                method: "log".into(),
                params: json!({}),
            },
        ] {
            let mut out: Vec<u8> = Vec::new();
            write(&mut out, &message).unwrap();
            let mut input = std::io::Cursor::new(out);
            assert_eq!(read(&mut input).unwrap(), Some(message));
        }
    }

    #[test]
    fn the_frame_carries_a_content_length_header() {
        let mut out: Vec<u8> = Vec::new();
        write(&mut out, &request()).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("Content-Length: "));
        assert!(text.contains("\r\n\r\n"));
    }

    #[test]
    fn several_frames_read_back_in_order() {
        let mut out: Vec<u8> = Vec::new();
        for id in 1..=3u64 {
            write(
                &mut out,
                &Message::Response {
                    id,
                    result: Some(json!(id)),
                    error: None,
                },
            )
            .unwrap();
        }
        let mut input = std::io::Cursor::new(out);
        for id in 1..=3u64 {
            match read(&mut input).unwrap().unwrap() {
                Message::Response { id: got, .. } => assert_eq!(got, id),
                other => panic!("expected a response, got {other:?}"),
            }
        }
        assert_eq!(read(&mut input).unwrap(), None);
    }

    #[test]
    fn a_clean_end_of_stream_is_not_an_error() {
        let mut input = std::io::Cursor::new(Vec::new());
        assert_eq!(read(&mut input).unwrap(), None);
    }

    #[test]
    fn unknown_headers_are_ignored() {
        let body = serde_json::to_vec(&request()).unwrap();
        let mut framed = format!(
            "Content-Type: application/vscode-jsonrpc\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        framed.extend_from_slice(&body);
        let mut input = std::io::Cursor::new(framed);
        assert_eq!(read(&mut input).unwrap(), Some(request()));
    }

    #[test]
    fn header_names_are_matched_case_insensitively() {
        let body = serde_json::to_vec(&request()).unwrap();
        let mut framed = format!("content-length: {}\r\n\r\n", body.len()).into_bytes();
        framed.extend_from_slice(&body);
        let mut input = std::io::Cursor::new(framed);
        assert_eq!(read(&mut input).unwrap(), Some(request()));
    }

    #[test]
    fn a_missing_content_length_is_an_error() {
        let mut input = std::io::Cursor::new(b"X-Thing: 1\r\n\r\n{}".to_vec());
        assert!(matches!(
            read(&mut input),
            Err(FrameError::MissingContentLength)
        ));
    }

    #[test]
    fn a_malformed_header_is_an_error() {
        let mut input = std::io::Cursor::new(b"not a header\r\n\r\n".to_vec());
        assert!(matches!(
            read(&mut input),
            Err(FrameError::MalformedHeader { .. })
        ));
    }

    #[test]
    fn a_non_numeric_content_length_is_an_error() {
        let mut input = std::io::Cursor::new(b"Content-Length: lots\r\n\r\n".to_vec());
        assert!(matches!(
            read(&mut input),
            Err(FrameError::MalformedHeader { .. })
        ));
    }

    #[test]
    fn an_absurd_content_length_is_refused_before_allocating() {
        let claimed = MAX_FRAME_BYTES + 1;
        let mut input =
            std::io::Cursor::new(format!("Content-Length: {claimed}\r\n\r\n").into_bytes());
        match read(&mut input) {
            Err(FrameError::TooLarge { size, limit }) => {
                assert_eq!(size, claimed);
                assert_eq!(limit, MAX_FRAME_BYTES);
            }
            other => panic!("expected a size refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_truncated_body_is_an_error_rather_than_a_short_read() {
        let mut input = std::io::Cursor::new(b"Content-Length: 100\r\n\r\n{}".to_vec());
        assert!(read(&mut input).is_err());
    }

    #[test]
    fn a_body_that_is_not_a_message_is_an_error() {
        let body = b"{\"nope\": true}";
        let mut framed = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        framed.extend_from_slice(body);
        let mut input = std::io::Cursor::new(framed);
        assert!(matches!(read(&mut input), Err(FrameError::Malformed(_))));
    }
}
