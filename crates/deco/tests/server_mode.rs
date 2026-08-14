//! `deco --server --stdio`, run as a process.
//!
//! `deco_remote::server` has tests for what it answers; these are for the part
//! only a process can show: that the flag reaches the server at all, that the
//! protocol is the *only* thing on stdout, and that a client on the other end of
//! a pipe gets frames back.
//!
//! Run in the ordinary suite — this needs no Node, no container and no network,
//! only the binary that `cargo test` has already built.

use std::io::{BufReader, Write};
use std::process::{Command, Stdio};

use deco_remote::frame::{self, Message};

/// A workspace with one file in it.
fn workspace(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "deco-server-mode-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).expect("a directory");
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("a file");
    root
}

fn request(id: u64, method: &str, params: serde_json::Value) -> Message {
    Message::Request {
        id,
        method: method.to_owned(),
        params,
    }
}

/// Runs the server over pipes, sending `asked` and collecting every reply.
fn session(root: &std::path::Path, asked: &[Message]) -> (Vec<Message>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_deco"))
        .args(["--server", "--stdio", "--workspace"])
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("deco should start");

    {
        let mut stdin = child.stdin.take().expect("a pipe");
        for message in asked {
            frame::write(&mut stdin, message).expect("a frame");
        }
        stdin.flush().expect("flushed");
        // Dropped, so the server sees the end of the stream and stops even if
        // nothing asked it to.
    }

    let output = child.wait_with_output().expect("deco should finish");
    let mut replies = Vec::new();
    let mut reader = BufReader::new(output.stdout.as_slice());
    while let Some(message) = frame::read(&mut reader).expect("a readable frame") {
        replies.push(message);
    }
    (
        replies,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The result of a reply, or its error.
fn result(message: &Message) -> Result<&serde_json::Value, &str> {
    match message {
        Message::Response {
            result: Some(value),
            ..
        } => Ok(value),
        Message::Response {
            error: Some(error), ..
        } => Err(error),
        other => panic!("expected a response, got {other:?}"),
    }
}

#[test]
fn the_server_answers_over_pipes_and_puts_nothing_else_on_stdout() {
    // The claim that only a process can check: `frame::read` consuming the whole
    // of stdout means nothing else was written to it. A stray `println!` would be
    // read as a header and this would fail rather than mysteriously breaking a
    // real client.
    let root = workspace("answers");
    let (replies, stderr) = session(
        &root,
        &[
            request(1, deco_remote::server::HANDSHAKE, serde_json::json!({})),
            request(2, "fs.read", serde_json::json!({ "path": "src/main.rs" })),
            request(3, "$/shutdown", serde_json::json!({})),
        ],
    );

    assert_eq!(replies.len(), 3, "{replies:?}; stderr: {stderr}");
    assert_eq!(
        result(&replies[0]).expect("a handshake")["protocol"],
        deco_remote::server::PROTOCOL_VERSION
    );
    assert_eq!(
        result(&replies[1]).expect("a read")["text"],
        "fn main() {}\n"
    );
    assert!(stderr.is_empty(), "stderr should be quiet: {stderr}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_client_asking_for_something_outside_the_workspace_is_refused_by_the_real_binary() {
    // The confinement, through the actual process rather than through a `Server`
    // built in a test — which is where a wiring mistake would show: a binary that
    // served the current directory instead of `--workspace` would pass every unit
    // test and fail this one.
    let root = workspace("refuses");
    let outside = root
        .parent()
        .expect("a parent")
        .join("outside-the-root.txt");
    std::fs::write(&outside, "secret\n").expect("a file");

    let (replies, _) = session(
        &root,
        &[
            request(
                1,
                "fs.read",
                serde_json::json!({ "path": outside.display().to_string() }),
            ),
            request(
                2,
                "fs.read",
                serde_json::json!({ "path": "../outside-the-root.txt" }),
            ),
        ],
    );
    assert_eq!(replies.len(), 2);
    for reply in &replies {
        let error = result(reply).expect_err("should have been refused");
        assert!(error.contains("outside the workspace"), "{error}");
    }
    let _ = std::fs::remove_file(&outside);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_stream_ending_stops_the_server_without_a_shutdown() {
    // What happens when the SSH connection drops: the far end must exit rather
    // than sit holding a workspace open forever.
    let root = workspace("eof");
    let (replies, _) = session(
        &root,
        &[request(
            1,
            deco_remote::server::HANDSHAKE,
            serde_json::json!({}),
        )],
    );
    assert_eq!(replies.len(), 1);
    let _ = std::fs::remove_dir_all(&root);
}
