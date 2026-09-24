//! A whole remote session: this binary as the client, and as the server.
//!
//! `deco-remote`'s own tests cover each half against a buffer. This file runs
//! the two halves against each other, over pipes, through the real binary, with
//! `deco --server --stdio` in place of `ssh host deco --server --stdio`. The
//! transport only adds an argument vector, which is tested separately without
//! running anything, so nothing here needs a network or an SSH daemon.

use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use deco_remote::client::Client;
use deco_remote::transport::Command;

/// A workspace with a couple of files in it.
fn workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "deco-remote-session-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).expect("a directory");
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("a file");
    std::fs::write(root.join("README.md"), "# hello\n").expect("a file");
    root
}

/// A client connected to a server serving `root`.
fn connect(root: &Path) -> Client {
    Client::start(&Command {
        program: env!("CARGO_BIN_EXE_deco").to_owned(),
        args: vec![
            "--server".to_owned(),
            "--stdio".to_owned(),
            "--workspace".to_owned(),
            root.display().to_string(),
        ],
    })
    .expect("the server should start")
}

/// Turns a workspace into a repository, or skips when the test machine has no
/// git. Every other failure is part of the test setup and remains a failure.
fn repository(name: &str) -> Option<PathBuf> {
    let root = workspace(name);
    let init = ProcessCommand::new("git")
        .args(["init", "--quiet"])
        .current_dir(&root)
        .status();
    match init {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipped: no git on this machine");
            return None;
        }
        Ok(status) => assert!(status.success(), "git init failed"),
        Err(error) => panic!("git init could not run: {error}"),
    }
    for args in [
        ["config", "user.name", "deco test"],
        ["config", "user.email", "deco@example.invalid"],
    ] {
        assert!(ProcessCommand::new("git")
            .args(args)
            .current_dir(&root)
            .status()
            .expect("git config should run")
            .success());
    }
    assert!(ProcessCommand::new("git")
        .args(["add", "--all"])
        .current_dir(&root)
        .status()
        .expect("git add should run")
        .success());
    assert!(ProcessCommand::new("git")
        .args(["commit", "--quiet", "--message", "initial"])
        .current_dir(&root)
        .status()
        .expect("git commit should run")
        .success());
    Some(root)
}

#[test]
fn a_session_opens_a_file_edits_it_and_saves_it_back() {
    // The core behaviour, end to end: the bytes read back are the bytes that
    // were sent, and they are written to the file on the remote side.
    let root = workspace("round-trip");
    let mut client = connect(&root);

    let hello = client.handshake().expect("a handshake");
    assert_eq!(
        Path::new(&hello.workspace),
        root.canonicalize().expect("canonical")
    );
    assert!(hello.methods.iter().any(|method| method == "fs.write"));

    assert_eq!(client.read("src/main.rs").expect("read"), "fn main() {}\n");

    client
        .write("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n")
        .expect("write");
    assert_eq!(
        std::fs::read_to_string(root.join("src/main.rs")).expect("the file"),
        "fn main() {\n    println!(\"hi\");\n}\n"
    );
    // Also read back through the connection, not only from disk: a write that
    // is correct only on disk would still be a broken session.
    assert_eq!(
        client.read("src/main.rs").expect("read"),
        "fn main() {\n    println!(\"hi\");\n}\n"
    );

    client.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_listing_is_what_a_picker_would_show() {
    let root = workspace("listing");
    let mut client = connect(&root);
    client.handshake().expect("a handshake");
    let files = client.list().expect("a listing");
    assert_eq!(files, vec!["README.md", "src/main.rs"]);
    // Every listed path can be read back. A picker depends on this: whatever is
    // listed can be opened.
    for file in &files {
        client
            .read(file)
            .expect("each listed file should be readable");
    }
    client.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_refusal_reaches_the_client_as_an_error_and_leaves_the_session_usable() {
    // A rejection by the remote side must not look like a broken connection:
    // one bad request fails only that request.
    let root = workspace("refusal");
    let mut client = connect(&root);
    client.handshake().expect("a handshake");

    // A file that exists, one directory up. On Windows, `../../etc/passwd` would
    // be rejected for *not existing* rather than for being outside, and the test
    // would pass without checking what it names.
    let outside = root
        .parent()
        .expect("a parent")
        .join("outside-the-root.txt");
    std::fs::write(&outside, "secret\n").expect("a file");
    let error = client
        .read("../outside-the-root.txt")
        .expect_err("outside the workspace")
        .to_string();
    assert!(error.contains("outside the workspace"), "{error}");
    let _ = std::fs::remove_file(&outside);

    assert_eq!(
        client.read("README.md").expect("still working"),
        "# hello\n"
    );
    client.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_transport_that_is_not_a_server_fails_with_something_a_person_can_act_on() {
    // Covers the case where `deco` is not installed on the remote and the
    // transport runs something else. The client must not hang waiting for a
    // frame that never arrives.
    let root = workspace("not-a-server");
    let mut client = Client::start(&Command {
        program: env!("CARGO_BIN_EXE_deco").to_owned(),
        // A real deco, asked to do something that is not serving: it prints the
        // configuration and exits.
        args: vec!["--print-config".to_owned()],
    })
    .expect("it starts");
    let error = client.handshake().expect_err("no handshake from that");
    let said = error.to_string();
    assert!(
        said.contains("stopped without answering") || said.contains("connection"),
        "{said}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_search_crosses_the_connection_and_every_hit_can_be_opened() {
    // Every path the search reports must be readable over the *same*
    // connection. If a result and `fs.read` wrote paths differently, results
    // could not be opened, and neither half would look wrong on its own.
    let root = workspace("search");
    std::fs::write(root.join("src/main.rs"), "fn main() {\n    let x = 1;\n}\n").expect("a file");
    std::fs::write(root.join("README.md"), "# hello\nlet x be x\n").expect("a file");
    let mut client = connect(&root);
    client.handshake().expect("a handshake");

    let found = client
        .search("let", deco_core::search::SearchOptions::default())
        .expect("a search");
    let mut paths: Vec<&str> = found
        .matches
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    paths.sort_unstable();
    paths.dedup();
    assert_eq!(paths, ["README.md", "src/main.rs"], "{paths:?}");
    assert!(!found.truncated);
    assert_eq!(found.files_searched, 2);

    for entry in &found.matches {
        let text = client
            .read(&entry.path)
            .unwrap_or_else(|error| panic!("{} should be readable: {error}", entry.path));
        // The line number in the match is correct for the file, so the editor
        // opens at the reported position.
        let line = text
            .lines()
            .nth(entry.line as usize)
            .unwrap_or_else(|| panic!("{} has no line {}", entry.path, entry.line));
        assert!(line.contains("let"), "{line:?}");
        // The displayed text is that line, trimmed.
        assert_eq!(entry.text, line.trim());
    }

    client.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn source_control_reads_and_writes_the_repository_on_the_far_end() {
    let Some(root) = repository("source-control") else {
        return;
    };
    std::fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"remote\"); }\n",
    )
    .expect("a changed file");

    let mut client = connect(&root);
    let hello = client.handshake().expect("a handshake");
    for method in [
        "scm.status",
        "scm.committed",
        "scm.comparison",
        "scm.branches",
        "scm.checkoutPlan",
        "scm.apply",
    ] {
        assert!(hello.serves(method), "the handshake omitted {method}");
    }

    let (repository, status) = client.scm_status().expect("a status");
    assert_eq!(repository, root.canonicalize().expect("canonical"));
    assert_eq!(status.changed(), 1);
    assert_eq!(status.staged(), 0);
    assert_eq!(
        client
            .scm_committed(Path::new("src/main.rs"))
            .expect("committed text"),
        Some("fn main() {}\n".to_owned())
    );
    let request = deco_scm::ComparisonRequest {
        path: PathBuf::from("src/main.rs"),
        original: None,
        kind: deco_scm::ComparisonKind::WorkingTree,
    };
    let comparison = client
        .scm_comparison(&request)
        .expect("a working-tree comparison");
    assert_eq!(comparison.original.as_deref(), Some("fn main() {}\n"));
    assert_eq!(
        comparison.modified.as_deref(),
        Some("fn main() { println!(\"remote\"); }\n")
    );

    let stage = deco_scm::Operation::Stage(PathBuf::from("src/main.rs"));
    client.scm_apply(&stage).expect("stage on the far end");
    assert_eq!(client.scm_status().expect("staged status").1.staged(), 1);
    let comparison = client
        .scm_comparison(&deco_scm::ComparisonRequest {
            kind: deco_scm::ComparisonKind::Staged,
            ..request
        })
        .expect("a staged comparison");
    assert_eq!(comparison.original.as_deref(), Some("fn main() {}\n"));
    assert_eq!(
        comparison.modified.as_deref(),
        Some("fn main() { println!(\"remote\"); }\n")
    );

    client
        .scm_apply(&deco_scm::Operation::Commit("remote change".to_owned()))
        .expect("commit on the far end");
    assert!(client.scm_status().expect("clean status").1.is_clean());

    client.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn source_control_does_not_expand_a_served_subdirectory_to_its_parent_repository() {
    let Some(root) = repository("source-control-confined") else {
        return;
    };
    let mut client = connect(&root.join("src"));
    client.handshake().expect("a handshake");

    let error = client
        .scm_status()
        .expect_err("the repository begins outside the served workspace")
        .to_string();
    assert!(error.contains("begins outside"), "{error}");
    assert!(error.contains("served workspace"), "{error}");

    client.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn source_control_refuses_external_metadata_for_a_linked_worktree() {
    let Some(primary) = repository("source-control-worktree-primary") else {
        return;
    };
    let linked = primary.parent().expect("a temporary parent").join(format!(
        "deco-remote-session-source-control-linked-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&linked);
    let added = ProcessCommand::new("git")
        .args(["worktree", "add", "--quiet", "--detach"])
        .arg(&linked)
        .arg("HEAD")
        .current_dir(&primary)
        .status()
        .expect("git worktree add should run");
    assert!(added.success(), "git worktree add failed");

    let mut client = connect(&linked);
    client.handshake().expect("a handshake");
    let error = client
        .scm_status()
        .expect_err("linked-worktree metadata is outside the workspace")
        .to_string();
    assert!(error.contains("Git metadata"), "{error}");
    assert!(error.contains("outside the served workspace"), "{error}");

    client.shutdown();
    let removed = ProcessCommand::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&linked)
        .current_dir(&primary)
        .status()
        .expect("git worktree remove should run");
    assert!(removed.success(), "git worktree remove failed");
    let _ = std::fs::remove_dir_all(&primary);
}
