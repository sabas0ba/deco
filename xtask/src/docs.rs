//! Generates the animated demonstrations in `docs/img`.
//!
//! # Why these are generated rather than recorded
//!
//! `deco_tui::render` is a pure function of a session and a terminal size, which
//! is what lets the layout be asserted in CI with no terminal attached. The same
//! property makes it a screenshot source: a scenario here presses real chords
//! through [`deco_editor::Session`] and captures whatever the real renderer
//! produced. Nothing is drawn by hand, so a demonstration cannot show a feature
//! behaving in a way the code does not.
//!
//! `cargo xtask docs --check` re-runs the scenarios and compares them against
//! what is committed, so a change in behaviour fails CI instead of quietly
//! leaving the documentation describing an editor that no longer exists.
//!
//! # Why SVG and not GIF
//!
//! An animated SVG is text: it diffs, it reviews, and it needs no encoder and no
//! embedded font. A GIF would need either a third-party encoder or a hand-written
//! one plus bitmap glyphs for every character drawn — a dependency or several
//! hundred lines and a font blob, for a file that reviews as noise. GitHub
//! animates SVG referenced from Markdown, so the result is the same to a reader.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use deco_core::{Position, Range, SelectionSet};
use deco_editor::Session;
use deco_keymap::binding::Platform;
use deco_keymap::keys::Chord;
use deco_lsp::requests::{CompletionItem, CompletionKind};
use deco_lsp::{Diagnostic, Hover, Severity};
use deco_theme::Rgba;
use deco_tui::render::{self, Frame};
use deco_tui::suggest::Suggest;

/// Writes every demonstration into `root/docs/img`.
///
/// With `check`, writes nothing and reports which files would change.
pub fn run(root: &Path, check: bool) -> Result<Vec<PathBuf>> {
    let dir = root.join("docs/img");
    if !check {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    let mut all = Vec::new();
    let mut stale = Vec::new();
    for demo in demos() {
        let path = dir.join(format!("{}.svg", demo.name));
        let svg = (demo.build)();
        if check {
            // A missing file reads as empty and so counts as stale, which is
            // right: `--check` on a fresh clone that forgot to commit them should
            // fail rather than pass.
            if std::fs::read_to_string(&path).unwrap_or_default() != svg {
                stale.push(path.clone());
            }
        } else {
            std::fs::write(&path, &svg).with_context(|| format!("writing {}", path.display()))?;
        }
        all.push(path);
    }

    if !stale.is_empty() {
        bail!(
            "these demonstrations are out of date; run `cargo xtask docs`:\n{}",
            stale
                .iter()
                .map(|path| format!("  {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    Ok(all)
}

/// One demonstration: a file name and the scenario that produces it.
struct Demo {
    name: &'static str,
    build: fn() -> String,
}

fn demos() -> Vec<Demo> {
    vec![
        Demo {
            name: "editing",
            build: editing,
        },
        Demo {
            name: "multi-cursor",
            build: multi_cursor,
        },
        Demo {
            name: "find",
            build: find,
        },
        Demo {
            name: "replace",
            build: replace,
        },
        Demo {
            name: "diagnostics",
            build: diagnostics,
        },
        Demo {
            name: "hover",
            build: hover,
        },
        Demo {
            name: "rename",
            build: rename,
        },
        Demo {
            name: "code-actions",
            build: code_actions,
        },
        Demo {
            name: "completion",
            build: completion,
        },
        Demo {
            name: "command-palette",
            build: command_palette,
        },
        Demo {
            name: "extension-commands",
            build: extension_commands,
        },
        Demo {
            name: "go-to-line",
            build: go_to_line,
        },
        Demo {
            name: "highlighting",
            build: highlighting,
        },
        Demo {
            name: "semantic-tokens",
            build: semantic_tokens,
        },
        Demo {
            name: "go-to-symbol",
            build: go_to_symbol,
        },
        Demo {
            name: "chrome",
            build: chrome,
        },
        Demo {
            name: "file-tree",
            build: file_tree,
        },
        Demo {
            name: "file-mutations",
            build: file_mutations,
        },
        Demo {
            name: "git-status",
            build: git_status,
        },
        Demo {
            name: "git-gutter",
            build: git_gutter,
        },
        Demo {
            name: "tabs",
            build: tabs,
        },
        Demo {
            name: "save-all",
            build: save_all,
        },
        Demo {
            name: "split",
            build: split,
        },
        Demo {
            name: "block-comment",
            build: block_comment,
        },
        Demo {
            name: "word-wrap",
            build: word_wrap,
        },
        Demo {
            name: "detect-indentation",
            build: detect_indentation,
        },
        Demo {
            name: "view-settings",
            build: view_settings,
        },
        Demo {
            name: "wrapping-indent",
            build: wrapping_indent,
        },
        Demo {
            name: "auto-closing-brackets",
            build: auto_closing_brackets,
        },
        Demo {
            name: "auto-indent",
            build: auto_indent,
        },
        Demo {
            name: "trim-auto-whitespace",
            build: trim_auto_whitespace,
        },
        Demo {
            name: "save-as",
            build: save_as,
        },
        Demo {
            name: "language-mode",
            build: language_mode,
        },
        Demo {
            name: "color-theme",
            build: color_theme,
        },
        Demo {
            name: "quick-open",
            build: quick_open,
        },
        Demo {
            name: "search-in-files",
            build: search_in_files,
        },
        Demo {
            name: "replace-in-files",
            build: replace_in_files,
        },
    ]
}

// ---- Driving a session ---------------------------------------------------

/// How wide and tall every demonstration's terminal is.
///
/// One size for all of them so the images sit side by side in a page without
/// jumping, and small enough that the text is legible when GitHub scales it into
/// a column.
const COLUMNS: usize = 76;
const ROWS: usize = 14;

/// A session being scripted, and the frames captured from it so far.
struct Take {
    session: Session,
    shots: Vec<Shot>,
    /// The workspace the file tree is shown over, if a demonstration set one.
    ///
    /// The frontend's job in the real editor, done here by the demonstration —
    /// which is the point of the tree asking for listings rather than reading
    /// them: a workspace can be stated instead of created on disk. Each entry is
    /// a path and the text that file holds.
    workspace: Vec<(String, String)>,
}

/// One captured moment: what the screen showed, and what produced it.
struct Shot {
    frame: Frame,
    caption: String,
    /// The editor background when this frame was captured.
    ///
    /// Per frame rather than per demonstration, because a theme can change
    /// mid-scenario and a light frame drawn on a dark page is not what the editor
    /// looked like.
    bg: Rgba,
    /// How many time slots this frame occupies. A frame worth reading gets
    /// several; an intermediate keystroke gets one.
    hold: u32,
}

impl Take {
    fn new(file: &str, text: &str) -> Self {
        Self::with_settings(deco_config::Settings::with_defaults(), file, text)
    }

    /// The same, for a demonstration whose subject *is* a setting.
    fn with_settings(settings: deco_config::Settings, file: &str, text: &str) -> Self {
        // The Linux keymap rather than the host's: a demonstration that pressed
        // `ctrl+d` would be pressing an unbound key when generated on a Mac, and
        // the committed file would differ by who ran the command.
        let mut session = Session::new(settings, None, Platform::Linux);
        session.open(PathBuf::from(format!("/demo/{file}")), text);
        session.resize(COLUMNS, ROWS - 1);
        Self {
            session,
            shots: Vec::new(),
            workspace: Vec::new(),
        }
    }

    /// Captures the same file again under an additional settings layer.
    ///
    /// For demonstrations whose subject is a setting: the frames before and after sit
    /// in one animation, which is the only way a reader can see what the setting did.
    fn append(&mut self, user_json: &str, caption: &str, hold: u32) -> &mut Self {
        let path = self
            .session
            .document
            .path
            .clone()
            .expect("a demonstration opens a file");
        let text = self.session.document.buffer.text();
        let selections = self.session.view.selections.clone();

        let mut settings = deco_config::Settings::with_defaults();
        settings
            .load_layer(deco_config::Scope::User, user_json)
            .expect("the settings in this file parse");
        let mut session = Session::new(settings, None, Platform::Linux);
        session.open(path, &text);
        session.view.selections = selections;
        session.resize(COLUMNS, ROWS - 1);

        self.session = session;
        self.resize_for_chrome();
        self.capture(caption, hold)
    }

    /// Puts the caret somewhere, without pressing anything.
    fn at(&mut self, line: u32, character: u32) -> &mut Self {
        self.session.view.selections = SelectionSet::caret(Position::new(line, character));
        self
    }

    /// Presses `keys` in order, capturing a frame after each.
    ///
    /// The caption shows the key, so a reader can tell what caused the change
    /// rather than inferring it.
    fn press(&mut self, keys: &[&str]) -> &mut Self {
        for key in keys {
            let chord = Chord::parse(key).expect("demonstrations only press keys that parse");
            let outcome = self
                .session
                .handle_chord(chord, self.shots.len() as u64 * 10_000);
            // The frontend's other half of the bargain. Without this the tree
            // would appear to open a file and not open it, and the frames after
            // would show typing going into whatever was already there — a
            // demonstration of something the editor does not do, which is the
            // one thing these files exist to make impossible.
            // The same bargain for the tree's mutations: the session decided
            // what should happen to a file, and this demonstration is the thing
            // with the "filesystem". Applying it to the in-memory workspace is
            // what makes the frames after show the tree as it really would be.
            if let deco_editor::Outcome::FileOperation(ref operation) = outcome {
                let operation = operation.clone();
                self.apply_file_operation(&operation);
                self.session.file_operation_done(&operation);
                if let deco_editor::FileOperation::CreateFile(path) = &operation {
                    self.session.open(path.clone(), "");
                }
            }
            // The frontend's half again: the session decided the document
            // should be written, and this demonstration is the thing with the
            // "filesystem". Without it the tab would stay dirty, and — the
            // reason it is here — the session would never mark its git status
            // stale, so a demonstration of the status bar would show a count
            // that never moved.
            if matches!(outcome, deco_editor::Outcome::Save) {
                self.write_open_document();
                self.session.mark_saved();
            }
            if let deco_editor::Outcome::OpenFile { path, at } = outcome {
                let relative = path
                    .strip_prefix("/demo")
                    .expect("a demonstration only opens files in its own workspace");
                let text = self
                    .workspace
                    .iter()
                    .find(|(candidate, _)| Path::new(candidate) == relative)
                    .map(|(_, text)| text.clone())
                    .unwrap_or_else(|| {
                        panic!("this demonstration opens {relative:?} without saying what is in it")
                    });
                self.session.open(path, &text);
                if let Some(at) = at {
                    self.session.view.selections = SelectionSet::caret(at);
                }
            }
            self.fill_tree();
            self.resize_for_chrome();
            self.capture(key, 1);
        }
        self
    }

    /// Puts what is on screen into the in-memory workspace, as a save would.
    fn write_open_document(&mut self) {
        let Some(path) = self.session.document.path.clone() else {
            return;
        };
        let relative = path
            .strip_prefix("/demo")
            .expect("a demonstration only saves files in its own workspace")
            .to_string_lossy()
            .into_owned();
        let text = self.session.document.buffer.text();
        match self
            .workspace
            .iter_mut()
            .find(|(candidate, _)| *candidate == relative)
        {
            Some((_, held)) => *held = text,
            None => self.workspace.push((relative, text)),
        }
    }

    /// Answers the session's standing question about git, if it has one.
    ///
    /// `output` is what `git status --porcelain=v2 --branch -z` would have
    /// written. The demonstration is the thing with a `git` here, exactly as it
    /// is the thing with a filesystem for the tree — and the gate is the point
    /// rather than an optimisation: calling this when nothing has asked leaves
    /// the bar alone, which is how a demonstration can show that typing does
    /// not spawn a process and saving does.
    fn git(&mut self, output: &str) -> &mut Self {
        if !self.session.scm_wanted() {
            return self;
        }
        self.session.scm_started();
        self.session.fill_scm(Some(
            deco_scm::parse(output).expect("a demonstration writes git's own format"),
        ));
        self
    }

    /// Says what `HEAD` had for the file on screen.
    ///
    /// The demonstration is the thing with a `git` here, as it is the thing
    /// with a filesystem for the tree. Handed the committed text rather than a
    /// diff: the marks that come out are computed by the editor's own code
    /// from this and the buffer, so the frames cannot show a gutter the
    /// editor would not draw.
    fn committed(&mut self, text: &str) -> &mut Self {
        let path = self
            .session
            .document
            .path
            .clone()
            .expect("a demonstration opens a file");
        self.session.fill_committed(path, Some(text.to_owned()));
        self.session.refresh_diffs();
        self
    }

    /// Carries out one of the tree's operations on the in-memory workspace.
    ///
    /// A rename moves every path *under* the old one, not just the entry
    /// itself — renaming a directory in the real filesystem takes its contents
    /// with it, and a demonstration where it did not would be showing something
    /// the editor does not do.
    fn apply_file_operation(&mut self, operation: &deco_editor::FileOperation) {
        use deco_editor::FileOperation;
        let relative = |path: &Path| {
            path.strip_prefix("/demo")
                .expect("a demonstration only changes files in its own workspace")
                .to_string_lossy()
                .into_owned()
        };
        match operation {
            FileOperation::CreateFile(path) => {
                self.workspace.push((relative(path), String::new()));
            }
            // Directories are implied by the paths under them, so an empty one
            // needs a marker to exist at all — the trailing slash the workspace
            // shape already uses.
            FileOperation::CreateFolder(path) => {
                self.workspace
                    .push((format!("{}/", relative(path)), String::new()));
            }
            FileOperation::Rename { from, to, .. } => {
                let (from, to) = (relative(from), relative(to));
                for (path, _) in self.workspace.iter_mut() {
                    if *path == from {
                        *path = to.clone();
                    } else if let Some(rest) = path.strip_prefix(&format!("{from}/")) {
                        *path = format!("{to}/{rest}");
                    }
                }
            }
            // The demonstration's workspace holds no content for a created
            // file, so "if empty" is always true here — the refusal is a
            // property of the real filesystem, and belongs to the frontend's
            // tests rather than to a picture.
            FileOperation::Delete { path, .. } | FileOperation::DeleteIfEmpty { path, .. } => {
                let gone = relative(path);
                let under = format!("{gone}/");
                self.workspace
                    .retain(|(path, _)| *path != gone && !path.starts_with(&under));
            }
        }
    }

    /// Gives the file tree a workspace to show, and roots it there.
    ///
    /// Paths are `dir/file`, a trailing `/` marking a directory — the same shape
    /// the tests use. Supplied rather than read from disk because that is how
    /// the tree really works: the session is *handed* directory contents by
    /// whoever has a filesystem, so a demonstration can be that whoever, and the
    /// rows on screen are the model's own.
    fn workspace(&mut self, files: &[(&str, &str)]) -> &mut Self {
        self.workspace = files
            .iter()
            .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
            .collect();
        self.session.set_workspace_root("/demo");
        self.fill_tree();
        self
    }

    /// Answers whatever listings the tree is waiting on, from `workspace`.
    fn fill_tree(&mut self) {
        for _ in 0..16 {
            let Some(dir) = self.session.directory_wanted() else {
                return;
            };
            let prefix = match dir.strip_prefix("/demo") {
                Ok(rest) if rest.as_os_str().is_empty() => String::new(),
                Ok(rest) => format!("{}/", rest.display()),
                Err(_) => return,
            };
            let mut entries: Vec<deco_editor::explorer::Entry> = Vec::new();
            for (path, _) in &self.workspace {
                let Some(rest) = path.strip_prefix(&prefix) else {
                    continue;
                };
                let (name, is_dir) = match rest.split_once('/') {
                    Some((head, _)) => (head, true),
                    None if rest.is_empty() => continue,
                    None => (rest, false),
                };
                if !entries.iter().any(|entry| entry.name == name) {
                    entries.push(if is_dir {
                        deco_editor::explorer::Entry::dir(name)
                    } else {
                        deco_editor::explorer::Entry::file(name)
                    });
                }
            }
            self.session.fill_directory(&dir, entries);
        }
    }

    /// Presses `keys` and holds the result for longer, for the frame that shows
    /// what the feature did.
    fn press_and_hold(&mut self, keys: &[&str], hold: u32) -> &mut Self {
        self.press(keys);
        if let Some(last) = self.shots.last_mut() {
            last.hold = hold;
        }
        self
    }

    /// Types `text` one character at a time, capturing only the finished result.
    ///
    /// A frame per letter makes a long word into a slideshow nobody reads.
    fn type_text(&mut self, text: &str) -> &mut Self {
        for c in text.chars() {
            self.type_char(c);
        }
        self.resize_for_chrome();
        self.capture(&format!("type “{text}”"), 3);
        self
    }

    /// Types one character without capturing a frame.
    ///
    /// The chord is built rather than parsed, because that is what a terminal
    /// sends: a space arrives as the character `' '`, not as the named `space`
    /// key — and the named key types nothing.
    fn type_char(&mut self, c: char) {
        let chord = deco_keymap::keys::Chord {
            key: deco_keymap::keys::Key::Char(c),
            modifiers: Default::default(),
        };
        self.session
            .handle_chord(chord, self.shots.len() as u64 * 10_000);
    }

    /// Keeps the text area's height right when a bar opens or closes.
    ///
    /// The terminal frontend does this on every frame; a demonstration that
    /// skipped it would show the find bar covering the last line of the file.
    fn resize_for_chrome(&mut self) {
        let chrome = render::chrome_height(&self.session, ROWS);
        self.session.resize(COLUMNS, ROWS.saturating_sub(chrome));
    }

    /// Captures the current screen.
    fn capture(&mut self, caption: &str, hold: u32) -> &mut Self {
        // What `Driver::frame` does before it draws, and here for the same
        // reason: the git marks are derived from the buffer, so they are
        // brought up to date at the moment of drawing rather than by the
        // renderer. Here rather than in `press`, because a frame can be taken
        // without a key being pressed — and a demonstration showing the gutter
        // as it was one edit ago would be showing something the editor does
        // not do.
        self.session.refresh_diffs();
        let frame = render::render(&self.session, COLUMNS, ROWS);
        self.shots.push(Shot {
            frame,
            caption: caption.to_owned(),
            bg: self.background(),
            hold,
        });
        self
    }

    /// Captures the current screen with an overlay the frontend owns.
    fn capture_overlay(
        &mut self,
        caption: &str,
        hold: u32,
        hover: Option<&Hover>,
        suggest: Option<&Suggest>,
    ) -> &mut Self {
        let frame = render::render_with_overlays(&self.session, COLUMNS, ROWS, hover, suggest);
        self.shots.push(Shot {
            frame,
            caption: caption.to_owned(),
            bg: self.background(),
            hold,
        });
        self
    }

    /// The current theme's editor background.
    fn background(&self) -> Rgba {
        self.session
            .theme
            .color("editor.background")
            .unwrap_or(Rgba::BLACK)
    }

    fn finish(&self) -> String {
        svg(&self.shots, COLUMNS, ROWS)
    }
}

// ---- The scenarios -------------------------------------------------------

const SAMPLE: &str =
    "fn main() {\n    let total = 1;\n    let count = 2;\n    println!(\"{total}\");\n}\n";

fn editing() -> String {
    let mut take = Take::new("main.rs", SAMPLE);
    take.at(1, 4)
        .capture("a Rust file, four spaces of indent", 3)
        .press_and_hold(&["alt+down"], 4)
        .press_and_hold(&["ctrl+/"], 4)
        .press_and_hold(&["ctrl+z"], 3)
        .press(&["end"])
        .press_and_hold(&["ctrl+shift+alt+down"], 4);
    take.finish()
}

fn multi_cursor() -> String {
    let mut take = Take::new("main.rs", SAMPLE);
    take.at(1, 9)
        .capture("the caret is inside `total`", 3)
        .press_and_hold(&["ctrl+d"], 3)
        .press_and_hold(&["ctrl+d"], 4)
        .type_text("sum")
        .press_and_hold(&["escape"], 3);
    take.finish()
}

fn find() -> String {
    let mut take = Take::new("main.rs", SAMPLE);
    take.at(0, 0)
        .capture("press ctrl+f to search the open file", 2)
        .press(&["ctrl+f"])
        .type_text("let")
        .press_and_hold(&["enter"], 3)
        .press_and_hold(&["enter"], 3)
        .press_and_hold(&["alt+w"], 4)
        .press_and_hold(&["escape"], 3);
    take.finish()
}

fn replace() -> String {
    let mut take = Take::new("main.rs", SAMPLE);
    take.at(0, 0)
        .capture("press ctrl+h to replace", 2)
        .press(&["ctrl+h"])
        .type_text("sum")
        .press_and_hold(&["tab"], 3)
        .type_text("total")
        .press_and_hold(&["ctrl+alt+enter"], 5);
    take.finish()
}

fn diagnostics() -> String {
    let mut take = Take::new("main.rs", SAMPLE);
    // Injected rather than fetched: a demonstration must not need a language
    // server installed to build, and the renderer cannot tell the difference —
    // this is the same list a `publishDiagnostics` notification produces.
    take.session.set_diagnostics(vec![
        Diagnostic {
            range: Range::new(Position::new(2, 8), Position::new(2, 13)),
            severity: Severity::Warning,
            code: Some("unused_variables".to_owned()),
            source: Some("rustc".to_owned()),
            message: "unused variable: `count`".to_owned(),
        },
        Diagnostic {
            range: Range::new(Position::new(3, 4), Position::new(3, 12)),
            severity: Severity::Error,
            code: Some("E0425".to_owned()),
            source: Some("rustc".to_owned()),
            message: "cannot find value `totl` in this scope".to_owned(),
        },
    ]);
    take.at(0, 0)
        .capture("the status bar tallies what the server found", 4)
        .press_and_hold(&["f8"], 4)
        .press_and_hold(&["f8"], 4)
        .press_and_hold(&["f8"], 4);
    take.finish()
}

fn code_actions() -> String {
    // A file with something wrong with it, so the diagnostic and the fix for it
    // are on screen together — which is the pairing the feature is about.
    const TEXT: &str = "fn main() {\n    let count = 2;\n    println!(\"hello\");\n}\n";
    let mut take = Take::new("main.rs", TEXT);
    take.session.set_diagnostics(vec![Diagnostic {
        range: Range::new(Position::new(1, 8), Position::new(1, 13)),
        severity: Severity::Warning,
        code: Some("unused_variables".to_owned()),
        source: Some("rustc".to_owned()),
        message: "unused variable: `count`".to_owned(),
    }]);
    take.at(1, 8).capture("the caret is on the warning", 4);

    // The list the server answered with, injected for the same reason the
    // diagnostics are: a demonstration must not need a server installed to
    // build. Everything after this is the editor's own code.
    take.session.offer_code_actions(vec![
        deco_editor::commands::PaletteEntry::new("0", "Prefix the name with an underscore")
            .with_detail("quickfix"),
        deco_editor::commands::PaletteEntry::new("1", "Remove this line").with_detail("quickfix"),
        deco_editor::commands::PaletteEntry::new("2", "Extract into function")
            .with_detail("extract"),
        deco_editor::commands::PaletteEntry::new("3", "Inline variable")
            .with_detail("unavailable — not on a variable"),
    ]);
    take.resize_for_chrome();
    take.capture("ctrl+.", 7);

    // Choosing the first one. The edit is the server's; applying it is the same
    // workspace-edit path a rename uses, which is why one `ctrl+z` takes it back.
    take.session
        .run("workbench.action.acceptSelectedQuickOpenItem", None, 0);
    let edit = deco_lsp::WorkspaceEdit {
        changes: vec![document_edits("file:///demo/main.rs", &[(1, 8, 8)], "_")],
    };
    let plan = take
        .session
        .plan_workspace_edit(
            &edit,
            |uri| uri.to_path(deco_lsp::uri::PathStyle::Unix).ok(),
            |_| None,
        )
        .expect("the demonstration's own URI")
        .with_contents(|_| unreachable!("the only file it touches is open"))
        .expect("nothing to read");
    let applied = take
        .session
        .apply_workspace_edit(plan, 0)
        .expect("nothing overlaps");
    take.session.status = Some(applied.summary("Prefix the name with an underscore"));
    take.resize_for_chrome();
    take.capture("enter", 7);

    take.press_and_hold(&["ctrl+z"], 5);
    take.finish()
}

/// Two files that both mention `greet`, only one of which is open.
const RENAME_MAIN: &str = "fn main() {\n    greet(\"world\");\n}\n\nfn greet(who: &str) {\n    println!(\"hello {who}\");\n}\n";
const RENAME_HELPER: &str = "fn again() {\n    greet(\"again\");\n}\n";

fn rename() -> String {
    let mut take = Take::new("main.rs", RENAME_MAIN);
    take.at(1, 4).capture("the caret is on `greet`", 3);

    // `f2` is gated on `editorHasRenameProvider`, which is set from what a
    // server announced — so the prompt is opened the way the frontend opens it
    // once that check has passed, rather than by pressing a key that would be
    // unbound with no server running.
    take.session.offer_rename();
    take.resize_for_chrome();
    take.capture("f2", 4);

    take.type_text("welcome");
    take.session
        .run("workbench.action.acceptSelectedQuickOpenItem", None, 0);

    // The server's answer, injected for the same reason the diagnostics above
    // are: a demonstration must not need a language server installed to build.
    // This is the `WorkspaceEdit` a rename provider returns, and everything from
    // here on is the editor's own code deciding what to do with it.
    let edit = deco_lsp::WorkspaceEdit {
        changes: vec![
            document_edits("file:///demo/main.rs", &[(1, 4, 9), (4, 3, 8)], "welcome"),
            document_edits("file:///demo/helper.rs", &[(1, 4, 9)], "welcome"),
        ],
    };
    let plan = take
        .session
        .plan_workspace_edit(
            &edit,
            |uri| uri.to_path(deco_lsp::uri::PathStyle::Unix).ok(),
            |_| None,
        )
        .expect("the demonstration's own URIs")
        .with_contents(|_| Ok(RENAME_HELPER.to_owned()))
        .expect("helper.rs is right here");
    let applied = take
        .session
        .apply_workspace_edit(plan, 0)
        .expect("nothing overlaps");
    // The sentence the terminal frontend puts there, built the same way, so the
    // frame shows the status line a user would actually get.
    take.session.status = Some(applied.summary("Renamed"));

    take.resize_for_chrome();
    // The tab bar is the part worth pausing on: `helper.rs` was not open a
    // moment ago, and the status line says how much is now unsaved.
    take.capture("enter", 7);

    take.press_and_hold(&["ctrl+z"], 6);
    take.finish()
}

/// One document's edits, each replacing `line[from..to]` with `new_text`.
fn document_edits(
    uri: &str,
    ranges: &[(u32, u32, u32)],
    new_text: &str,
) -> deco_lsp::DocumentEdits {
    deco_lsp::DocumentEdits {
        uri: deco_lsp::uri::Uri::from_string(uri),
        version: None,
        edits: ranges
            .iter()
            .map(|(line, from, to)| deco_lsp::TextEdit {
                range: Range::new(Position::new(*line, *from), Position::new(*line, *to)),
                new_text: new_text.to_owned(),
            })
            .collect(),
    }
}

fn hover() -> String {
    let mut take = Take::new("main.rs", SAMPLE);
    take.at(1, 9).capture("the caret is on `total`", 3);
    let hover = Hover {
        contents: "let total: i32\n\nThe running total. Hover text arrives from the language \
                   server already flattened to plain lines."
            .to_owned(),
        range: Some(Range::new(Position::new(1, 8), Position::new(1, 13))),
    };
    take.capture_overlay("ctrl+k ctrl+i", 6, Some(&hover), None);
    take.capture_overlay("escape", 3, None, None);
    take.finish()
}

fn completion() -> String {
    // A blank line to complete on, so the frames show the prefix being typed
    // into the file rather than a caption claiming it was.
    let mut take = Take::new("main.rs", "fn main() {\n    let total = 1;\n    \n}\n");
    take.at(2, 4);

    let items = vec![
        item("println!", CompletionKind::Snippet, "macro"),
        item("print!", CompletionKind::Snippet, "macro"),
        item("panic!", CompletionKind::Snippet, "macro"),
        item("total", CompletionKind::Value, "i32"),
        item("count", CompletionKind::Value, "i32"),
    ];
    let mut suggest = Suggest::new(items, Position::new(2, 4), false);
    take.capture_overlay("ctrl+space", 4, None, Some(&suggest));

    // Both the document and the list, in that order — which is what the event
    // loop does, and why a keystroke narrows the list and inserts itself.
    for c in "pr".chars() {
        take.type_char(c);
        suggest.push(c);
    }
    take.capture_overlay("type “pr”", 4, None, Some(&suggest));
    suggest.next();
    take.capture_overlay("down", 4, None, Some(&suggest));

    // Accepting inserts the selected label, which is what the frontend does with
    // `Session::replace_range` once the list has answered.
    let selected = suggest.selected_item().expect("the list has a selection");
    let insert = selected.insert.clone();
    let anchor = suggest.anchor();
    let caret = take.session.view.selections.primary().active;
    take.session
        .replace_range(Range::new(anchor, caret), &insert, 99_000);
    take.capture("enter", 5);
    take.finish()
}

fn highlighting() -> String {
    // One frame per language, so the colours can be compared across them. Each is
    // a separate document, which is also what proves the lexer is chosen from the
    // file name rather than guessed at.
    let samples: [(&str, &str); 5] = [
        (
            "main.rs",
            "// A Rust sample.\nfn total(items: &[u32]) -> u32 {\n    let mut sum = 0;\n    for n in items {\n        sum += *n;\n    }\n    sum\n}\n",
        ),
        (
            "app.ts",
            "// TypeScript.\ninterface User { name: string; age: number }\n\nexport function greet(user: User): string {\n  return `hello ${user.name}`;\n}\n",
        ),
        (
            "script.py",
            "# Python.\nclass Counter:\n    \"\"\"Counts things.\"\"\"\n\n    def __init__(self, start=0):\n        self.value = start\n\n    def bump(self):\n        self.value += 1\n        return self.value\n",
        ),
        (
            "config.toml",
            "# A manifest.\n[package]\nname = \"deco\"\nedition = \"2021\"\n\n[dependencies]\nropey = { version = \"1\", default-features = false }\n",
        ),
        (
            "data.json",
            "{\n  \"name\": \"deco\",\n  \"version\": 1,\n  \"nested\": { \"ok\": true, \"missing\": null },\n  \"list\": [1, 2, 3]\n}\n",
        ),
    ];

    let mut take = Take::new(samples[0].0, samples[0].1);
    take.capture(
        &format!("{} — colours come from the theme", samples[0].0),
        5,
    );
    for (file, text) in &samples[1..] {
        take.session
            .open(PathBuf::from(format!("/demo/{file}")), text);
        take.session.resize(COLUMNS, ROWS - 1);
        take.capture(file, 5);
    }
    take.finish()
}

fn go_to_symbol() -> String {
    const TEXT: &str = "struct Counter {\n    value: u32,\n}\n\nimpl Counter {\n    fn new() -> Self {\n        Self { value: 0 }\n    }\n\n    fn bump(&mut self) {\n        self.value += 1;\n    }\n}\n";

    let mut take = Take::new("counter.rs", TEXT);
    take.at(0, 0).capture("counter.rs", 4);

    // Injected rather than fetched, as the diagnostics demonstration injects its
    // own: building the documentation must not need rust-analyzer installed, and
    // this is the list `textDocument/documentSymbol` decodes to.
    take.session.offer_symbols(vec![
        symbol_entry("Counter", "struct", 0, 7),
        symbol_entry("Counter.value", "field", 1, 4),
        symbol_entry("Counter.new", "method", 5, 7),
        symbol_entry("Counter.bump", "method", 9, 7),
    ]);
    take.resize_for_chrome();
    take.capture("ctrl+shift+o — the kind is the right-hand column", 6);

    // Typing filters, and `bump` is reachable by its own name even though the
    // list shows it qualified.
    take.type_text("bump");

    // What the frontend does with the `OpenFile` outcome `enter` produces. Spelled
    // out here because the core has no filesystem: it names the file and the
    // position, and the frontend is what goes there. For a document already open
    // this is a tab switch onto itself, which is why unsaved changes survive.
    take.session.prompt = None;
    take.session.open(PathBuf::from("/demo/counter.rs"), TEXT);
    take.at(9, 7);
    take.resize_for_chrome();
    take.capture("enter — the caret lands on the name", 6);
    take.finish()
}

/// One row of a go-to-symbol list, in the shape the frontend builds.
fn symbol_entry(
    qualified: &str,
    kind: &str,
    line: u32,
    character: u32,
) -> deco_editor::commands::PaletteEntry {
    deco_editor::commands::PaletteEntry::at(
        "/demo/counter.rs",
        qualified,
        Position::new(line, character),
    )
    .with_detail(kind)
}

fn semantic_tokens() -> String {
    // Deliberately a sample the lexer gets *right* as far as it can, so the
    // difference the frames show is only what a lexer cannot know: which names
    // are parameters, which calls are methods, and that `LIMIT` is a constant
    // rather than the type its capitals suggest.
    const TEXT: &str = "const LIMIT: u32 = 10;\n\nfn scale(values: &mut [u32], factor: u32) {\n    for value in values.iter_mut() {\n        *value = (*value * factor).min(LIMIT);\n    }\n}\n";

    let mut take = Take::new("main.rs", TEXT);
    take.at(0, 0)
        .capture("the lexer alone — LIMIT reads as a type", 5);

    // Injected rather than fetched, for the same reason the diagnostics
    // demonstration injects: building the documentation must not need
    // rust-analyzer installed, and the renderer cannot tell the difference —
    // this is the list `textDocument/semanticTokens/full` decodes to.
    take.session.semantic_tokens = vec![
        semantic("variable", &["readonly"], 0, 6, 11),
        semantic("function", &[], 2, 3, 8),
        semantic("parameter", &[], 2, 9, 15),
        semantic("parameter", &[], 2, 29, 35),
        semantic("variable", &[], 3, 8, 13),
        semantic("parameter", &[], 3, 17, 23),
        semantic("method", &[], 3, 24, 32),
        semantic("variable", &[], 4, 9, 14),
        semantic("variable", &[], 4, 18, 23),
        semantic("parameter", &[], 4, 26, 32),
        semantic("method", &[], 4, 34, 37),
        semantic("variable", &["readonly"], 4, 38, 43),
    ];
    take.capture("the server: LIMIT is a constant, and names are bound", 6);

    // Off again, in place: the setting is read on every frame, so the same
    // document and the same token list answer both ways without reopening it.
    take.session
        .settings
        .load_layer(
            deco_config::Scope::User,
            r#"{ "editor.semanticHighlighting.enabled": false }"#,
        )
        .expect("the setting is valid JSON");
    take.capture("editor.semanticHighlighting.enabled: false", 5);
    take.finish()
}

/// One token in a server's answer, in the shape the decoder produces.
fn semantic(
    token_type: &str,
    modifiers: &[&str],
    line: u32,
    from: u32,
    to: u32,
) -> deco_lsp::requests::SemanticSpan {
    deco_lsp::requests::SemanticSpan {
        range: Range::new(Position::new(line, from), Position::new(line, to)),
        token_type: token_type.to_owned(),
        modifiers: modifiers.iter().map(|m| (*m).to_owned()).collect(),
    }
}

fn chrome() -> String {
    let mut take = Take::new("main.rs", SAMPLE);
    take.at(1, 8).capture("the editor has the whole window", 4);

    // Real keys: both of these were named refusals until this chapter shipped.
    take.press_and_hold(&["ctrl+b"], 6)
        .press_and_hold(&["ctrl+j"], 6);

    // The keyboard is still in the text — showing a region does not cost you
    // your place, which is the thing that is easiest to get wrong and hardest
    // to see in a still image.
    take.type_text("mut ");

    take.press_and_hold(&["ctrl+b"], 4)
        .press_and_hold(&["ctrl+j"], 4);
    take.finish()
}

fn file_tree() -> String {
    let mut take = Take::new("main.rs", SAMPLE);
    take.workspace(&[
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        ("README.md", "# demo\n"),
        ("src/main.rs", SAMPLE),
        ("src/lib.rs", "pub mod parse;\n"),
        ("src/parse/mod.rs", "pub mod lexer;\n"),
        (
            "src/parse/lexer.rs",
            "pub fn tokens(src: &str) -> Vec<&str> {\n    src.split_whitespace().collect()\n}\n",
        ),
        ("tests/parse.rs", "#[test]\nfn it_parses() {}\n"),
    ]);
    take.at(1, 8).capture("a workspace, and one file open", 3);

    // The tree appears without taking the keyboard: `ctrl+b` shows, it does not
    // focus. `ctrl+shift+e` is what moves into it.
    take.press_and_hold(&["ctrl+b"], 5);
    take.press_and_hold(&["ctrl+shift+e"], 4);

    // Two levels down, a directory at a time. Each `right` is what asks for
    // that directory's contents — nothing below a closed folder has been read.
    take.press_and_hold(&["right"], 3)
        .press(&["down"])
        .press_and_hold(&["right"], 4)
        .press(&["down"]);

    // Enter opens the file and takes the keyboard with it, so what is typed
    // next goes into what was just opened rather than into what was there.
    take.press_and_hold(&["enter"], 5);
    take.type_text("// ");
    take.capture("the caret is in the file the tree opened", 5);
    take.finish()
}

fn git_status() -> String {
    // Assembled as the bytes `git status --porcelain=v2 --branch -z` writes,
    // and parsed by the same code the editor uses. A `Status` built by hand
    // here would be a demonstration of a struct rather than of git.
    fn status(entries: &[&str]) -> String {
        let mut out = String::from(
            "# branch.oid 1c9d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d\0\
             # branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -0\0",
        );
        for entry in entries {
            out.push_str(entry);
            out.push('\0');
        }
        out
    }
    const MODIFIED: &str = "1 .M N... 100644 100644 100644 aaaaaaa bbbbbbb src/main.rs";
    const UNTRACKED: &str = "? src/notes.md";

    let mut take = Take::new("main.rs", SAMPLE);
    take.workspace(&[
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        ("src/main.rs", SAMPLE),
    ]);
    take.git(&status(&[]));
    take.at(1, 8)
        .capture("on `main`, two commits to push, nothing else to say", 5);

    // Typing changes the file on screen and nothing else: `git` is not run per
    // keystroke, so the bar is still describing the last thing it was told.
    take.type_text("mut ");
    take.git(&status(&[MODIFIED]))
        .capture("edited, and the bar has not moved", 5);

    // A save is what asks again — and the same `git` call is what answers.
    take.press_and_hold(&["ctrl+s"], 2);
    take.git(&status(&[MODIFIED]))
        .capture("saving is what asks git again", 6);

    // So is making a file. One more thing git has to say about, before it has
    // ever been written to.
    take.press_and_hold(&["ctrl+b"], 3)
        .press_and_hold(&["ctrl+shift+e"], 3)
        .press_and_hold(&["ctrl+n"], 3);
    take.type_text("notes.md");
    take.press_and_hold(&["enter"], 3);
    take.git(&status(&[MODIFIED, UNTRACKED]))
        .capture("a new file counts before it holds anything", 6);
    take.finish()
}

fn git_gutter() -> String {
    // The file as it was committed. The demonstration then edits it with real
    // keys, so every mark on screen is one the editor worked out from this
    // text and the buffer — not one the scenario asked for.
    const HEAD: &str = "fn main() {\n    let total = 0;\n    let count = 2;\n\
                            let extra = 4;\n    let last = 5;\n    report();\n}\n";

    let mut take = Take::new("main.rs", HEAD);
    take.committed(HEAD);
    take.at(0, 0).capture(
        "nothing edited since the last commit, so nothing beside it",
        5,
    );

    // A line that says something else.
    take.at(1, 0).press(&["end", "backspace", "backspace"]);
    take.type_text("9;");
    take.capture("`│` — this line differs from the committed one", 6);

    // A line that was not there at all. Separated from the change above by an
    // untouched line, which is what keeps the two marks distinct rather than
    // one block: git reports a replaced run as a single hunk.
    take.at(2, 0).press(&["end", "enter"]);
    take.type_text("let sum = 3;");
    take.capture("`┃` — this line is new", 6);

    // And one taken away. Nothing is left to draw beside, so the mark sits on
    // the top edge of the line that took its place.
    take.at(5, 0).press_and_hold(&["ctrl+shift+k"], 6);
    take.capture("`▔` — a line was removed just above", 6);
    take.finish()
}

fn file_mutations() -> String {
    let mut take = Take::new("main.rs", SAMPLE);
    take.workspace(&[
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        ("src/main.rs", SAMPLE),
        ("src/lib.rs", "pub mod parse;\n"),
    ]);
    take.press_and_hold(&["ctrl+b"], 3);
    take.press_and_hold(&["ctrl+shift+e"], 3);

    // Into `src`, so the new file lands beside its siblings rather than at the
    // root — the directory a new file goes in is the one that is selected.
    take.press_and_hold(&["right"], 3).press(&["down"]);
    take.capture("src/lib.rs selected", 3);

    // `ctrl+n` in the tree is a new file, not a new untitled buffer.
    take.press_and_hold(&["ctrl+n"], 3);
    take.type_text("parse.rs");
    take.press_and_hold(&["enter"], 5);

    // It was created *and* opened, and the keyboard went with it — so this
    // types into the new file.
    take.type_text("pub fn parse() {}");
    take.capture("created, opened, and being typed into", 5);

    // Back to the tree to rename it. `F2` here is the file; `F2` in the text is
    // the symbol under the cursor, and they are told apart by what has focus.
    take.press_and_hold(&["ctrl+shift+e"], 3);
    take.press_and_hold(&["f2"], 4);
    take.type_text("lexer.rs");
    take.press_and_hold(&["enter"], 5);
    take.capture("renamed — and the tab followed it", 5);

    // The tree's own undo. `ctrl+z` here puts the file back; in the text it
    // would put characters back.
    take.press_and_hold(&["ctrl+z"], 6);
    take.finish()
}

fn tabs() -> String {
    let mut take = Take::new(
        "main.rs",
        "fn main() {\n    let total = items().sum();\n    println!(\"{total}\");\n}\n",
    );
    take.capture("one file open — no tab bar", 3);
    take.session.open(
        PathBuf::from("/demo/lib.rs"),
        "/// The numbers to add up.\npub fn items() -> Vec<u32> {\n    vec![1, 2, 3]\n}\n",
    );
    take.resize_for_chrome();
    take.capture("a second file opens in a new tab", 4);
    take.press_and_hold(&["ctrl+tab"], 3)
        .at(1, 8)
        .type_text("mut ")
        .press_and_hold(&["ctrl+tab"], 3)
        .press_and_hold(&["ctrl+tab"], 4)
        // A dirty tab refuses to close — losing edits to a keystroke is the
        // worst thing an editor can do, and deco has no dialog to ask with.
        .press_and_hold(&["ctrl+w"], 5);
    // The refusal has been read; a status message persists until the next one,
    // and carrying it into the closing frames would read as a second refusal.
    take.session.status = None;
    take.press_and_hold(&["ctrl+tab"], 2)
        // The clean tab closes, and with one document left the bar goes away.
        .press_and_hold(&["ctrl+w"], 5);
    take.finish()
}

fn save_all() -> String {
    let mut take = Take::new(
        "main.rs",
        "fn main() {\n    let total = items().sum();\n    println!(\"{total}\");\n}\n",
    );
    take.session.open(
        PathBuf::from("/demo/lib.rs"),
        "pub fn items() -> Vec<u32> {\n    vec![1, 2, 3]\n}\n",
    );
    take.resize_for_chrome();
    take.at(1, 17).capture("two files open, neither edited", 4);

    // Edit both tabs, so both carry the bar's dirty marker.
    take.type_text(" // three");
    take.press_and_hold(&["ctrl+tab"], 3)
        .at(1, 8)
        .type_text("mut ");
    take.capture("two tabs edited — the bar marks both", 5);

    // Exactly what the frontend does with `Outcome::SaveAll`: the loop and the
    // reporting are the core's, and only the write belongs to the frontend. Here
    // the write succeeds without touching a disk, which is the same closure the
    // tests use.
    if let deco_editor::commands::Outcome::Message(report) = take.session.save_all(|_, _| Ok(())) {
        take.session.status = Some(report);
    }
    take.resize_for_chrome();
    take.capture("ctrl+k s — both written, and it says how many", 6);
    take.finish()
}

fn color_theme() -> String {
    let mut take = Take::new("main.rs", SAMPLE);
    take.at(1, 8).capture("Default Dark Modern", 5);
    take.press(&["ctrl+k"]);
    take.press(&["ctrl+t"]);

    // The list is the frontend's, since a contributed theme is a file in an
    // extension directory. Handed in rather than walked, so the demonstration does
    // not depend on what happens to be installed where it is generated — but
    // through the real row builder, so the columns are the ones a reader will see.
    let installed = [
        ("Default Dark Modern", None, "dark"),
        ("Default Light Modern", None, "light"),
        ("Night Owl", Some("/ext/owl/themes/owl.json"), "dark"),
        ("Paper", Some("/ext/paper/paper.json"), "light"),
    ]
    .map(|(label, path, kind)| deco_tui::themes::Available {
        label: label.to_owned(),
        path: path.map(PathBuf::from),
        kind,
    });
    take.session
        .offer_themes(deco_tui::themes::rows(&installed));
    take.resize_for_chrome();
    take.capture("dark or light is the second column", 6);
    take.type_text("light");

    // Exactly what the frontend does with `Outcome::LoadTheme`: the picker names a
    // theme, and reading it belongs to the side with a filesystem. A built-in
    // needs only its label.
    take.session.prompt = None;
    if let deco_editor::commands::Outcome::Message(report) = take.session.set_theme(
        deco_theme::defaults::builtin("Default Light Modern")
            .expect("the built-in light theme must parse"),
    ) {
        take.session.status = Some(report);
    }
    take.resize_for_chrome();
    take.capture("enter — the same theme keys, resolved again", 6);
    take.finish()
}

fn split() -> String {
    // Long enough that the two groups can be looking at different parts of it,
    // which is the reason to split at all.
    let mut text = String::from("fn main() {\n");
    for n in 1..=40 {
        text.push_str(&format!("    step_{n}();\n"));
    }
    text.push_str("}\n");

    let mut take = Take::new("main.rs", &text);
    take.at(1, 4).capture("one group, one file", 4);
    take.press_and_hold(&["ctrl+\\"], 6);

    // Scroll the new group to the end of the function while the first stays at
    // the top: two places in one file, at once.
    take.session.view.scroll_top = 28;
    take.at(30, 4);
    take.resize_for_chrome();
    take.capture("the second group scrolls on its own", 6);

    // An edit in the focused group shows in both, because there is one document.
    take.type_text("// ");
    take.press_and_hold(&["ctrl+1"], 5);
    take.press_and_hold(&["ctrl+w"], 5);
    take.finish()
}

fn block_comment() -> String {
    let mut take = Take::new(
        "main.rs",
        "fn main() {\n    let total = 1;\n    let count = 2;\n    println!(\"{total}\");\n}\n",
    );
    // Two whole lines, which is what makes the difference from `ctrl+/` visible:
    // one comment around the block rather than a token on each line.
    take.session.view.selections = SelectionSet::single(deco_core::Selection::new(
        Position::new(1, 4),
        Position::new(2, 18),
    ));
    take.capture("two lines selected", 4);
    take.press_and_hold(&["ctrl+shift+a"], 6);
    take.press_and_hold(&["ctrl+shift+a"], 5);
    // And with nothing selected, an empty comment with the caret inside it.
    take.at(3, 23);
    take.press_and_hold(&["ctrl+shift+a"], 5);
    take.type_text("why");
    take.finish()
}

fn word_wrap() -> String {
    // Prose rather than code, because that is where the long line comes from, and
    // where moving by row rather than by line is the visible difference.
    let mut take = Take::new(
        "notes.md",
        "# Word wrap\n\nA paragraph long enough to run several rows past the right edge \
         of the window, which is what wrapping is for and what the arrow keys then have \
         to walk one row at a time.\nshort line\n",
    );
    take.at(2, 0)
        .capture("line 3 runs off the edge — the rest is not on screen", 5)
        .press_and_hold(&["alt+z"], 6);

    // Down the wrapped rows. Each press moves one row and not one line, which the
    // status bar's column reports and a still image cannot show.
    take.press(&["down"]);
    take.press(&["down"]);
    take.press_and_hold(&["down"], 4);
    take.press_and_hold(&["end"], 5);
    take.press_and_hold(&["alt+z"], 5);
    take.finish()
}

fn wrapping_indent() -> String {
    // Indented code, where the continuation row's own indentation is the point: at
    // column zero it would sit beside the unindented lines around it.
    const NESTED: &str = "fn main() {\n    if ready {\n        let total = one + two + three + four + five + six + seven + eight;\n    }\n}\n";
    let mut take = Take::new("main.rs", NESTED);
    take.at(2, 8).capture(
        "editor.wrappingIndent defaults to \"same\" — press alt+z to see it",
        5,
    );
    take.press_and_hold(&["alt+z"], 7);
    take.append(
        r#"{"editor.wordWrap": "on", "editor.wrappingIndent": "none"}"#,
        "\"none\" — the continuation starts at column zero, beside the outer block",
        7,
    );
    take.append(
        r#"{"editor.wordWrap": "on", "editor.wrappingIndent": "deepIndent"}"#,
        "\"deepIndent\" — two levels past the line, so a wrap cannot be misread as code",
        7,
    );
    take.finish()
}

fn detect_indentation() -> String {
    // Two-space TypeScript, opened by an editor whose `editor.tabSize` is four.
    let mut take = Take::new(
        "config.ts",
        "export const config = {\n  retries: 3,\n  timeout: 500,\n};\n",
    );
    // At the start of the closing line, where one press of `tab` inserts exactly
    // one level and the width of that level is the thing being demonstrated.
    take.at(3, 0).capture(
        "two-space TypeScript — the status bar says the file overruled the setting",
        6,
    );
    // One press of `tab` in somebody else's project must not reindent it.
    take.press_and_hold(&["tab"], 6);

    // The same key in a file that indents by four, for contrast.
    take.session.open(
        PathBuf::from("/demo/main.rs"),
        "fn main() {\n    let total = 1;\n}\n",
    );
    take.at(2, 0).resize_for_chrome();
    take.capture(
        "four-space Rust — the file agrees with the setting, so nothing is said",
        5,
    );
    take.press_and_hold(&["tab"], 6);
    take.finish()
}

fn view_settings() -> String {
    // Three settings that draw rather than behave, so one scenario shows all three
    // against the same file by turning them on in turn.
    let mut take = Take::new("main.rs", RULED);
    take.at(1, 4).capture(
        "the defaults: whitespace shows inside a selection, and nowhere else",
        5,
    );

    // Selected, which is what the default mode marks.
    take.session.view.selections = SelectionSet::single(deco_core::Selection::new(
        Position::new(1, 0),
        Position::new(1, 18),
    ));
    take.resize_for_chrome();
    take.capture("a selection — its spaces are dotted", 5);

    take.append(
        r#"{"editor.renderWhitespace": "all"}"#,
        "editor.renderWhitespace: \"all\" — every space, and an arrow per tab",
        6,
    );
    take.append(
        r#"{"editor.renderWhitespace": "all", "editor.rulers": [24],
            "editor.lineNumbers": "interval"}"#,
        "…with editor.rulers: [24] and lineNumbers: \"interval\"",
        7,
    );
    take.finish()
}

/// A file with lines either side of column 24, so a ruler has something to warn
/// about; one tab-indented line, so an arrow has somewhere to go; and enough lines
/// that `lineNumbers: "interval"` reaches its first tenth.
const RULED: &str = "fn main() {\n    let total = 1;\n    let long = total + 2 + 3 + 4;\n\tlet tabbed = 5;\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n    let e = 5;\n    let f = 6;\n    println!(\"{total}\");\n}\n";

fn auto_closing_brackets() -> String {
    let mut take = Take::new("main.rs", "fn main() {\n    \n}\n");
    take.at(1, 4).capture("an empty line, ready for a call", 4);
    // Every keystroke captured: the pair appearing and the caret staying inside it
    // is the whole feature, and it happens one character at a time.
    take.press(&["p", "r", "i", "n", "t"]);
    take.press_and_hold(&["("], 6);
    take.press_and_hold(&["\""], 6);
    take.press(&["h", "i"]);
    // And back out over both closers, typing them rather than moving past them.
    take.press_and_hold(&["\""], 5);
    take.press_and_hold(&[")"], 6);
    take.press_and_hold(&[";"], 5);
    take.finish()
}

fn auto_indent() -> String {
    let mut take = Take::new("main.rs", "fn main() {\n    if ready \n}\n");
    take.at(1, 14)
        .capture("the caret after `if ready`, one level in", 4);
    // `{` closes itself, and `enter` opens the pair into a block.
    take.press_and_hold(&["{"], 5);
    take.press_and_hold(&["enter"], 7);
    take.press(&["r", "u", "n"]);
    take.press_and_hold(&["("], 4);
    take.press_and_hold(&[")"], 4);
    take.press_and_hold(&[";"], 6);
    // And a second `enter`, which keeps the indentation it is on.
    take.press_and_hold(&["enter"], 6);
    take.finish()
}

fn trim_auto_whitespace() -> String {
    // Whitespace is invisible, so the demonstration turns it on: with
    // `renderWhitespace: "all"` the indent that is taken back can be seen going.
    let mut take = Take::new("main.rs", "fn main() {\n    let total = 1;\n}\n");
    take.append(
        r#"{"editor.renderWhitespace": "all"}"#,
        "whitespace shown, so the indent can be watched",
        4,
    );
    take.shots.remove(0);
    take.session.view.selections = SelectionSet::caret(Position::new(1, 18));
    take.resize_for_chrome();
    take.capture("the caret at the end of an indented line", 4);
    take.press_and_hold(&["enter"], 6);
    take.press_and_hold(&["enter"], 7);
    take.finish()
}

fn save_as() -> String {
    let mut take = Take::new("notes.txt", "name = \"deco\"\nedition = \"2021\"\n");
    take.at(0, 0)
        .capture("notes.txt — plain, and named as such", 4);
    take.press_and_hold(&["ctrl+shift+s"], 5);

    // The seed opens selected, so typing replaces it — which is what makes
    // seeding the field with the current path worth doing.
    take.type_text("/demo/Cargo.toml");

    // What the frontend does with `Outcome::SaveAs`: resolve the path, write it,
    // and hand it back. The write is elided here — a demonstration must not touch a
    // disk — but the renaming is the real thing.
    take.session.prompt = None;
    if let deco_editor::commands::Outcome::Message(report) =
        take.session.rename_to(PathBuf::from("/demo/Cargo.toml"))
    {
        take.session.status = Some(report);
    }
    take.resize_for_chrome();
    take.capture("enter — written, renamed, and TOML from now on", 6);
    take.finish()
}

fn language_mode() -> String {
    // A `.txt` file that is really TOML. Nothing about the name says so, so the
    // lexer has nothing to go on until it is told.
    let mut take = Take::new(
        "notes.txt",
        "# A manifest, in a file that does not say so.\n[package]\nname = \"deco\"\nedition = \"2021\"\n",
    );
    take.at(0, 0)
        .capture("notes.txt — no language, so no colour", 5);
    take.press(&["ctrl+k"]);
    take.press_and_hold(&["m"], 5);
    take.type_text("toml");
    take.press_and_hold(&["enter"], 6);
    take.finish()
}

fn quick_open() -> String {
    let mut take = Take::new("main.rs", SAMPLE);
    // The list is handed in rather than walked: a demonstration must not depend on
    // what happens to be in the working directory when it is generated, or the
    // committed file would differ by who ran the command.
    let files = [
        "src/main.rs",
        "src/lib.rs",
        "src/config/mod.rs",
        "src/config/parse.rs",
        "tests/smoke.rs",
        "README.md",
        "Cargo.toml",
    ];
    take.capture("ctrl+p opens any file in the workspace", 3);
    take.session.offer_files(
        files
            .iter()
            .map(|path| deco_editor::commands::PaletteEntry::new(&format!("/demo/{path}"), path))
            .collect(),
    );
    take.resize_for_chrome();
    take.capture("ctrl+p", 4);
    take.type_text("conf");
    take.press_and_hold(&["down"], 4);
    // Accepting asks the frontend to read the file; the demonstration stands in
    // for that, since there is nothing on disk to read.
    take.session.prompt = None;
    take.session.open(
        PathBuf::from("/demo/src/config/parse.rs"),
        "pub fn parse(text: &str) -> Config {\n    Config::from(text)\n}\n",
    );
    take.resize_for_chrome();
    take.capture("enter — opened in a new tab", 5);

    // And again, now that two files have been on screen: they come first, most
    // recently first, which is most of what makes the key fast.
    take.session.offer_files(
        files
            .iter()
            .map(|path| deco_editor::commands::PaletteEntry::new(&format!("/demo/{path}"), path))
            .collect(),
    );
    take.resize_for_chrome();
    take.capture(
        "ctrl+p again — the two files that have been open are at the top",
        7,
    );
    take.finish()
}

fn replace_in_files() -> String {
    // Two files that both say `amount`, one of them not open — the case the
    // feature is about, and the reason the tab bar changes partway through.
    const OPEN: &str = "fn total(rows: &[Row]) -> u32 {\n    let mut amount = 0;\n    for row in rows {\n        amount += row.value;\n    }\n    amount\n}\n";
    const OTHER: &str = "pub struct Row {\n    pub amount: u32,\n}\n";

    let mut take = Take::new("report.rs", OPEN);
    take.at(1, 12).capture("two files say `amount`", 4);

    take.session.run("workbench.action.replaceInFiles", None, 0);
    take.resize_for_chrome();
    take.capture("ctrl+shift+h", 3);
    // The field opens seeded from the caret and selected, so this is a
    // demonstration of typing over it rather than of clearing it first.
    take.type_text("amount");
    take.session
        .run("workbench.action.acceptSelectedQuickOpenItem", None, 0);
    take.resize_for_chrome();
    take.capture("enter — and it asks what to put there", 4);
    take.type_text("subtotal");

    // The search is the frontend's, and a demonstration has no workspace to
    // walk; the two paths it would have found are handed in. Everything after
    // this is the editor's own code deciding what the edit is.
    take.session
        .run("workbench.action.acceptSelectedQuickOpenItem", None, 0);
    let plan = take
        .session
        .plan_replacements(
            &[
                PathBuf::from("/demo/report.rs"),
                PathBuf::from("/demo/row.rs"),
            ],
            "amount",
            "subtotal",
            Default::default(),
            |_| Ok(OTHER.to_owned()),
        )
        .expect("row.rs is right here");
    let applied = take
        .session
        .apply_workspace_edit(plan, 0)
        .expect("nothing overlaps");
    take.session.status = Some(applied.summary("Replaced `amount`"));
    take.resize_for_chrome();
    take.capture("enter", 7);

    take.press_and_hold(&["ctrl+z"], 6);
    take.finish()
}

fn search_in_files() -> String {
    let mut take = Take::new("main.rs", SAMPLE);
    take.at(1, 9)
        .capture("the caret is on `total`", 3)
        // The field is seeded from the caret, and the seed is only a seed: it
        // opens selected, so searching for something the cursor is nowhere near
        // is just typing it.
        .press_and_hold(&["ctrl+shift+f"], 5)
        .type_text("amount")
        // The prompt has one line and no room to draw the options, so the toggle
        // reports them. They are this search's own, not the find bar's.
        .press_and_hold(&["alt+c"], 5);

    // Handed in for the same reason as the quick-open list: a demonstration must
    // not depend on what happens to be on disk when it is generated.
    take.session.offer_search_results(
        "amount",
        vec![
            entry(
                "/demo/src/report.rs",
                "src/report.rs:7: total += row.amount;",
                6,
                26,
            ),
            entry("/demo/src/row.rs", "src/row.rs:3: pub amount: u32,", 2, 8),
            entry(
                "/demo/tests/totals.rs",
                "tests/totals.rs:5: Row { amount: 2 },",
                4,
                10,
            ),
        ],
    );
    take.resize_for_chrome();
    take.capture("enter — three matches, listed", 5);

    take.session.prompt = None;
    take.session.open(
        PathBuf::from("/demo/src/report.rs"),
        "use crate::Row;\n\n/// Adds up a column.\npub fn sum(rows: &[Row]) -> u32 {\n    let mut total = 0;\n    for row in rows {\n        total += row.amount;\n    }\n    total\n}\n",
    );
    take.at(6, 26);
    take.resize_for_chrome();
    take.capture("enter again — the file opens at the match", 5);
    take.finish()
}

/// A search result: the file to open, the line to show, and where to land.
fn entry(
    path: &str,
    title: &str,
    line: u32,
    character: u32,
) -> deco_editor::commands::PaletteEntry {
    deco_editor::commands::PaletteEntry::at(path, title, Position::new(line, character))
}

fn command_palette() -> String {
    let mut take = Take::new("main.rs", SAMPLE);
    // The terminal frontend's own list, so the demonstration cannot offer a
    // command the editor does not.
    take.session.frontend_commands = deco_tui::app::frontend_commands();
    take.at(1, 4)
        .capture("ctrl+shift+p lists every command", 2)
        .press(&["ctrl+shift+p"])
        .type_text("comment")
        // Down to `Toggle Line Comment`, which is the one with something to show:
        // `Remove Line Comment` on a line that is not commented correctly does
        // nothing, and a demonstration of nothing happening teaches nothing.
        .press(&["down"])
        .press_and_hold(&["down"], 3)
        .press_and_hold(&["enter"], 5);
    take.finish()
}

/// The palette listing what an extension contributes.
///
/// The catalogue is built from a manifest written here rather than from a
/// directory, because the generator has to produce the same picture on every
/// machine and a real extensions directory is whatever the reader happens to have
/// installed. What is *drawn* is the real palette over the real rows.
fn extension_commands() -> String {
    let manifest = deco_ext::Manifest::parse(
        r#"{
  "name": "prettier-vscode",
  "publisher": "esbenp",
  "displayName": "Prettier",
  "main": "./out/extension.js",
  "contributes": {
    "commands": [
      { "command": "prettier.forceFormatDocument", "title": "Format Document (Forced)" },
      { "command": "prettier.openOutput", "title": "Open Output" }
    ]
  }
}"#,
    )
    .expect("a manifest");
    let catalogue = deco_ext::catalogue::Catalogue::build([(
        std::path::PathBuf::from("/extensions/esbenp.prettier-vscode-11.0.0"),
        manifest,
    )]);

    let mut take = Take::new("main.rs", SAMPLE);
    take.session.frontend_commands = deco_tui::app::frontend_commands();
    // The same rows the editor puts there, from the same function.
    take.session
        .frontend_commands
        .extend(deco_tui::extensions::rows(&catalogue));
    take.at(1, 4)
        .capture(
            "an extension's commands are in the palette, named by extension",
            2,
        )
        .press(&["ctrl+shift+p"])
        .type_text("format")
        .press_and_hold(&["down"], 4);
    take.finish()
}

fn go_to_line() -> String {
    let mut take = Take::new(
        "main.rs",
        "fn main() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n}\n",
    );
    take.at(0, 0)
        .capture("ctrl+g jumps to a line", 2)
        .press(&["ctrl+g"])
        .type_text("4")
        .press_and_hold(&["enter"], 5);
    take.finish()
}

/// A completion item with the fields a demonstration cares about.
fn item(label: &str, kind: CompletionKind, detail: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_owned(),
        kind,
        detail: Some(detail.to_owned()),
        insert: label.to_owned(),
        replace: None,
        filter: label.to_owned(),
        sort: None,
        preselect: false,
        was_snippet: false,
    }
}

// ---- Writing the SVG -----------------------------------------------------

/// Width of one character cell, in user units.
///
/// Every run of text is drawn with a `textLength` of exactly its cell count
/// times this, so the glyphs are stretched or squeezed to the grid rather than
/// trusting the reader's monospace font to advance by the width we assumed.
/// Without that, the text drifts out of its background rectangles on any machine
/// whose default monospace differs from the one used to pick this number.
const CELL: f32 = 8.5;
/// Height of one row.
const LINE: f32 = 19.0;
/// Padding around the terminal.
const PAD: f32 = 12.0;
/// Height of the caption strip under the terminal.
const CAPTION: f32 = 26.0;
/// Font size for the terminal text.
const FONT: f32 = 14.0;
/// How long one time slot lasts, in seconds.
const SLOT: f32 = 0.55;

/// Renders a sequence of frames as one animated SVG.
fn svg(shots: &[Shot], columns: usize, rows: usize) -> String {
    let width = PAD * 2.0 + columns as f32 * CELL;
    let height = PAD * 2.0 + rows as f32 * LINE + CAPTION;
    let slots: u32 = shots.iter().map(|shot| shot.hold).sum();
    let total = slots as f32 * SLOT;

    let mut out = String::new();
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width:.0}\" height=\"{height:.0}\" \
         viewBox=\"0 0 {width:.0} {height:.0}\" role=\"img\">\n"
    ));

    // A generated file, and one a reader may well open directly.
    out.push_str(
        "<!-- Generated by `cargo xtask docs` from deco's own renderer. Do not edit by hand. -->\n",
    );

    out.push_str("<style>\n");
    out.push_str(
        "  text { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, \
         \"DejaVu Sans Mono\", monospace; white-space: pre }\n",
    );
    out.push_str("  .s { opacity: 0 }\n");
    out.push_str(&format!(
        "  .c {{ font-size: {:.0}px; fill: {} }}\n",
        FONT - 1.0,
        hex(Rgba::rgb(0x8a, 0x8a, 0x8a))
    ));
    let mut start = 0u32;
    for (index, shot) in shots.iter().enumerate() {
        let from = 100.0 * start as f32 / slots as f32;
        let to = 100.0 * (start + shot.hold) as f32 / slots as f32;
        out.push_str(&format!(
            "  .s{index} {{ animation: s{index} {total:.2}s step-end infinite }}\n"
        ));
        // `step-end` and explicit 0% keeps every frame hidden outside its slot;
        // the first frame is the one that also has to be visible at 0%.
        if start == 0 {
            out.push_str(&format!(
                "  @keyframes s{index} {{ 0% {{ opacity: 1 }} {to:.3}% {{ opacity: 0 }} }}\n"
            ));
        } else {
            out.push_str(&format!(
                "  @keyframes s{index} {{ 0% {{ opacity: 0 }} {from:.3}% {{ opacity: 1 }} \
                 {to:.3}% {{ opacity: 0 }} }}\n"
            ));
        }
        start += shot.hold;
    }
    out.push_str("</style>\n");

    // The page, in the first frame's colours. It is what shows if the animation
    // does not run, since every frame group starts hidden.
    let page = shots.first().map(|shot| shot.bg).unwrap_or(Rgba::BLACK);
    out.push_str(&format!(
        "<rect width=\"{width:.0}\" height=\"{height:.0}\" rx=\"6\" fill=\"{}\"/>\n",
        hex(page)
    ));

    for (index, shot) in shots.iter().enumerate() {
        out.push_str(&format!("<g class=\"s s{index}\">\n"));
        // Repainted per frame so that a theme change is drawn to the edges rather
        // than leaving the previous theme in the margin.
        out.push_str(&format!(
            "<rect width=\"{width:.0}\" height=\"{height:.0}\" rx=\"6\" fill=\"{}\"/>\n",
            hex(shot.bg)
        ));
        frame_body(&mut out, &shot.frame, shot.bg);
        out.push_str(&format!(
            "<text class=\"c\" x=\"{:.1}\" y=\"{:.1}\">{}</text>\n",
            PAD,
            PAD + rows as f32 * LINE + CAPTION - 8.0,
            escape(&shot.caption)
        ));
        out.push_str("</g>\n");
    }

    out.push_str("</svg>\n");
    out
}

/// The rectangles, text and caret of one frame.
fn frame_body(out: &mut String, frame: &Frame, bg: Rgba) {
    for (row_index, row) in frame.rows.iter().enumerate() {
        let top = PAD + row_index as f32 * LINE;
        let mut column = 0usize;
        for span in &row.spans {
            let cells = span.text.chars().count();
            if cells == 0 {
                continue;
            }
            let x = PAD + column as f32 * CELL;
            if span.bg != bg {
                out.push_str(&format!(
                    "<rect x=\"{x:.1}\" y=\"{top:.1}\" width=\"{:.1}\" height=\"{LINE:.1}\" \
                     fill=\"{}\"/>\n",
                    cells as f32 * CELL,
                    hex(span.bg)
                ));
            }
            // Blank runs are common — the padding on every row — and drawing
            // spaces costs bytes for nothing.
            if span.text.trim().is_empty() {
                column += cells;
                continue;
            }
            out.push_str(&format!(
                "<text x=\"{x:.1}\" y=\"{:.1}\" font-size=\"{FONT:.0}px\" fill=\"{}\" \
                 textLength=\"{:.1}\" lengthAdjust=\"spacingAndGlyphs\">{}</text>\n",
                top + LINE - 5.0,
                hex(span.fg),
                cells as f32 * CELL,
                escape(&span.text)
            ));
            column += cells;
        }
    }

    // The caret last, so it is never covered by a background rectangle.
    if let Some((x, y)) = frame.cursor {
        out.push_str(&format!(
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{LINE:.1}\" fill=\"{}\" \
             opacity=\"0.85\"/>\n",
            PAD + x as f32 * CELL,
            PAD + y as f32 * LINE,
            CELL,
            hex(Rgba::rgb(0xd0, 0xd0, 0xd0))
        ));
    }
}

/// `#rrggbb`, which is what SVG wants and what a reviewer can read.
fn hex(color: Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

/// Escapes the five characters that cannot appear literally in XML text.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_demonstration_produces_a_well_formed_document() {
        for demo in demos() {
            let svg = (demo.build)();
            assert!(
                svg.starts_with("<svg xmlns="),
                "{} did not start with an svg element",
                demo.name
            );
            assert!(svg.trim_end().ends_with("</svg>"), "{}", demo.name);
            // Every group opened is a group closed. A mismatch renders as a blank
            // image in some viewers and as the last frame only in others, so it
            // is worth catching here rather than by looking at it.
            assert_eq!(
                svg.matches("<g ").count(),
                svg.matches("</g>").count(),
                "{} has unbalanced groups",
                demo.name
            );
        }
    }

    #[test]
    fn demonstration_names_are_unique() {
        let mut names: Vec<&str> = demos().iter().map(|demo| demo.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "two demonstrations share a file name");
    }

    #[test]
    fn a_scenario_is_reproducible() {
        // The same scenario twice must produce the same bytes, or `--check`
        // fails on a clean tree and the committed files churn.
        for demo in demos() {
            assert_eq!(
                (demo.build)(),
                (demo.build)(),
                "{} is not stable",
                demo.name
            );
        }
    }

    #[test]
    fn frames_cover_the_whole_timeline_without_overlapping() {
        // Each frame's slot begins where the previous one's ended. Gaps show as a
        // flash of background; overlaps draw two frames at once.
        let shots = vec![
            Shot {
                frame: Frame {
                    rows: Vec::new(),
                    cursor: None,
                },
                caption: "one".to_owned(),
                bg: Rgba::BLACK,
                hold: 1,
            },
            Shot {
                frame: Frame {
                    rows: Vec::new(),
                    cursor: None,
                },
                caption: "two".to_owned(),
                bg: Rgba::BLACK,
                hold: 3,
            },
        ];
        let out = svg(&shots, 4, 1);
        assert!(out.contains("@keyframes s0 { 0% { opacity: 1 } 25.000% { opacity: 0 } }"));
        assert!(out.contains("25.000% { opacity: 1 } 100.000% { opacity: 0 }"));
        // Two frames, four slots, so the loop is four slots long.
        assert!(out.contains(&format!("{:.2}s step-end infinite", 4.0 * SLOT)));
    }

    #[test]
    fn checking_a_directory_with_no_demonstrations_fails() {
        // `--check` has to fail on a tree that never committed them, or the CI
        // step passes while the documentation shows nothing.
        let empty = std::env::temp_dir().join("deco-docs-check-empty");
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(empty.join("docs/img")).unwrap();
        let error = run(&empty, true).expect_err("an empty directory is not up to date");
        assert!(error.to_string().contains("out of date"), "{error}");
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn markup_in_the_document_is_escaped() {
        assert_eq!(escape("a<b>&\"c\""), "a&lt;b&gt;&amp;&quot;c&quot;");
    }

    #[test]
    fn a_frame_draws_its_text_stretched_to_the_cell_grid() {
        let shots = vec![Shot {
            frame: Frame {
                rows: vec![deco_tui::Row {
                    spans: vec![deco_tui::Span {
                        text: "ab".to_owned(),
                        fg: Rgba::WHITE,
                        bg: Rgba::BLACK,
                    }],
                }],
                cursor: None,
            },
            caption: String::new(),
            bg: Rgba::BLACK,
            hold: 1,
        }];
        let out = svg(&shots, 2, 1);
        assert!(out.contains("textLength=\"17.0\""), "{out}");
        assert!(out.contains("lengthAdjust=\"spacingAndGlyphs\""));
        // The background matched the page, so no rectangle was drawn for it.
        assert!(!out.contains("<rect x=\"12.0\" y=\"12.0\""), "{out}");
    }

    #[test]
    fn a_span_that_differs_from_the_background_gets_a_rectangle() {
        let shots = vec![Shot {
            frame: Frame {
                rows: vec![deco_tui::Row {
                    spans: vec![deco_tui::Span {
                        text: "ab".to_owned(),
                        fg: Rgba::WHITE,
                        bg: Rgba::rgb(1, 2, 3),
                    }],
                }],
                cursor: None,
            },
            caption: String::new(),
            bg: Rgba::BLACK,
            hold: 1,
        }];
        let out = svg(&shots, 2, 1);
        assert!(out.contains("fill=\"#010203\""), "{out}");
    }

    #[test]
    fn blank_runs_are_not_drawn_as_text() {
        let shots = vec![Shot {
            frame: Frame {
                rows: vec![deco_tui::Row {
                    spans: vec![deco_tui::Span {
                        text: "    ".to_owned(),
                        fg: Rgba::WHITE,
                        bg: Rgba::BLACK,
                    }],
                }],
                cursor: None,
            },
            caption: String::new(),
            bg: Rgba::BLACK,
            hold: 1,
        }];
        let out = svg(&shots, 4, 1);
        assert!(!out.contains("<text x="), "{out}");
    }

    #[test]
    fn the_caret_is_drawn_where_the_frame_put_it() {
        let shots = vec![Shot {
            frame: Frame {
                rows: Vec::new(),
                cursor: Some((2, 1)),
            },
            caption: String::new(),
            bg: Rgba::BLACK,
            hold: 1,
        }];
        let out = svg(&shots, 4, 2);
        let x = PAD + 2.0 * CELL;
        let y = PAD + LINE;
        assert!(out.contains(&format!("x=\"{x:.1}\" y=\"{y:.1}\"")), "{out}");
    }

    #[test]
    fn the_find_demonstration_shows_the_bar_it_is_demonstrating() {
        // The scenarios are the part most likely to rot: a keybinding changes and
        // the animation quietly shows nothing happening. Assert the feature
        // actually appears.
        assert!(find().contains("Find:"));
        assert!(replace().contains("With:"));
    }

    #[test]
    fn the_hover_demonstration_shows_the_hover_text() {
        assert!(hover().contains("let total: i32"));
    }

    #[test]
    fn the_completion_demonstration_shows_the_list() {
        assert!(completion().contains("println!"));
    }

    #[test]
    fn the_diagnostics_demonstration_shows_the_tally_and_a_message() {
        let svg = diagnostics();
        assert!(
            svg.contains("cannot find value"),
            "the message reaches the bar"
        );
        // `×1 ⚠1` in the status bar, escaped or not.
        assert!(svg.contains('×') && svg.contains('⚠'), "the tally is drawn");
    }
}
