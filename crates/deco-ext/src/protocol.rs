//! The wire protocol between deco and the Node extension host.
//!
//! Newline-delimited JSON over the host process's stdin and stdout. The host is
//! started with no filesystem, network or process access of its own, so every
//! privileged operation an extension performs arrives here as a request that
//! deco either brokers or refuses.
//!
//! The single most important function in this module is
//! [`required_capability`]: it maps a method name to the capability it needs.
//! It fails closed — an unrecognised method is refused rather than allowed —
//! so adding a privileged method to the host without adding it here makes that
//! method unusable rather than unguarded.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capability::{Capability, PathScope};

/// A request that expects a response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Correlation id, unique within the connection.
    pub id: u64,
    /// The method being called.
    pub method: String,
    /// Method arguments.
    #[serde(default)]
    pub params: Value,
}

/// A reply to a [`Request`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// The id of the request being answered.
    pub id: u64,
    /// The result, when the call succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The failure, when it did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    /// A successful reply.
    pub fn ok(id: u64, result: Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    /// A failed reply.
    pub fn err(id: u64, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// A message that expects no reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    /// The method being signalled.
    pub method: String,
    /// Method arguments.
    #[serde(default)]
    pub params: Value,
}

/// Why a request failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    /// A machine-readable code.
    pub code: ErrorCode,
    /// A human-readable explanation.
    pub message: String,
}

/// Error categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    /// The method name is not known to deco.
    MethodNotFound,
    /// The parameters were missing or the wrong shape.
    InvalidParams,
    /// The capability model refused the operation.
    PermissionDenied,
    /// The operation failed for an ordinary reason (file missing, and so on).
    OperationFailed,
    /// deco hit an unexpected problem.
    Internal,
}

/// Anything that can travel over the connection.
///
/// Tagged explicitly with a `type` field rather than relying on shape. An
/// untagged enum cannot tell a request from a response here: both carry an
/// `id`, every field of `Response` has a default, and serde ignores the extra
/// `method`, so a request silently decodes as an empty response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Message {
    /// A call.
    Request(Request),
    /// A reply.
    Response(Response),
    /// A one-way message.
    Notification(Notification),
}

impl Message {
    /// Encodes as a single line of JSON, newline included.
    pub fn encode(&self) -> String {
        let mut line = serde_json::to_string(self).unwrap_or_else(|_| {
            // Serialising these types cannot fail in practice; a malformed line
            // is still better than a panic inside the editor's event loop.
            String::from(r#"{"method":"$/encodeError","params":{}}"#)
        });
        line.push('\n');
        line
    }

    /// Decodes one line of JSON.
    pub fn decode(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line.trim())
    }
}

/// The capability a method requires, or `None` when the method is mediated by
/// deco and needs no additional privilege.
///
/// Returns `Err(())` for methods deco does not know, which callers must treat
/// as [`ErrorCode::MethodNotFound`] rather than as "no capability needed".
#[allow(clippy::result_unit_err)]
pub fn required_capability(method: &str, params: &Value) -> Result<Option<Capability>, ()> {
    let path = |key: &str| -> Option<PathBuf> { params.get(key)?.as_str().map(PathBuf::from) };
    let read_path = |key: &str| -> Option<Capability> {
        Some(Capability::ReadFile {
            scope: PathScope::Subtree { path: path(key)? },
        })
    };
    let write_path = |key: &str| -> Option<Capability> {
        Some(Capability::WriteFile {
            scope: PathScope::Subtree { path: path(key)? },
        })
    };

    let capability = match method {
        // --- Filesystem, brokered -----------------------------------------
        "fs.readFile" | "fs.stat" | "fs.readDirectory" => read_path("path"),
        "fs.writeFile" | "fs.delete" | "fs.createDirectory" => write_path("path"),
        // A rename touches two places; the destination is the stricter of the
        // two, and the source is checked by the caller as a second request.
        "fs.rename" | "fs.copy" => write_path("target"),

        // --- Editor edits --------------------------------------------------
        // Applying an edit to a file writes it, even though it goes through the
        // editor. Treating it as an editor operation would leave a hole wide
        // enough to drive an extension through.
        "workspace.applyEdit" => write_path("path"),

        // --- Network -------------------------------------------------------
        "net.fetch" | "net.connect" => {
            let url = params
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            host_of(url).map(|host| Capability::Network { host })
        }

        // --- Process -------------------------------------------------------
        "process.spawn" | "process.exec" => {
            params
                .get("program")
                .and_then(Value::as_str)
                .map(|program| Capability::Process {
                    program: program.to_owned(),
                })
        }

        // --- Environment and system ----------------------------------------
        "env.get" => params
            .get("name")
            .and_then(Value::as_str)
            .map(|name| Capability::Env {
                name: name.to_owned(),
            }),
        "env.clipboard.readText" | "env.clipboard.writeText" => Some(Capability::Clipboard),
        "env.openExternal" => Some(Capability::OpenExternal),
        "secrets.get" | "secrets.store" | "secrets.delete" => Some(Capability::Secrets),

        // --- Mediated editor surface, no extra privilege --------------------
        // These only touch state deco already owns and shows to the user.
        "window.showInformationMessage"
        | "window.showWarningMessage"
        | "window.showErrorMessage"
        | "window.showQuickPick"
        | "window.showInputBox"
        | "window.setStatusBarMessage"
        | "window.activeTextEditor"
        | "workspace.getConfiguration"
        | "workspace.workspaceFolders"
        | "workspace.textDocuments"
        | "commands.registerCommand"
        | "commands.executeCommand"
        | "commands.getCommands"
        | "languages.registerProvider"
        | "languages.setDiagnostics"
        | "extension.setContext"
        | "log.append"
        | "$/ready"
        | "$/activated"
        | "$/heartbeat" => None,

        // Fail closed.
        _ => return Err(()),
    };

    // A known privileged method whose parameters did not carry what the check
    // needs is a malformed request, not a free pass.
    match method {
        "window.showInformationMessage"
        | "window.showWarningMessage"
        | "window.showErrorMessage"
        | "window.showQuickPick"
        | "window.showInputBox"
        | "window.setStatusBarMessage"
        | "window.activeTextEditor"
        | "workspace.getConfiguration"
        | "workspace.workspaceFolders"
        | "workspace.textDocuments"
        | "commands.registerCommand"
        | "commands.executeCommand"
        | "commands.getCommands"
        | "languages.registerProvider"
        | "languages.setDiagnostics"
        | "extension.setContext"
        | "log.append"
        | "$/ready"
        | "$/activated"
        | "$/heartbeat" => Ok(None),
        _ => match capability {
            Some(capability) => Ok(Some(capability)),
            None => Err(()),
        },
    }
}

/// Extracts the host from a URL without pulling in a URL parser.
///
/// Only the authority component is needed, and getting it wrong fails closed:
/// an unparseable URL yields `None`, which the caller turns into a refusal.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    // Strip any userinfo, then any port. `user@evil.com@good.com` resolves to
    // the *last* `@` segment, which is what a browser does too.
    let host = authority.rsplit('@').next()?;
    let host = match host.strip_prefix('[') {
        // IPv6 literal: the port, if any, follows the closing bracket.
        Some(inner) => inner.split(']').next()?,
        None => host.split(':').next()?,
    };
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_round_trip_through_the_wire_format() {
        let cases = vec![
            Message::Request(Request {
                id: 1,
                method: "fs.readFile".into(),
                params: json!({}),
            }),
            Message::Response(Response::ok(1, json!("contents"))),
            Message::Response(Response::err(2, ErrorCode::PermissionDenied, "nope")),
            Message::Notification(Notification {
                method: "$/heartbeat".into(),
                params: json!({}),
            }),
        ];
        for message in cases {
            let line = message.encode();
            assert!(line.ends_with('\n'));
            assert_eq!(Message::decode(&line).unwrap(), message, "{line}");
        }
    }

    #[test]
    fn requests_and_responses_are_never_confused() {
        // Both carry an `id`; only the tag tells them apart.
        let response = Message::Response(Response::ok(7, json!(null))).encode();
        assert!(matches!(
            Message::decode(&response).unwrap(),
            Message::Response(_)
        ));

        let request = Message::Request(Request {
            id: 7,
            method: "fs.stat".into(),
            params: json!({}),
        })
        .encode();
        assert!(matches!(
            Message::decode(&request).unwrap(),
            Message::Request(_)
        ));
    }

    #[test]
    fn an_untagged_message_is_rejected() {
        // Nothing without a `type` may be accepted as any variant.
        assert!(Message::decode(r#"{"id":1,"method":"fs.readFile"}"#).is_err());
    }

    #[test]
    fn a_malformed_line_is_an_error() {
        assert!(Message::decode("not json").is_err());
    }

    #[test]
    fn file_reads_and_writes_map_to_the_right_capability() {
        let params = json!({"path": "/w/a.txt"});
        assert_eq!(
            required_capability("fs.readFile", &params).unwrap(),
            Some(Capability::ReadFile {
                scope: PathScope::Subtree {
                    path: "/w/a.txt".into()
                }
            })
        );
        assert_eq!(
            required_capability("fs.writeFile", &params).unwrap(),
            Some(Capability::WriteFile {
                scope: PathScope::Subtree {
                    path: "/w/a.txt".into()
                }
            })
        );
        assert_eq!(
            required_capability("fs.delete", &params).unwrap(),
            Some(Capability::WriteFile {
                scope: PathScope::Subtree {
                    path: "/w/a.txt".into()
                }
            })
        );
    }

    #[test]
    fn applying_a_workspace_edit_counts_as_writing() {
        assert_eq!(
            required_capability("workspace.applyEdit", &json!({"path": "/w/a.txt"})).unwrap(),
            Some(Capability::WriteFile {
                scope: PathScope::Subtree {
                    path: "/w/a.txt".into()
                }
            })
        );
    }

    #[test]
    fn network_requests_map_to_their_host() {
        assert_eq!(
            required_capability(
                "net.fetch",
                &json!({"url": "https://api.example.com/v1?x=1"})
            )
            .unwrap(),
            Some(Capability::Network {
                host: "api.example.com".into()
            })
        );
    }

    #[test]
    fn process_and_env_requests_carry_their_target() {
        assert_eq!(
            required_capability("process.spawn", &json!({"program": "rustfmt"})).unwrap(),
            Some(Capability::Process {
                program: "rustfmt".into()
            })
        );
        assert_eq!(
            required_capability("env.get", &json!({"name": "PATH"})).unwrap(),
            Some(Capability::Env {
                name: "PATH".into()
            })
        );
    }

    #[test]
    fn mediated_editor_methods_need_no_capability() {
        for method in [
            "window.showInformationMessage",
            "commands.registerCommand",
            "workspace.getConfiguration",
            "languages.setDiagnostics",
            "$/ready",
        ] {
            assert_eq!(
                required_capability(method, &json!({})).unwrap(),
                None,
                "{method}"
            );
        }
    }

    #[test]
    fn an_unknown_method_is_refused_rather_than_allowed() {
        assert!(required_capability("fs.mountRoot", &json!({})).is_err());
        assert!(required_capability("", &json!({})).is_err());
        assert!(required_capability("eval", &json!({"code": "1"})).is_err());
    }

    #[test]
    fn a_privileged_method_with_missing_params_is_refused() {
        // No path, so there is nothing to scope the check to. Returning "no
        // capability required" here would be an unguarded write.
        assert!(required_capability("fs.writeFile", &json!({})).is_err());
        assert!(required_capability("net.fetch", &json!({})).is_err());
        assert!(required_capability("process.spawn", &json!({})).is_err());
    }

    #[test]
    fn extracts_hosts_from_urls() {
        assert_eq!(
            host_of("https://example.com/path"),
            Some("example.com".into())
        );
        assert_eq!(
            host_of("http://example.com:8080/"),
            Some("example.com".into())
        );
        assert_eq!(host_of("https://EXAMPLE.com"), Some("example.com".into()));
        assert_eq!(
            host_of("wss://api.example.com/socket?x=1"),
            Some("api.example.com".into())
        );
    }

    #[test]
    fn userinfo_cannot_disguise_the_real_host() {
        // A naive parser reads `good.com` here; the connection actually goes to
        // `evil.com`, so the capability check must see `evil.com`.
        assert_eq!(
            host_of("https://good.com@evil.com/x"),
            Some("evil.com".into())
        );
        assert_eq!(
            host_of("https://user:pass@evil.com/x"),
            Some("evil.com".into())
        );
        assert_eq!(host_of("https://a@b@evil.com/x"), Some("evil.com".into()));
    }

    #[test]
    fn ipv6_literals_keep_their_brackets_off_and_their_port_stripped() {
        assert_eq!(host_of("http://[::1]:8080/x"), Some("::1".into()));
        assert_eq!(host_of("http://[2001:db8::1]/"), Some("2001:db8::1".into()));
    }

    #[test]
    fn unparseable_urls_yield_no_host() {
        assert_eq!(host_of("not a url"), None);
        assert_eq!(host_of("https://"), None);
        assert_eq!(host_of("https:///path"), None);
    }
}
