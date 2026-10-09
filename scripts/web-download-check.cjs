// Real-browser download regression probe; fixtures contain no user data.
// Usage: node web-download-check.cjs <base> <password-file>
const fs = require('node:fs/promises');
const path = require('node:path');
const assert = require('node:assert/strict');
const { randomUUID } = require('node:crypto');
const puppeteer = require('puppeteer-core');

(async () => {
    const [base = 'http://127.0.0.1:8002', passwordFile] = process.argv.slice(2);
    assert.ok(passwordFile, 'A mounted password file is required');
    const directory = path.resolve('download-probe-' + randomUUID());
    await fs.mkdir(directory);
    const browser = await puppeteer.launch({ executablePath: '/usr/bin/chromium', headless: true,
        args: ['--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu'] });
    try {
        const page = await browser.newPage();
        await page.authenticate({ username: 'tt', password: (await fs.readFile(passwordFile, 'utf8')).trim() });
        await page.goto(base + '/', { waitUntil: 'domcontentloaded', timeout: 60000 });
        await page.waitForFunction(() => window.SillyTavern?.getContext?.()?.characters?.length > 0, { timeout: 120000 });
        const cdp = await browser.target().createCDPSession();
        await cdp.send('Browser.setDownloadBehavior', { behavior: 'allowAndName', downloadPath: directory, eventsEnabled: true });
        const fixtures = [
            { kind: 'host', fileName: 'host-fixture.bin' },
            { kind: 'connected', fileName: 'extension-connected.bin' },
            { kind: 'detached', fileName: 'extension-detached.bin' },
            { kind: 'iframe', fileName: 'extension-iframe.bin' },
        ];
        const bytes = [0, 1, 127, 128, 255, 42, 0, 13, 10];
        const results = [];
        for (const fixture of fixtures) {
            let begin;
            let timer;
            let onBegin;
            let onProgress;
            const completed = new Promise((resolve, reject) => {
                timer = setTimeout(() => reject(new Error('No completed browser download: ' + fixture.kind)), 15000);
                onBegin = event => { begin = event; };
                onProgress = event => {
                    if (!begin || begin.guid !== event.guid) return;
                    if (event.state === 'completed') resolve(begin);
                    if (event.state === 'canceled') reject(new Error('Browser canceled fixture download'));
                };
                cdp.on('Browser.downloadWillBegin', onBegin);
                cdp.on('Browser.downloadProgress', onProgress);
            });
            // Attach immediately so a download timeout cannot become an unhandled rejection
            // while the page action is still in flight.
            completed.catch(() => {});
            try {
                const handoff = await page.evaluate(async ({ fixture, bytes }) => {
                    if (fixture.kind === 'host') {
                        const { deliverBlob } = await import('/scripts/file-export.js');
                        return deliverBlob(new Blob([new Uint8Array(bytes)]), fixture.fileName);
                    }
                    let frame;
                    let target = window;
                    if (fixture.kind === 'iframe') {
                        frame = document.createElement('iframe');
                        const loaded = new Promise(resolve => frame.addEventListener('load', resolve, { once: true }));
                        frame.src = 'about:blank';
                        document.body.append(frame);
                        await loaded;
                        target = frame.contentWindow;
                    }
                    const url = target.URL.createObjectURL(new target.Blob([new Uint8Array(bytes)]));
                    const anchor = target.document.createElement('a');
                    anchor.href = url;
                    anchor.download = fixture.fileName;
                    if (fixture.kind !== 'detached') target.document.body.append(anchor);
                    anchor.click();
                    anchor.remove();
                    // Clean URL/frame after the browser has read them, not to infer saving.
                    window.setTimeout(() => { target.URL.revokeObjectURL(url); frame?.remove(); }, 60000);
                    return null;
                }, { fixture, bytes });
                if (fixture.kind === 'host') assert.deepEqual(handoff, { status: 'handed-off' });
                const download = await completed;
                assert.equal(download.suggestedFilename, fixture.fileName);
                assert.deepEqual(await fs.readFile(path.join(directory, download.guid)), Buffer.from(bytes));
                results.push({ ...fixture, bytes: bytes.length, completed: true });
            } finally {
                clearTimeout(timer);
                cdp.off('Browser.downloadWillBegin', onBegin);
                cdp.off('Browser.downloadProgress', onProgress);
            }
        }
        console.log(JSON.stringify({ ok: true, results }, null, 2));
    } finally {
        await browser.close();
        await fs.rm(directory, { recursive: true, force: true });
    }
})().catch(error => { console.error(error.stack || String(error)); process.exitCode = 1; });
