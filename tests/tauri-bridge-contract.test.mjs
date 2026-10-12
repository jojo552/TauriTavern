import { installHostIdentity } from './helpers/host-identity.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

test.beforeEach(t => t.after(installHostIdentity()));

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

// Mock Tauri Environment
let lastInvokedCommand = null;
let lastInvokedArgs = null;

global.window = {
    __TAURI_RUNNING__: true,
    __TAURI__: {
        core: {
            invoke: async (command, args) => {
                lastInvokedCommand = command;
                lastInvokedArgs = args;
                return { success: true };
            }
        }
    }
};

// Import the bridge under test
const tauriBridgePath = path.join(REPO_ROOT, 'src/tauri-bridge.js');
const {
    updateTauriTavernSettings,
    openDialog,
    setDataRoot,
    initializeBridge,
    waitUntilWebConnected,
} = await import(pathToFileURL(tauriBridgePath).href);

test('updateTauriTavernSettings contract validation', async (t) => {
    await t.test('accepts valid plain object', async () => {
        lastInvokedCommand = null;
        lastInvokedArgs = null;
        const dto = { theme: 'dark' };
        await updateTauriTavernSettings(dto);
        assert.equal(lastInvokedCommand, 'update_tauritavern_settings');
        assert.deepEqual(lastInvokedArgs.dto, dto);
    });

    await t.test('rejects null', async () => {
        await assert.rejects(
            updateTauriTavernSettings(null),
            /Invalid TauriTavern settings DTO/
        );
    });

    await t.test('rejects class instances', async () => {
        class Settings {}
        await assert.rejects(
            updateTauriTavernSettings(new Settings()),
            /Invalid TauriTavern settings DTO/
        );
    });
});

test('openDialog contract validation', async (t) => {
    await t.test('accepts valid plain object options', async () => {
        lastInvokedCommand = null;
        lastInvokedArgs = null;
        const options = { title: 'Choose File' };
        await openDialog(options);
        assert.equal(lastInvokedCommand, 'plugin:dialog|open');
        assert.deepEqual(lastInvokedArgs.options, options);
    });

    await t.test('normalizes omitted options to empty object', async () => {
        lastInvokedCommand = null;
        lastInvokedArgs = null;
        await openDialog();
        assert.equal(lastInvokedCommand, 'plugin:dialog|open');
        assert.deepEqual(lastInvokedArgs.options, {});
    });

    await t.test('rejects null', async () => {
        await assert.rejects(
            openDialog(null),
            /Invalid dialog options: expected an object/
        );
    });


});

test('browser recovery stays silent until a terminal failure requires manual reload', async () => {
    const keys = ['window', 'document', 'fetch', 'EventSource', 'toastr', 'reportError', 'setTimeout', 'clearTimeout'];
    const previous = new Map(keys.map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
    const restoreHost = installHostIdentity({ platform: 'browser', kind: 'web' });
    const warnings = [], errors = [], timers = new Map();
    let nextTimer = 0, sessions = 0, nextSource = Promise.withResolvers();
    class Events {
        constructor() { nextSource.resolve(this); }
        close() { this.closed = true; }
        frame(value) { this.onmessage({ data: JSON.stringify(value), lastEventId: '' }); }
    }
    try {
        globalThis.window = new EventTarget();
        globalThis.document = new EventTarget();
        globalThis.document.visibilityState = 'hidden';
        globalThis.EventSource = Events;
        globalThis.toastr = { warning: (...args) => warnings.push(args) };
        globalThis.reportError = error => errors.push(error);
        globalThis.setTimeout = (callback, delay) => { const id = ++nextTimer; timers.set(id, { callback, delay }); return id; };
        globalThis.clearTimeout = id => timers.delete(id);
        globalThis.fetch = async (url, init) => {
            if (url === '/__tt/session') {
                sessions++;
                return Response.json({ sessionId: `page-${sessions}`, csrfToken: `csrf-${sessions}` });
            }
            assert.equal(url, '/__tt/invoke');
            assert.equal(JSON.parse(init.body).command, 'is_ready');
            return Response.json(true);
        };
        const initial = initializeBridge();
        const first = await nextSource.promise;
        first.frame({ type: 'ready', sessionId: 'page-1' });
        await initial;

        nextSource = Promise.withResolvers();
        first.onerror(); // A background connection loss is recovered, not a user action request.
        assert.deepEqual(warnings, []);
        const [id, timer] = [...timers.entries()][0];
        timers.delete(id);
        timer.callback();
        const resumed = await nextSource.promise;
        globalThis.document.visibilityState = 'visible';
        globalThis.document.dispatchEvent(new Event('visibilitychange'));
        resumed.frame({ type: 'ready', sessionId: 'page-1' });
        await waitUntilWebConnected();
        assert.deepEqual(warnings, []);

        nextSource = Promise.withResolvers();
        resumed.frame({ type: 'session-reset', reason: 'expired' });
        const replacement = await nextSource.promise;
        replacement.frame({ type: 'ready', sessionId: 'page-2' });
        await waitUntilWebConnected();
        assert.deepEqual(warnings, [], 'Successful automatic session replacement is not a reload request');

        replacement.frame({ type: 'heartbeat', sessionId: 'wrong-page' });
        await assert.rejects(waitUntilWebConnected(), /another session/);
        assert.equal(warnings.length, 1);
        assert.match(warnings[0][0], /手动刷新/);
        assert.equal(warnings[0][2].timeOut, 0);
        assert.equal(errors.length, 1, 'The fatal error remains reported, not swallowed');
        assert.equal(timers.size, 0);
        replacement.onerror();
        assert.equal(warnings.length, 1, 'Late callbacks must not repeat the reload notice');
    } finally {
        restoreHost();
        for (const [key, descriptor] of previous) {
            if (descriptor) Object.defineProperty(globalThis, key, descriptor);
            else delete globalThis[key];
        }
    }
});

test('setDataRoot contract validation', async (t) => {
    await t.test('accepts valid non-empty path string', async () => {
        lastInvokedCommand = null;
        lastInvokedArgs = null;
        await setDataRoot('/valid/path');
        assert.equal(lastInvokedCommand, 'set_data_root');
        assert.equal(lastInvokedArgs.data_root, '/valid/path');
    });

    await t.test('rejects whitespace-only string', async () => {
        await assert.rejects(
            setDataRoot('   '),
            /Invalid data root path/
        );
    });

    await t.test('rejects non-string values', async () => {
        await assert.rejects(
            setDataRoot(42),
            /Invalid data root path/
        );
        await assert.rejects(
            setDataRoot(null),
            /Invalid data root path/
        );
    });
});
