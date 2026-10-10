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

const [releaseArg, expectedRevision, expectedBranch] = process.argv.slice(2);
if (!releaseArg || !expectedRevision || !expectedBranch) {
    throw new Error('usage: verify-web-server.mjs <release dir> <revision> <branch>');
}
const release = path.resolve(releaseArg);
assert.equal((await fs.readFile(path.join(release, 'SOURCE_COMMIT'), 'utf8')).trim(), expectedRevision);
const root = await fs.mkdtemp(path.join(os.tmpdir(), 'tt-release-verify-'));
const base = 'http://127.0.0.1:18002';
const password = randomBytes(32).toString('hex');
const authorization = 'Basic ' + Buffer.from('verify:' + password).toString('base64');
let server, exited, deadline, session, cookie, streamReader;

async function request(route, init = {}) {
    return fetch(base + route, { ...init, signal: AbortSignal.timeout(15000),
        headers: { authorization, origin: base, ...(cookie ? { cookie } : {}), ...init.headers } });
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
async function readReady() {
    const decoder = new TextDecoder();
    let buffer = '';
    for (;;) {
        const { value, done } = await streamReader.read();
        if (done) throw new Error('Event stream ended before ready');
        buffer += decoder.decode(value, { stream: true });
        if (buffer.length > 65536) throw new Error('Event stream did not provide a bounded ready frame');
        let boundary;
        while ((boundary = /\r?\n\r?\n/.exec(buffer))) {
            const event = buffer.slice(0, boundary.index);
            buffer = buffer.slice(boundary.index + boundary[0].length);
            const data = event.split(/\r?\n/).filter(line => line.startsWith('data:'))
                .map(line => line.slice(5).replace(/^ /, '')).join('\n');
            if (!data) continue; // Keepalive comments are not a ready message.
            const frame = JSON.parse(data);
            assert.equal(frame.type, 'ready');
            assert.equal(frame.sessionId, session.sessionId);
            return;
        }
    }
}
try {
    // Every operation after mkdtemp is covered by cleanup, including password/setup/spawn failures.
    await fs.writeFile(path.join(root, 'password'), password, { mode: 0o600 });
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
    const denied = await fetch(base + '/', { signal: AbortSignal.timeout(15000) });
    assert.equal(denied.status, 401);
    const index = await request('/');
    assert.equal(index.status, 200);
    assert.ok((await index.text()).includes("platform: 'browser', kind: 'web'"));
    const created = await request('/__tt/session');
    assert.equal(created.status, 200);
    cookie = created.headers.get('set-cookie').split(';')[0];
    session = await created.json();
    const events = await request('/__tt/events?sessionId=' + session.sessionId);
    assert.equal(events.status, 200);
    streamReader = events.body.getReader();
    await readReady();

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
    const closed = await request('/__tt/session/close', { method: 'POST',
        headers: { 'content-type': 'application/json', 'x-tt-csrf': session.csrfToken },
        body: JSON.stringify({ sessionId: session.sessionId }) });
    assert.equal(closed.status, 204);
    console.log('Packaged host verified: identity, authentication, SSE ready, gzip frames and commit reclamation');
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
