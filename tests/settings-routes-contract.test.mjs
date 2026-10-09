import { installHostIdentity } from './helpers/host-identity.mjs';
import { gunzipSync } from 'node:zlib';
import test from 'node:test';
import assert from 'node:assert/strict';
import { jsonResponse } from '../src/tauri/main/http-utils.js';
import { createRouteRegistry } from '../src/tauri/main/router.js';
import { registerSettingsRoutes } from '../src/tauri/main/routes/settings-routes.js';

test.beforeEach(t => t.after(installHostIdentity()));

const revision = { token: 'loaded' };

test('settings saves preserve the text and pass only an explicit revision to the host', async () => {
    for (const expected of [null, revision]) {
        const router = createRouteRegistry();
        const text = '{ "extension_settings": { "history": "🍀世界书" } }';
        const chunks = [];
        let size = 0;
        registerSettingsRoutes(router, {
            invokeTransport: async (command, args) => {
                switch (command) {
                    case 'begin_settings_commit':
                        assert.deepEqual(args.expectedRevision, expected);
                        return { sessionId: 'session', maxFrameBytes: 1024 };
                    case 'append_settings_commit_chunk':
                        chunks.push(args);
                        size += args.byteLength;
                        return size;
                    case 'finish_settings_commit':
                        return { result: 'ok', tauritavern_settings_revision: revision };
                    default: throw new Error(command);
                }
            },
        }, { jsonResponse });
        await router.handle({
            method: 'POST', path: '/api/settings/save', body: text,
            init: { headers: expected ? { 'X-TauriTavern-Settings-Revision': JSON.stringify(expected) } : {} },
        });
        assert.equal(Buffer.concat(chunks).toString(), text);
        const invalid = await router.handle({
            method: 'POST', path: '/api/settings/save', body: text,
            init: { headers: { 'X-TauriTavern-Settings-Revision': 'null' } },
        });
        assert.equal(invalid.status, 400);
    }
});

test('browser settings saves gzip text frames while offsets and the final size stay in decoded bytes', async t => {
    t.after(installHostIdentity({ platform: 'browser', kind: 'web' }));
    const router = createRouteRegistry();
    const text = JSON.stringify({ presets: Array.from({ length: 4000 }, (_, i) => ({ name: `预设 ${i}`, prompt: 'x'.repeat(40) })) });
    const decoded = [];
    let finished;
    registerSettingsRoutes(router, {
        invokeTransport: async (command, args, options) => {
            switch (command) {
                case 'begin_settings_commit': return { sessionId: 'session', maxFrameBytes: 64 * 1024 };
                case 'append_settings_commit_chunk': {
                    assert.equal(options.headers['content-encoding'], 'gzip');
                    const frame = gunzipSync(args);
                    assert.ok(frame.byteLength <= 64 * 1024 && args.byteLength < frame.byteLength);
                    assert.equal(Number(options.headers.offset), Buffer.concat(decoded).byteLength);
                    decoded.push(frame);
                    return Buffer.concat(decoded).byteLength;
                }
                case 'finish_settings_commit':
                    finished = args.expectedSize;
                    return { result: 'ok' };
                default: throw new Error(command);
            }
        },
    }, { jsonResponse });
    const response = await router.handle({ method: 'POST', path: '/api/settings/save', body: text, init: {} });
    assert.equal(response.status, 200);
    assert.ok(decoded.length > 1);
    assert.equal(Buffer.concat(decoded).toString(), text);
    assert.equal(finished, Buffer.byteLength(text));
});
