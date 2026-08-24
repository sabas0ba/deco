//! Finding your way around: quick open, go to line, find, replace, and search
//! in files.
//!
//! Every one of these walks a real directory or a real document and then shows a
//! list. The list is on the screen rather than in a struct, so that is where
//! these scenarios look.

use deco_e2e::Scenario;

/// A workspace with enough in it for a picker to have to choose.
fn workspace(name: &str) -> Scenario {
    Scenario::new(name)
        .file("src/main.rs", "fn main() {\n    greet();\n}\n")
        .file(
            "src/greet.rs",
            "pub fn greet() {\n    println!(\"hi\");\n}\n",
        )
        .file("README.md", "# project\n\nIt greets.\n")
        .file("notes/todo.txt", "greet better\n")
}

#[test]
fn quick_open_lists_the_workspace_and_opens_what_is_chosen() {
    let scenario = workspace("quick-open");
    let mut editor = scenario.launch(&["src/main.rs"]);

    editor.press("ctrl+p");
    let screen = editor.screen();
    screen.assert_fits();
    screen.assert_shows("greet.rs");

    editor.type_text("greet");
    editor.press("enter");

    assert!(
        editor.path().is_some_and(|p| p.ends_with("greet.rs")),
        "quick open should have opened greet.rs, not {:?}",
        editor.path()
    );
    editor.screen().assert_shows("println!");
}

#[test]
fn quick_open_can_be_cancelled_and_leaves_the_document_alone() {
    let scenario = workspace("quick-open-cancel");
    let mut editor = scenario.launch(&["src/main.rs"]);

    editor.press("ctrl+p");
    editor.type_text("greet");
    editor.press("escape");

    assert!(editor.path().is_some_and(|p| p.ends_with("main.rs")));
    // And the text that was typed into the picker did not land in the file.
    assert_eq!(editor.text(), "fn main() {\n    greet();\n}\n");
}

#[test]
fn quick_open_does_not_offer_files_the_settings_exclude() {
    // `files.exclude` is how a repository keeps `target/` out of every picker,
    // and a picker that lists 40,000 build artefacts is no picker at all.
    let scenario = workspace("excluded")
        .user_settings(r#"{ "files.exclude": { "**/notes": true } }"#)
        .file("notes/secret.txt", "hidden\n");
    let mut editor = scenario.launch(&["src/main.rs"]);

    editor.press("ctrl+p");
    let screen = editor.screen();
    screen.assert_shows("main.rs");
    screen.assert_lacks("secret.txt");
}

#[test]
fn go_to_line_moves_the_caret_and_says_where_it_is() {
    let text: String = (1..=50).map(|n| format!("line {n}\n")).collect();
    let scenario = Scenario::new("go-to-line").file("a.txt", &text);
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+g");
    editor.type_text("42");
    editor.press("enter");

    let screen = editor.screen();
    screen.assert_status("Ln 42");
    screen.assert_shows("line 42");
}

#[test]
fn go_to_a_line_past_the_end_says_so_instead_of_moving_somewhere_arbitrary() {
    // deco refuses and names the range. VS Code clamps to the last line instead;
    // this pins which of the two deco does, so that a change to it is a decision
    // rather than a surprise.
    let scenario = Scenario::new("go-to-line-past").file("a.txt", "one\ntwo\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+g");
    editor.type_text("9999");
    editor.press("enter");

    let screen = editor.screen();
    screen.assert_fits();
    screen.assert_status("9999");
    screen.assert_status("Ln 1");
}

#[test]
fn find_shows_the_bar_and_moves_between_matches() {
    let scenario = Scenario::new("find").file("a.txt", "alpha\nbeta\nalpha\ngamma\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+f");
    editor.type_text("alpha");

    let screen = editor.screen();
    screen.assert_fits();
    // The bar counts them, which is the whole reason to look at it.
    screen.assert_shows("1 of 2");

    editor.press("enter");
    editor.press("escape");
    editor.screen().assert_status("Ln 3");
}

#[test]
fn find_leaves_the_document_untouched() {
    // Typing into the find bar must not type into the file, which is the failure
    // that makes a find bar terrifying.
    let scenario = Scenario::new("find-safe").file("a.txt", "alpha\nbeta\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+f");
    editor.type_text("beta");
    editor.press("escape");

    assert_eq!(editor.text(), "alpha\nbeta\n");
    assert!(!editor.is_dirty());
}

#[test]
fn replace_all_changes_every_match_and_saves_what_it_changed() {
    let scenario = Scenario::new("replace").file("a.txt", "cat\ndog\ncat\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+h");
    // Nothing was selected, so `ctrl+h` opens with an empty query and the
    // keyboard on it; `tab` moves to the replacement.
    editor.type_text("cat");
    editor.press("tab");
    editor.type_text("bird");
    editor.press("ctrl+alt+enter");
    editor.press("escape");
    editor.press("ctrl+s");

    assert_eq!(editor.on_disk("a.txt"), "bird\ndog\nbird\n");
}

#[test]
fn ctrl_h_focuses_the_query_when_there_is_nothing_to_replace_yet() {
    // `ctrl+h` seeds the query only from a selection, so reaching it with nothing
    // selected and nothing searched for yet leaves nothing to replace. The first
    // thing typed is the word being searched for, and it has to land in the
    // query — the field VS Code focuses in the same situation.
    let scenario = Scenario::new("replace-focus").file("a.txt", "cat\ndog\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+h");
    editor.type_text("cat");

    assert_eq!(
        editor.session().find.query(),
        "cat",
        "typing `cat` after ctrl+h should search for it"
    );
    assert_eq!(
        editor.session().find.replace(),
        "",
        "the replacement is not what the user came here to write first"
    );
    // And the screen shows the query filled in with the replacement row still
    // open and empty, ready for `tab`.
    let screen = editor.screen();
    screen.assert_shows("With:");
    assert!(
        screen.lines().iter().any(|line| line.contains("Find: cat")),
        "the query should hold the typed word{}",
        screen.dump()
    );
}

#[test]
fn ctrl_h_focuses_the_replacement_when_the_query_is_seeded() {
    // The premise the replacement-first focus rests on: with a word selected the
    // query arrives already filled in, so the replacement is the only thing left
    // to type and the keyboard belongs there.
    let scenario = Scenario::new("replace-focus-seeded").file("a.txt", "cat\ndog\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+shift+right");
    editor.press("ctrl+h");
    editor.type_text("bird");

    assert_eq!(
        editor.session().find.query(),
        "cat",
        "seeded from the selection"
    );
    assert_eq!(editor.session().find.replace(), "bird");
}

#[test]
fn a_search_with_no_matches_says_so_rather_than_looking_broken() {
    let scenario = Scenario::new("find-nothing").file("a.txt", "alpha\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+f");
    editor.type_text("zzz");

    let screen = editor.screen();
    screen.assert_fits();
    assert!(
        screen.text().contains("No results") || screen.text().contains('0'),
        "a search with no matches should say so{}",
        screen.dump()
    );
}

#[test]
fn search_in_files_finds_a_line_in_another_file_and_opens_it_there() {
    let scenario = workspace("search-files");
    let mut editor = scenario.launch(&["src/main.rs"]);

    // The project-search prompt opens seeded with the word under the cursor, and
    // typing appends to it rather than replacing it — so a query has to be
    // cleared first. `ctrl+x` is the only key that does that; see the note on
    // `a_seeded_prompt_can_only_be_cleared_by_cutting_it` below.
    editor.press("ctrl+shift+f");
    editor.press("ctrl+x");
    editor.type_text("println");
    editor.press("enter");

    let screen = editor.screen();
    screen.assert_fits();
    screen.assert_shows("greet.rs");

    editor.press("enter");
    assert!(
        editor.path().is_some_and(|p| p.ends_with("greet.rs")),
        "choosing a result should open the file it is in, not {:?}",
        editor.path()
    );
    editor.screen().assert_status("Ln 2");
}

#[test]
fn replace_in_files_changes_every_file_and_one_undo_takes_it_back() {
    let scenario = workspace("replace-files");
    let mut editor = scenario.launch(&["src/main.rs"]);

    editor.press("ctrl+shift+h");
    editor.type_text("greet");
    editor.press("enter");
    // The second prompt: what to put there.
    assert_eq!(
        editor.session().prompt.as_ref().map(|p| p.kind()),
        Some(deco_editor::PromptKind::ReplaceQuery),
        "the query is only the first half"
    );
    editor.type_text("welcome");
    editor.press("enter");

    let status = editor.status().unwrap_or_default().to_owned();
    assert!(
        status.starts_with("Replaced `greet`"),
        "the status should report what was replaced: {status}"
    );

    // The file on screen, and one that was opened to be changed.
    assert_eq!(editor.text(), "fn main() {\n    welcome();\n}\n");
    let unsaved = editor.session().unsaved();
    let greet = unsaved
        .iter()
        .find(|(path, _)| path.ends_with("greet.rs"))
        .map(|(_, text)| text.clone())
        .expect("greet.rs should have been opened and changed");
    assert_eq!(greet, "pub fn welcome() {\n    println!(\"hi\");\n}\n");

    // Nothing reached the disk.
    assert_eq!(
        editor.on_disk("src/greet.rs"),
        "pub fn greet() {\n    println!(\"hi\");\n}\n"
    );

    editor.press("ctrl+z");
    assert_eq!(editor.text(), "fn main() {\n    greet();\n}\n");
    assert_eq!(
        editor
            .session()
            .unsaved()
            .iter()
            .find(|(path, _)| path.ends_with("greet.rs"))
            .map(|(_, text)| text.clone())
            .as_deref(),
        Some("pub fn greet() {\n    println!(\"hi\");\n}\n"),
        "every file came back in the same step"
    );
    editor.screen().assert_fits();
}

#[test]
fn replace_in_files_acts_on_the_buffer_rather_than_the_file_on_disk() {
    // The search reads the disk; this tab has unsaved changes. Replacing against
    // what the search read would edit positions in a document that no longer
    // exists, and then save the result over the real one.
    let scenario = workspace("replace-files-dirty");
    let mut editor = scenario.launch(&["src/main.rs"]);

    // A second `greet` in the open buffer that the file on disk does not have.
    editor.press("ctrl+end");
    editor.type_text("// greet again\n");

    editor.press("ctrl+shift+h");
    editor.type_text("greet");
    editor.press("enter");
    editor.type_text("welcome");
    editor.press("enter");

    assert_eq!(
        editor.text(),
        "fn main() {\n    welcome();\n}\n// welcome again\n",
        "the occurrence that only exists in the buffer was replaced too"
    );
}

#[test]
fn an_empty_replacement_takes_every_occurrence_out() {
    let scenario = workspace("replace-files-empty");
    let mut editor = scenario.launch(&["src/main.rs"]);

    editor.press("ctrl+shift+h");
    editor.type_text("greet");
    editor.press("enter");
    // Nothing typed: the replacement is empty on purpose.
    editor.press("enter");

    assert_eq!(editor.text(), "fn main() {\n    ();\n}\n");
}

#[test]
fn replace_in_files_says_when_nothing_matched() {
    let scenario = workspace("replace-files-nothing");
    let mut editor = scenario.launch(&["src/main.rs"]);

    editor.press("ctrl+shift+h");
    editor.type_text("nothinglikethis");
    editor.press("enter");
    editor.type_text("x");
    editor.press("enter");

    assert_eq!(
        editor.status(),
        Some("no matches for `nothinglikethis`"),
        "and nothing should have been opened or changed"
    );
    assert!(!editor.is_dirty());
}

#[test]
fn search_in_files_says_when_nothing_matched() {
    let scenario = workspace("search-files-nothing");
    let mut editor = scenario.launch(&["src/main.rs"]);

    editor.press("ctrl+shift+f");
    editor.press("ctrl+x");
    editor.type_text("nothinglikethis");
    editor.press("enter");

    let screen = editor.screen();
    screen.assert_fits();
    assert!(
        !screen.status_line().is_empty() || screen.text().contains("No"),
        "a search that found nothing should say so{}",
        screen.dump()
    );
}

#[test]
fn the_command_palette_runs_a_command_that_has_no_key_bound_to_it() {
    let scenario = Scenario::new("palette").file("a.txt", "one\ntwo\nthree\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+shift+p");
    editor.screen().assert_fits();
    editor.type_text("Select All");
    editor.press("enter");

    editor.type_text("x");
    assert_eq!(
        editor.text(),
        "x",
        "select all should have replaced the file"
    );
}

#[test]
fn the_palette_finds_a_command_by_its_vs_code_identifier() {
    // Somebody who knows the identifier from `keybindings.json` should be able to
    // type it, which is a promise the palette's ranking makes.
    let scenario = Scenario::new("palette-id").file("a.rs", "let x = 1;\n");
    let mut editor = scenario.launch(&["a.rs"]);

    editor.palette("commentLine");

    assert_eq!(editor.text(), "// let x = 1;\n");
}

#[test]
fn a_palette_query_matching_nothing_says_so_instead_of_running_something_else() {
    // The dangerous failure: the palette closes and *some* command runs.
    let scenario = Scenario::new("palette-miss").file("a.txt", "one\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.palette("qqqqqqq");

    assert_eq!(editor.text(), "one\n");
    let screen = editor.screen();
    assert!(
        screen.status_line().contains("no command"),
        "the palette should say nothing matched{}",
        screen.dump()
    );
}

#[test]
fn a_search_result_from_a_file_that_has_since_changed_still_opens_safely() {
    // A result carries a position, and the file may have been rewritten between
    // the search and the choosing. Landing past the end of the file is a panic
    // waiting to happen.
    let scenario = workspace("stale-result");
    let mut editor = scenario.launch(&["src/main.rs"]);

    editor.press("ctrl+shift+f");
    editor.press("ctrl+x");
    editor.type_text("println");
    editor.press("enter");
    editor.change_on_disk("src/greet.rs", "x\n");
    editor.press("enter");

    editor.screen().assert_fits();
    assert_eq!(editor.text(), "x\n");
}

#[test]
fn typing_over_a_seeded_prompt_replaces_the_seed() {
    // Save As and Find in Files open with text already in them — the current
    // path, the word under the cursor. VS Code selects that text so the next key
    // replaces it; deco used to leave the caret at the end with no selection, so
    // the next key *appended*: `ctrl+shift+f` on the word `fn` and then typing
    // `println` searched for `fnprintln`, which is in no file anywhere.
    let scenario = Scenario::new("prompt-seed").file("a.txt", "cat\ndog\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+shift+f");
    let seeded = editor.session().prompt.as_ref().expect("a prompt");
    assert_eq!(
        seeded.text(),
        "cat",
        "seeded with the word under the cursor"
    );
    assert!(seeded.text_selected(), "and all of it selected");

    editor.type_text("dog");
    assert_eq!(
        editor.session().prompt.as_ref().expect("a prompt").text(),
        "dog",
        "the first thing typed replaces the seed"
    );
}

#[test]
fn select_all_in_a_prompt_makes_the_next_key_replace_it() {
    // `ctrl+a` used to be swallowed as a no-op, so a field the user wanted to
    // empty could only be cleared by `ctrl+x` — a way out nobody would guess and
    // nothing on screen mentioned.
    let scenario = Scenario::new("prompt-select-all").file("a.txt", "cat\ndog\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+shift+f");
    editor.type_text("bird");
    editor.press("ctrl+a");
    editor.press("backspace");
    assert_eq!(
        editor.session().prompt.as_ref().expect("a prompt").text(),
        "",
        "ctrl+a then backspace should empty the field"
    );
}

#[test]
fn a_seeded_prompt_can_still_be_edited_rather_than_replaced() {
    // The other half of a selection: a path you meant to *edit* survives the
    // moment you move into it, which is what makes seeding save-as worth doing.
    let scenario = Scenario::new("prompt-edit-seed").file("a.txt", "hello\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+shift+s");
    let seeded = editor
        .session()
        .prompt
        .as_ref()
        .expect("a prompt")
        .text()
        .to_owned();
    assert!(seeded.ends_with("a.txt"), "{seeded}");

    // Move into it, then edit from the end: `a.txt` becomes `a.txt.bak`.
    editor.press("end");
    editor.type_text(".bak");
    editor.press("enter");

    assert_eq!(editor.on_disk("a.txt.bak"), "hello\n");
}

#[test]
fn a_selected_seed_is_drawn_as_selected() {
    // Or the difference between replacing and appending is something the user
    // only discovers by losing what they typed.
    let scenario = Scenario::new("prompt-seed-drawn").file("a.txt", "cat\ndog\n");
    let mut editor = scenario.launch(&["a.txt"]);

    editor.press("ctrl+shift+f");
    let screen = editor.screen();
    let row = screen
        .row_of("Search:")
        .unwrap_or_else(|| panic!("the prompt row{}", screen.dump()));
    let line = screen.line(row);
    let seed = line
        .find("cat")
        .unwrap_or_else(|| panic!("the seed on the prompt row{}", screen.dump()));

    let label = screen.colours_at(row, 1).expect("the label's colours");
    let seeded = screen.colours_at(row, seed).expect("the seed's colours");
    assert_ne!(
        seeded,
        label,
        "the selected seed should not look like the label beside it{}",
        screen.dump()
    );
    // And once it is no longer selected, it looks like the rest of the row again.
    editor.press("end");
    let screen = editor.screen();
    assert_eq!(
        screen.colours_at(row, seed),
        Some(label),
        "a collapsed selection should not stay highlighted{}",
        screen.dump()
    );
}

#[test]
fn the_find_bar_seeds_from_a_selection_and_selects_what_it_seeded() {
    let scenario = Scenario::new("find-seed").file("a.txt", "cat\ndog\n");
    let mut editor = scenario.launch(&["a.txt"]);

    // Nothing selected: the bar opens empty, and there is nothing to replace.
    editor.press("ctrl+f");
    assert_eq!(editor.session().find.query(), "");
    assert!(!editor.session().find.text_selected());
    editor.press("escape");

    // A word selected: it is seeded, and typing replaces it rather than
    // appending — the same rule the prompts follow.
    editor.press("ctrl+shift+right");
    editor.press("ctrl+f");
    assert_eq!(editor.session().find.query(), "cat");
    assert!(editor.session().find.text_selected());
    editor.type_text("dog");
    assert_eq!(editor.session().find.query(), "dog");
}
