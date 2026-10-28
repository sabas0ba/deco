# The integrated terminal

![Opening the terminal with ctrl+`, running two commands, and moving back to the text and to the terminal again](img/terminal.svg)

`` ctrl+` `` opens the [panel](chrome.md) with a shell in it and gives the shell the keyboard. Pressing it again in the terminal hides the panel and returns the keyboard to the text; the shell keeps running, and the next `` ctrl+` `` shows it again as it was. The shell starts in the workspace's directory.

| Key | Command |
| --- | --- |
| `` ctrl+` `` | `workbench.action.terminal.toggleTerminal` |
| `shift+pageup` / `shift+pagedown` | `workbench.action.terminal.scrollUpPage` / `…scrollDownPage` |
| `ctrl+home` / `ctrl+end` | `workbench.action.terminal.scrollToTop` / `…scrollToBottom` |
| `ctrl+shift+v` (`cmd+v` on macOS) | `workbench.action.terminal.paste` |
| `` ctrl+shift+` `` | `workbench.action.terminal.new` |
| `ctrl+pagedown` / `ctrl+pageup` (`cmd+shift+]` / `cmd+shift+[` on macOS) | `workbench.action.terminal.focusNext` / `…focusPrevious` |
| | `workbench.action.terminal.kill` |

The scrolling keys apply only while the program is on its normal screen; a full-screen program such as `vim` or `less` receives them. When the shell exits, the status line shows its exit code and the screen stays until a key is pressed in it. The next `` ctrl+` `` starts a new shell.

## Several terminals

`` ctrl+shift+` `` starts another shell in a terminal of its own and shows it. Each terminal keeps its own screen, history and shell; keys go only to the one shown, and output reaches its own screen whichever is shown. The panel's title names the one shown, as `TERMINAL 2/3`, because there are no tabs. `ctrl+pagedown` and `ctrl+pageup` show the next and previous terminal, wrapping. `workbench.action.terminal.kill` stops the shell of the one shown and shows the next; killing the last hides the panel. An exited terminal closes on the next key pressed in it, and the panel stays on another one if any is left. All terminals have the panel's size, and are resized with it.

Many terminal emulators send nothing distinct for `` ctrl+shift+` ``, so the key may arrive as another or not at all. **Terminal: Create New Terminal** in the command palette starts one in every terminal emulator, and the key can be rebound in `keybindings.json`.

## Which keys the shell gets

While the terminal has the keyboard, every key goes to the program in it except those bound to the following commands, which stay with the editor. This is the part of VS Code's `terminal.integrated.commandsToSkipShell` default that deco implements:

- the terminal commands in the table above;
- `ctrl+j`, `ctrl+b`, `ctrl+shift+p`, `ctrl+p`, `ctrl+q`, and `ctrl+shift+m`, which switches the panel to the [problems view](language-servers.md#the-problems-view);
- the keys that move the keyboard to another region: `ctrl+1`, `ctrl+2`, `ctrl+shift+e`, `ctrl+shift+g`, and `workbench.action.focusActiveEditorGroup`.

A chord prefix such as `ctrl+k` belongs to the shell in the terminal, so `ctrl+k` deletes to the end of the line there as it does in any terminal. A prompt opened over the terminal, such as the command palette, takes the keys until it closes.

Keys are encoded as xterm encodes them, which is what `TERM=xterm-256color` promises: control characters for `ctrl` with a letter, `ESC` before a key held with `alt`, and `ESC [ 1 ; 5 C` for `ctrl+right`. Pasted text is bracketed when the program asks for it, and an end marker inside the pasted text is removed, so a paste cannot end early and run the rest as typed commands.

## The shell

The shell comes from the user's settings, as in VS Code:

```jsonc
{
  "terminal.integrated.defaultProfile.linux": "bash",
  "terminal.integrated.profiles.linux": {
    "bash": { "path": "bash", "args": ["-l"], "env": { "HISTSIZE": "5000" } }
  }
}
```

The platform suffix is `linux`, `osx` or `windows`. `path` may also be a list of candidates, and the first that exists is used. Without a profile, deco starts `$SHELL` on Unix and `%COMSPEC%` on Windows, and `/bin/sh` or `cmd.exe` when the variable is unset.

**Profiles are read from user settings only.** A workspace's `.vscode/settings.json` or a remote machine's settings could otherwise name a program that runs as soon as the terminal opens. A profile set there is ignored, and the problems list says so. The same rule applies to [language servers](language-servers.md).

Every shell gets `TERM=xterm-256color`, `COLORTERM=truecolor`, `TERM_PROGRAM=deco` and `TERM_PROGRAM_VERSION`. A profile's `env` is applied after these and can override them.

`terminal.integrated.scrollback` sets how many lines of history are kept; the default is 1000, as in VS Code. `terminal.integrated.drawBoldTextInBrightColors` is honoured and defaults to `true`.

## In a remote session

In a [remote session](remote.md), `` ctrl+` `` runs the shell on the remote machine, in the workspace the remote serves. The editor starts `deco --pty --workspace DIR --stdio` on the remote through the same transport as the file server (SSH, a container or WSL), and the remote deco opens the pseudo-terminal there. Keys, size changes, output and the exit code travel as small binary frames over that process's standard input and output; see [Remote](remote.md#the-integrated-terminal).

The remote shell is the remote user's default (`$SHELL`, or `/bin/sh`). The `terminal.integrated.profiles.*` settings name programs on the editor's machine and are not used for it. A remote deco too old to run a terminal is detected from its handshake, and `` ctrl+` `` says to update it.

## How it is drawn

The terminal's screen is drawn into the panel below its title row, one terminal cell per panel cell, through the same renderer as the rest of the window. When the panel changes size, the screen and the shell are told the new size. Lines are not reflowed: a line keeps its cells and is cut or padded, as in xterm.

Colours come from the theme: `terminal.foreground` and `terminal.background` for the defaults, falling back to the panel's colours, and `terminal.ansiBlack` to `terminal.ansiBrightWhite` for the sixteen ANSI colours, with VS Code's defaults for dark and light themes when the theme does not set them. The other 240 indexed colours are xterm's fixed cube and grey ramp, and direct colour (`38;2;r;g;b`) is drawn as given. Inverse, dim and hidden text are drawn; bold, italic, underline and strikethrough are not, because the terminal frontend's frames carry colours only.

## How it works

The terminal is the `deco-term` crate, and none of it comes from a terminal-emulation crate:

- **The parser** follows the DEC-compatible state machine described at [vt100.net](https://vt100.net/emu/dec_ansi_parser), which is the model xterm implements. Text is UTF-8, so the 8-bit C1 controls are not recognised, as in xterm's UTF-8 mode.
- **The screen** implements the part of [XTerm Control Sequences](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html) that shells and full-screen programs use: cursor movement, erasing with the current background, inserting and deleting characters and lines, scroll regions, tab stops, the DEC line-drawing set, SGR with 256 and direct colours, wide and combining characters, the alternate screen (modes 47, 1047 and 1049), bracketed paste, and the cursor-position and device-attribute reports.
- **The pseudo-terminal** is opened through the operating system: `openpty`, `fork` and `execve` on Unix, and a pseudo console (ConPTY, Windows 10 1809 and later) with `CreateProcessW` on Windows. Both are called through `libc` and `windows-sys`, which deco already depended on, so the terminal added no crate. The shell's output is read on a thread of its own, and the event loop polls every 10 ms while a shell is running.

The editor core holds the screen and decides where each key goes; it never starts a process. The frontend starts the shell when the core asks, and moves bytes between the two. This is why the key routing and the screen are tested without a pseudo-terminal, and why the animation above is generated with a scripted shell: the bytes it prints still go through the real parser, screen and renderer.

A program in the terminal cannot reach the editor. Window titles are kept but not shown, and the sequences that would set the clipboard (OSC 52), change colours or open links are ignored.

## Not built yet

- **No tabs or splits.** Several terminals are switched between, one at a time; they cannot be shown side by side, renamed or reordered.
- **No mouse.** Text cannot be selected in the terminal, so there is no copy from it, and mouse reporting to programs is accepted but not sent.
- **Not in the GPU frontend**, which reports that the terminal is only in the terminal frontend so far.
- **No `terminal.integrated.cwd`, `env.*` or `commandsToSkipShell` settings.** The working directory is the workspace and the list of editor keys is fixed.
