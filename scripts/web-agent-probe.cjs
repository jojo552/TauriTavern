// End-to-end agent run in a real browser: session profile bound to the user's current gproxy
// target, one short task, and the live projection + event stream observed until completion.
// Usage: node agent-probe.cjs <base> <user> <pass> <secretId>
const puppeteer = require("puppeteer-core");

(async () => {
  const [base, user, pass, secretId] = process.argv.slice(2);
  const browser = await puppeteer.launch({
    executablePath: "/usr/bin/chromium", headless: true,
    args: ["--no-sandbox", "--disable-dev-shm-usage", "--disable-gpu"],
    protocolTimeout: 600000,
  });
  const page = await browser.newPage();
  await page.authenticate({ username: user, password: pass });
  const failedInvokes = [];
  const pageErrors = [];
  page.on("pageerror", e => pageErrors.push(String(e.stack || e).slice(0, 400)));
  page.on("response", async r => {
    if (r.url().includes("/__tt/invoke") && r.status() >= 400) {
      let cmd = ""; try { cmd = JSON.parse(r.request().postData() || "{}").command; } catch {}
      let body = ""; try { body = (await r.text()).slice(0, 300); } catch {}
      if (cmd !== "sd_handle") failedInvokes.push(`${cmd} -> ${r.status()} ${body}`);
    }
  });
  await page.goto(base + "/", { waitUntil: "domcontentloaded", timeout: 60000 });
  await page.waitForFunction(() => window.SillyTavern?.getContext?.()?.characters?.length > 0, { timeout: 120000 });
  await new Promise(r => setTimeout(r, 10000));

  const result = await page.evaluate(async (secretId) => {
    const agent = window.__TAURITAVERN__.api.agent;
    const { createAssistantProfileForProbe } = { createAssistantProfileForProbe: null };
    void createAssistantProfileForProbe;
    const mt = await import("/scripts/tauritavern/agent/model-target-llm-connection.js");
    const target = {
      kind: "tauritavern.modelTarget", mode: "cc", id: "probe-gproxy", name: "gproxy claude",
      api: "custom_claude_messages", model: "cc/claude-sonnet-5-5",
      "custom-api-format": "claude_messages", "api-url": "https://g.223457.xyz/v1",
      secretRef: { key: "api_key_custom", id: secretId },
    };
    const loaded = await agent.sessions.profile.load();
    const profile = loaded.profile ?? (await import("/scripts/extensions/in-app-agent/dist/index.bundle.js").catch(() => null), null);
    const base = loaded.profile ?? {
      schemaVersion: 4, kind: "tauritavern.agentProfile", id: "in-app-assistant", displayName: "In-App Assistant",
      preset: { mode: "ref", ref: { apiId: "openai", name: "Default" }, required: true },
      run: { presentation: "foreground", stream: true, directRunnable: true, modelRetry: { maxRetries: 1, intervalMs: 2000 } },
      context: { initialChatHistoryMessages: -1, includeActivatedWorldInfo: false },
      delegation: { canDelegate: false, canHandoff: false, callable: false, allowAsSubagent: false, allowAsHandoffTarget: false,
        allowNestedDelegation: false, allowedCallers: ["*"], descriptionForAgents: null, maxConcurrentInvocations: 3,
        maxInvocationsPerRun: 8, resultBudgetTokens: 8000, maxHandoffDepth: 8 },
      instructions: { agentSystemPrompt: "You are a test agent. Do exactly what the user asks, briefly." },
      tools: { allow: ["extension/in-app-agent:app.evaluate"], deny: [], toolDescriptions: {}, maxRounds: 4, maxCallsPerRun: 6, externalResultInlineCharLimit: 50000, maxCallsPerTool: {} },
      skills: { visible: [], deny: [] },
      workspace: { visibleRoots: ["work", "tmp"], writableRoots: ["work", "tmp"] },
      plan: { mode: "none", beta: true, nodes: [] },
      output: { artifacts: [] },
    };
    void profile;
    base.model = mt.modelBindingFromTarget(target);
    const contextHost = window.SillyTavern.getContext();
    contextHost.extensionSettings.connectionManager ??= {};
    contextHost.extensionSettings.connectionManager.modelTargets = [...(contextHost.extensionSettings.connectionManager.modelTargets || []).filter(t => t.id !== target.id), target];
    await agent.sessions.profile.save(base);
    const { session } = await agent.sessions.create();
    // The page answers this tool call: it travels to the page over the event stream and back by invoke.
    window.__probeToolMarker = "browser-host-ok-" + Date.now();
    const handle = await agent.sessions.send({ sessionId: session.id, text: "Use app.evaluate exactly once to run `return window.__probeToolMarker;`. Then write a 3-sentence reply that starts with the returned value." });

    const live = [];
    const events = [];
    const unsubLive = agent.subscribeLiveProjection(handle.runId, u => live.push(u.type || u.kind || Object.keys(u)[0]), { onError: e => live.push("error: " + (e?.message || e)) });
    const unsubEvents = agent.subscribe(handle.runId, e => events.push(e.type || e.kind || e.event || "event"));
    const started = Date.now();
    let status = "";
    while (Date.now() - started < 180000) {
      await new Promise(r => setTimeout(r, 2000));
      const runs = await window.__TAURITAVERN__.invoke.safeInvoke("list_agent_runs", { dto: { limit: 5 } }).catch(e => ({ error: String(e) }));
      const run = (runs.runs || runs || []).find?.(r => (r.runId || r.id) === handle.runId);
      status = run?.status || status;
      if (["completed", "failed", "cancelled"].includes(status)) break;
    }
    unsubLive(); await Promise.resolve(unsubEvents());
    const transcript = await agent.sessions.read({ sessionId: session.id }).catch(e => ({ error: String(e) }));
    const reply = JSON.stringify(transcript).match(/browser-host-ok-\d+/g);
    return { runId: handle.runId, status, marker: window.__probeToolMarker, markerInReply: (reply || []).includes(window.__probeToolMarker),
      liveUpdates: live.length, liveKinds: [...new Set(live)].slice(0, 10), eventCount: events.length, eventKinds: [...new Set(events)].slice(0, 12) };
  }, secretId);

  console.log(JSON.stringify({ result, failedInvokes, pageErrors: pageErrors.slice(0, 10) }, null, 2));
  await browser.close();
})().catch(e => { console.error("driver failed:", e.stack || e.message); process.exit(1); });
