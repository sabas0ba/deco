# Changelog

Notable changes per release, newest first. The heading text is what a release's
notes are built from — see `cargo xtask release-notes`.

## 0.1.0

The first release. deco reads your existing VS Code configuration and means the
same thing by it, runs in a terminal or a GPU-accelerated window, and runs VS
Code extensions in a Node process that has had its ambient authority taken away.

**This is an early release.** The compatibility layers — the parts that decide
whether VS Code compatibility is achievable at all — are implemented and tested,
and the editor is usable for editing files. Several headline features are not
built; the README's *What is not built yet* section is explicit about which, and
each documentation page says what its own feature does not do.

### Configuration, meant the way VS Code means it

- `settings.json` and `keybindings.json` are read from the places VS Code reads
  them, layered default < user < remote < workspace < folder, with JSONC
  comments and trailing commas.
- Colour themes load from VS Code theme files, including the ones inside
  installed extensions.
- `deco --print-config` prints what was resolved and where each problem came
  from, which is the quickest answer to "why is my setting not applying".

### Editing

Rope-backed buffers, multiple cursors, block and line comments, word wrap,
undo and redo, tabs and splits, find and replace, quick open, go to line and
symbol, and project-wide search.

### Language servers

Diagnostics, hover, go to definition, references, document symbols, completion,
semantic tokens and formatting, from the servers named in your own settings.

### Extensions

A Node extension host with a `vscode` API shim, run under a capability broker:
an extension declares what it wants, the declaration is a ceiling rather than a
grant, and anything it has not been granted is refused **by name** rather than
answered with a plausible empty value. Decisions are asked about, remembered per
extension version, and can be taken back from the command palette.

### Remote development

`--remote ssh-remote+host`, `wsl+Distro` and `dev-container+id`: open, edit and
save files on another machine, search it, forward a port from it, and run
language servers and extension file access where the files are. `--remote-install`
puts deco on a remote that has none, when asked, and only when it can run there.

### Known limits

- No rename or code actions; no diff view; no integrated terminal.
- The extension surface is small: no editor state, quick pick, tree views,
  webviews or debug adapters, and `process`, `net`, `env`, `secrets` and
  `openExternal` are brokered and then refused because nothing implements them.
- Extension hosts always run on the local machine, so a remote session cannot
  run a project's own tools through an extension.
- Provisioning a remote of a *different* platform is refused rather than guessed
  at.
