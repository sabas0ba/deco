//! A forwarded port, end to end, with this binary as both halves.
//!
//! `deco-remote` tests the individual parts (parsing a spec, rejecting a
//! non-loopback target, freeing the port on drop) without any network. This
//! file runs the whole path: a real listener stands in for the dev server on
//! the remote, the real `deco --forward-to` is the remote side of the tunnel,
//! and a real client connects to the local port and expects its bytes back.
//!
//! Only the `ssh host` prefix is omitted, as in `remote_session.rs`; that
//! argument vector is tested separately.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use deco_remote::transport::Command;
use deco_remote::{Forward, PortSpec};

/// A service on the "remote": it echoes whatever is sent, in upper case, so a
/// reply cannot be confused with the request that produced it.
fn shouting_echo_service() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a listener");
    let port = listener.local_addr().expect("an address").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buffer = [0u8; 1024];
                while let Ok(read) = stream.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    let shouted = buffer[..read].to_ascii_uppercase();
                    if stream.write_all(&shouted).is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

/// The command a transport would wrap, with the transport left off.
fn far_end(port: u16) -> Command {
    let built = deco_remote::forward::forward_command(env!("CARGO_BIN_EXE_deco"), port);
    let (program, args) = built.split_first().expect("a program");
    Command {
        program: program.clone(),
        args: args.to_vec(),
    }
}

/// Sends `message` through `address` and returns what came back.
fn round_trip(address: std::net::SocketAddr, message: &str) -> String {
    let mut stream = TcpStream::connect(address).expect("a connection to the forward");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a timeout");
    stream.write_all(message.as_bytes()).expect("a write");
    stream.flush().expect("a flush");

    let mut back = vec![0u8; message.len()];
    stream.read_exact(&mut back).expect("a reply");
    String::from_utf8(back).expect("text")
}

#[test]
fn a_forwarded_port_carries_bytes_to_the_service_and_back() {
    let service = shouting_echo_service();
    let forward = Forward::start(
        far_end(service),
        PortSpec {
            local: 0,
            remote: service,
        },
    )
    .expect("a forward");

    // The purpose of the feature: a client connecting to this machine's port
    // reaches a service it has no direct route to.
    assert_eq!(round_trip(forward.address(), "hello"), "HELLO");
    // The tunnel is not single-use; a second connection gets its own process.
    assert_eq!(round_trip(forward.address(), "again"), "AGAIN");
}

#[test]
fn two_connections_at_once_each_get_their_own_pipe() {
    // A single stdio pipe cannot carry two connections, so a forward that
    // reused one would interleave their data. This test checks that each
    // connection has its own process.
    let service = shouting_echo_service();
    let forward = Forward::start(
        far_end(service),
        PortSpec {
            local: 0,
            remote: service,
        },
    )
    .expect("a forward");
    let address = forward.address();

    let first = std::thread::spawn(move || round_trip(address, "aaaaaaaaaa"));
    let second = std::thread::spawn(move || round_trip(address, "bbbbbbbbbb"));
    assert_eq!(first.join().expect("a thread"), "AAAAAAAAAA");
    assert_eq!(second.join().expect("a thread"), "BBBBBBBBBB");
}

#[test]
fn a_forward_to_a_port_with_nothing_on_it_closes_rather_than_hanging() {
    // A dev server that is not running yet is common. The client should see a
    // closed connection rather than wait forever.
    let dead = {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a listener");
        listener.local_addr().expect("an address").port()
    };
    let forward = Forward::start(
        far_end(dead),
        PortSpec {
            local: 0,
            remote: dead,
        },
    )
    .expect("a forward");

    let mut stream = TcpStream::connect(forward.address()).expect("a connection");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a timeout");
    let _ = stream.write_all(b"anyone there?");
    let mut back = Vec::new();
    // Ends at end-of-file with no data. A hang here indicates the failure.
    stream.read_to_end(&mut back).expect("a closed connection");
    assert!(back.is_empty(), "{back:?}");
}

#[test]
fn the_far_end_refuses_a_target_that_is_not_loopback() {
    // The rejection is unit-tested; this checks the binary itself, because this
    // argument could otherwise make a deco server a route into the remote's
    // network.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_deco"))
        .args(["--forward-to", "10.0.0.5:5432", "--stdio"])
        .output()
        .expect("it should run");
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("loopback"), "{said}");
    assert!(said.contains("10.0.0.5"), "{said}");
}

#[test]
fn a_forward_without_a_remote_is_a_usage_error_rather_than_a_dead_port() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_deco"))
        .args(["--forward", "3000", "--print-config"])
        .output()
        .expect("it should run");
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("--remote"), "{said}");
}
