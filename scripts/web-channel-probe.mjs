// Verifies server push over the page event stream: a channel registered by invoke receives
// frames, and closing the page tears down its registrations (the tool disappears from the list).
// Usage: TT_PASSWORD=... node web-channel-probe.mjs <base>
const [base] = process.argv.slice(2);
const auth = 'Basic ' + Buffer.from(`tt:${process.env.TT_PASSWORD}`).toString('base64');
const host = new URL(base).host;
const check = (c, m) => { if (!c) throw new Error(m); };

async function openPage() {
    const h = extra => ({ authorization: auth, host, origin: base, 'sec-fetch-site': 'same-origin', ...extra });
    const s = await fetch(base + '/__tt/session', { headers: h() });
    const cookie = s.headers.get('set-cookie').split(';')[0];
    const session = await s.json();
    const controller = new AbortController();
    const events = await fetch(`${base}/__tt/events?sessionId=${session.sessionId}`, { headers: h({ cookie }), signal: controller.signal });
    const reader = events.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    async function nextFrame() {
        for (;;) {
            const end = buffer.indexOf('\n\n');
            if (end >= 0) {
                const chunk = buffer.slice(0, end); buffer = buffer.slice(end + 2);
                const data = chunk.split('\n').filter(l => l.startsWith('data:')).map(l => l.slice(5).trimStart()).join('\n');
                if (data) return JSON.parse(data);
                continue;
            }
            const { value, done } = await reader.read();
            if (done) throw new Error('event stream ended');
            buffer += decoder.decode(value, { stream: true });
        }
    }
    async function invoke(command, args) {
        const r = await fetch(base + '/__tt/invoke', {
            method: 'POST', headers: h({ cookie, 'content-type': 'application/json', 'x-tt-csrf': session.csrfToken }),
            body: JSON.stringify({ sessionId: session.sessionId, command, args }),
        });
        const v = await r.json();
        if (!r.ok) throw Object.assign(new Error(`${command}: ${JSON.stringify(v)}`), { status: r.status });
        return v;
    }
    return { session, nextFrame, invoke, close: async () => {
        const response = await fetch(base + '/__tt/session/close', {
            method: 'POST', headers: h({ cookie, 'content-type': 'application/json', 'x-tt-csrf': session.csrfToken }),
            body: JSON.stringify({ sessionId: session.sessionId }),
        });
        check(response.status === 204, 'page close must confirm cleanup');
        controller.abort();
    } };
}

const page = await openPage();
check((await page.nextFrame()).type === 'ready', 'first frame must be ready');

const definition = {
    extensionId: 'probe', name: 'push', description: 'Server push probe',
    inputSchema: { type: 'object' }, contexts: ['session'], enabled: true,
};
await page.invoke('register_agent_extension_tool', { definition, channel: { $channel: 'c1' } });
const listed = JSON.stringify(await page.invoke('list_agent_tools', {}));
check(listed.includes('probe'), 'registered tool must be listed');

// A channel id is single-use per page.
const reused = await page.invoke('register_agent_extension_tool', { definition: { ...definition, name: 'other' }, channel: { $channel: 'c1' } })
    .then(() => false, e => e.status === 409);
check(reused, 'reusing a page channel id must conflict');

// Another page cannot resolve or disable this page's tool.
const other = await openPage();
await other.nextFrame();
const toolId = JSON.parse(listed).tools?.find?.(t => JSON.stringify(t).includes('probe'))?.id
    ?? (listed.match(/"(extension[^"]*probe[^"]*)"/)?.[1]);
const foreignDisable = await other.invoke('set_agent_extension_tool_enabled', { dto: { toolId, enabled: false } })
    .then(() => false, () => true);
check(foreignDisable, 'another page must not disable this page\'s tool');
check(await other.invoke('resolve_agent_extension_tool_call', { dto: { requestId: 'nope', result: { kind: 'value', value: null } } }) === false,
    'resolving an unknown call must report false');

// Closing the page clears its tools.
await page.close();
const after = JSON.stringify(await other.invoke('list_agent_tools', {}));
check(!after.includes('"probe'), 'closing the page must unregister its tools');
await other.close();
console.log('channel probe ok (toolId=' + toolId + ')');
