//! The integrated terminal on the remote machine.
//!
//! A terminal is a long-lived, two-way byte stream with a size that changes,
//! which the request–response protocol of `deco --server` does not carry. It
//! uses its own transport process instead, as a [port forward](crate::forward)
//! does: the editor runs `deco --pty --workspace DIR --stdio` on the remote
//! through the same transport as the file server, and the remote deco opens a
//! pseudo-terminal there and runs the remote user's shell in it.
//!
//! Over that process's stdin and stdout the two ends exchange [`Frame`]s: the
//! editor sends keys and sizes, and the remote sends output and, at the end,
//! the shell's exit code. A frame is a one-byte tag, a four-byte big-endian
//! length, and the payload. Binary rather than JSON, because terminal output
//! is arbitrary bytes and is most of the traffic.
//!
//! # What the remote runs
//!
//! The remote user's login shell (`$SHELL`, or `/bin/sh`; `%COMSPEC%` on
//! Windows), started in the workspace directory. The local
//! `terminal.integrated.profiles.*` name programs on *this* machine and are
//! not sent. A shell is arbitrary code execution by design, so unlike the
//! file server it is not confined to the workspace; it runs with the remote
//! user's own permissions, which the transport already gave the editor.

use std::io::{self, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::transport::Command;

/// The name a remote deco advertises in its handshake when it can run a
/// terminal, so an editor does not start one on an older remote that cannot.
pub const CAPABILITY: &str = "pty.stdio";

/// The largest payload one frame may carry. Output is sent in chunks well
/// below this; the bound stops a corrupt length from allocating without limit.
pub const MAX_PAYLOAD: usize = 1 << 20;

/// One message between the editor and the remote terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Keys for the shell, or output from it.
    Data(Vec<u8>),
    /// The terminal's size in cells. The first frame the editor sends.
    Resize {
        /// Columns.
        cols: u16,
        /// Rows.
        rows: u16,
    },
    /// The shell has exited, with its code if it had one. The last frame the
    /// remote sends.
    Exit(Option<i32>),
}

const DATA: u8 = 0;
const RESIZE: u8 = 1;
const EXIT: u8 = 2;

/// Writes one frame and flushes it.
pub fn write_frame(output: &mut impl Write, frame: &Frame) -> io::Result<()> {
    let (tag, payload): (u8, Vec<u8>) = match frame {
        Frame::Data(bytes) => (DATA, bytes.clone()),
        Frame::Resize { cols, rows } => {
            let mut payload = cols.to_be_bytes().to_vec();
            payload.extend(rows.to_be_bytes());
            (RESIZE, payload)
        }
        Frame::Exit(code) => (
            EXIT,
            code.map(|code| code.to_be_bytes().to_vec())
                .unwrap_or_default(),
        ),
    };
    let length = u32::try_from(payload.len())
        .ok()
        .filter(|length| *length as usize <= MAX_PAYLOAD)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    output.write_all(&[tag])?;
    output.write_all(&length.to_be_bytes())?;
    output.write_all(&payload)?;
    output.flush()
}

/// Reads one frame, or `None` at a clean end of the stream.
pub fn read_frame(input: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut tag = [0u8; 1];
    match input.read_exact(&mut tag) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let mut length = [0u8; 4];
    input.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a {length}-byte frame is larger than the {MAX_PAYLOAD}-byte limit"),
        ));
    }
    let mut payload = vec![0u8; length];
    input.read_exact(&mut payload)?;
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    Ok(Some(match tag[0] {
        DATA => Frame::Data(payload),
        RESIZE => {
            let [c0, c1, r0, r1] = payload[..] else {
                return Err(invalid("a resize frame holds four bytes"));
            };
            Frame::Resize {
                cols: u16::from_be_bytes([c0, c1]),
                rows: u16::from_be_bytes([r0, r1]),
            }
        }
        EXIT => match payload[..] {
            [] => Frame::Exit(None),
            [a, b, c, d] => Frame::Exit(Some(i32::from_be_bytes([a, b, c, d]))),
            _ => return Err(invalid("an exit frame holds no code or four bytes")),
        },
        other => return Err(invalid(&format!("unknown frame type {other}"))),
    }))
}

/// The remote command that runs a terminal for `workspace`.
pub fn pty_command(server_path: &str, workspace: &str) -> Vec<String> {
    vec![
        server_path.to_owned(),
        "--pty".to_owned(),
        "--workspace".to_owned(),
        workspace.to_owned(),
        "--stdio".to_owned(),
    ]
}

/// What [`RemotePty::read`] found: the same shape as a local terminal's.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    /// Everything the shell wrote since the last read.
    pub bytes: Vec<u8>,
    /// Whether the stream has ended: the shell exited, or the connection
    /// was lost.
    pub ended: bool,
}

/// A shell in a terminal on the remote machine, seen from the editor.
///
/// The same operations as a local `deco_term::pty::Pty`, so the frontend
/// drives either the same way.
#[derive(Debug)]
pub struct RemotePty {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: Receiver<Frame>,
    ended: bool,
    exit: Option<Option<i32>>,
}

impl RemotePty {
    /// Starts `command`, the transport running [`pty_command`], and tells
    /// the remote the terminal's size.
    ///
    /// The transport's standard error, such as an SSH failure or an older
    /// remote deco rejecting `--pty`, is shown in the terminal as output, so
    /// a terminal that cannot start says why.
    pub fn spawn(command: &Command, cols: u16, rows: u16) -> io::Result<Self> {
        let mut child = std::process::Command::new(&command.program)
            .args(&command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (sender, frames) = mpsc::channel();

        if let Some(stdout) = stdout {
            let sender = sender.clone();
            std::thread::Builder::new()
                .name("deco-remote-pty".to_owned())
                .spawn(move || {
                    let mut stdout = BufReader::new(stdout);
                    while let Ok(Some(frame)) = read_frame(&mut stdout) {
                        if sender.send(frame).is_err() {
                            break;
                        }
                    }
                })?;
        }
        if let Some(mut stderr) = stderr {
            std::thread::Builder::new()
                .name("deco-remote-pty-stderr".to_owned())
                .spawn(move || {
                    let mut buffer = [0u8; 4096];
                    while let Ok(count @ 1..) = stderr.read(&mut buffer) {
                        // Line breaks as a terminal needs them, so a message
                        // does not run off to the right.
                        let text = String::from_utf8_lossy(&buffer[..count]).replace('\n', "\r\n");
                        if sender.send(Frame::Data(text.into_bytes())).is_err() {
                            break;
                        }
                    }
                })?;
        }
        if let Some(stdin) = stdin.as_mut() {
            write_frame(stdin, &Frame::Resize { cols, rows })?;
        }
        Ok(Self {
            child,
            stdin,
            frames,
            ended: false,
            exit: None,
        })
    }

    /// What arrived since the last call, without waiting.
    pub fn read(&mut self) -> Output {
        let mut output = Output::default();
        loop {
            match self.frames.try_recv() {
                Ok(Frame::Data(bytes)) => output.bytes.extend(bytes),
                Ok(Frame::Exit(code)) => self.exit = Some(code),
                Ok(Frame::Resize { .. }) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.ended = true;
                    break;
                }
            }
        }
        output.ended = self.ended;
        output
    }

    /// Sends keys to the shell.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.send(&Frame::Data(bytes.to_vec()))
    }

    /// Tells the remote terminal its new size.
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.send(&Frame::Resize { cols, rows })
    }

    /// The shell's exit code, once it has exited: from the remote's exit
    /// frame, or the transport's own status when the connection ended
    /// without one.
    pub fn exit_code(&mut self) -> Option<i32> {
        if let Some(code) = self.exit {
            return code;
        }
        match self.child.try_wait() {
            Ok(Some(status)) if self.ended => status.code(),
            _ => None,
        }
    }

    fn send(&mut self, frame: &Frame) -> io::Result<()> {
        match self.stdin.as_mut() {
            Some(stdin) => write_frame(stdin, frame),
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the remote terminal has closed",
            )),
        }
    }
}

impl Drop for RemotePty {
    /// Closing the transport ends the remote shell: its input ends, and the
    /// remote deco hangs it up.
    fn drop(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_frame_reads_back_as_written() {
        let frames = [
            Frame::Data(b"ls\r".to_vec()),
            Frame::Data(Vec::new()),
            Frame::Resize { cols: 80, rows: 24 },
            Frame::Exit(Some(-1)),
            Frame::Exit(None),
        ];
        let mut wire = Vec::new();
        for frame in &frames {
            write_frame(&mut wire, frame).unwrap();
        }
        let mut input = &wire[..];
        for frame in &frames {
            assert_eq!(read_frame(&mut input).unwrap().as_ref(), Some(frame));
        }
        assert_eq!(read_frame(&mut input).unwrap(), None, "a clean end");
    }

    #[test]
    fn a_corrupt_frame_is_an_error_rather_than_an_allocation() {
        let mut huge = vec![DATA];
        huge.extend(u32::MAX.to_be_bytes());
        assert!(read_frame(&mut &huge[..]).is_err());
        let bad_tag = [9u8, 0, 0, 0, 0];
        assert!(read_frame(&mut &bad_tag[..]).is_err());
        let short_resize = [RESIZE, 0, 0, 0, 1, 7];
        assert!(read_frame(&mut &short_resize[..]).is_err());
    }

    #[test]
    fn the_remote_command_names_the_workspace() {
        assert_eq!(
            pty_command("deco", "/srv/app"),
            ["deco", "--pty", "--workspace", "/srv/app", "--stdio"]
        );
    }
}
