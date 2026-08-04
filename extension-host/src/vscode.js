'use strict';

/**
 * The `vscode` module extensions import.
 *
 * Every function here is a thin wrapper over an RPC to deco. Nothing in this
 * file touches the filesystem, the network or a child process directly — it
 * cannot, because the sandbox has removed those — so the capability check in
 * deco is unavoidable rather than merely conventional.
 *
 * This is a subset of the VS Code API, and deliberately so: each method added
 * here is a method that must first be given a capability mapping in
 * crates/deco-ext/src/protocol.rs. A method with no mapping is refused by deco,
 * so the two files cannot drift apart silently in the unsafe direction.
 */

/** A zero-based document position, matching `vscode.Position`. */
class Position {
  constructor(line, character) {
    this.line = line;
    this.character = character;
    Object.freeze(this);
  }

  isBefore(other) {
    return this.line < other.line || (this.line === other.line && this.character < other.character);
  }

  translate(lineDelta = 0, characterDelta = 0) {
    return new Position(this.line + lineDelta, this.character + characterDelta);
  }
}

/** A document range, matching `vscode.Range`. */
class Range {
  constructor(start, end) {
    // VS Code normalises reversed ranges rather than rejecting them.
    if (end.isBefore(start)) {
      [start, end] = [end, start];
    }
    this.start = start;
    this.end = end;
    Object.freeze(this);
  }

  get isEmpty() {
    return this.start.line === this.end.line && this.start.character === this.end.character;
  }

  get isSingleLine() {
    return this.start.line === this.end.line;
  }
}

/** A selection, matching `vscode.Selection`. */
class Selection extends Range {
  constructor(anchor, active) {
    super(anchor, active);
    this.anchor = anchor;
    this.active = active;
  }

  get isReversed() {
    return this.active.isBefore(this.anchor);
  }
}

/** A disposable, matching `vscode.Disposable`. */
class Disposable {
  constructor(callOnDispose) {
    this._callOnDispose = callOnDispose;
  }

  dispose() {
    if (this._callOnDispose) {
      this._callOnDispose();
      this._callOnDispose = null;
    }
  }

  static from(...disposables) {
    return new Disposable(() => disposables.forEach((d) => d.dispose()));
  }
}

/** A minimal event emitter, matching `vscode.EventEmitter`. */
class EventEmitter {
  constructor() {
    this._listeners = new Set();
    this.event = (listener) => {
      this._listeners.add(listener);
      return new Disposable(() => this._listeners.delete(listener));
    };
  }

  fire(value) {
    for (const listener of [...this._listeners]) {
      try {
        listener(value);
      } catch {
        // One bad listener must not stop the others; deco logs the throw.
      }
    }
  }

  dispose() {
    this._listeners.clear();
  }
}

/**
 * Builds the `vscode` module for one extension.
 *
 * @param {import('./rpc').RpcConnection} rpc
 * @param {{extensionId: string}} context
 */
function createApi(rpc, context) {
  const commands = new Map();

  rpc.onRequest('$/executeCommand', async ({ command, args }) => {
    const handler = commands.get(command);
    if (!handler) throw new Error(`command ${command} is not registered`);
    return (await handler(...(args ?? []))) ?? null;
  });

  const api = {
    version: '1.0.0-deco',

    Position,
    Range,
    Selection,
    Disposable,
    EventEmitter,

    commands: {
      registerCommand(command, callback) {
        commands.set(command, callback);
        rpc.request('commands.registerCommand', { command });
        return new Disposable(() => {
          commands.delete(command);
        });
      },
      executeCommand(command, ...args) {
        return rpc.request('commands.executeCommand', { command, args });
      },
      getCommands(filterInternal = false) {
        return rpc.request('commands.getCommands', { filterInternal });
      },
    },

    window: {
      showInformationMessage: (message, ...items) =>
        rpc.request('window.showInformationMessage', { message, items }),
      showWarningMessage: (message, ...items) =>
        rpc.request('window.showWarningMessage', { message, items }),
      showErrorMessage: (message, ...items) =>
        rpc.request('window.showErrorMessage', { message, items }),
      showQuickPick: (items, options) => rpc.request('window.showQuickPick', { items, options }),
      showInputBox: (options) => rpc.request('window.showInputBox', { options }),
      setStatusBarMessage: (text, hideAfterTimeout) =>
        rpc.request('window.setStatusBarMessage', { text, hideAfterTimeout }),
      get activeTextEditor() {
        return rpc.request('window.activeTextEditor', {});
      },
    },

    workspace: {
      getConfiguration: (section, scope) =>
        rpc.request('workspace.getConfiguration', { section, scope }),
      workspaceFolders: () => rpc.request('workspace.workspaceFolders', {}),
      textDocuments: () => rpc.request('workspace.textDocuments', {}),
      applyEdit: (edit) => rpc.request('workspace.applyEdit', edit),

      /**
       * The brokered filesystem. Every call names a path, and deco checks it
       * against the extension's declared scopes before touching anything.
       */
      fs: {
        readFile: (path) => rpc.request('fs.readFile', { path }),
        writeFile: (path, content) => rpc.request('fs.writeFile', { path, content }),
        delete: (path, options) => rpc.request('fs.delete', { path, options }),
        rename: (source, target) => rpc.request('fs.rename', { source, target }),
        copy: (source, target) => rpc.request('fs.copy', { source, target }),
        createDirectory: (path) => rpc.request('fs.createDirectory', { path }),
        readDirectory: (path) => rpc.request('fs.readDirectory', { path }),
        stat: (path) => rpc.request('fs.stat', { path }),
      },
    },

    languages: {
      registerProvider: (kind, selector) =>
        rpc.request('languages.registerProvider', { kind, selector }),
      setDiagnostics: (uri, diagnostics) =>
        rpc.request('languages.setDiagnostics', { uri, diagnostics }),
    },

    env: {
      get: (name) => rpc.request('env.get', { name }),
      openExternal: (uri) => rpc.request('env.openExternal', { uri }),
      clipboard: {
        readText: () => rpc.request('env.clipboard.readText', {}),
        writeText: (text) => rpc.request('env.clipboard.writeText', { text }),
      },
    },

    secrets: {
      get: (key) => rpc.request('secrets.get', { key }),
      store: (key, value) => rpc.request('secrets.store', { key, value }),
      delete: (key) => rpc.request('secrets.delete', { key }),
    },

    /** deco-specific replacements for the Node APIs the sandbox removed. */
    deco: {
      extensionId: context.extensionId,
      fetch: (url, init) => rpc.request('net.fetch', { url, init }),
      spawn: (program, args, options) =>
        rpc.request('process.spawn', { program, args, options }),
      log: (message) => rpc.notify('log.append', { message }),
    },
  };

  return api;
}

module.exports = { createApi, Position, Range, Selection, Disposable, EventEmitter };
