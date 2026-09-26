//! A working language server for editor scenarios.
//!
//! `deco-lsp` has a separate fake server for a different purpose. It simulates
//! failures (exiting at startup, sending invalid responses, never answering a
//! request) to test the client's recovery. This server behaves correctly and
//! answers every request with recognisable content. A scenario can press `f12`
//! and check that the caret moved, or press `ctrl+space` and check that the list
//! on screen contains what the server sent.
//!
//! Both servers are needed. A failing server cannot test whether
//! go-to-definition works, and a working server cannot test what happens when a
//! server exits during a session.
//!
//! The role is `argv[1]`, because `deco.lsp.servers` in `settings.json` can pass
//! `args` but not environment variables. This matches what a user's own
//! configuration can express.
//!
//! Every response is derived from the request: a definition is in the URI the
//! request named, and diagnostics are published for the URI the editor opened.
//! A scenario therefore tests the editor's path-to-URI mapping in both
//! directions, rather than a path defined in this file.

use std::io::{self, BufRead, Write};

fn main() {
    let role = std::env::args().nth(1).unwrap_or_else(|| "full".to_owned());
    std::process::exit(serve(&role));
}

fn serve(role: &str) -> i32 {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    // The document the editor last opened, used in responses.
    let mut open_uri = String::new();

    loop {
        let Some((method, id, params)) = read_frame(&mut input) else {
            return 0;
        };

        match method.as_str() {
            "initialize" => {
                // Exactly the capabilities this server implements. The editor
                // converts them to `editorHas…Provider` context keys, which
                // determine whether keys such as `f12` are bound. Claiming extra
                // capabilities would prevent testing an unbound key.
                let offers_hover = role != "no-hover";
                let offers_rename = role != "no-rename";
                send(
                    &mut output,
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"capabilities": {
                            "positionEncoding": "utf-16",
                            "textDocumentSync": {
                                "openClose": true, "change": 1, "save": true,
                            },
                            "hoverProvider": offers_hover,
                            "definitionProvider": true,
                            "referencesProvider": true,
                            "documentSymbolProvider": true,
                            "renameProvider": offers_rename,
                            // With resolve support, to test code actions that
                            // arrive without an edit and must be resolved.
                            "codeActionProvider": {"resolveProvider": true},
                            "documentFormattingProvider": true,
                            "completionProvider": {
                                "triggerCharacters": ["."],
                            },
                        }},
                    }),
                );
            }
            "textDocument/didOpen" => {
                open_uri = uri_of(&params);
                if role == "diagnostics" {
                    publish(&mut output, &open_uri, 1);
                }
            }
            "textDocument/didChange" => {
                // A different diagnostic after the document is edited, so a
                // scenario can distinguish a stale response from a new one.
                if role == "diagnostics" {
                    publish(&mut output, &open_uri, 2);
                }
            }
            "textDocument/hover" => send(
                &mut output,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "contents": {
                            "kind": "markdown",
                            "value": "**greet**\n\nsays hello to somebody",
                        },
                    },
                }),
            ),
            // Line 2, character 0 of the requested file. A scenario asserts that
            // the caret moved there, which requires the editor's URI mapping to
            // work in both directions.
            "textDocument/definition" => send(
                &mut output,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "uri": uri_of(&params),
                        "range": {
                            "start": {"line": 2, "character": 0},
                            "end": {"line": 2, "character": 5},
                        },
                    },
                }),
            ),
            "textDocument/references" => send(
                &mut output,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": [
                        {
                            "uri": uri_of(&params),
                            "range": {
                                "start": {"line": 0, "character": 3},
                                "end": {"line": 0, "character": 8},
                            },
                        },
                        {
                            "uri": uri_of(&params),
                            "range": {
                                "start": {"line": 3, "character": 1},
                                "end": {"line": 3, "character": 6},
                            },
                        },
                    ],
                }),
            ),
            // Four actions, one for each case the editor must handle: one ready
            // to apply, one whose edit is only available from
            // `codeAction/resolve`, one marked unavailable, and a bare `Command`.
            "textDocument/codeAction" => {
                let open = uri_of(&params);
                send(
                    &mut output,
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": [
                            {
                                "title": "Prefix the name with an underscore",
                                "kind": "quickfix",
                                "isPreferred": true,
                                "edit": {"documentChanges": [{
                                    "textDocument": {"uri": open, "version": null},
                                    "edits": [{
                                        "range": {
                                            "start": {"line": 1, "character": 4},
                                            "end": {"line": 1, "character": 4},
                                        },
                                        "newText": "_",
                                    }],
                                }]},
                            },
                            {
                                "title": "Extract into function",
                                "kind": "refactor.extract",
                                // No edit: `resolve` returns it. The server uses
                                // `data` to identify the action.
                                "data": {"assist": "extract", "uri": open},
                            },
                            {
                                "title": "Inline variable",
                                "kind": "refactor.inline",
                                "disabled": {"reason": "not on a variable"},
                            },
                            {"title": "Organize imports", "command": "example.organizeImports"},
                        ],
                    }),
                );
            }
            // Returns the received action with its edit added. The action
            // received here is the one the editor was sent.
            "codeAction/resolve" => {
                let open = params
                    .get("data")
                    .and_then(|data| data.get("uri"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned();
                let mut action = params.clone();
                action["edit"] = serde_json::json!({"documentChanges": [{
                    "textDocument": {"uri": open, "version": null},
                    "edits": [{
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 0},
                        },
                        "newText": "// extracted\n",
                    }],
                }]});
                send(
                    &mut output,
                    &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": action}),
                );
            }
            // Both occurrences of `greet` in the open file, and the one in
            // `helper.rs`, which the editor has not opened. The scenario tests
            // that second file, because a rename mostly changes files that are
            // not open.
            "textDocument/rename" => {
                let new_name = params
                    .get("newName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("renamed")
                    .to_owned();
                let open = uri_of(&params);
                let helper = open.replace("main.rs", "helper.rs");
                let edit = |line: u32, from: u32, to: u32| {
                    serde_json::json!({
                        "range": {
                            "start": {"line": line, "character": from},
                            "end": {"line": line, "character": to},
                        },
                        "newText": new_name,
                    })
                };
                send(
                    &mut output,
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"documentChanges": [
                            {
                                // No version, because this server does not track
                                // versions. Sending a number would make the
                                // editor check it.
                                "textDocument": {"uri": open, "version": null},
                                "edits": [edit(1, 4, 9), edit(4, 3, 8)],
                            },
                            {
                                "textDocument": {"uri": helper, "version": null},
                                "edits": [edit(1, 4, 9)],
                            },
                        ]},
                    }),
                );
            }
            "textDocument/documentSymbol" => send(
                &mut output,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": [{
                        "name": "greet",
                        "kind": 12,
                        "range": {
                            "start": {"line": 1, "character": 0},
                            "end": {"line": 3, "character": 1},
                        },
                        "selectionRange": {
                            "start": {"line": 1, "character": 3},
                            "end": {"line": 1, "character": 8},
                        },
                    }],
                }),
            ),
            "textDocument/completion" => send(
                &mut output,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "isIncomplete": false,
                        "items": [
                            {"label": "greet_loudly", "kind": 3, "detail": "fn(&str)"},
                            {"label": "greet_quietly", "kind": 3, "detail": "fn(&str)"},
                        ],
                    },
                }),
            ),
            // One edit: an empty range at the start of the document, which
            // inserts a `// formatted` line above the existing text. The edit is
            // small on purpose: a whole-document rewrite would pass even if the
            // editor applied the edit at the wrong offset.
            "textDocument/formatting" => send(
                &mut output,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": [{
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 0},
                        },
                        "newText": "// formatted\n",
                    }],
                }),
            ),
            "shutdown" => send(
                &mut output,
                &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": null}),
            ),
            "exit" => return 0,
            other => {
                // Answer every request, as a real server must, because an
                // unanswered request stalls the client. Notifications have no id
                // and get no response.
                if let Some(id) = id {
                    if !id.is_null() {
                        eprintln!("fake server: nothing to say about {other}");
                        send(
                            &mut output,
                            &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": null}),
                        );
                    }
                }
            }
        }
    }
}

/// Publishes one diagnostic for `uri`, with a message that identifies the
/// round.
fn publish(output: &mut impl Write, uri: &str, round: u32) {
    send(
        output,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": [{
                    "range": {
                        "start": {"line": 1, "character": 2},
                        "end": {"line": 1, "character": 7},
                    },
                    "severity": 1,
                    "code": "E0001",
                    "source": "fake",
                    "message": format!("something is wrong (round {round})"),
                }],
            },
        }),
    );
}

/// The `textDocument.uri` of a request or notification.
fn uri_of(params: &serde_json::Value) -> String {
    params
        .get("textDocument")
        .and_then(|document| document.get("uri"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Reads one framed message and reports its method, id and params.
fn read_frame(
    input: &mut impl BufRead,
) -> Option<(String, Option<serde_json::Value>, serde_json::Value)> {
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length: ") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0u8; length?];
    input.read_exact(&mut body).ok()?;
    let message: serde_json::Value = serde_json::from_slice(&body).ok()?;
    let method = message.get("method")?.as_str()?.to_owned();
    let id = message.get("id").cloned();
    let params = message
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Some((method, id, params))
}

/// Writes one framed message.
fn send(output: &mut impl Write, message: &serde_json::Value) {
    let body = serde_json::to_vec(message).expect("a serialisable message");
    let _ = write!(output, "Content-Length: {}\r\n\r\n", body.len());
    let _ = output.write_all(&body);
    let _ = output.flush();
}
