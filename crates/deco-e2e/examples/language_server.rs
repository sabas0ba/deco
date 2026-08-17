//! A language server that answers, for scenarios about the editor.
//!
//! `deco-lsp` already has a fake server, and it is a different instrument: that
//! one acts out failure modes — dying at startup, answering with garbage,
//! leaving a request hanging — to exercise the client's recovery. This one does
//! the opposite. It behaves impeccably and answers every request with something
//! recognisable, so that a scenario can press `f12` and ask whether the caret
//! moved, press `ctrl+space` and ask whether the list on screen holds what the
//! server sent.
//!
//! The two are worth having separately. A server that misbehaves cannot tell you
//! whether go-to-definition works, and a server that works cannot tell you what
//! happens when one dies mid-session.
//!
//! The role is `argv[1]`, because `deco.lsp.servers` in `settings.json` can pass
//! `args` and cannot pass environment variables — so this is also the shape a
//! user's own configuration has to be able to express.
//!
//! Everything it answers is derived from what it was *asked*: a definition lands
//! in the URI the request named, diagnostics are published against the URI the
//! editor opened. That is deliberate — it means a scenario sees the editor's own
//! path-to-URI mapping and back, rather than a path this file made up.

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
    // The document the editor last opened, so that answers can be about it.
    let mut open_uri = String::new();

    loop {
        let Some((method, id, params)) = read_frame(&mut input) else {
            return 0;
        };

        match method.as_str() {
            "initialize" => {
                // Every capability this server really answers, and none it does
                // not: the editor turns these into `editorHas…Provider` context
                // keys, which is what decides whether `f12` is bound to anything
                // at all. A server claiming more than it does would make a
                // scenario about an unbound key impossible to write.
                let offers_hover = role != "no-hover";
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
                // A second, different diagnostic once the document has been
                // edited, so a scenario can tell a stale answer from a fresh one.
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
            // Line 2, character 0 of whichever file was asked about. A scenario
            // asserts the caret landed there, which it can only do if the
            // editor's URI mapping survived the round trip.
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
            // One edit covering the first line, replacing it with a canonical
            // form. Narrow on purpose: a whole-document rewrite would pass even
            // if the editor applied the edit at the wrong offset.
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
                // Every request is answered, as a real server must: one left
                // hanging would stall the client. Notifications carry no id and
                // are answered with nothing, which is also correct.
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

/// Publishes one diagnostic against `uri`, worded so a scenario can tell which
/// round it came from.
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
