// Real-browser load measurement under an emulated remote network (read-only).
// Reports time to APP_READY-equivalent, time until third-party extensions finish activating
// (EXTENSION_SETTINGS_LOADED), transferred bytes, and steady-state CPU after load.
// Usage: node web-load-network.cjs <base> <password-file> [latencyMs] [downMbps]
const fs = require('node:fs');
const assert = require('node:assert/strict');
const puppeteer = require('puppeteer-core');

(async () => {
    const [base = 'http://127.0.0.1:8002', passwordFile, latency = '120', down = '40'] = process.argv.slice(2);
    assert.ok(passwordFile, 'A mounted password file is required');
    const browser = await puppeteer.launch({ executablePath: '/usr/bin/chromium', headless: true, protocolTimeout: 300000,
        args: ['--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu'] });
    try {
        for (const encoding of ['identity', 'compressed']) {
            const page = await browser.newPage();
            await page.authenticate({ username: 'tt', password: fs.readFileSync(passwordFile, 'utf8').trim() });
            const cdp = await page.createCDPSession();
            await cdp.send('Network.enable');
            await cdp.send('Network.clearBrowserCache');
            await cdp.send('Network.emulateNetworkConditions', { offline: false, latency: Number(latency),
                downloadThroughput: Number(down) * 125000, uploadThroughput: 10 * 125000 });
            if (encoding === 'identity') await cdp.send('Network.setExtraHTTPHeaders', { headers: { 'Accept-Encoding': 'identity' } });
            let bytes = 0;
            cdp.on('Network.loadingFinished', e => { bytes += e.encodedDataLength; });
            const t0 = Date.now();
            let extensionsLoaded = null;
            page.on('console', m => { if (extensionsLoaded === null && m.text().includes('Event emitted: extension_settings_loaded')) extensionsLoaded = Date.now() - t0; });
            await page.goto(base + '/', { waitUntil: 'domcontentloaded', timeout: 300000 });
            await page.waitForFunction(() => window.SillyTavern?.getContext?.()?.characters?.length > 0, { timeout: 300000, polling: 200 });
            const appReady = Date.now() - t0;
            const deadline = Date.now() + 300000;
            while (extensionsLoaded === null && Date.now() < deadline) await new Promise(r => setTimeout(r, 200));
            await new Promise(r => setTimeout(r, 5000));
            const before = await cdp.send('Performance.enable').then(() => cdp.send('Performance.getMetrics'));
            await new Promise(r => setTimeout(r, 20000));
            const after = await cdp.send('Performance.getMetrics');
            const metric = (m, n) => m.metrics.find(x => x.name === n)?.value ?? 0;
            const idleCpuPct = Math.round((metric(after, 'TaskDuration') - metric(before, 'TaskDuration')) / 20 * 100);
            const heapMB = Math.round(metric(after, 'JSHeapUsedSize') / 1048576);
            console.log(JSON.stringify({ encoding, latencyMs: Number(latency), downMbps: Number(down), appReadyMs: appReady,
                extensionsLoadedMs: extensionsLoaded, transferredMiB: +(bytes / 1048576).toFixed(1), idleCpuPct, heapMB }));
            await page.close();
        }
    } finally { await browser.close(); }
})().catch(e => { console.error(e.stack || String(e)); process.exitCode = 1; });
