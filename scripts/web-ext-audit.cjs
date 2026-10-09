// Load the app in a real browser and report every extension's load state and attributable error.
// Usage: node ext-audit.cjs <base> <user> <pass>
const puppeteer = require("puppeteer-core");

(async () => {
  const [base, user, pass] = process.argv.slice(2);
  const browser = await puppeteer.launch({
    executablePath: "/usr/bin/chromium", headless: true,
    args: ["--no-sandbox", "--disable-dev-shm-usage", "--disable-gpu"],
  });
  const page = await browser.newPage();
  await page.setViewport({ width: 1280, height: 900 });
  await page.authenticate({ username: user, password: pass });

  const errors = [];
  const failedInvokes = new Map();
  const failedRequests = [];
  const origin = new URL(base).origin;
  page.on("console", m => { if (m.type() === "error") errors.push("console: " + m.text().slice(0, 400)); });
  page.on("pageerror", e => errors.push("pageerror: " + String(e.stack || e).slice(0, 700)));
  page.on("requestfailed", r => failedRequests.push(r.url().slice(0, 160) + " " + (r.failure() || {}).errorText));
  page.on("response", async r => {
    const url = r.url();
    if (url.includes("/__tt/invoke") && r.status() >= 400) {
      let cmd = "";
      try { cmd = JSON.parse(r.request().postData() || "{}").command || r.request().headers()["x-tt-command"]; } catch {}
      let body = "";
      try { body = (await r.text()).slice(0, 220); } catch {}
      failedInvokes.set(cmd, `${r.status()} ${body}`);
    } else if (!url.includes("/__tt/") && r.status() >= 400 && new URL(url).origin === origin) {
      failedRequests.push(`${r.status()} ${url.slice(0, 160)}`);
    }
  });

  await page.goto(base + "/", { waitUntil: "domcontentloaded", timeout: 60000 });
  await page.waitForFunction(() => window.SillyTavern?.getContext?.()?.characters?.length > 0, { timeout: 120000 });
  await new Promise(r => setTimeout(r, 20000));

  const ext = await page.evaluate(async () => {
    const mod = await import("/scripts/extensions.js");
    const settings = window.SillyTavern.getContext().extensionSettings || {};
    return {
      activeExtensions: mod.activeExtensions ? [...mod.activeExtensions] : null,
      extensionTypes: mod.extensionTypes || null,
      disabled: settings.disabledExtensions || null,
      globals: {
        TavernHelper: typeof window.TavernHelper,
        Mvu: typeof window.Mvu,
        LittleWhiteBox: typeof window.LittleWhiteBox,
        EjsTemplate: typeof window.EjsTemplate,
      },
    };
  });

  console.log(JSON.stringify({
    ext,
    failedInvokes: Object.fromEntries(failedInvokes),
    failedRequests: [...new Set(failedRequests)].slice(0, 40),
    errors: [...new Set(errors)].slice(0, 80),
  }, null, 2));
  await browser.close();
})().catch(e => { console.error("driver failed:", e.stack || e.message); process.exit(1); });
