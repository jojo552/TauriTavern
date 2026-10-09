// Real-browser load profile: where startup and third-party extension activation spend time.
// Read-only: loads the page, never sends generation requests or edits settings.
// Usage: node web-load-profile.cjs <base> <password-file>
const fs = require('node:fs');
const assert = require('node:assert/strict');
const puppeteer = require('puppeteer-core');

(async () => {
    const [base = 'http://127.0.0.1:8002', passwordFile] = process.argv.slice(2);
    assert.ok(passwordFile, 'A mounted password file is required');
    const browser = await puppeteer.launch({ executablePath: '/usr/bin/chromium', headless: true, protocolTimeout: 300000,
        args: ['--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu'] });
    try {
        for (const run of ['cold', 'warm']) {
            const page = await browser.newPage();
            await page.authenticate({ username: 'tt', password: fs.readFileSync(passwordFile, 'utf8').trim() });
            const requests = new Map();
            const cdp = await page.createCDPSession();
            await cdp.send('Network.enable');
            if (run === 'cold') await cdp.send('Network.clearBrowserCache');
            cdp.on('Network.responseReceived', e => requests.set(e.requestId, { url: e.response.url, status: e.response.status, cache: e.response.fromDiskCache || e.response.fromMemoryCache, enc: e.response.headers['content-encoding'] || e.response.headers['Content-Encoding'] || '' }));
            cdp.on('Network.loadingFinished', e => { const r = requests.get(e.requestId); if (r) r.bytes = e.encodedDataLength; });
            const invokes = [];
            page.on('request', r => { if (r.url().endsWith('/__tt/invoke')) { let c; try { c = JSON.parse(r.postData() || '{}').command; } catch {} invokes.push({ c: c || r.headers()['x-tt-command'], t: Date.now() }); } });
            await cdp.send('Profiler.enable');
            await cdp.send('Profiler.start');
            const t0 = Date.now();
            await page.goto(base + '/', { waitUntil: 'domcontentloaded', timeout: 120000 });
            await page.waitForFunction(() => window.SillyTavern?.getContext?.()?.characters?.length > 0, { timeout: 240000, polling: 200 });
            const appReady = Date.now() - t0;
            await page.waitForFunction(() => !!document.querySelector('#extensions_settings2, #extensions_settings') && window.__TAURITAVERN_STARTUP_STAGE__ === 'full', { timeout: 240000, polling: 200 }).catch(() => {});
            // Third-party activation ends with EXTENSION_SETTINGS_LOADED; wait for network idle as proxy.
            await page.waitForNetworkIdle({ idleTime: 3000, timeout: 240000 }).catch(() => {});
            const settled = Date.now() - t0;
            const { profile } = await cdp.send('Profiler.stop');
            const perf = await page.evaluate(() => ({
                marks: performance.getEntriesByType('mark').map(m => [m.name, Math.round(m.startTime)]),
                longTasks: 0,
                heapMB: Math.round((performance.memory?.usedJSHeapSize || 0) / 1048576),
                dom: document.getElementsByTagName('*').length,
                scripts: performance.getEntriesByType('resource').filter(r => r.initiatorType === 'script' || r.name.endsWith('.js')).length,
            }));
            // Aggregate self time per script URL from the CPU profile.
            const self = new Map();
            const byId = new Map(profile.nodes.map(n => [n.id, n]));
            const dt = profile.timeDeltas; const samples = profile.samples;
            for (let i = 0; i < samples.length; i++) {
                const n = byId.get(samples[i]); const url = n.callFrame.url || `(${n.callFrame.functionName || 'native'})`;
                const key = url.replace(base, '').replace(/\?.*$/, '');
                self.set(key, (self.get(key) || 0) + (dt[i] || 0) / 1000);
            }
            const topCpu = [...self.entries()].sort((a, b) => b[1] - a[1]).slice(0, 18).map(([u, ms]) => [u, Math.round(ms)]);
            const all = [...requests.values()];
            const totalBytes = all.reduce((s, r) => s + (r.bytes || 0), 0);
            const ext = all.filter(r => r.url.includes('/scripts/extensions/third-party/'));
            const counts = {};
            for (const i of invokes) counts[i.c] = (counts[i.c] || 0) + 1;
            const heavyInvokes = Object.entries(counts).sort((a, b) => b[1] - a[1]).slice(0, 15);
            const biggest = all.filter(r => r.bytes).sort((a, b) => b.bytes - a.bytes).slice(0, 15).map(r => [r.url.replace(base, ''), Math.round(r.bytes / 1024) + 'KiB', r.enc || '-', r.cache ? 'cache' : r.status]);
            console.log(JSON.stringify({ run, appReadyMs: appReady, settledMs: settled, requests: all.length, totalMiB: +(totalBytes / 1048576).toFixed(2),
                thirdPartyRequests: ext.length, thirdPartyMiB: +(ext.reduce((s, r) => s + (r.bytes || 0), 0) / 1048576).toFixed(2),
                cachedResponses: all.filter(r => r.cache || r.status === 304).length, invokes: invokes.length, heavyInvokes, biggest, topCpu, perf }, null, 1));
            await page.close();
        }
    } finally { await browser.close(); }
})().catch(e => { console.error(e.stack || String(e)); process.exitCode = 1; });
