//! A pseudo-terminal with a program running in it.
//!
//! On Unix the pair comes from `openpty`, and the program is started with
//! `fork` and `execve` after making the pseudo-terminal its controlling
//! terminal. On Windows it is a pseudo console (ConPTY, Windows 10 1809 and
//! later), and the program is started with `CreateProcessW`. Both are called
//! through `libc` and `windows-sys` directly, which the editor already
//! depends on, rather than through a crate that wraps them.
//!
//! The program's output is read on a thread of its own and handed over
//! through a channel, so [`Pty::read`] never blocks the event loop.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};

/// What to run.
#[derive(Debug, Clone, Default)]
pub struct Command {
    /// The program, as a path or a name looked up in `PATH`.
    pub program: OsString,
    /// The arguments, not including the program.
    pub args: Vec<OsString>,
    /// The working directory; the editor's own when `None`.
    pub cwd: Option<PathBuf>,
    /// Variables set on top of the editor's environment.
    pub env: Vec<(OsString, OsString)>,
}

/// A size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
}

/// What [`Pty::read`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    /// Everything the program wrote since the last read.
    pub bytes: Vec<u8>,
    /// Whether the output has ended: the program and everything it started
    /// have closed the terminal.
    pub ended: bool,
}

/// A program running in a pseudo-terminal.
pub struct Pty {
    child: platform::Child,
    output: Receiver<Vec<u8>>,
    ended: bool,
}

impl Pty {
    /// Starts `command` in a new pseudo-terminal of `size`.
    pub fn spawn(command: &Command, size: Size) -> io::Result<Self> {
        let (child, reader) = platform::spawn(command, size)?;
        let (sender, output) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("deco-pty-reader".to_owned())
            .spawn(move || {
                let mut reader = reader;
                let mut buffer = vec![0; 64 * 1024];
                loop {
                    match io::Read::read(&mut reader, &mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            if sender.send(buffer[..read].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        // Linux reports EIO once the last program holding the
                        // terminal has closed it.
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Self {
            child,
            output,
            ended: false,
        })
    }

    /// Takes what the program has written, without waiting.
    pub fn read(&mut self) -> Output {
        let mut output = Output::default();
        loop {
            match self.output.try_recv() {
                Ok(bytes) => output.bytes.extend_from_slice(&bytes),
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

    /// Sends `bytes` to the program as typed input.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.child.write(bytes)
    }

    /// Tells the program the terminal's new size.
    pub fn resize(&mut self, size: Size) -> io::Result<()> {
        self.child.resize(size)
    }

    /// The program's exit code once it has exited. A program ended by a
    /// signal reports 128 plus the signal number, as a shell does.
    pub fn exit_code(&mut self) -> Option<i32> {
        self.child.exit_code()
    }
}

#[cfg(unix)]
mod platform {
    //! `openpty`, `fork` and `execve`.

    use std::ffi::{CString, OsStr, OsString};
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::{Command, Size};

    pub struct Child {
        master: File,
        pid: libc::pid_t,
        status: Option<i32>,
    }

    fn cstring(bytes: &[u8]) -> io::Result<CString> {
        CString::new(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "an argument or variable contains a NUL byte",
            )
        })
    }

    /// The program's path: as given when it names a path, otherwise the
    /// first match in `path`.
    fn resolve(program: &OsStr, path: Option<&OsStr>) -> io::Result<PathBuf> {
        if program.as_bytes().contains(&b'/') {
            return Ok(PathBuf::from(program));
        }
        let found = path
            .into_iter()
            .flat_map(std::env::split_paths)
            .find_map(|dir| {
                let candidate = dir.join(program);
                let metadata = std::fs::metadata(&candidate).ok()?;
                use std::os::unix::fs::PermissionsExt;
                (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
                    .then_some(candidate)
            });
        found.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} was not found in PATH", program.to_string_lossy()),
            )
        })
    }

    fn check(result: libc::c_int) -> io::Result<libc::c_int> {
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result)
        }
    }

    fn winsize(size: Size) -> libc::winsize {
        libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    }

    pub fn spawn(command: &Command, size: Size) -> io::Result<(Child, File)> {
        // The environment: the editor's, with the command's variables on top.
        let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(name, _)| !command.env.iter().any(|(set, _)| set == name))
            .collect();
        env.extend(command.env.iter().cloned());
        let path = env
            .iter()
            .find(|(name, _)| name == "PATH")
            .map(|(_, value)| value.clone());

        // Everything the child needs is prepared before `fork`: after it,
        // only async-signal-safe calls are allowed, which excludes allocation.
        let program = resolve(&command.program, path.as_deref())?;
        let program = cstring(program.as_os_str().as_bytes())?;
        let mut args = vec![program.clone()];
        for arg in &command.args {
            args.push(cstring(arg.as_bytes())?);
        }
        let mut arg_pointers: Vec<*const libc::c_char> =
            args.iter().map(|arg| arg.as_ptr()).collect();
        arg_pointers.push(std::ptr::null());
        let env: Vec<CString> = env
            .into_iter()
            .filter_map(|(name, value)| {
                let mut entry = name.into_vec();
                entry.push(b'=');
                entry.extend(value.into_vec());
                CString::new(entry).ok()
            })
            .collect();
        let mut env_pointers: Vec<*const libc::c_char> =
            env.iter().map(|entry| entry.as_ptr()).collect();
        env_pointers.push(std::ptr::null());
        let cwd = match &command.cwd {
            Some(cwd) => Some(cstring(cwd.as_os_str().as_bytes())?),
            None => None,
        };

        let (mut master, mut slave) = (-1, -1);
        let mut size = winsize(size);
        // SAFETY: the pointers refer to live locals; `openpty` writes the two
        // descriptors and reads the size. The size is passed as `&mut` because
        // macOS declares the parameter mutable, while Linux declares it const.
        #[allow(clippy::unnecessary_mut_passed)]
        check(unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        })?;
        // SAFETY: `openpty` succeeded, so both are open descriptors that
        // nothing else owns.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        use std::os::fd::AsRawFd;
        let (master_fd, slave_fd) = (master.as_raw_fd(), slave.as_raw_fd());
        // Not inherited by other programs the editor starts, such as language
        // servers. The child's own copies are made with `dup2`, which clears
        // the flag.
        for fd in [master_fd, slave_fd] {
            // SAFETY: `fd` is open.
            check(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) })?;
        }

        // SAFETY: `fork` has no preconditions. The child below calls only
        // async-signal-safe functions before `execve` or `_exit`.
        let pid = check(unsafe { libc::fork() })?;
        if pid == 0 {
            // SAFETY: async-signal-safe calls on descriptors and pointers
            // prepared before the fork.
            unsafe {
                libc::setsid();
                libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
                for target in 0..3 {
                    libc::dup2(slave_fd, target);
                }
                if slave_fd > 2 {
                    libc::close(slave_fd);
                }
                libc::close(master_fd);
                // The Rust runtime ignores SIGPIPE; a shell expects the
                // default, and an ignored signal survives `execve`.
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                let mut empty: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut empty);
                libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
                if let Some(cwd) = &cwd {
                    libc::chdir(cwd.as_ptr());
                }
                libc::execve(
                    program.as_ptr(),
                    arg_pointers.as_ptr(),
                    env_pointers.as_ptr(),
                );
                libc::_exit(127);
            }
        }
        drop(slave);
        let master = File::from(master);
        let reader = master.try_clone()?;
        Ok((
            Child {
                master,
                pid,
                status: None,
            },
            reader,
        ))
    }

    impl Child {
        pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.master.write_all(bytes)
        }

        pub fn resize(&mut self, size: Size) -> io::Result<()> {
            let size = winsize(size);
            use std::os::fd::AsRawFd;
            // SAFETY: the descriptor is open and `size` is a live local.
            check(unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) })?;
            Ok(())
        }

        pub fn exit_code(&mut self) -> Option<i32> {
            if self.status.is_none() {
                self.status = wait(self.pid, false);
            }
            self.status
        }
    }

    /// Reaps `pid`, waiting for it when `block` is true.
    fn wait(pid: libc::pid_t, block: bool) -> Option<i32> {
        let mut status = 0;
        let flags = if block { 0 } else { libc::WNOHANG };
        // SAFETY: `status` is a live local.
        let reaped = unsafe { libc::waitpid(pid, &mut status, flags) };
        if reaped != pid {
            return None;
        }
        if libc::WIFEXITED(status) {
            Some(libc::WEXITSTATUS(status))
        } else if libc::WIFSIGNALED(status) {
            Some(128 + libc::WTERMSIG(status))
        } else {
            None
        }
    }

    impl Drop for Child {
        /// Hangs up, as closing a terminal window does, and reaps the program
        /// on a thread so a slow exit does not hold up the editor. A program
        /// that ignores the hangup is killed after two seconds.
        fn drop(&mut self) {
            if self.status.is_some() || self.exit_code().is_some() {
                return;
            }
            let pid = self.pid;
            // SAFETY: signalling the process group the child leads, and the
            // child itself.
            unsafe {
                libc::kill(-pid, libc::SIGHUP);
                libc::kill(pid, libc::SIGHUP);
            }
            let _ = std::thread::Builder::new()
                .name("deco-pty-reaper".to_owned())
                .spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while Instant::now() < deadline {
                        if wait(pid, false).is_some() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    // SAFETY: as above.
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                        libc::kill(pid, libc::SIGKILL);
                    }
                    wait(pid, true);
                });
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::Path;

        #[test]
        fn a_name_is_looked_up_in_path_and_a_path_is_kept() {
            let path = std::env::var_os("PATH");
            let sh = resolve(OsStr::new("sh"), path.as_deref()).expect("sh is installed");
            assert!(sh.is_absolute() && sh.ends_with("sh"), "{sh:?}");
            assert_eq!(resolve(OsStr::new("./x"), None).unwrap(), Path::new("./x"));
            assert!(resolve(OsStr::new("no-such-program-deco"), path.as_deref()).is_err());
        }
    }
}

#[cfg(windows)]
mod platform {
    //! The Windows pseudo console.

    use std::ffi::{OsStr, OsString};
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
    use windows_sys::Win32::System::Console::{
        ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
        InitializeProcThreadAttributeList, UpdateProcThreadAttribute, CREATE_UNICODE_ENVIRONMENT,
        EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
        STARTF_USESTDHANDLES, STARTUPINFOEXW,
    };

    use super::{Command, Size};

    pub struct Child {
        console: HPCON,
        input: File,
        process: OwnedHandle,
        status: Option<i32>,
    }

    // SAFETY: the pseudo console handle is used from one thread at a time,
    // through `&mut self`, and the API does not tie it to the creating thread.
    unsafe impl Send for Child {}

    fn coord(size: Size) -> COORD {
        COORD {
            X: size.cols.min(i16::MAX as u16) as i16,
            Y: size.rows.min(i16::MAX as u16) as i16,
        }
    }

    fn last_error() -> io::Error {
        io::Error::last_os_error()
    }

    /// Two ends of an anonymous pipe, closed when dropped.
    fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
        let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: the pointers refer to live locals.
        if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
            return Err(last_error());
        }
        // SAFETY: `CreatePipe` succeeded, so both handles are open and owned
        // by nothing else.
        Ok(unsafe {
            (
                OwnedHandle::from_raw_handle(read as RawHandle),
                OwnedHandle::from_raw_handle(write as RawHandle),
            )
        })
    }

    fn wide(text: &OsStr) -> Vec<u16> {
        text.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// One argument quoted so that the C runtime's parser reads it back as
    /// it was: backslashes are doubled only before a quote.
    fn quote(arg: &OsStr, line: &mut Vec<u16>) {
        let units: Vec<u16> = arg.encode_wide().collect();
        let needs_quotes = units.is_empty()
            || units.iter().any(|&unit| {
                unit == u16::from(b' ') || unit == u16::from(b'\t') || unit == u16::from(b'"')
            });
        if !needs_quotes {
            line.extend_from_slice(&units);
            return;
        }
        line.push(u16::from(b'"'));
        let mut backslashes = 0;
        for &unit in &units {
            if unit == u16::from(b'\\') {
                backslashes += 1;
            } else {
                if unit == u16::from(b'"') {
                    line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes + 1));
                }
                backslashes = 0;
            }
            line.push(unit);
        }
        line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
        line.push(u16::from(b'"'));
    }

    pub fn spawn(command: &Command, size: Size) -> io::Result<(Child, File)> {
        let (input_read, input_write) = pipe()?;
        let (output_read, output_write) = pipe()?;
        use std::os::windows::io::AsRawHandle;
        let mut console: HPCON = 0;
        // SAFETY: the handles are open pipe ends and `console` is a live
        // local.
        let result = unsafe {
            CreatePseudoConsole(
                coord(size),
                input_read.as_raw_handle() as HANDLE,
                output_write.as_raw_handle() as HANDLE,
                0,
                &mut console,
            )
        };
        if result < 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        // The pseudo console holds its own duplicates of these ends.
        drop((input_read, output_write));

        let started = start(command, console);
        let process = match started {
            Ok(process) => process,
            Err(error) => {
                // SAFETY: created above and not used elsewhere.
                unsafe { ClosePseudoConsole(console) };
                return Err(error);
            }
        };
        Ok((
            Child {
                console,
                input: File::from(input_write),
                process,
                status: None,
            },
            File::from(output_read),
        ))
    }

    /// Starts the program attached to `console`.
    fn start(command: &Command, console: HPCON) -> io::Result<OwnedHandle> {
        let mut line = Vec::new();
        quote(&command.program, &mut line);
        for arg in &command.args {
            line.push(u16::from(b' '));
            quote(arg, &mut line);
        }
        line.push(0);

        // The environment block: `NAME=value` entries, each ended by a NUL,
        // and the block by another.
        let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(name, _)| {
                !command.env.iter().any(|(set, _)| {
                    set.to_string_lossy()
                        .eq_ignore_ascii_case(&name.to_string_lossy())
                })
            })
            .collect();
        env.extend(command.env.iter().cloned());
        let mut block: Vec<u16> = Vec::new();
        for (name, value) in env {
            let mut entry = name;
            entry.push("=");
            entry.push(value);
            block.extend(entry.encode_wide());
            block.push(0);
        }
        block.push(0);
        let cwd = command.cwd.as_ref().map(|cwd| wide(cwd.as_os_str()));

        let mut size = 0;
        // SAFETY: a null list asks for the size needed; the call reports
        // failure by design and sets `size`.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        let mut list = vec![0u8; size];
        let list_pointer = list.as_mut_ptr().cast();
        // SAFETY: `list` has the size the first call asked for.
        if unsafe { InitializeProcThreadAttributeList(list_pointer, 1, 0, &mut size) } == 0 {
            return Err(last_error());
        }
        struct List(*mut core::ffi::c_void);
        impl Drop for List {
            fn drop(&mut self) {
                // SAFETY: initialised above; `list` outlives this guard.
                unsafe { DeleteProcThreadAttributeList(self.0) };
            }
        }
        let _guard = List(list_pointer);
        // SAFETY: the attribute's value is the console handle itself, passed
        // by value as the documentation requires.
        if unsafe {
            UpdateProcThreadAttribute(
                list_pointer,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                console as *const core::ffi::c_void,
                std::mem::size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(last_error());
        }

        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        // Without this the program inherits the editor's own standard
        // handles instead of the pseudo console's.
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.lpAttributeList = list_pointer;
        let mut info = PROCESS_INFORMATION::default();
        // SAFETY: every pointer refers to a live, NUL-terminated buffer or
        // struct prepared above.
        let created = unsafe {
            CreateProcessW(
                std::ptr::null(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                block.as_ptr().cast(),
                cwd.as_ref().map_or(std::ptr::null(), |cwd| cwd.as_ptr()),
                &startup.StartupInfo,
                &mut info,
            )
        };
        if created == 0 {
            return Err(last_error());
        }
        // SAFETY: `CreateProcessW` succeeded, so both handles are open.
        unsafe { CloseHandle(info.hThread) };
        Ok(unsafe { OwnedHandle::from_raw_handle(info.hProcess as RawHandle) })
    }

    impl Child {
        pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.input.write_all(bytes)
        }

        pub fn resize(&mut self, size: Size) -> io::Result<()> {
            // SAFETY: the console is open until `drop`.
            let result = unsafe { ResizePseudoConsole(self.console, coord(size)) };
            if result < 0 {
                return Err(io::Error::from_raw_os_error(result));
            }
            Ok(())
        }

        pub fn exit_code(&mut self) -> Option<i32> {
            if self.status.is_none() {
                use std::os::windows::io::AsRawHandle;
                let mut code = 0u32;
                // SAFETY: the process handle is open and `code` is a live local.
                let ok = unsafe {
                    GetExitCodeProcess(self.process.as_raw_handle() as HANDLE, &mut code)
                };
                if ok != 0 && code != STILL_ACTIVE as u32 {
                    self.status = Some(code as i32);
                }
            }
            self.status
        }
    }

    impl Drop for Child {
        /// Closing the pseudo console ends the programs attached to it and
        /// closes the output pipe, which ends the reader thread.
        fn drop(&mut self) {
            // SAFETY: created in `spawn` and closed only here.
            unsafe { ClosePseudoConsole(self.console) };
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::windows::ffi::OsStringExt;

        fn quoted(args: &[&str]) -> String {
            let mut line = Vec::new();
            for (index, arg) in args.iter().enumerate() {
                if index > 0 {
                    line.push(u16::from(b' '));
                }
                quote(OsStr::new(arg), &mut line);
            }
            OsString::from_wide(&line).to_string_lossy().into_owned()
        }

        #[test]
        fn arguments_are_quoted_for_the_c_runtime() {
            assert_eq!(quoted(&["cmd.exe", "/c", "echo"]), "cmd.exe /c echo");
            assert_eq!(quoted(&["a b", ""]), "\"a b\" \"\"");
            assert_eq!(quoted(&[r#"say "hi""#]), r#""say \"hi\"""#);
            assert_eq!(quoted(&[r"C:\dir\ x\"]), r#""C:\dir\ x\\""#);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Reads until the output ends or `done` holds, for at most ten seconds.
    fn read_until(pty: &mut Pty, done: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut text = Vec::new();
        while Instant::now() < deadline {
            let output = pty.read();
            text.extend_from_slice(&output.bytes);
            if output.ended || done(&String::from_utf8_lossy(&text)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        String::from_utf8_lossy(&text).into_owned()
    }

    #[cfg(unix)]
    fn shell(script: &str) -> Command {
        Command {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: None,
            env: vec![("DECO_PTY_TEST".into(), "set".into())],
        }
    }

    #[cfg(windows)]
    fn shell(script: &str) -> Command {
        Command {
            program: "cmd.exe".into(),
            args: vec!["/d".into(), "/c".into(), script.into()],
            cwd: None,
            env: vec![("DECO_PTY_TEST".into(), "set".into())],
        }
    }

    /// The `in_a_pseudo_console_` prefix keeps this out of the Wine pass;
    /// see `WINE_SKIPS` in `xtask/src/cross.rs`.
    #[test]
    fn in_a_pseudo_console_a_program_s_output_and_exit_code_come_back() {
        let size = Size { cols: 80, rows: 24 };
        #[cfg(unix)]
        let script = "printf 'out:%s' \"$DECO_PTY_TEST\"; exit 3";
        #[cfg(windows)]
        let script = "echo out:%DECO_PTY_TEST%& exit 3";
        let mut pty = Pty::spawn(&shell(script), size).expect("a shell starts");
        let text = read_until(&mut pty, |text| text.contains("out:set"));
        assert!(text.contains("out:set"), "{text:?}");
        let deadline = Instant::now() + Duration::from_secs(10);
        while pty.exit_code().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(pty.exit_code(), Some(3));
    }

    #[cfg(unix)]
    #[test]
    fn input_reaches_the_program_and_the_size_is_its_terminal_s() {
        let mut pty = Pty::spawn(
            &shell("read line; echo \"got:$line\"; stty size"),
            Size { cols: 80, rows: 24 },
        )
        .expect("a shell starts");
        pty.resize(Size {
            cols: 100,
            rows: 30,
        })
        .expect("a resize");
        pty.write(b"hello\r").expect("a write");
        let text = read_until(&mut pty, |text| text.contains("30 100"));
        assert!(text.contains("got:hello"), "{text:?}");
        assert!(text.contains("30 100"), "{text:?}");
    }

    #[test]
    fn a_program_that_is_not_there_is_an_error() {
        let command = Command {
            program: "no-such-program-deco".into(),
            ..Command::default()
        };
        let started = Pty::spawn(&command, Size { cols: 80, rows: 24 });
        // On Windows the failure comes from `CreateProcessW`.
        assert!(started.is_err());
    }
}
