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
 * with an environment built from nothing but `DECO_EXTENSION_ID` and
 * `DECO_HOST_PROTOCOL`. See crates/deco-ext/src/host.rs.
 *
 * Order matters here: the sandbox is installed before any extension code can be
 * reached, and the `vscode` shim is registered before activation. Loading an
 * extension first would hand it an unguarded `require`.
 */

const path = require('path');
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
    // A mismatch means deco and this script came from different builds. Failing
    // here is far better than half-speaking an older protocol.
    fail(
      `protocol mismatch: deco speaks ${process.env.DECO_HOST_PROTOCOL}, ` +
        `this host speaks ${PROTOCOL_VERSION}`,
    );
  }

  const rpc = new RpcConnection(process.stdin, process.stdout);
  const api = createApi(rpc, { extensionId });

  // Capture the loader before the sandbox replaces it, then register `vscode`
  // so that extension `require('vscode')` resolves to the shim rather than
  // failing to resolve at all.
  const Module = require('module');
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

    // `--allow-fs-read` already limits which paths can be loaded at all; this
    // check makes the failure legible rather than a permission trap.
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

  // An uncaught throw in extension code must not take the host down silently.
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
