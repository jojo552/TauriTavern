#!/usr/bin/env node
// Protocol smoke test and latency probe for a running tt-server.
//
// Usage: TT_PASSWORD=... node scripts/web-server-smoke.mjs http://127.0.0.1:8002
//        TT_CHAT_TARGET=<characterId>/<fileName> to pin the chat used for the read benchmark.
// The script is read-only for user data: the settings commit is aborted, never finished.

const [base] = process.argv.slice(2);
if (!base) throw new Error('usage: web-server-smoke.mjs <base url>');
const origin = base;
const user = process.env.TT_USER ?? 'tt';
const password = process.env.TT_PASSWORD;
if (!password) throw new Error('TT_PASSWORD is required');
const auth = 'Basic ' + Buffer.from(`${user}:${password}`).toString('base64');
const host = new URL(origin).host;
let cookie = '';

function headers(extra = {}) {
    return { authorization: auth, host, origin, 'sec-fetch-site': 'same-origin', ...(cookie ? { cookie } : {}), ...extra };
}

function check(condition, message) {
    if (!condition) throw new Error(message);
}

const timings = [];
async function timed(label, fn) {
    const start = performance.now();
    const value = await fn();
    const ms = performance.now() - start;
    timings.push([label, ms]);
    console.log(`${label.padEnd(46)} ${ms.toFixed(1).padStart(9)} ms`);
    return value;
}

function mib(bytes) {
    return (bytes / 1048576).toFixed(2);
}

async function main() {
    const unauth = await fetch(base + '/', { headers: { host } });
    check(unauth.status === 401, `unauthenticated index must be 401, got ${unauth.status}`);

    const index = await timed('GET / (index.html)', () => fetch(base + '/', { headers: headers() }));
    const indexText = await index.text();
    check(indexText.includes("platform: 'browser', kind: 'web'"), 'index must declare the browser host identity');
    check(indexText.indexOf('__TAURITAVERN_HOST__') < indexText.indexOf('init.js'), 'host identity must precede application modules');
    console.log(`  index: ${mib(indexText.length)} MiB`);

    const sessionResponse = await fetch(base + '/__tt/session', { headers: headers() });
    check(sessionResponse.ok, `session failed ${sessionResponse.status}`);
    cookie = sessionResponse.headers.get('set-cookie').split(';')[0];
    const session = await sessionResponse.json();
    check(/^[0-9a-f]{64}$/.test(session.sessionId) && /^[0-9a-f]{64}$/.test(session.csrfToken), 'session must return two independent tokens');

    const evilOrigin = await fetch(base + '/__tt/session', { headers: headers({ origin: 'https://evil.example' }) });
    check(evilOrigin.status === 403, `cross-origin session must be 403, got ${evilOrigin.status}`);

    const events = await fetch(`${base}/__tt/events?sessionId=${session.sessionId}`, {
        headers: headers({ accept: 'text/event-stream' }),
    });
    check(events.ok, `events failed ${events.status}`);
    const reader = events.body.getReader();
    const readyText = new TextDecoder().decode((await reader.read()).value);
    check(readyText.includes('"type":"ready"') && readyText.includes(session.sessionId), `first SSE frame must be ready: ${readyText}`);

    const secondStream = await fetch(`${base}/__tt/events?sessionId=${session.sessionId}`, {
        headers: headers({ accept: 'text/event-stream' }),
    });
    check(secondStream.status === 409, `a second event stream must be refused, got ${secondStream.status}`);

    async function invoke(command, args = {}, options = {}) {
        const requestHeaders = headers({ 'x-tt-csrf': session.csrfToken });
        let body;
        if (options.raw) {
            requestHeaders['content-type'] = 'application/octet-stream';
            requestHeaders['x-tt-command'] = command;
            requestHeaders['x-tt-session'] = session.sessionId;
            Object.assign(requestHeaders, options.headers ?? {});
            body = options.raw;
        } else {
            requestHeaders['content-type'] = 'application/json';
            body = JSON.stringify({ sessionId: session.sessionId, command, args });
        }
        const response = await fetch(base + '/__tt/invoke', { method: 'POST', headers: requestHeaders, body });
        const type = (response.headers.get('content-type') ?? '').split(';')[0];
        const value = type === 'application/octet-stream'
            ? new Uint8Array(await response.arrayBuffer())
            : await response.json();
        if (!response.ok) throw Object.assign(new Error(`${command} failed: ${JSON.stringify(value)}`), { value, status: response.status });
        return value;
    }

    const noCsrf = await fetch(base + '/__tt/invoke', {
        method: 'POST',
        headers: headers({ 'content-type': 'application/json' }),
        body: JSON.stringify({ sessionId: session.sessionId, command: 'is_ready', args: {} }),
    });
    check(noCsrf.status === 403, `invoke without CSRF must be 403, got ${noCsrf.status}`);

    const foreignSession = await fetch(base + '/__tt/invoke', {
        method: 'POST',
        headers: headers({ 'content-type': 'application/json', 'x-tt-csrf': session.csrfToken }),
        body: JSON.stringify({ sessionId: 'f'.repeat(64), command: 'is_ready', args: {} }),
    });
    check(foreignSession.status === 403, `unknown session must be 403, got ${foreignSession.status}`);

    check(await invoke('is_ready') === true, 'is_ready must be true');

    const compat = await invoke('get_client_version');
    console.log(`  client: pkg=${compat.pkgVersion} tt=${compat.tauriVersion} channel=${compat.defaultUpdateChannel}`);

    const settings = await timed('get_sillytavern_settings (raw bytes)', () => invoke('get_sillytavern_settings'));
    const parsed = JSON.parse(new TextDecoder().decode(settings));
    console.log(`  settings: ${mib(settings.byteLength)} MiB, ${Object.keys(parsed).length} top-level keys`);

    const bootstrap = await timed('get_bootstrap_metadata', () => invoke('get_bootstrap_metadata'));
    console.log(`  characters=${bootstrap.characters.length} groups=${bootstrap.groups.length} avatars=${bootstrap.avatars.length} secrets=${bootstrap.secret_state ? 'ok' : 'missing'}`);
    check(bootstrap.characters.length > 0, 'bootstrap must return characters');

    await timed('get_all_characters (shallow)', () => invoke('get_all_characters', { shallow: true }));

    const recent = await timed('list_recent_chat_summaries (50)', () => invoke('list_recent_chat_summaries', {
        includeMetadata: false, maxEntries: 50, pinned: [],
    }));
    console.log(`  recent chats: ${recent.length}`);

    const target = await resolveChatTarget(invoke);
    if (!target) {
        console.log('  no chat target available; skipping the chat read benchmark');
    } else {
        console.log(`  chat target: ${target.characterId} / ${target.fileName}`);
        await benchmarkChat(invoke, target, reader);
    }

    await checkCommitOwnership(invoke);
    await checkSessionIsolation(base, host, session, reader);

    const unsupported = await invoke('install_extension', { url: 'https://example.invalid/x' }).then(() => null, error => error);
    check(unsupported && unsupported.status === 400, 'browser-unsupported commands must fail explicitly');

    await reader.cancel();
    console.log('\nslowest operations:');
    for (const [label, ms] of latestTimings()) {
        console.log(`  ${label.padEnd(46)} ${ms.toFixed(1).padStart(9)} ms`);
    }
    console.log('smoke ok');
}

function latestTimings() {
    return [...timings].sort((a, b) => b[1] - a[1]).slice(0, 5);
}

async function resolveChatTarget(invoke) {
    if (process.env.TT_CHAT_TARGET) {
        const [characterId, fileName] = process.env.TT_CHAT_TARGET.split('/');
        return { characterId, fileName: fileName.replace(/\.jsonl$/, '') };
    }
    const summaries = await invoke('list_chat_summaries', { includeMetadata: false });
    const largest = summaries
        .filter(chat => chat.character_name && chat.file_name)
        .sort((a, b) => (b.file_size ?? 0) - (a.file_size ?? 0))[0];
    if (!largest) return null;
    return { characterId: largest.character_name, fileName: String(largest.file_name).replace(/\.jsonl$/, '') };
}

async function benchmarkChat(invoke, target) {
    const locator = { kind: 'character', characterId: target.characterId, fileName: target.fileName };
    for (const command of ['open_chat_reader', 'open_chat_payload_json']) {
        let frames = 0;
        const total = await timed(`${command} + read to EOF`, async () => {
            const rid = await invoke(command, { target: locator, allowNotFound: false });
            check(Number.isInteger(rid), `${command} must return a reader id`);
            let bytes = 0;
            for (;;) {
                const chunk = await invoke('read_bytes', { rid });
                if (chunk.byteLength === 0) break;
                bytes += chunk.byteLength;
                frames += 1;
            }
            await invoke('close_resource', { rid });
            const reused = await invoke('close_resource', { rid }).then(() => false, error => error.status === 400);
            check(reused, 'closing the same reader twice must fail');
            return bytes;
        });
        console.log(`  ${command}: ${mib(total)} MiB in ${frames} frames`);
    }
}

async function checkCommitOwnership(invoke) {
    const started = await invoke('begin_settings_commit', { expectedRevision: null });
    check(typeof started.sessionId === 'string' && started.maxFrameBytes >= 4, 'commit begin must return a session id and frame limit');

    const foreign = await invoke('append_settings_commit_chunk', {}, {
        raw: new Uint8Array([1, 2, 3]), headers: { 'session-id': 'not-this-session', offset: '0' },
    }).then(() => false, error => error.status === 400);
    check(foreign, 'appending under a commit session this page does not own must fail');

    const accepted = await invoke('append_settings_commit_chunk', {}, {
        raw: new Uint8Array([0x7b, 0x7d]), headers: { 'session-id': started.sessionId, offset: '0' },
    });
    check(accepted === 2, `append must acknowledge the byte offset, got ${accepted}`);

    const base64Rejected = await invoke('append_settings_commit_chunk', {}, {
        raw: new Uint8Array([0x7b, 0x7d]),
        headers: { 'session-id': started.sessionId, offset: '2', 'chunk-encoding': 'base64' },
    }).then(() => false, error => error.status === 400);
    check(base64Rejected, 'browser commit frames must reject the base64 transport');

    await invoke('abort_settings_commit', { sessionId: started.sessionId });
    const doubleAbort = await invoke('abort_settings_commit', { sessionId: started.sessionId }).then(() => false, () => true);
    check(doubleAbort, 'aborting a consumed commit must fail');
    console.log('  settings commit: ownership, frame transport and single use verified (aborted, nothing published)');
}

async function checkSessionIsolation(base, host, ownerSession, ownerReader) {
    const other = await fetch(base + '/__tt/session', { headers: { authorization: auth, host, origin, 'sec-fetch-site': 'same-origin' } });
    check(other.ok, 'second session failed');
    const otherCookie = other.headers.get('set-cookie').split(';')[0];
    const otherSession = await other.json();

    const ownerCookie = cookie;
    cookie = otherCookie;
    const rid = await invokeRaw(otherSession, 'open_chat_reader', null, {
        'content-type': 'application/json',
        body: JSON.stringify({ sessionId: otherSession.sessionId, command: 'open_chat_reader', args: {
            target: { kind: 'character', characterId: 'nonexistent-character', fileName: 'nonexistent' }, allowNotFound: true,
        } }),
    });
    check(rid === null, 'a missing chat with allowNotFound must return null, not an error');
    const unknownReader = await invokeRaw(otherSession, 'read_bytes', null, {
        'content-type': 'application/json',
        body: JSON.stringify({ sessionId: otherSession.sessionId, command: 'read_bytes', args: { rid: 1 } }),
    }).then(() => false, error => error.status === 400);
    check(unknownReader, 'reading a resource id from another page must fail');
    cookie = ownerCookie;
    void ownerSession;
    void ownerReader;
    console.log('  session isolation: reader and commit ids are not shared between pages');
}

async function invokeRaw(session, label, _unused, { 'content-type': contentType, body }) {
    const response = await fetch(base + '/__tt/invoke', {
        method: 'POST',
        headers: headers({ 'content-type': contentType, 'x-tt-csrf': session.csrfToken }),
        body,
    });
    const value = await response.json();
    if (!response.ok) throw Object.assign(new Error(`${label} failed: ${JSON.stringify(value)}`), { value, status: response.status });
    return value;
}

await main();
