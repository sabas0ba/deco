'use strict';

/**
 * Entry point for the deco extension host.
 *
 * Started by deco as:
 *
 *     node --permission --allow-fs-read=<host> --allow-fs-read=<extension> \
 *          --disallow-code-generation-from-strings \
 *          --max-old-space-size=<mb> bootstrap.js
 *
 * with an environment that contains only `DECO_EXTENSION_ID` and
 * `DECO_HOST_PROTOCOL`. See crates/deco-ext/src/host.rs.
 *
 * Order matters: the sandbox is installed before any extension code can run, and
 * the `vscode` shim is registered before activation. Loading an extension first
 * would give it an unguarded `require`.
 */

const path = require('node:path');
const { install } = require('./sandbox');
const { RpcConnection, PROTOCOL_VERSION } = require('./rpc');
const { createApi } = require('./vscode');

function fail(message) {
  process.stderr.write(`deco extension host: ${message}\n`);
  process.exit(1);
}

function main() {
  const extensionId = process.env.DECO_EXTENSION_ID;
  if (!extensionId) {
    fail('DECO_EXTENSION_ID is not set');
  }
  if (process.env.DECO_HOST_PROTOCOL !== PROTOCOL_VERSION) {
    // Reject incompatible protocol versions before accepting any requests.
    fail(
      `protocol mismatch: deco speaks ${process.env.DECO_HOST_PROTOCOL}, ` +
        `this host speaks ${PROTOCOL_VERSION}`,
    );
  }

  const rpc = new RpcConnection(process.stdin, process.stdout);
  const api = createApi(rpc, { extensionId });

  // Capture the loader before the sandbox replaces it, then register `vscode`
  // so that `require('vscode')` in an extension resolves to the shim instead of
  // failing.
  const Module = require('node:module');
  const sandbox = install({ moduleRequire: require, globals: globalThis });
  const guardedLoad = Module._load;
  Module._load = function decoResolveVscode(specifier, parent, isMain) {
    if (specifier === 'vscode') {
      return api;
    }
    return guardedLoad.call(this, specifier, parent, isMain);
  };

  let activated = false;

  rpc.onRequest('$/activate', async ({ extensionPath, main }) => {
    if (activated) return { alreadyActive: true };
    activated = true;

    // `--allow-fs-read` already limits which paths can be loaded. This check
    // gives a clear error instead of a permission failure.
    const entry = path.resolve(extensionPath, main);
    if (!entry.startsWith(path.resolve(extensionPath))) {
      throw new Error(`entry point ${main} escapes the extension directory`);
    }

    const extension = guardedLoad.call(Module, entry, null, false);
    const context = {
      extensionId,
      extensionPath,
      subscriptions: [],
      // Storage paths are supplied by deco; the extension cannot pick its own.
      globalStorageUri: null,
      workspaceState: new Map(),
      globalState: new Map(),
    };

    if (typeof extension.activate === 'function') {
      await extension.activate(context);
    }
    rpc.notify('$/activated', { extensionId });
    return { activated: true };
  });

  rpc.onRequest('$/deactivate', async () => {
    const entry = require.cache && Object.values(require.cache).find((m) => m?.exports?.deactivate);
    if (entry && typeof entry.exports.deactivate === 'function') {
      await entry.exports.deactivate();
    }
    return { deactivated: true };
  });

  rpc.onNotification('$/shutdown', () => {
    sandbox.restore();
    process.exit(0);
  });

  // `$/shutdown` is the normal way to stop. This handles deco exiting without
  // it. Otherwise an extension holding a timer would keep this process, and in
  // a container the container, running after the editor has exited.
  rpc.onClosed(() => {
    sandbox.restore();
    process.exit(0);
  });

  // Report uncaught errors from extension code to deco instead of letting them
  // terminate the host without a message.
  process.on('uncaughtException', (error) => {
    rpc.notify('log.append', {
      level: 'error',
      message: `uncaught exception in ${extensionId}: ${error && error.stack}`,
    });
  });
  process.on('unhandledRejection', (reason) => {
    rpc.notify('log.append', {
      level: 'error',
      message: `unhandled rejection in ${extensionId}: ${reason}`,
    });
  });

  rpc.notify('$/ready', { extensionId, protocol: PROTOCOL_VERSION });
}

if (require.main === module) {
  main();
}

module.exports = { main };
