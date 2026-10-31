# The mouse

Both frontends take the mouse: the terminal frontend from the terminal, and the GPU frontend from the window. A click lands on what is drawn under it, because the position is looked up in the same layout the renderer draws (`deco_tui::mouse`).

| Where | Press | Double / triple click | Drag | Wheel |
| --- | --- | --- | --- | --- |
| Text | Moves the caret; with shift, extends the selection | Selects the word / the line | Selects, by words or lines after a double or triple click; past the top or bottom it scrolls | Scrolls three rows a step without moving the caret |
| Tab bar | Shows that tab | | | |
| Tree | Selects the row: opens a file as a preview, keeping the keyboard in the tree, or opens and closes a folder | Opens the file and moves the keyboard to it | | Scrolls the tree |
| Source-control view | Selects the file | Opens its diff | | |
| Problems view | Opens the file at the problem | | | |
| Terminal | Gives it the keyboard and starts a selection | | Selects text; `ctrl+shift+c` copies it | Scrolls the history |
| Prompt list | Chooses the entry | | | |

A press in a region gives that region the keyboard and closes an open prompt, as in VS Code. Clicking in the gutter puts the caret at the start of the line, and below the last line at the end of the document.

## Programs in the terminal

A program that asks for mouse events, such as `vim` with `set mouse=a`, `htop` or `less --mouse`, receives presses, releases, drags and the wheel over the terminal's screen, in the encoding it asked for: xterm's SGR form (mode 1006) or the original form (modes 1000, 1002 and 1003). Holding shift keeps the mouse for selecting text, as in xterm. A program that did not ask receives nothing, and the mouse selects and scrolls as above.

The selection is cleared when the program writes to the screen, when the history scrolls and when the panel changes size, because the cells under it no longer hold what was selected. `workbench.action.terminal.copySelection` (`ctrl+shift+c`, `cmd+c` on macOS) is bound only while text is selected (`terminalTextSelected`), so the key reaches the shell otherwise.

## In the terminal frontend

With the mouse taken, the terminal's own selection usually needs shift held while dragging; the exact modifier depends on the terminal. `"deco.mouse": false` in `settings.json` leaves the mouse to the terminal instead, and takes effect when deco starts.

## Not built yet

- No context menu, and no drag of a tab or a file.
- No hover from the mouse: a language server's hover is still `ctrl+k ctrl+i`.
- A click does not add a cursor; `alt+click` places the caret as a plain click does.
- The GPU frontend counts a double click by the same 400 ms interval as the terminal frontend, not by the system's setting.
