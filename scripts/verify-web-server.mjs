#!/usr/bin/env node
// Exercise the actual packaged host with temporary, generated state; never connect to a deployment.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { randomBytes } from 'node:crypto';
import { gzipSync } from 'node:zlib';
import { createEventReader, createWebServerClient } from './web-server-client.mjs';

const [releaseArg, expectedRevision, expectedBranch] = process.argv.slice(2);
if (!releaseArg || !expectedRevision || !expectedBranch) {
    throw new Error('usage: verify-web-server.mjs <release dir> <revision> <branch>');
}
const release = path.resolve(releaseArg);
assert.equal((await fs.readFile(path.join(release, 'SOURCE_COMMIT'), 'utf8')).trim(), expectedRevision);
const root = await fs.mkdtemp(path.join(os.tmpdir(), 'tt-release-verify-'));
const base = 'http://127.0.0.1:18002';
const password = randomBytes(32).toString('hex');
const client = createWebServerClient(base);
const request = client.request;
let server, exited, deadline, session, streamReader;

async function assertLoginRequired(response) {
    assert.equal(response.status, 401);
    assert.equal(response.headers.get('x-tt-login-required'), '1');
    assert.equal(response.headers.get('www-authenticate'), null, 'never trigger a browser credential challenge');
    assert.match(response.headers.get('content-type'), /^application\/json/);
    await response.json();
}
async function invoke(command, args = {}, body) {
    const headers = { 'x-tt-csrf': session.csrfToken };
    if (body) {
        Object.assign(headers, { 'content-type': 'application/octet-stream', 'content-encoding': 'gzip',
            'x-tt-command': command, 'x-tt-session': session.sessionId,
            'session-id': args.sessionId, offset: String(args.offset) });
    } else {
        headers['content-type'] = 'application/json';
    }
    return request('/__tt/invoke', { method: 'POST', headers,
        body: body ? gzipSync(body) : JSON.stringify({ sessionId: session.sessionId, command, args }) });
}
const readFrame = () => streamReader.nextFrame();
async function readReady() {
    const frame = await readFrame();
    assert.equal(frame.type, 'ready');
    assert.equal(frame.sessionId, session.sessionId);
}
try {
    // Every operation after mkdtemp is covered by cleanup, including password/setup/spawn failures.
    await fs.writeFile(path.join(root, 'password'), password, { mode: 0o600 });
    // Exercise the real startup recovery producer, not a test-only error injection endpoint.
    await fs.mkdir(path.join(root, 'data/default-user/settings'), { recursive: true });
    await fs.writeFile(path.join(root, 'data/default-user/settings/dynamic-theme.json'), '{invalid synthetic settings');
    server = spawn(path.join(release, 'tt-server'), [
        '--listen', '127.0.0.1:18002', '--public-origin', base,
        '--data-root', path.join(root, 'data'), '--app-root', path.join(root, 'app'),
        '--resource-root', path.join(release, 'resources'), '--web-root', path.join(release, 'web'),
        '--username', 'verify', '--password-file', path.join(root, 'password'),
    ], { stdio: ['ignore', 'pipe', 'pipe'] });
    // Attach the rejection handler immediately: a spawn error must not become a detached rejection.
    exited = once(server, 'close').catch(() => {});
    const ready = Promise.withResolvers();
    let log = '';
    for (const output of [server.stdout, server.stderr]) {
        output.on('data', chunk => {
            log = (log + chunk).slice(-12000);
            if (log.includes('Native TT browser host ready')) ready.resolve();
        });
    }
    server.once('error', ready.reject);
    server.once('exit', code => ready.reject(new Error('Packaged host exited before ready: ' + code + '\n' + log)));
    deadline = setTimeout(() => ready.reject(new Error('Packaged host readiness timed out\n' + log)), 60000);
    await ready.promise;
    clearTimeout(deadline);
    const anonymous = createWebServerClient(base);
    const status = await anonymous.request('/__tt/auth/status');
    assert.equal(status.status, 200);
    assert.equal(status.headers.get('cache-control'), 'no-store');
    assert.deepEqual(await status.json(), { authenticated: false });
    assert.equal(status.headers.getSetCookie().length, 0, 'status must not allocate a browser/page');
    for (const document of ['/', '/index.html']) {
        const denied = await anonymous.request(document, { headers: { 'sec-fetch-dest': 'document' } });
        assert.equal(denied.status, 303);
        assert.equal(denied.headers.get('location'), '/login');
        assert.equal(denied.headers.get('www-authenticate'), null);
    }
    for (const route of ['/__tt/session', '/init.js', '/__tt/download/unknown', '/__tt/events?sessionId=unknown']) {
        await assertLoginRequired(await anonymous.request(route));
    }
    // A correct password in the retired wire format is not authentication.
    await assertLoginRequired(await anonymous.request('/__tt/session', {
        headers: { authorization: 'Basic ' + Buffer.from('verify:' + password).toString('base64') },
    }));
    const rejected = await anonymous.request('/__tt/auth/login', {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ username: 'verify', password: password + '-wrong' }),
    });
    assert.equal(rejected.status, 401);
    assert.equal(rejected.headers.get('www-authenticate'), null);
    assert.equal(rejected.headers.get('cache-control'), 'no-store');
    assert.equal(rejected.headers.getSetCookie().length, 0, 'wrong credentials must never mint cookies');
    await rejected.json();
    for (const headers of [{ origin: 'https://evil.example' }, { origin: 'http://127.0.0.1:18003' },
        { origin: '' }, { 'sec-fetch-site': 'same-site' }, { 'sec-fetch-site': 'cross-site' }]) {
        const rejectedOrigin = await anonymous.request('/__tt/auth/login', {
            method: 'POST', headers: { 'content-type': 'application/json', ...headers },
            body: JSON.stringify({ username: 'verify', password }),
        });
        assert.equal(rejectedOrigin.status, 403, 'login requires the exact configured public origin and same-origin FetchSite');
        assert.equal(rejectedOrigin.headers.getSetCookie().length, 0);
    }
    const oversizedLogin = await anonymous.request('/__tt/auth/login', {
        method: 'POST', headers: { 'content-type': 'application/json' }, body: ' '.repeat(8193),
    });
    assert.equal(oversizedLogin.status, 413, 'login has its own small body limit, independent of invoke frames');
    assert.equal(oversizedLogin.headers.getSetCookie().length, 0);
    const login = await client.login('verify', password);
    assert.equal(login.headers.get('cache-control'), 'no-store');
    const authenticated = await request('/__tt/auth/status');
    assert.equal(authenticated.headers.get('cache-control'), 'no-store');
    assert.deepEqual(await authenticated.json(), { authenticated: true });
    assert.equal(authenticated.headers.getSetCookie().length, 0, 'authentication status is not a page bootstrap');
    const index = await request('/');
    assert.equal(index.status, 200);
    assert.ok((await index.text()).includes("platform: 'browser', kind: 'web'"));
    const evilSession = await request('/__tt/session', { headers: { origin: 'https://evil.example' } });
    assert.equal(evilSession.status, 403);
    const created = await request('/__tt/session');
    assert.equal(created.status, 200);
    session = await created.json();
    const events = await request('/__tt/events?sessionId=' + session.sessionId);
    assert.equal(events.status, 200);
    streamReader = createEventReader(events);
    await readReady();
    const errorReady = await invoke('backend_error_bridge_ready');
    assert.equal(errorReady.status, 200);
    assert.deepEqual(await errorReady.json(), [], 'browser pending notifications use SSE, not the HTTP result');
    const startupError = await readFrame();
    assert.equal(startupError.event, 'tauritavern-backend-error');
    assert.ok(Number.isSafeInteger(startupError.id));
    assert.deepEqual(Object.keys(startupError.payload), ['message'], 'diagnostic fields do not cross the error bridge');
    assert.ok(startupError.payload.message.includes('dynamic-theme.json'));
    assert.ok(startupError.payload.message.includes('restoring this settings section to defaults'));
    assert.deepEqual(await (await invoke('backend_error_bridge_ready')).json(), [], 'ready is idempotent');

    // A swallowed, explicitly user-visible request failure is also delivered without enabling logs.
    await fs.mkdir(path.join(root, 'data/default-user/groups'), { recursive: true });
    await fs.writeFile(path.join(root, 'data/default-user/groups/verify-user-error.json'), '{invalid synthetic group');
    assert.equal((await invoke('clear_group_cache')).status, 200);
    const groups = await invoke('get_all_groups');
    assert.equal(groups.status, 200);
    assert.deepEqual(await groups.json(), []);
    const liveError = await readFrame();
    assert.equal(liveError.event, 'tauritavern-backend-error');
    assert.ok(liveError.payload.message.includes('verify-user-error.json'));

    const versionReply = await invoke('get_client_version');
    assert.equal(versionReply.status, 200);
    const version = await versionReply.json();
    assert.equal(version.gitRevision, expectedRevision, 'Binary revision must match SOURCE_COMMIT');
    assert.equal(version.gitBranch, expectedBranch);
    // Beyond the receiver's eight-slot capacity: malformed finishes must stay abortable and
    // must not leave hidden receiving sessions behind. Synthetic bytes are never published.
    for (let i = 0; i < 9; i++) {
        const began = await invoke('begin_settings_commit', { expectedRevision: null });
        assert.equal(began.status, 200);
        const commit = await began.json();
        const appended = await invoke('append_settings_commit_chunk', { sessionId: commit.sessionId, offset: 0 }, Buffer.from('{}'));
        assert.equal(appended.status, 200);
        assert.equal(await appended.json(), 2);
        const malformed = await invoke('finish_settings_commit', { sessionId: commit.sessionId });
        assert.equal(malformed.status, 400);
        const aborted = await invoke('abort_settings_commit', { sessionId: commit.sessionId });
        assert.equal(aborted.status, 200);
    }
    // Log panels: the backend buffer saw startup, and a page that enables the stream receives new
    // entries over its own events stream. A failed command is the deterministic log producer.
    const tail = await (await invoke('devlog_get_backend_log_tail', { limit: 800 })).json();
    assert.ok(tail.some(entry => entry.message.includes('Native TT browser host ready')), 'backend tail holds startup');
    assert.equal((await invoke('devlog_set_backend_log_stream_enabled', { enabled: true })).status, 200);
    assert.equal((await invoke('verify_unknown_command_for_log')).status, 400);
    for (;;) {
        const frame = await readFrame();
        assert.notEqual(frame.event, 'tauritavern-backend-error', 'ordinary command failures do not become backend error toasts');
        if (frame.type !== 'event' || frame.event !== 'tauritavern-backend-log') continue;
        // The same frame shape the browser transport validates before dispatching to listeners.
        assert.ok(Number.isSafeInteger(frame.id), 'event frames carry a numeric id');
        if (frame.payload.message.includes('verify_unknown_command_for_log')) break;
    }
    assert.equal((await invoke('devlog_set_backend_log_stream_enabled', { enabled: false })).status, 200);
    const llmIndex = await invoke('devlog_get_llm_api_log_index', { limit: 10 });
    assert.equal(llmIndex.status, 200);
    assert.deepEqual(await llmIndex.json(), [], 'fresh data has no LLM API requests');
    assert.equal((await invoke('devlog_get_llm_api_log_preview', { id: 1 })).status, 404);
    const unauthorizedLogs = await request('/__tt/invoke', { method: 'POST',
        headers: { 'content-type': 'application/json', 'x-tt-csrf': 'wrong' },
        body: JSON.stringify({ sessionId: session.sessionId, command: 'devlog_get_backend_log_tail', args: {} }) });
    assert.equal(unauthorizedLogs.status, 403, 'log reads require the page CSRF token');

    const closed = await request('/__tt/session/close', { method: 'POST',
        headers: { 'content-type': 'application/json', 'x-tt-csrf': session.csrfToken },
        body: JSON.stringify({ sessionId: session.sessionId }) });
    assert.equal(closed.status, 204);
    await streamReader.cancel();
    // Logout must revoke the lease, not just ask this client to forget its cookie. Keep a stale
    // credential snapshot and an active page stream to observe server-side revocation.
    const staleCookie = login.headers.getSetCookie().map(value => value.split(';', 1)[0]).join('; ');
    const logoutPage = await request('/__tt/session');
    assert.equal(logoutPage.status, 200);
    session = await logoutPage.json();
    const logoutEvents = await request('/__tt/events?sessionId=' + session.sessionId);
    assert.equal(logoutEvents.status, 200);
    streamReader = createEventReader(logoutEvents);
    await readReady();
    const forbiddenLogout = await request('/__tt/auth/logout', {
        method: 'POST', headers: { 'content-type': 'application/json', origin: 'https://evil.example' }, body: '{}',
    });
    assert.equal(forbiddenLogout.status, 403);
    assert.deepEqual(await (await request('/__tt/auth/status')).json(), { authenticated: true });
    const logout = await request('/__tt/auth/logout', {
        method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}',
    });
    assert.equal(logout.status, 204);
    await streamReader.waitForEnd();
    assert.deepEqual(await (await request('/__tt/auth/status')).json(), { authenticated: false });
    assert.deepEqual(await (await anonymous.request('/__tt/auth/status', { headers: { cookie: staleCookie } })).json(),
        { authenticated: false }, 'logout revokes stale copies of the auth cookie');
    await assertLoginRequired(await anonymous.request('/__tt/events?sessionId=' + session.sessionId,
        { headers: { cookie: staleCookie } }));
    console.log('Packaged host verified: Cookie login/logout, identity, origin/CSRF boundaries, SSE lifecycle, explicit backend errors, gzip frames, commit reclamation and log panels');
} finally {
    clearTimeout(deadline);
    try {
        await streamReader?.cancel().catch(() => {});
        if (server && server.exitCode === null && server.signalCode === null) server.kill('SIGTERM');
        await exited;
    } finally {
        await fs.rm(root, { recursive: true, force: true });
    }
}
