# deco documentation

A lightweight VS Code-compatible editor in Rust. This directory is the detailed
reference; the [top-level README](https://github.com/sabas0ba/deco#readme) provides an overview and lists missing features.

| Page | What it covers |
| --- | --- |
| [Editing](editing.md) | Motion, selection, line and block comments, multiple cursors, undo |
| [Tabs](tabs.md) | Several documents, one per tab; splitting; what a tab keeps |
| [Chrome](chrome.md) | The side bar and the panel: `ctrl+b`, `ctrl+j`, where the space comes from, and where the keyboard is |
| [The file tree](files.md) | Walking the workspace, opening files, and what it costs to open a big one |
| [Git](git.md) | The branch, changed-line marks, side-by-side diffs, and a view that stages and commits — by running the binary rather than linking a library |
| [Syntax highlighting](highlighting.md) | Scopes, languages, choosing one, and why not tree-sitter |
| [Find and replace](find-and-replace.md) | `ctrl+f`, `ctrl+h`, `F3`, the multi-cursor find keys, and replacing across the workspace |
| [Running commands](commands.md) | The command palette, quick open, go to symbol, search in files, go to line |
| [Language servers](language-servers.md) | Diagnostics, hover, definition, references, completion, symbols, semantic tokens, formatting, rename, code actions |
| [Configuration](configuration.md) | `settings.json`, `keybindings.json`, colour themes, and where they are read from |
| [Extensions](extensions.md) | The capability model, and why an extension gets less power here than in VS Code |
| [Remote](remote.md) | SSH, container and WSL authorities, and which parts are implemented |
| [Testing](testing.md) | Unit tests, end-to-end scenarios, and what each one is for |
| [Roadmap](roadmap.md) | What VS Code has that deco does not, the plan for each, and what is worth building because deco is not Electron |

## About the animations

Every animation in these pages is **generated from deco's own renderer**. None are screen recordings or hand-drawn:

```console
$ cargo xtask docs            # regenerate them
$ cargo xtask docs --check    # fail if they no longer match the code
```

`deco_tui::render` calculates a frame from an editor session and terminal size, allowing layout checks without an attached terminal. Scenarios in `xtask/src/docs.rs` send key chords through `Session` and capture the renderer's output. CI runs `--check` to detect differences between generated animations and the committed files.

The animations are animated SVG rather than GIF. SVG is text, so changes can be diffed and reviewed in a pull request, and generation needs no encoder dependency or embedded bitmap font. GitHub renders the animation in Markdown.

The caption under each frame is the key that was pressed to produce it.

## About this site

These pages are also published at [sabas0ba.github.io/deco](https://sabas0ba.github.io/deco/). GitHub Pages builds the site directly from this directory, so the site and the Markdown on GitHub have the same content.

The theme is stored in this directory rather than in a gem, in `_layouts/default.html`, `_data/nav.yml` and `assets/css/deco.css`. The palette uses deco's default themes: Default Dark Modern for the dark variant and Default Light Modern for the light variant, both taken from `crates/deco-theme/src/defaults.rs`. The page layout follows the editor layout, with a bar at the top and bottom and an explorer on the left. The site uses only plugins that GitHub Pages enables by default, so there is no separate build or deploy step.

```console
$ cd docs && bundle install
$ bundle exec jekyll serve    # preview the site at http://127.0.0.1:4000/deco/
```
