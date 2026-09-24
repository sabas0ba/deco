'use strict';

/**
 * Line-delimited JSON RPC over a pair of streams.
 *
 * The host has no network and no filesystem access, so this connection to deco
 * is its only channel to the outside. Every message is one line of JSON with an
 * explicit `type` tag. crates/deco-ext/src/protocol.rs defines the same format
 * and must stay consistent with this file.
 */

const PROTOCOL_VERSION = '1';

class RpcConnection {
  /**
   * @param {NodeJS.ReadableStream} input
   * @param {NodeJS.WritableStream} output
   */
  constructor(input, output) {
    this._output = output;
    this._nextId = 1;
    this._pending = new Map();
    this._handlers = new Map();
    this._notificationHandlers = new Map();
    this._buffer = '';

    this._closedHandler = null;

    input.setEncoding('utf8');
    input.on('data', (chunk) => this._onData(chunk));
    // The input stream closing is the only signal that deco has exited. Without
    // handling it, the host outlives the editor. In a container, `--rm` then never
    // takes effect and the container keeps running with the access it was
    // granted.
    input.on('end', () => this._onClosed());
    input.on('close', () => this._onClosed());
  }

  _onClosed() {
    const handler = this._closedHandler;
    this._closedHandler = null;
    if (handler) handler();
  }

  _onData(chunk) {
    this._buffer += chunk;
    let newline;
    while ((newline = this._buffer.indexOf('\n')) !== -1) {
      const line = this._buffer.slice(0, newline);
      this._buffer = this._buffer.slice(newline + 1);
      if (line.trim() === '') continue;
      let message;
      try {
        message = JSON.parse(line);
      } catch {
        // deco also skips unparseable lines. Dropping the connection would let
        // any stray write stop the host.
        continue;
      }
      this._dispatch(message);
    }
  }

  _dispatch(message) {
    switch (message.type) {
      case 'response': {
        const pending = this._pending.get(message.id);
        if (!pending) return;
        this._pending.delete(message.id);
        if (message.error) {
          const error = new Error(message.error.message);
          error.code = message.error.code;
          pending.reject(error);
        } else {
          pending.resolve(message.result);
        }
        return;
      }
      case 'request': {
        const handler = this._handlers.get(message.method);
        if (!handler) {
          this._write({
            type: 'response',
            id: message.id,
            error: { code: 'methodNotFound', message: `no handler for ${message.method}` },
          });
          return;
        }
        Promise.resolve()
          .then(() => handler(message.params))
          .then(
            (result) =>
              this._write({ type: 'response', id: message.id, result: result ?? null }),
            (error) =>
              this._write({
                type: 'response',
                id: message.id,
                error: { code: 'operationFailed', message: String(error && error.message) },
              }),
          );
        return;
      }
      case 'notification': {
        const handler = this._notificationHandlers.get(message.method);
        if (handler) handler(message.params);
        return;
      }
      default:
        // An unknown message type is ignored.
        return;
    }
  }

  _write(message) {
    this._output.write(`${JSON.stringify(message)}\n`);
  }

  /** Calls a method on deco and resolves with its result. */
  request(method, params = {}) {
    const id = this._nextId++;
    return new Promise((resolve, reject) => {
      this._pending.set(id, { resolve, reject });
      this._write({ type: 'request', id, method, params });
    });
  }

  /** Sends a one-way message. */
  notify(method, params = {}) {
    this._write({ type: 'notification', method, params });
  }

  /** Registers a handler for requests deco makes of the host. */
  onRequest(method, handler) {
    this._handlers.set(method, handler);
  }

  /** Registers a handler for notifications deco sends. */
  onNotification(method, handler) {
    this._notificationHandlers.set(method, handler);
  }

  /**
   * Registers what to do when deco closes the connection. Called at most once,
   * whichever of `end` and `close` arrives first.
   */
  onClosed(handler) {
    this._closedHandler = handler;
  }

  /** Number of requests awaiting a reply, exposed for the host's own limits. */
  get pendingCount() {
    return this._pending.size;
  }
}

module.exports = { RpcConnection, PROTOCOL_VERSION };
