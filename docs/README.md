# deco documentation

A lightweight VS Code-compatible editor in Rust. This directory is the detailed
reference; the [top-level README](../README.md) is the short version, and it is
the honest place to look for what is **not** built yet.

| Page | What it covers |
| --- | --- |
| [Editing](editing.md) | Motion, selection, line operations, multiple cursors, undo |
| [Tabs](tabs.md) | Several documents, one per tab; what a tab keeps |
| [Syntax highlighting](highlighting.md) | Scopes, languages, and why not tree-sitter |
| [Find and replace](find-and-replace.md) | `ctrl+f`, `ctrl+h`, `F3`, and the multi-cursor find keys |
| [Running commands](commands.md) | The command palette, quick open, and go to line |
| [Language servers](language-servers.md) | Diagnostics, hover, go-to-definition, completion, formatting |
| [Configuration](configuration.md) | `settings.json`, `keybindings.json`, colour themes, and where they are read from |
| [Extensions](extensions.md) | The capability model, and why an extension gets less power here than in VS Code |
| [Remote](remote.md) | SSH, container and WSL authorities — and which half of it exists |

## About the animations

Every animation in these pages is **generated from deco's own renderer**, not
recorded off a screen and not drawn by hand:

```console
$ cargo xtask docs            # regenerate them
$ cargo xtask docs --check    # fail if they no longer match the code
```

`deco_tui::render` is a pure function of an editor session and a terminal size —
the same property that lets the layout be asserted in CI with no terminal
attached. A scenario in `xtask/src/docs.rs` presses real chords through a real
`Session` and captures whatever the real renderer produced, so an animation
cannot show a feature behaving in a way the code does not. `--check` runs in CI,
so a behaviour change fails the build rather than quietly leaving the
documentation describing an editor that no longer exists.

They are animated SVG rather than GIF. An SVG is text: it diffs, it reviews in a
pull request, and it needs neither an encoder dependency nor an embedded bitmap
font. GitHub animates it in Markdown all the same.

The caption under each frame is the key that was pressed to produce it.
