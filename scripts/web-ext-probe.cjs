// Functional probe: exercise each installed extension through its public surface in a real
// browser, and report what works. Read-only except where noted (no generation requests).
// Usage: node ext-probe.cjs <base> <user> <pass> <characterName>
const puppeteer = require("puppeteer-core");

(async () => {
  const [base, user, pass, charName] = process.argv.slice(2);
  const browser = await puppeteer.launch({
    executablePath: "/usr/bin/chromium", headless: true,
    args: ["--no-sandbox", "--disable-dev-shm-usage", "--disable-gpu"],
  });
  const page = await browser.newPage();
  await page.setViewport({ width: 1280, height: 900 });
  await page.authenticate({ username: user, password: pass });
  const errors = [];
  const failedInvokes = [];
  page.on("pageerror", e => errors.push(String(e.stack || e).slice(0, 500)));
  page.on("console", m => { if (m.type() === "error") errors.push("console: " + m.text().slice(0, 300)); });
  page.on("response", async r => {
    if (r.url().includes("/__tt/invoke") && r.status() >= 400) {
      let cmd = "";
      try { cmd = JSON.parse(r.request().postData() || "{}").command || r.request().headers()["x-tt-command"]; } catch {}
      let body = ""; try { body = (await r.text()).slice(0, 200); } catch {}
      failedInvokes.push(`${cmd} -> ${r.status()} ${body}`);
    }
  });

  await page.goto(base + "/", { waitUntil: "domcontentloaded", timeout: 60000 });
  await page.waitForFunction(() => window.SillyTavern?.getContext?.()?.characters?.length > 0, { timeout: 120000 });
  await new Promise(r => setTimeout(r, 12000));

  const chid = await page.evaluate(name => window.SillyTavern.getContext().characters.findIndex(c => c.name === name), charName);
  await page.evaluate(async id => { await window.SillyTavern.getContext().selectCharacterById(id); }, chid);
  await new Promise(r => setTimeout(r, 10000));

  const results = await page.evaluate(async () => {
    const ctx = window.SillyTavern.getContext();
    const out = {};
    const run = async (name, fn) => {
      try { out[name] = { ok: true, value: await fn() }; }
      catch (e) { out[name] = { ok: false, error: String(e?.stack || e).slice(0, 400) }; }
    };

    // Built-in slash commands go through the same execution path extensions use.
    await run("slash /echo", async () => (await ctx.executeSlashCommandsWithOptions("/pass hello-slash")).pipe);
    await run("slash /getvar after /setvar", async () => {
      await ctx.executeSlashCommandsWithOptions("/setvar key=probe_var probe-value");
      return (await ctx.executeSlashCommandsWithOptions("/getvar probe_var")).pipe;
    });

    // JS-Slash-Runner (TavernHelper): chat, character and variable APIs.
    await run("TavernHelper present", async () => Object.keys(window.TavernHelper || {}).length);
    await run("TavernHelper.getChatMessages", async () => {
      const messages = window.TavernHelper.getChatMessages("0-{{lastMessageId}}");
      return Array.isArray(messages) ? messages.length : typeof messages;
    });
    await run("TavernHelper.getVariables(chat)", async () => typeof window.TavernHelper.getVariables({ type: "chat" }));
    await run("TavernHelper.getCharData", async () => window.TavernHelper.getCharData?.("current")?.name ?? null);
    await run("TavernHelper.getWorldbookNames", async () => (await window.TavernHelper.getWorldbookNames?.())?.length ?? "n/a");
    await run("TavernHelper.getScriptTrees/getTavernRegexes", async () => (window.TavernHelper.getTavernRegexes?.({ scope: "all" }) ?? []).length);

    // ST-Prompt-Template (EjsTemplate): render a template that reads chat variables.
    await run("EjsTemplate present", async () => typeof window.EjsTemplate);
    await run("EjsTemplate.evalTemplate", async () =>
      await window.EjsTemplate.evalTemplate?.("<%= 1 + 2 %>-<%= typeof variables %>", {}) ?? "no evalTemplate");

    // LittleWhiteBox: its global namespace and settings storage round trip through /api/files.
    await run("LittleWhiteBox XB", async () => Object.keys(window.XB || {}).slice(0, 12));
    await run("LittleWhiteBox modules", async () => ["xiaobaixAssistant", "xiaobaixDraw", "xiaobaixTavern", "xiaobaixTts"]
      .map(k => `${k}:${typeof window[k]}`));

    // Extension store: JSON and blob round trips go through commit sessions and byte readers.
    await run("extension store JSON round trip", async () => {
      const store = window.__TAURITAVERN__.api.extension.store;
      await store.setJson({ namespace: "probe", key: "json", value: { at: 1, text: "中文" } });
      const value = await store.getJson({ namespace: "probe", key: "json" });
      await store.deleteJson({ namespace: "probe", key: "json" });
      return value;
    });
    await run("extension store blob round trip", async () => {
      const store = window.__TAURITAVERN__.api.extension.store;
      const bytes = new Uint8Array(300000).map((_, i) => i % 251);
      await store.setBlob({ namespace: "probe", key: "blob.bin", data: new Blob([bytes]) });
      const blob = await store.getBlob({ namespace: "probe", key: "blob.bin" });
      const back = new Uint8Array(await blob.arrayBuffer());
      await store.deleteBlob({ namespace: "probe", key: "blob.bin" });
      return back.length === bytes.length && back.every((b, i) => b === bytes[i]);
    });

    // Agent extension tool: register through the page channel (SSE), then list it via the agent.
    await run("agent tool register + list", async () => {
      const tools = window.__TAURITAVERN__.api.agent.tools;
      await tools.register({
        extensionId: "probe", name: "echo", description: "Echo arguments back",
        inputSchema: { type: "object", properties: { text: { type: "string" } } },
        contexts: ["chat", "session"], enabled: true,
      }, async (args) => ({ echoed: args.text }));
      const listed = await window.__TAURITAVERN__.invoke.safeInvoke("list_agent_tools", {});
      const all = JSON.stringify(listed);
      return all.includes("probe") ? "registered and listed" : "registered but not listed: " + all.slice(0, 200);
    });

    // Data archive export: the same job service as native, delivered as a one-shot download.
    await run("data archive export (small user backup)", async () => {
      const response = await fetch("/api/users/backup", { method: "POST", headers: ctx.getRequestHeaders(), body: JSON.stringify({ handle: "default-user" }) });
      const payload = await response.json();
      return { status: response.status, handedOff: payload.status, includesSecrets: payload.includes_secrets };
    });
    // Live projection on a finished/unknown run must return immediately with an empty snapshot.
    await run("agent live projection (no active run)", async () => {
      const updates = [];
      await new Promise((resolve, reject) => {
        const unsubscribe = window.__TAURITAVERN__.api.agent.subscribeLiveProjection("run_nonexistent", u => { updates.push(u.kind ?? u.type ?? Object.keys(u)[0]); }, { onError: reject });
        setTimeout(() => { unsubscribe(); resolve(); }, 3000);
      });
      return updates;
    });
    // Notifications use the browser's own permission model.
    await run("notification permission state", async () => typeof Notification === "function" ? Notification.permission : "no Notification API");
    await run("update check", async () => {
      const result = await window.__TAURITAVERN__.invoke.safeInvoke("check_for_update", { channel: "stable" });
      return { has_update: result.has_update, current: result.current_version };
    });

    // Extension management: what the extensions panel queries.
    await run("get_extension_version JS-Slash-Runner", async () =>
      await window.__TAURITAVERN__.invoke.safeInvoke("get_extension_version", { extensionName: "JS-Slash-Runner", global: true }));

    // World info, tokenizer, quick reply: backends that extensions commonly call.
    await run("tokenizer count", async () => await ctx.getTokenCountAsync("测试 token count"));
    await run("world info get", async () => {
      const name = (await window.TavernHelper.getWorldbookNames())[0];
      const r = await fetch("/api/worldinfo/get", { method: "POST", headers: ctx.getRequestHeaders(), body: JSON.stringify({ name }) });
      return `${name}: ${Object.keys((await r.json()).entries || {}).length} entries`;
    });
    await run("quick reply sets", async () => (window.quickReplyApi?.listSets?.() ?? []).length);
    return out;
  });

  console.log(JSON.stringify({ results, failedInvokes, errors: [...new Set(errors)].slice(0, 40) }, null, 2));
  await browser.close();
})().catch(e => { console.error("driver failed:", e.stack || e.message); process.exit(1); });
