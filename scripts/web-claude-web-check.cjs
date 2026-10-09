// Real-browser claude-web probe. Only run against an isolated test data copy:
// the upstream extension persists its theme/safety state when first enabled.
// Usage: node web-claude-web-check.cjs <base> <password-file>
const fs = require("node:fs");
const assert = require("node:assert/strict");
const puppeteer = require("puppeteer-core");

(async () => {
  const [base = "http://127.0.0.1:8002", passwordFile] = process.argv.slice(2);
  assert.ok(passwordFile, "A mounted password file is required");
  const password = fs.readFileSync(passwordFile, "utf8").trim();
  const browser = await puppeteer.launch({
    executablePath: "/usr/bin/chromium", headless: true, protocolTimeout: 240000,
    args: ["--no-sandbox", "--disable-dev-shm-usage", "--disable-gpu"],
  });
  const results = [];
  try {
    for (const viewport of [{ width: 1280, height: 900 }, { width: 390, height: 844, isMobile: true, hasTouch: true }]) {
      const page = await browser.newPage();
      await page.setViewport(viewport);
      await page.authenticate({ username: "tt", password });
      const pageErrors = [], failedInvokes = [], extensionFailures = [], logs = [];
      page.on("pageerror", e => pageErrors.push(String(e.stack || e).slice(0, 400)));
      page.on("console", m => { if (/claude.?web/i.test(m.text())) logs.push(m.text().slice(0, 250)); });
      page.on("requestfailed", r => {
        if (/third-party\/claude-web\//.test(r.url())) extensionFailures.push(r.failure()?.errorText + " " + new URL(r.url()).pathname);
      });
      page.on("response", async r => {
        if (r.url().includes("/__tt/invoke") && r.status() >= 400) {
          let command;
          try { command = JSON.parse(r.request().postData() || "{}").command; } catch {}
          failedInvokes.push({ command: command || r.request().headers()["x-tt-command"], status: r.status() });
        }
        if (/third-party\/claude-web\//.test(r.url()) && r.status() >= 400) extensionFailures.push(r.status() + " " + new URL(r.url()).pathname);
      });
      await page.goto(base + "/", { waitUntil: "domcontentloaded", timeout: 60000 });
      await page.waitForFunction(() => window.SillyTavern?.getContext?.()?.characters?.length > 0, { timeout: 120000 });
      await page.waitForFunction(() => document.querySelector("#claude-web-build")?.textContent.includes("2.0.127") && window.__claudeOfficialLayout && document.documentElement.dataset.claudeEnabled === "on", { timeout: 90000 });
      const version = await page.evaluate(async () => {
        const ctx = window.SillyTavern.getContext();
        const r = await fetch("/api/extensions/version", { method: "POST", headers: ctx.getRequestHeaders(), body: JSON.stringify({ extensionName: "claude-web", global: true }) });
        if (!r.ok) throw new Error("Extension version check failed: " + r.status);
        return r.json();
      });
      assert.equal(version.remoteUrl, "https://github.com/claudenoshujin/claude-web");
      const panelResults = [];
      for (const id of ["rm_extensions_block", "user-settings-block", "rm_api_block"]) {
        await page.evaluate(id => window.__claudeOfficialLayout.activate(id), id);
        await page.waitForFunction(id => document.querySelector("#cw-v4-settings")?.dataset.panel === id && !document.querySelector("#cw-v4-settings").hidden, { timeout: 30000 }, id);
        panelResults.push(id);
      }
      await page.evaluate(() => document.querySelector(".cw-v4-close")?.click());
      await page.waitForFunction(() => document.querySelector("#cw-v4-settings")?.hidden, { timeout: 30000 });
      await page.evaluate(async () => { await window.SillyTavern.getContext().selectCharacterById(0); });
      await page.waitForFunction(() => window.SillyTavern.getContext().chat.length > 0 && document.querySelector("#chat > .mes"), { timeout: 90000 });
      const state = await page.evaluate(() => {
        const composer = document.querySelector("#send_textarea");
        const rect = composer.getBoundingClientRect();
        return {
          build: document.querySelector("#claude-web-build").textContent,
          themeEnabled: document.documentElement.dataset.claudeEnabled,
          styleNodes: [...document.querySelectorAll("link[rel=stylesheet]")].filter(n => /claude-web/.test(n.href)).length,
          chatMessages: window.SillyTavern.getContext().chat.length,
          mountedMessages: document.querySelectorAll("#chat > .mes").length,
          composerVisible: rect.width > 0 && rect.height > 0 && rect.top < innerHeight && rect.bottom > 0,
          settingsMounted: !!document.querySelector("#claude-web-settings"),
        };
      });
      console.log(JSON.stringify({ viewport: viewport.width, state, version, panels: panelResults, failedInvokes, extensionFailures, pageErrors, logs }, null, 2));
      assert.ok(state.composerVisible, "Composer is not visible");
      assert.ok(state.settingsMounted && state.styleNodes >= 2, "Theme or settings missing");
      assert.equal(extensionFailures.length, 0, "claude-web resource load failed");
      assert.equal(pageErrors.length, 0, "Browser page raised uncaught errors");
      // The test copy has no Stable Diffusion WebUI; that provider failure is unrelated to CW.
      const unexpectedInvokes = failedInvokes.filter(e => e.command !== "sd_handle");
      assert.equal(unexpectedInvokes.length, 0, "Unexpected failed host commands");
      results.push({ viewport: viewport.width, state, version, panels: panelResults, failedInvokes, extensionFailures, pageErrors, logs });
      await page.close();
    }
    console.log(JSON.stringify({ ok: true, results }, null, 2));
  } finally { await browser.close(); }
})().catch(e => { console.error(e.stack || String(e)); process.exitCode = 1; });
