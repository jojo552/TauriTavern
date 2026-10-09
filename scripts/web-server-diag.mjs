// Diagnose which invokes fail. Usage: TT_PASSWORD=... node diag.mjs <base> <command> [jsonArgs]
const [base, command, rawArgs] = process.argv.slice(2);
const auth = 'Basic ' + Buffer.from(`tt:${process.env.TT_PASSWORD}`).toString('base64');
const host = new URL(base).host;
const h = extra => ({ authorization: auth, host, origin: base, 'sec-fetch-site': 'same-origin', ...extra });
const s = await fetch(base + '/__tt/session', { headers: h() });
const cookie = s.headers.get('set-cookie').split(';')[0];
const session = await s.json();
const ev = await fetch(`${base}/__tt/events?sessionId=${session.sessionId}`, { headers: h({ cookie }) });
await ev.body.getReader().read();
const r = await fetch(base + '/__tt/invoke', {
    method: 'POST',
    headers: h({ cookie, 'content-type': 'application/json', 'x-tt-csrf': session.csrfToken }),
    body: JSON.stringify({ sessionId: session.sessionId, command, args: rawArgs ? JSON.parse(rawArgs) : {} }),
});
const text = await r.text();
console.log(r.status, text.slice(0, 1500));
process.exit(0);
