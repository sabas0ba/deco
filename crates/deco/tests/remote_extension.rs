//! An extension reading files in a remote session.
//!
//! The capability broker has always decided *whether* an extension may read a
//! path. What it never decided is *where* the read happens, because until now no
//! read happened at all — `fs.readFile` was answered "deco does not implement
//! this yet". Once it is implemented, a remote session has a wrong answer
//! available to it: reading whatever is at that path on this machine, which is
//! not the checkout being edited and which nothing in the reply would reveal.
//!
//! So these tests are about the *where*. The far end is a real
//! `deco --server`, the extension is a real Node process, and the interesting
//! assertion is the one where the two disagree: a path this machine can read and
//! the server refuses.
//!
//! `#[ignore]`d, like every other test that needs Node — `cargo xtask host-test`
//! runs them in the CI job that has one.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deco_config::{Scope, Settings};
use deco_editor::Session;
use deco_remote::transport::Command;
use deco_tui::extensions::{discover, rows, Files, Hosts};

/// The repository root, from this crate's own location.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/deco is two levels down")
        .to_path_buf()
}

/// Tells `Hosts` where deco's own host code is: a test binary is not in the
/// layout an installed deco has, so the search would come up empty.
fn point_at_the_host() {
    std::env::set_var(
        "DECO_HOST_BOOTSTRAP",
        repo_root().join("extension-host/src/bootstrap.js"),
    );
}

/// A workspace on the "remote", plus a file outside it on this machine.
///
/// Both are real directories on this disk, because the far end of these tests is
/// this machine. What makes them mean different things is the server: it serves
/// one of them and refuses everything outside it.
struct World {
    root: PathBuf,
    workspace: PathBuf,
    outside: PathBuf,
}

fn world(name: &str) -> World {
    let root = std::env::temp_dir().join(format!("deco-remote-ext-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let workspace = root.join("work");
    std::fs::create_dir_all(&workspace).expect("a workspace");
    std::fs::write(workspace.join("notes.txt"), "from the far end\n").expect("a file");
    let outside = root.join("private.txt");
    std::fs::write(&outside, "not for an extension\n").expect("a file");
    World {
        root,
        workspace,
        outside,
    }
}

/// An extension that reads one path and returns what it got.
fn install(world: &World, path: &Path) -> PathBuf {
    let directory = world.root.join("extensions/acme.reader-1.0.0");
    std::fs::create_dir_all(&directory).expect("a directory");
    std::fs::write(
        directory.join("package.json"),
        format!(
            r#"{{
  "name": "reader",
  "publisher": "acme",
  "displayName": "Acme Reader",
  "main": "./extension.js",
  "contributes": {{ "commands": [{{ "command": "acme.read", "title": "Read" }}] }},
  "deco": {{
    "capabilities": [
      {{ "capability": "readFile", "scope": {{ "kind": "workspace" }} }},
      {{ "capability": "readFile", "scope": {{ "kind": "subtree", "path": {path:?} }} }}
    ]
  }}
}}"#
        ),
    )
    .expect("a manifest");
    // Both scopes are declared so that the interesting failure is the server's
    // refusal rather than the broker's: what these tests are about is where the
    // read goes, and a capability that was never declared would stop it earlier.
    std::fs::write(
        directory.join("extension.js"),
        format!(
            r#"'use strict';
const vscode = require('vscode');
function activate(context) {{
  context.subscriptions.push(
    vscode.commands.registerCommand('acme.read', async () => {{
      try {{
        const text = await vscode.workspace.fs.readFile({path:?});
        return `read ${{JSON.stringify(text)}}`;
      }} catch (error) {{
        return `refused: ${{error && error.message}}`;
      }}
    }}),
  );
}}
module.exports = {{ activate }};
"#
        ),
    )
    .expect("an extension");
    world.root.join("extensions")
}

/// A session that runs extensions as a plain process rather than in a container.
fn session() -> Session {
    let mut settings = Settings::with_defaults();
    settings.set(
        Scope::User,
        deco_ext::sandbox::SANDBOX_KEY,
        serde_json::json!("process"),
    );
    // `allow` rather than the default `prompt`: there is nowhere to ask for
    // consent yet, so a prompting session refuses every declared capability and
    // these tests would be asserting about that instead of about where a read
    // goes. A person wanting extensions to read files today makes the same
    // choice, and it is the deliberate downgrade the setting says it is.
    settings.set(
        Scope::User,
        deco_ext::capability::DEFAULT_POLICY_KEY,
        serde_json::json!("allow"),
    );
    Session::new(settings, None, deco_keymap::binding::Platform::Linux)
}

/// A client to a real server serving `workspace`.
fn serve(workspace: &Path) -> deco_remote::Client {
    let mut client = deco_remote::Client::start(&Command {
        program: env!("CARGO_BIN_EXE_deco").to_owned(),
        args: vec![
            "--server".to_owned(),
            "--stdio".to_owned(),
            "--workspace".to_owned(),
            workspace.display().to_string(),
        ],
    })
    .expect("the server should start");
    client.handshake().expect("the server should answer");
    client
}

/// Runs `acme.read` in a session whose files are on `files`, and returns the
/// status line the extension produced.
fn read_through(world: &World, path: &Path, files: &mut Files<'_>) -> String {
    let extensions = install(world, path);
    point_at_the_host();
    let mut session = session();
    let catalogue = discover(std::slice::from_ref(&extensions));
    session.frontend_commands.extend(rows(&catalogue));
    let mut hosts = Hosts::rooted(catalogue, vec![world.workspace.clone()]);

    assert!(
        hosts.run_command(&mut session, "acme.read"),
        "the frontend should own the extension's command"
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        hosts.poll(&mut session, files);
        if session
            .status
            .as_deref()
            .is_some_and(|said| said.contains("read ") || said.contains("refused"))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let said = session.status.clone().unwrap_or_else(|| {
        panic!(
            "the extension said nothing; log:\n{}",
            hosts.log().collect::<Vec<_>>().join("\n")
        )
    });
    hosts.shutdown();
    said
}

/// An extension that stats `file` and lists `directory`, and reports both.
fn inspector(world: &World, file: &Path, directory: &Path) -> PathBuf {
    // `installed` rather than `directory`: the parameter above is what the
    // extension will *list*, and a local of the same name silently shadowed it —
    // the generated extension asked about its own install directory and the
    // refusal that produced looked like a broker bug.
    let installed = world.root.join("inspector/acme.inspector-1.0.0");
    std::fs::create_dir_all(&installed).expect("a directory");
    std::fs::write(
        installed.join("package.json"),
        r#"{
  "name": "inspector",
  "publisher": "acme",
  "displayName": "Acme Inspector",
  "main": "./extension.js",
  "contributes": { "commands": [{ "command": "acme.inspect", "title": "Inspect" }] },
  "deco": {
    "capabilities": [
      { "capability": "readFile", "scope": { "kind": "workspace" } }
    ]
  }
}"#,
    )
    .expect("a manifest");
    std::fs::write(
        installed.join("extension.js"),
        format!(
            r#"'use strict';
const vscode = require('vscode');
function activate(context) {{
  context.subscriptions.push(
    vscode.commands.registerCommand('acme.inspect', async () => {{
      try {{
        const stat = await vscode.workspace.fs.stat({file:?});
        const entries = await vscode.workspace.fs.readDirectory({directory:?});
        const names = entries.map(([name, kind]) => `${{name}}:${{kind}}`).join(',');
        return `type=${{stat.type}} size=${{stat.size}} entries=${{names}}`;
      }} catch (error) {{
        return `refused: ${{error && error.message}}`;
      }}
    }}),
  );
}}
module.exports = {{ activate }};
"#,
            file = file.display().to_string(),
            directory = directory.display().to_string()
        ),
    )
    .expect("an extension");
    world.root.join("inspector")
}

/// An extension that creates, moves and deletes under the workspace.
fn editor_extension(world: &World) -> PathBuf {
    let installed = world.root.join("writer/acme.writer-1.0.0");
    std::fs::create_dir_all(&installed).expect("a directory");
    std::fs::write(
        installed.join("package.json"),
        r#"{
  "name": "writer",
  "publisher": "acme",
  "displayName": "Acme Writer",
  "main": "./extension.js",
  "contributes": { "commands": [{ "command": "acme.write", "title": "Write" }] },
  "deco": {
    "capabilities": [
      { "capability": "writeFile", "scope": { "kind": "workspace" } }
    ]
  }
}"#,
    )
    .expect("a manifest");
    std::fs::write(
        installed.join("extension.js"),
        format!(
            r#"'use strict';
const vscode = require('vscode');
function activate(context) {{
  const at = (name) => {workspace:?} + '/' + name;
  context.subscriptions.push(
    vscode.commands.registerCommand('acme.write', async () => {{
      try {{
        await vscode.workspace.fs.createDirectory(at('made/deeper'));
        await vscode.workspace.fs.writeFile(at('made/deeper/one.txt'), 'written by an extension');
        await vscode.workspace.fs.rename(at('made/deeper/one.txt'), at('made/two.txt'));
        await vscode.workspace.fs.copy(at('made/two.txt'), at('made/three.txt'));
        await vscode.workspace.fs.delete(at('made/three.txt'));
        return 'done';
      }} catch (error) {{
        return `refused: ${{error && error.message}}`;
      }}
    }}),
  );
}}
module.exports = {{ activate }};
"#,
            workspace = world.workspace.display().to_string()
        ),
    )
    .expect("an extension");
    world.root.join("writer")
}

#[test]
#[ignore = "needs Node; run by `cargo xtask host-test`"]
fn an_extension_creates_moves_and_deletes_on_the_far_end() {
    // The write side, through a real host and a real server. What makes this
    // checkable is that the assertions are about the *server's* workspace on
    // disk: nothing here writes to it except through the connection.
    let world = world("writes");
    let extensions = editor_extension(&world);
    point_at_the_host();

    let mut client = serve(&world.workspace);
    let mut session = session();
    let catalogue = discover(std::slice::from_ref(&extensions));
    session.frontend_commands.extend(rows(&catalogue));
    let mut hosts = Hosts::rooted(catalogue, vec![world.workspace.clone()]);
    assert!(hosts.run_command(&mut session, "acme.write"));

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        hosts.poll(&mut session, &mut Files::Remote(&mut client));
        if session
            .status
            .as_deref()
            .is_some_and(|said| said.contains("done") || said.contains("refused"))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let said = session.status.clone().unwrap_or_default();
    assert!(
        said.contains("done"),
        "{said}\nlog:\n{}",
        hosts.log().collect::<Vec<_>>().join("\n")
    );
    hosts.shutdown();

    // Created, then renamed, then copied, then the copy deleted — all of it on
    // the machine the server is serving.
    assert!(world.workspace.join("made/deeper").is_dir());
    assert!(!world.workspace.join("made/deeper/one.txt").exists());
    assert_eq!(
        std::fs::read_to_string(world.workspace.join("made/two.txt")).expect("the moved file"),
        "written by an extension"
    );
    assert!(!world.workspace.join("made/three.txt").exists());

    let _ = std::fs::remove_dir_all(&world.root);
}

#[test]
#[ignore = "needs Node; run by `cargo xtask host-test`"]
fn an_extension_stats_and_lists_the_far_end_rather_than_this_machine() {
    // The read side of the filesystem API, over the connection. The list is what
    // makes this checkable: the far end's workspace holds `notes.txt`, and the
    // directory this machine would list at the same path holds the same thing —
    // so the assertion that matters is the one below, where the two disagree.
    let world = world("inspect");
    std::fs::write(world.workspace.join("notes.txt"), "from the far end\n").expect("a file");
    let extensions = inspector(
        &world,
        &world.workspace.join("notes.txt"),
        &world.workspace.clone(),
    );
    point_at_the_host();

    let mut client = serve(&world.workspace);
    let mut session = session();
    let catalogue = discover(std::slice::from_ref(&extensions));
    session.frontend_commands.extend(rows(&catalogue));
    let mut hosts = Hosts::rooted(catalogue, vec![world.workspace.clone()]);
    assert!(hosts.run_command(&mut session, "acme.inspect"));

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        hosts.poll(&mut session, &mut Files::Remote(&mut client));
        if session
            .status
            .as_deref()
            .is_some_and(|said| said.contains("type=") || said.contains("refused"))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let said = session.status.clone().unwrap_or_default();
    // VS Code's numbering, unchanged on the way through: 1 is a file.
    assert!(
        said.contains("type=1"),
        "{said}\nlog:\n{}",
        hosts.log().collect::<Vec<_>>().join("\n")
    );
    assert!(said.contains("size=17"), "{said}");
    assert!(said.contains("notes.txt:1"), "{said}");
    hosts.shutdown();

    let _ = std::fs::remove_dir_all(&world.root);
}

#[test]
#[ignore = "needs Node; run by `cargo xtask host-test`"]
fn a_stat_in_a_remote_session_goes_through_the_server_and_not_around_it() {
    // The same disagreement the read test uses, for the other half of the API.
    // `private.txt` sits outside the directory the server serves, and the
    // session's workspace root is the whole world — so the broker allows it and
    // only the server's own rule can stop it.
    let world = world("stat-around");
    let extensions = inspector(&world, &world.outside.clone(), &world.root.clone());
    point_at_the_host();

    let inspect = |files: &mut Files<'_>| -> String {
        let mut session = session();
        let catalogue = discover(std::slice::from_ref(&extensions));
        session.frontend_commands.extend(rows(&catalogue));
        let mut hosts = Hosts::rooted(catalogue, vec![world.root.clone()]);
        assert!(hosts.run_command(&mut session, "acme.inspect"));
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            hosts.poll(&mut session, files);
            if session
                .status
                .as_deref()
                .is_some_and(|said| said.contains("type=") || said.contains("refused"))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let said = session.status.clone().unwrap_or_default();
        hosts.shutdown();
        said
    };

    // Served here, it works.
    let locally = inspect(&mut Files::Here);
    assert!(locally.contains("type=1"), "{locally}");

    // Served through the connection, the server refuses it by name — which is
    // what proves the stat crossed the connection rather than going around it.
    let mut client = serve(&world.workspace);
    let remotely = inspect(&mut Files::Remote(&mut client));
    assert!(
        !remotely.contains("type=1"),
        "a remote session must not stat around the server: {remotely}"
    );
    assert!(
        remotely.contains("refused") && remotely.contains("outside the workspace"),
        "and it should say why: {remotely}"
    );

    let _ = std::fs::remove_dir_all(&world.root);
}

#[test]
#[ignore = "needs Node; run by `cargo xtask host-test`"]
fn an_extension_reads_the_far_ends_file_through_the_connection() {
    let world = world("reads");
    let mut client = serve(&world.workspace);
    let wanted = world.workspace.join("notes.txt");

    let said = read_through(&world, &wanted, &mut Files::Remote(&mut client));
    assert!(said.contains("from the far end"), "{said}");

    let _ = std::fs::remove_dir_all(&world.root);
}

#[test]
#[ignore = "needs Node; run by `cargo xtask host-test`"]
fn a_remote_session_reads_through_the_server_and_not_around_it() {
    // The assertion this file exists for. `private.txt` is readable on this
    // machine and outside the directory the server serves, so:
    //
    //   - served locally, the extension gets its contents;
    //   - served through the connection, the server refuses it by name.
    //
    // Which means a passing local case and a refused remote case together prove
    // the read really crossed the connection. Answering from this machine's disk
    // in a remote session would be invisible in the reply and would hand an
    // extension a file the session is not editing.
    let world = world("not-around");

    let locally = read_through(&world, &world.outside.clone(), &mut Files::Here);
    assert!(
        locally.contains("not for an extension"),
        "the local case should read it: {locally}"
    );

    let mut client = serve(&world.workspace);
    let remotely = read_through(
        &world,
        &world.outside.clone(),
        &mut Files::Remote(&mut client),
    );
    assert!(
        !remotely.contains("not for an extension"),
        "a remote session must not read around the server: {remotely}"
    );
    assert!(
        remotely.contains("refused") && remotely.contains("outside the workspace"),
        "and it should say why: {remotely}"
    );

    let _ = std::fs::remove_dir_all(&world.root);
}
