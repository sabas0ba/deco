# deco documentation

A lightweight VS Code-compatible editor in Rust. This directory is the detailed
reference; the [top-level README](https://github.com/sabas0ba/deco#readme) is
the short version, and it is the honest place to look for what is **not** built
yet.

| Page | What it covers |
| --- | --- |
| [Editing](editing.md) | Motion, selection, line and block comments, multiple cursors, undo |
| [Tabs](tabs.md) | Several documents, one per tab; splitting; what a tab keeps |
| [Chrome](chrome.md) | The side bar and the panel: `ctrl+b`, `ctrl+j`, where the space comes from, and where the keyboard is |
| [The file tree](files.md) | Walking the workspace, opening files, and what it costs to open a big one |
| [Syntax highlighting](highlighting.md) | Scopes, languages, choosing one, and why not tree-sitter |
| [Find and replace](find-and-replace.md) | `ctrl+f`, `ctrl+h`, `F3`, the multi-cursor find keys, and replacing across the workspace |
| [Running commands](commands.md) | The command palette, quick open, go to symbol, search in files, go to line |
| [Language servers](language-servers.md) | Diagnostics, hover, definition, references, completion, symbols, semantic tokens, formatting, rename, code actions |
| [Configuration](configuration.md) | `settings.json`, `keybindings.json`, colour themes, and where they are read from |
| [Extensions](extensions.md) | The capability model, and why an extension gets less power here than in VS Code |
| [Remote](remote.md) | SSH, container and WSL authorities — and which half of it exists |
| [Testing](testing.md) | Unit tests, end-to-end scenarios, and what each one is for |
| [Roadmap](roadmap.md) | What VS Code has that deco does not, the plan for each, and what is worth building because deco is not Electron |

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

## About this site

These pages are also published at
[sabas0ba.github.io/deco](https://sabas0ba.github.io/deco/), built by GitHub
Pages straight from this directory — so the site and the Markdown you are
reading on GitHub never drift apart.

The theme lives here rather than in a gem, in `_layouts/default.html`,
`_data/nav.yml` and `assets/css/deco.css`. Its palette is deco's own: the dark
side is Default Dark Modern and the light side Default Light Modern, both read
off `crates/deco-theme/src/defaults.rs`, and the page is arranged the way the
editor is — a bar top and bottom, an explorer down the left. The animations are
captures of the real renderer, and this is the surrounding they were captured
in. Only plugins GitHub Pages already enables are used, so there is no build
step to run and nothing to deploy.

```console
$ cd docs && bundle install
$ bundle exec jekyll serve    # preview the site at http://127.0.0.1:4000/deco/
```
