// Diagnose one invoke without replaying failures. Usage: TT_PASSWORD=... node web-server-diag.mjs <base> <command> [jsonArgs]
import { createEventReader, createWebServerClient } from './web-server-client.mjs';

const [base, command, rawArgs] = process.argv.slice(2);
if (!base || !command) throw new Error('usage: web-server-diag.mjs <base> <command> [jsonArgs]');
const password = process.env.TT_PASSWORD;
if (!password) throw new Error('TT_PASSWORD is required');
const client = createWebServerClient(base, { publicOrigin: process.env.TT_PUBLIC_ORIGIN ?? new URL(base).origin });
const args = rawArgs ? JSON.parse(rawArgs) : {};
await client.login(process.env.TT_USER ?? 'tt', password);
const created = await client.request('/__tt/session');
if (!created.ok) throw new Error(`Session failed (HTTP ${created.status})`);
const session = await created.json();
const controller = new AbortController();
const events = await client.request(`/__tt/events?sessionId=${session.sessionId}`, { signal: controller.signal });
const reader = createEventReader(events);
try {
    const ready = await reader.nextFrame();
    if (ready.type !== 'ready' || ready.sessionId !== session.sessionId) throw new Error('Events did not confirm this page');
    const response = await client.request('/__tt/invoke', {
        method: 'POST', headers: { 'content-type': 'application/json', 'x-tt-csrf': session.csrfToken },
        body: JSON.stringify({ sessionId: session.sessionId, command, args }),
    });
    console.log(response.status, (await response.text()).slice(0, 1500));
    if (!response.ok) process.exitCode = 1;
} finally {
    controller.abort();
    await reader.cancel().catch(() => {});
    const closed = await client.request('/__tt/session/close', {
        method: 'POST', headers: { 'content-type': 'application/json', 'x-tt-csrf': session.csrfToken },
        body: JSON.stringify({ sessionId: session.sessionId }),
    });
    if (closed.status !== 204) throw new Error(`Page close failed (HTTP ${closed.status})`);
}
