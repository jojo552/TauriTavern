// Verifies server push over the page event stream: a channel registered by invoke receives
// frames, and closing the page tears down its registrations (the tool disappears from the list).
// Usage: TT_PASSWORD=... node web-channel-probe.mjs <base>
import { createEventReader, createWebServerClient } from './web-server-client.mjs';

const [base] = process.argv.slice(2);
if (!base) throw new Error('usage: web-channel-probe.mjs <base>');
const password = process.env.TT_PASSWORD;
if (!password) throw new Error('TT_PASSWORD is required');
const publicOrigin = process.env.TT_PUBLIC_ORIGIN ?? new URL(base).origin;
const check = (c, m) => { if (!c) throw new Error(m); };

async function openPage() {
    const client = createWebServerClient(base, { publicOrigin });
    await client.login(process.env.TT_USER ?? 'tt', password);
    const created = await client.request('/__tt/session');
    check(created.ok, `session failed ${created.status}`);
    const session = await created.json();
    const controller = new AbortController();
    const events = await client.request(`/__tt/events?sessionId=${session.sessionId}`, { signal: controller.signal });
    const reader = createEventReader(events);
    async function invoke(command, args) {
        const r = await client.request('/__tt/invoke', {
            method: 'POST', headers: { 'content-type': 'application/json', 'x-tt-csrf': session.csrfToken },
            body: JSON.stringify({ sessionId: session.sessionId, command, args }),
        });
        const v = await r.json();
        if (!r.ok) throw Object.assign(new Error(`${command}: ${JSON.stringify(v)}`), { status: r.status });
        return v;
    }
    return { session, nextFrame: reader.nextFrame, invoke, close: async () => {
        const response = await client.request('/__tt/session/close', {
            method: 'POST', headers: { 'content-type': 'application/json', 'x-tt-csrf': session.csrfToken },
            body: JSON.stringify({ sessionId: session.sessionId }),
        });
        check(response.status === 204, 'page close must confirm cleanup');
        controller.abort();
        await reader.cancel().catch(() => {});
    } };
}

const page = await openPage();
const ready = await page.nextFrame();
check(ready.type === 'ready' && ready.sessionId === page.session.sessionId, 'first frame must confirm this page');

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
const otherReady = await other.nextFrame();
check(otherReady.type === 'ready' && otherReady.sessionId === other.session.sessionId, 'second page must receive matching ready');
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
