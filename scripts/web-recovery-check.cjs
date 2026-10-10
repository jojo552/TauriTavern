// Real-browser recovery probe; run only against the nc personal-data test copy.
// Changes a random extension-store entry and restores it; no generation/API calls.
// Usage: node web-recovery-check.cjs <base> <password-file> [restart-fifo]
const fs = require('node:fs/promises');
const assert = require('node:assert/strict');
const puppeteer = require('puppeteer-core');

(async () => {
    const [base = 'http://127.0.0.1:8002', passwordFile, restartFifo] = process.argv.slice(2);
    assert.ok(passwordFile, 'A mounted password file is required');
    const browser = await puppeteer.launch({ executablePath: '/usr/bin/chromium', headless: true, protocolTimeout: 300000,
        args: ['--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu'] });
    try {
        const page = await browser.newPage();
        await page.authenticate({ username: 'tt', password: (await fs.readFile(passwordFile, 'utf8')).trim() });
        await page.evaluateOnNewDocument(() => {
            const NativeEvents = window.EventSource;
            window.__probeEvents = [];
            window.EventSource = class extends NativeEvents {
                constructor(url, options) { super(url, options); window.__probeEvents.push(this); }
            };
        });
        const invokes = [], failures = [];
        let settingsReads = 0, currentSession;
        page.on('request', request => {
            if (!request.url().endsWith('/__tt/invoke')) return;
            try {
                const value = JSON.parse(request.postData() || '{}');
                currentSession = value.sessionId || currentSession;
                invokes.push(value.command || request.headers()['x-tt-command']);
                if (value.command === 'get_sillytavern_settings') settingsReads++;
            } catch { invokes.push(request.headers()['x-tt-command']); }
        });
        page.on('pageerror', error => failures.push(String(error).slice(0, 300)));
        await page.goto(base + '/', { waitUntil: 'domcontentloaded', timeout: 90000 });
        await page.waitForFunction(() => window.TavernHelper && !document.querySelector('#preloader') && window.__probeEvents.length > 0, { timeout: 180000 });
        assert.equal(settingsReads, 1, 'Quick Reply must reuse the one startup settings snapshot');
        const beforeEpoch = await page.evaluate(async () => (await import('/tauri-bridge.js')).hostSessionEpoch());
        const session = currentSession;
        await page.evaluate(async () => {
            const { invoke, hostSessionEpoch } = await import('/tauri-bridge.js');
            const cold = await import('/scripts/tauri/chat/cold-swipes.js');
            const { commitChatPayload } = await import('/scripts/tauri/chat/commit.js');
            const original = [{ chat_metadata: {} }, { name: 'Probe', is_user: false, is_system: false, mes: 'active',
                swipe_id: 1, swipes: ['older', 'active'], swipe_info: [{ extra: {} }, { extra: {} }], extra: {} },
                { name: 'Probe', is_user: true, is_system: false, mes: 'tail', extra: {} }];
            const target = { kind: 'character', characterId: 'tt-recovery-probe', fileName: 'cold-' + crypto.randomUUID() };
            try {
                await commitChatPayload({ target, payload: original, force: true, commitReason: 'mutation' });
                const loaded = await cold.loadColdChatPayload(target, false);
                const cloned = structuredClone(loaded);
                if (!cloned[1].tt_swipe_cold) throw new Error('Fixture should have a cold projection: ' + JSON.stringify(cloned));
                await commitChatPayload({ target, payload: cloned, force: true, commitReason: 'mutation' });
                const restored = await invoke('open_chat_reader', { target, allowNotFound: false });
                const { createByteStream } = (await import('/tauri/main/services/files/readable-file-stream-service.js')).createReadableFileStreamService({ invoke });
                const payload = await (await import('/scripts/tauri/chat/jsonl.js')).jsonlStreamToPayload(createByteStream(restored));
                if (JSON.stringify(payload[1].swipes) !== JSON.stringify(['older', 'active']) || payload[1].tt_swipe_cold) throw new Error('Cloned cold commit lost swipes or stored its marker');
                window.__probeCold = cloned[1].tt_swipe_cold;
                window.__probeColdEpoch = hostSessionEpoch();
                cold.discardColdChatPayload(loaded);
            } finally { await invoke('delete_chat', { character_name: target.characterId, file_name: target.fileName }); }
        });
        const resources = await page.evaluate(async () => {
            const { invoke } = await import('/tauri-bridge.js');
            const summaries = await invoke('list_chat_summaries', { includeMetadata: false });
            const item = summaries.filter(s => s.file_name && s.character_name).sort((a, b) => (b.file_size || 0) - (a.file_size || 0))[0];
            if (!item) throw new Error('Test copy needs an existing chat');
            const target = { kind: 'character', characterId: item.character_name, fileName: item.file_name.replace(/\.jsonl$/, '') };
            const rid = await invoke('open_chat_reader', { target, allowNotFound: false });
            window.__probeReader = { rid, target };
            return { rid };
        });
        const connections = await page.evaluate(() => window.__probeEvents.length);
        // A second connection to the same page replaces and actually ends the existing SSE
        // response. The frontend must reopen it without a page reload or new session.
        await page.evaluate(async ({ session }) => {
            const controller = new AbortController();
            const response = await fetch('/__tt/events?sessionId=' + session, { signal: controller.signal });
            await response.body.getReader().read();
            controller.abort();
        }, { session });
        await page.waitForFunction(n => window.__probeEvents.length > n && window.__probeEvents.at(-1).readyState === EventSource.OPEN,
            { timeout: 30000 }, connections);
        const resumed = await page.evaluate(async () => {
            const { invoke, hostSessionEpoch } = await import('/tauri-bridge.js');
            const bytes = await invoke('read_bytes', { rid: window.__probeReader.rid });
            return { epoch: hostSessionEpoch(), bytes: bytes.byteLength, save: await (await import('/script.js')).saveSettings() };
        });
        assert.equal(resumed.epoch, beforeEpoch, 'A brief disconnect must retain the page');
        assert.ok(resumed.bytes > 0 && resumed.save === true, 'Reader and settings save must still work');
        const result = { settingsReads, resumed, connectionsBefore: connections };
        if (restartFifo) {
            // A host-side test controller blocks on this FIFO and restarts the nc Rust
            // process only after the browser has verified same-page recovery.
            await fs.writeFile(restartFifo, 'restart\n');
            await page.waitForFunction(async epoch => (await import('/tauri-bridge.js')).hostSessionEpoch() > epoch,
                { timeout: 45000 }, beforeEpoch);
            await page.waitForFunction(async () => {
                try { return await (await import('/tauri-bridge.js')).invoke('is_ready'); }
                catch { return false; }
            }, { timeout: 45000 });
            const after = await page.evaluate(async () => {
                const { invoke, hostSessionEpoch } = await import('/tauri-bridge.js');
                let staleError, coldError;
                try { await (await import('/scripts/tauri/chat/cold-swipes.js')).readColdSwipeRecord(window.__probeCold); }
                catch (error) { coldError = String(error?.message || error); }
                try { await invoke('read_bytes', { rid: window.__probeReader.rid }); }
                catch (error) { staleError = String(error?.message || error); }
                const next = await invoke('open_chat_reader', { target: window.__probeReader.target, allowNotFound: false });
                const bytes = await invoke('read_bytes', { rid: next });
                await invoke('close_resource', { rid: next });
                const name = 'tt-recovery-' + crypto.randomUUID();
                const store = window.__TAURITAVERN__.api.extension.store;
                const entry = { namespace: name, key: 'probe' };
                try {
                    await store.setJson({ ...entry, value: { restored: true } });
                    const read = await store.getJson(entry);
                    if (read.restored !== true) throw new Error('Store did not persist');
                } finally { await store.deleteJson(entry); }
                return { epoch: hostSessionEpoch(), staleError, coldError, newReaderBytes: bytes.byteLength,
                    settingsSave: await (await import('/script.js')).saveSettings() };
            });
            assert.match(after.staleError, /resource expired/);
            assert.match(after.coldError, /Cold swipe source expired/);
            assert.ok(after.newReaderBytes > 0 && after.settingsSave === true);
            try {
                await page.waitForFunction(async () => {
                    const { invoke } = await import('/tauri-bridge.js');
                    const list = await invoke('list_agent_tools');
                    return JSON.stringify(list).includes('extension/in-app-agent:app.evaluate');
                }, { timeout: 45000 });
            } catch (error) {
                console.log(JSON.stringify({ after, failures, registrationCalls: invokes.filter(c => c === 'register_agent_extension_tool').length,
                    tools: await page.evaluate(async () => (await (await import('/tauri-bridge.js')).invoke('list_agent_tools')).tools.map(t => t.id).filter(id => id.startsWith('extension'))),
                    connections: await page.evaluate(() => window.__probeEvents.map(e => ({ url: e.url, state: e.readyState }))) }, null, 2));
                throw error;
            }
            result.restart = after;
        }
        assert.equal(failures.length, 0, 'Recovery must not raise uncaught browser errors');
        console.log(JSON.stringify({ ok: true, ...result, pageErrors: failures, invokes: invokes.length }, null, 2));
    } finally { await browser.close(); }
})().catch(error => { console.error(error.stack || String(error)); process.exitCode = 1; });
