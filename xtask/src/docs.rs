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
            name: "completion",
            build: completion,
        },
        Demo {
            name: "command-palette",
            build: command_palette,
        },
        Demo {
            name: "go-to-line",
            build: go_to_line,
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
}

/// One captured moment: what the screen showed, and what produced it.
struct Shot {
    frame: Frame,
    caption: String,
    /// How many time slots this frame occupies. A frame worth reading gets
    /// several; an intermediate keystroke gets one.
    hold: u32,
}

impl Take {
    fn new(file: &str, text: &str) -> Self {
        // The Linux keymap rather than the host's: a demonstration that pressed
        // `ctrl+d` would be pressing an unbound key when generated on a Mac, and
        // the committed file would differ by who ran the command.
        let mut session = Session::new(
            deco_config::Settings::with_defaults(),
            None,
            Platform::Linux,
        );
        session.open(PathBuf::from(format!("/demo/{file}")), text);
        session.resize(COLUMNS, ROWS - 1);
        Self {
            session,
            shots: Vec::new(),
        }
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
            self.session
                .handle_chord(chord, self.shots.len() as u64 * 10_000);
            self.resize_for_chrome();
            self.capture(key, 1);
        }
        self
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
            let key = if c == ' ' {
                "space".to_owned()
            } else {
                c.to_string()
            };
            let chord = Chord::parse(&key).expect("a printable character is a chord");
            self.session
                .handle_chord(chord, self.shots.len() as u64 * 10_000);
        }
        self.resize_for_chrome();
        self.capture(&format!("type “{text}”"), 3);
        self
    }

    /// Types one character without capturing a frame.
    fn type_char(&mut self, c: char) {
        let chord = Chord::parse(&c.to_string()).expect("a printable character is a chord");
        self.session
            .handle_chord(chord, self.shots.len() as u64 * 10_000);
    }

    /// Keeps the text area's height right when a bar opens or closes.
    ///
    /// The terminal frontend does this on every frame; a demonstration that
    /// skipped it would show the find bar covering the last line of the file.
    fn resize_for_chrome(&mut self) {
        let chrome = render::chrome_height(&self.session);
        self.session.resize(COLUMNS, ROWS.saturating_sub(chrome));
    }

    /// Captures the current screen.
    fn capture(&mut self, caption: &str, hold: u32) -> &mut Self {
        let frame = render::render(&self.session, COLUMNS, ROWS);
        self.shots.push(Shot {
            frame,
            caption: caption.to_owned(),
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
            hold,
        });
        self
    }

    fn finish(&self) -> String {
        let bg = self
            .session
            .theme
            .color("editor.background")
            .unwrap_or(Rgba::BLACK);
        svg(&self.shots, COLUMNS, ROWS, bg)
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
        .press(&["shift+tab"])
        .type_text("total")
        .press_and_hold(&["tab"], 3)
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
fn svg(shots: &[Shot], columns: usize, rows: usize, bg: Rgba) -> String {
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

    out.push_str(&format!(
        "<rect width=\"{width:.0}\" height=\"{height:.0}\" rx=\"6\" fill=\"{}\"/>\n",
        hex(bg)
    ));

    for (index, shot) in shots.iter().enumerate() {
        out.push_str(&format!("<g class=\"s s{index}\">\n"));
        frame_body(&mut out, &shot.frame, bg);
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
                hold: 1,
            },
            Shot {
                frame: Frame {
                    rows: Vec::new(),
                    cursor: None,
                },
                caption: "two".to_owned(),
                hold: 3,
            },
        ];
        let out = svg(&shots, 4, 1, Rgba::BLACK);
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
            hold: 1,
        }];
        let out = svg(&shots, 2, 1, Rgba::BLACK);
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
            hold: 1,
        }];
        let out = svg(&shots, 2, 1, Rgba::BLACK);
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
            hold: 1,
        }];
        let out = svg(&shots, 4, 1, Rgba::BLACK);
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
            hold: 1,
        }];
        let out = svg(&shots, 4, 2, Rgba::BLACK);
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
