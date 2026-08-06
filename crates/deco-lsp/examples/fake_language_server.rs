//! A minimal, deliberately misbehaving language server, for tests.
//!
//! `tests/server_process.rs` spawns this to exercise the parts of the client
//! that only exist once a real process and real pipes are involved. It is an
//! example rather than a `[[bin]]` so that it is built by `cargo test` and
//! never shipped, and a separate program rather than the test binary
//! re-executing itself because libtest writes its own progress to stdout —
//! which would land in the middle of the frame stream and be read as a
//! malformed message.
//!
//! `DECO_TEST_LSP_ROLE` selects which behaviour to act out. Each role
//! corresponds to a failure mode the editor has to survive; see the test file.

use std::io::{self, BufRead, Write};
use std::time::Duration;

fn main() {
    let role = std::env::var("DECO_TEST_LSP_ROLE").unwrap_or_else(|_| "plain".to_owned());
    std::process::exit(serve(&role));
}

fn serve(role: &str) -> i32 {
    // Before reading anything, so that "died at startup" is distinguishable
    // from "never started".
    if role == "die-immediately" {
        eprintln!("fake server: refusing to run");
        return 3;
    }
    if role == "silent" {
        // Never answers `initialize`. Exercises the startup timeout, without
        // which a broken server hangs the editor at launch.
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();

    loop {
        let Some((method, id)) = read_frame(&mut input) else {
            return 0;
        };

        match method.as_str() {
            "initialize" => {
                if role == "garbage-on-initialize" {
                    // Not a frame at all. Exercises the protocol-error path
                    // across a real pipe.
                    let _ = output.write_all(b"this is not a frame\r\n\r\n");
                    let _ = output.flush();
                    return 0;
                }
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
                            "hoverProvider": true,
                        }},
                    }),
                );
                // Read by the test that asserts stderr is drained rather than
                // left to fill its pipe.
                eprintln!("fake server: initialized");
            }
            "textDocument/didOpen" => {
                if role == "publish-on-open" {
                    send(
                        &mut output,
                        &serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/publishDiagnostics",
                            "params": {
                                "uri": "file:///w/a.rs",
                                "version": 1,
                                "diagnostics": [{
                                    "range": {
                                        "start": {"line": 2, "character": 4},
                                        "end": {"line": 2, "character": 9},
                                    },
                                    "severity": 1,
                                    "code": "E0308",
                                    "source": "fake",
                                    "message": "mismatched types",
                                }],
                            },
                        }),
                    );
                }
            }
            "textDocument/didSave" => {
                if role == "die-on-save" {
                    // Leaving without a word, which is what a crash looks like
                    // from the editor's side. On save rather than on close, so
                    // the document is still open when the editor notices — that
                    // is the state in which a later edit has to fail cleanly.
                    eprintln!("fake server: crashing on save");
                    return 4;
                }
            }
            "shutdown" => send(
                &mut output,
                &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": null}),
            ),
            "exit" => return 0,
            _ => {
                // Every request is answered, as a real server must: one left
                // hanging would stall the client.
                if let Some(id) = id {
                    if !id.is_null() {
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

/// Reads one framed message, returning its method and id.
fn read_frame(input: &mut impl BufRead) -> Option<(String, Option<serde_json::Value>)> {
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
    std::io::Read::read_exact(input, &mut body).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&body).ok()?;
    Some((
        value.get("method")?.as_str()?.to_owned(),
        value.get("id").cloned(),
    ))
}

fn send(output: &mut impl Write, value: &serde_json::Value) {
    let body = serde_json::to_vec(value).expect("serialisable");
    let _ = write!(output, "Content-Length: {}\r\n\r\n", body.len());
    let _ = output.write_all(&body);
    let _ = output.flush();
}
