// Real-browser expressions upload probe; use an isolated test data copy only.
// Uploads to a random sprite set and deletes its test images on completion.
// Usage: node web-sprite-check.cjs <base> <password-file>
const fs = require("node:fs");
const assert = require("node:assert/strict");
const puppeteer = require("puppeteer-core");
const png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aRFcAAAAASUVORK5CYII=";
const zip = "UEsDBBQAAAAAAMZ5SF2w8A1iRAAAAEQAAAAHAAAAam95LnBuZ4lQTkcNChoKAAAADUlIRFIAAAABAAAAAQgEAAAAtRwMAgAAAAtJREFUeNpj/P8fAAMDAgDvmkRXAAAAAElFTkSuQmCCUEsDBBQAAAAAAMZ5SF2w8A1iRAAAAEQAAAAHAAAAc2FkLnBuZ4lQTkcNChoKAAAADUlIRFIAAAABAAAAAQgEAAAAtRwMAgAAAAtJREFUeNpj/P8fAAMDAgDvmkRXAAAAAElFTkSuQmCCUEsBAhQDFAAAAAAAxnlIXbDwDWJEAAAARAAAAAcAAAAAAAAAAAAAAIABAAAAAGpveS5wbmdQSwECFAMUAAAAAADGeUhdsPANYkQAAABEAAAABwAAAAAAAAAAAAAAgAFpAAAAc2FkLnBuZ1BLBQYAAAAAAgACAGoAAADSAAAAAAA=";

(async () => {
  const [base = "http://127.0.0.1:8002", passwordFile] = process.argv.slice(2);
  assert.ok(passwordFile, "A mounted password file is required");
  const browser = await puppeteer.launch({ executablePath: "/usr/bin/chromium", headless: true,
    args: ["--no-sandbox", "--disable-dev-shm-usage", "--disable-gpu"] });
  try {
    const page = await browser.newPage();
    await page.authenticate({ username: "tt", password: fs.readFileSync(passwordFile, "utf8").trim() });
    await page.goto(base + "/", { waitUntil: "domcontentloaded", timeout: 60000 });
    await page.waitForFunction(() => window.SillyTavern?.getContext?.()?.characters?.length > 0, { timeout: 120000 });
    const result = await page.evaluate(async ({ png, zip }) => {
      const name = "tt-web-sprite-probe-" + crypto.randomUUID();
      const ctx = window.SillyTavern.getContext();
      const headers = ctx.getRequestHeaders();
      const check = (ok, why) => { if (!ok) throw new Error(why); };
      const bytes = data => Uint8Array.from(atob(data), c => c.charCodeAt(0));
      const getList = async () => {
        const r = await fetch("/api/sprites/get?name=" + encodeURIComponent(name));
        check(r.ok, "Sprite list failed: " + r.status);
        return r.json();
      };
      const upload = async (path, data, filename, label) => {
        const body = new FormData();
        body.set("name", name);
        if (label) body.set("label", label);
        body.set("avatar", new File([bytes(data)], filename));
        const r = await fetch(path, { method: "POST", body });
        check(r.ok, "Sprite upload failed: " + r.status + " " + await r.clone().text());
        return r.json();
      };
      const out = { name };
      try {
        check((await getList()).length === 0, "Test sprite set must be new");
        out.single = await upload("/api/sprites/upload", png, "neutral.png", "neutral");
        const single = await getList();
        check(single.length === 1 && single[0].label === "neutral", "Single image missing from list");
        const r = await fetch(single[0].path);
        check(r.ok, "Uploaded sprite resource failed: " + r.status);
        const actual = new Uint8Array(await r.arrayBuffer());
        const expected = bytes(png);
        check(actual.length === expected.length && actual.every((b, i) => b === expected[i]), "Sprite bytes changed");
        out.pack = await upload("/api/sprites/upload-zip", zip, "sprites.zip");
        check(out.pack.count === 2, "ZIP must import two images");
        out.labels = (await getList()).map(s => s.label).sort();
        check(JSON.stringify(out.labels) === JSON.stringify(["joy", "neutral", "sad"]), "ZIP contents missing");
        for (const command of ["upload_sprite", "upload_sprite_pack"]) {
          let error;
          try {
            await window.__TAURITAVERN__.invoke.safeInvoke(command, { dto: {
              name, sprite_name: "outside", original_filename: "outside.png", file_path: "/etc/passwd",
            } });
          } catch (e) { error = String(e?.message || e); }
          check(error && /staging/i.test(error), "Outside staging path must be refused: " + command + " " + error);
        }
        out.outsidePathsRefused = true;
      } finally {
        for (const sprite of await getList()) {
          const r = await fetch("/api/sprites/delete", { method: "POST", headers,
            body: JSON.stringify({ name, spriteName: sprite.label }) });
          check(r.ok, "Test sprite cleanup failed: " + r.status);
        }
        check((await getList()).length === 0, "Test images left behind");
      }
      return { ...out, cleanedUp: true };
    }, { png, zip });
    console.log(JSON.stringify({ ok: true, result }, null, 2));
  } finally { await browser.close(); }
})().catch(e => { console.error(e.stack || String(e)); process.exitCode = 1; });
