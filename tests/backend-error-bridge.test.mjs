import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createWebTransport } from '../src/web/transport.js';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const BRIDGE_PATH = path.join(REPO_ROOT, 'src/tauri/main/bootstrap/backend-error-bridge.js');

async function importFreshBridge() {
    const url = `${pathToFileURL(BRIDGE_PATH).href}?t=${Date.now()}-${Math.random()}`;
    return import(url);
}

function installFakeWindow({ invoke, consumerReady = false }) {
    const dispatched = [];
    let listener = null;
    const calls = [];

    global.CustomEvent = class CustomEvent {
        constructor(type, options = {}) {
            this.type = type;
            this.detail = options.detail;
        }
    };

    global.window = {
        __TAURI__: {
            event: {
                async listen(eventName, callback) {
                    calls.push(['listen', eventName]);
                    listener = callback;
                    return () => {
                        calls.push(['unlisten', eventName]);
                        listener = null;
                    };
                },
            },
            core: {
                async invoke(command) {
                    calls.push(['invoke', command]);
                    return invoke(command);
                },
            },
        },
        __TAURITAVERN_BACKEND_ERROR_CONSUMER_READY__: consumerReady,
        dispatchEvent(event) {
            dispatched.push(event);
        },
    };

    return {
        calls,
        dispatched,
        get listener() {
            return listener;
        },
        window: global.window,
    };
}

function cleanupGlobals() {
    delete global.window;
    delete global.CustomEvent;
}

test('backend error bridge registers listener before draining pending errors', async () => {
    const fake = installFakeWindow({
        invoke(command) {
            assert.equal(command, 'backend_error_bridge_ready');
            return [' first ', { message: 'second' }, '', { message: '   ' }];
        },
    });

    try {
        const { installBackendErrorBridge } = await importFreshBridge();
        await installBackendErrorBridge();

        assert.equal(typeof fake.listener, 'function');
        assert.deepEqual(fake.calls, [
            ['listen', 'tauritavern-backend-error'],
            ['invoke', 'backend_error_bridge_ready'],
        ]);
        assert.deepEqual(fake.window.__TAURITAVERN_BACKEND_ERROR_QUEUE__, [
            { message: 'first' },
            { message: 'second' },
        ]);
    } finally {
        cleanupGlobals();
    }
});

test('backend error bridge forwards runtime events after consumer is ready', async () => {
    const fake = installFakeWindow({
        consumerReady: true,
        invoke() {
            return [];
        },
    });

    try {
        const { installBackendErrorBridge } = await importFreshBridge();
        await installBackendErrorBridge();

        fake.listener({ payload: ' later ' });

        assert.equal(fake.dispatched.length, 1);
        assert.equal(fake.dispatched[0].type, 'tauritavern:backend-error');
        assert.deepEqual(fake.dispatched[0].detail, { message: 'later' });
    } finally {
        cleanupGlobals();
    }
});

test('backend error bridge fails fast when ready command fails', async () => {
    const fake = installFakeWindow({
        invoke() {
            throw new Error('ready failed');
        },
    });

    try {
        const { installBackendErrorBridge } = await importFreshBridge();
        const resetHandlers = new Set();
        await assert.rejects(installBackendErrorBridge({
            sessionReset(handler) {
                resetHandlers.add(handler);
                return () => resetHandlers.delete(handler);
            },
        }), /ready failed/);
        assert.equal(fake.listener, null);
        assert.equal(resetHandlers.size, 0);
        assert.deepEqual(fake.calls.at(-1), ['unlisten', 'tauritavern-backend-error']);
    } finally {
        cleanupGlobals();
    }
});

function webHarness() {
    const streams = [], timers = [], declarations = [], errors = [];
    let sessions = 0, nextSource = Promise.withResolvers(), nextDeclaration, nextError, declarationFailure;
    let listenCount = 0;
    class Events {
        constructor(url) {
            this.url = url;
            streams.push(this);
            nextSource.resolve(this);
        }
        close() { this.closed = true; }
        frame(frame, id = '') { this.onmessage({ data: JSON.stringify(frame), lastEventId: id }); }
    }
    const transport = createWebTransport({
        EventSource: Events,
        schedule: callback => { timers.push(callback); return timers.length; },
        unschedule: () => {},
        onError: error => { errors.push(error); nextError?.resolve(error); },
        fetch: async (url, init) => {
            if (url === '/__tt/session') {
                sessions++;
                return Response.json({ sessionId: `page-${sessions}`, csrfToken: `csrf-${sessions}` });
            }
            assert.equal(url, '/__tt/invoke');
            const request = JSON.parse(init.body);
            assert.equal(request.command, 'backend_error_bridge_ready');
            declarations.push(request.sessionId);
            if (declarationFailure) {
                const error = declarationFailure;
                declarationFailure = null;
                throw error;
            }
            return Response.json([]);
        },
    });
    return {
        transport, streams, timers, declarations, errors,
        deps: {
            invoke: async command => {
                const result = await transport.invoke(command);
                nextDeclaration?.resolve();
                return result;
            },
            listen: (...args) => { listenCount++; return transport.listen(...args); },
            sessionReset: transport.onSessionReset,
        },
        get listenCount() { return listenCount; },
        source: () => nextSource.promise,
        expectSource() { nextSource = Promise.withResolvers(); return nextSource.promise; },
        expectDeclaration() { nextDeclaration = Promise.withResolvers(); return nextDeclaration.promise; },
        expectError() { nextError = Promise.withResolvers(); return nextError.promise; },
        failNextDeclaration(error) { declarationFailure = error; },
    };
}

// Exercise the real transport: it owns listeners, epochs, cursors and reset reporting.
// Only HTTP/SSE are faked; the bridge does not maintain another copy of that state.
test('browser backend errors replay on a short disconnect and declare readiness only on a replaced page', async () => {
    const fake = installFakeWindow({ consumerReady: true, invoke: () => assert.fail('native invoke is unused') });
    const h = webHarness();
    try {
        const { installBackendErrorBridge } = await importFreshBridge();
        const installed = installBackendErrorBridge(h.deps);
        const first = await h.source();
        first.frame({ type: 'ready', sessionId: 'page-1' });
        await installed;
        first.frame({ type: 'event', event: 'tauritavern-backend-error', id: 1, payload: { message: 'first' } }, '0');

        first.onerror();
        h.timers.shift()();
        const resumed = h.streams.at(-1);
        assert.match(resumed.url, /sessionId=page-1&after=0/);
        resumed.frame({ type: 'ready', sessionId: 'page-1' });
        resumed.frame({ type: 'event', event: 'tauritavern-backend-error', id: 1, payload: { message: 'duplicate' } }, '0');
        resumed.frame({ type: 'event', event: 'tauritavern-backend-error', id: 2, payload: { message: 'during disconnect' } }, '1');
        assert.deepEqual(h.declarations, ['page-1']);
        assert.equal(h.listenCount, 1);

        const replaced = h.expectSource();
        const declared = h.expectDeclaration();
        resumed.frame({ type: 'session-reset', reason: 'gap' });
        const next = await replaced;
        assert.deepEqual(h.declarations, ['page-1'], 'new page ready must precede its declaration');
        next.frame({ type: 'ready', sessionId: 'page-2' });
        await declared;
        next.frame({ type: 'event', event: 'tauritavern-backend-error', id: 0, payload: { message: 'new page' } }, '0');
        // A superseded EventSource cannot dispatch into or reset the replacement page.
        resumed.frame({ type: 'event', event: 'tauritavern-backend-error', id: 3, payload: { message: 'stale' } }, '2');
        assert.deepEqual(h.declarations, ['page-1', 'page-2']);
        assert.equal(h.listenCount, 1);
        assert.deepEqual(fake.dispatched.map(event => event.detail.message), ['first', 'during disconnect', 'new page']);
        assert.deepEqual(h.errors, []);

        // A failed declaration is observable, not a silently disabled toast channel or retry loop.
        const replacement = h.expectSource();
        const reported = h.expectError();
        h.failNextDeclaration(new TypeError('ready connection lost'));
        next.frame({ type: 'session-reset', reason: 'expired' });
        const last = await replacement;
        last.frame({ type: 'ready', sessionId: 'page-3' });
        const error = await reported;
        assert.match(error.message, /backend_error_bridge_ready: connection lost.*not retried/);
        assert.deepEqual(h.declarations, ['page-1', 'page-2', 'page-3']);
        assert.equal(h.listenCount, 1);
        assert.equal(h.errors.length, 1);
    } finally {
        h.transport.close();
        cleanupGlobals();
    }
});
