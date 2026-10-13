// Shared Node protocol client. Node fetch has no browser Cookie jar: keep every Set-Cookie,
// including the independent authentication and browser-owner cookies, on this client only.
export function createWebServerClient(base, { publicOrigin = new URL(base).origin } = {}) {
    const target = new URL(base);
    const origin = new URL(publicOrigin);
    if (target.username || target.password || origin.username || origin.password) {
        throw new Error('Credentials must not appear in server URLs');
    }
    if (!['http:', 'https:'].includes(target.protocol) || !['http:', 'https:'].includes(origin.protocol)
        || origin.origin !== publicOrigin) {
        throw new Error('Expected an HTTP(S) server URL and a configured public origin');
    }
    const cookies = new Map();
    async function request(route, init = {}) {
        const url = new URL(route, target);
        if (url.origin !== target.origin) throw new Error('Server requests must remain same-origin');
        const headers = new Headers({ host: origin.host, origin: publicOrigin, 'sec-fetch-site': 'same-origin' });
        if (cookies.size) headers.set('cookie', [...cookies].map(([name, value]) => `${name}=${value}`).join('; '));
        for (const [name, value] of new Headers(init.headers)) headers.set(name, value);
        const response = await fetch(url, {
            signal: AbortSignal.timeout(15000), ...init, headers,
            credentials: 'same-origin', cache: 'no-store', redirect: 'manual',
        });
        // Never split Headers.get('set-cookie'): combined headers lose cookies and Expires has commas.
        for (const setCookie of response.headers.getSetCookie()) {
            const [pair, ...attributes] = setCookie.split(';');
            const separator = pair.indexOf('=');
            if (separator < 1) throw new Error('Server returned an invalid Cookie');
            const name = pair.slice(0, separator).trim();
            const value = pair.slice(separator + 1).trim();
            const expired = attributes.some(attribute => {
                const [key, ...parts] = attribute.trim().split('=');
                if (key.toLowerCase() === 'max-age') return Number(parts.join('=')) <= 0;
                if (key.toLowerCase() === 'expires') return Date.parse(parts.join('=')) <= Date.now();
                return false;
            });
            if (!value || expired) cookies.delete(name);
            else cookies.set(name, value);
        }
        return response;
    }
    async function login(username, password) {
        if (typeof username !== 'string' || !username || typeof password !== 'string' || !password) {
            throw new Error('A username and password are required for login');
        }
        const response = await request('/__tt/auth/login', {
            method: 'POST', headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ username, password }),
        });
        if (response.status !== 200) throw new Error(`Server login failed (HTTP ${response.status})`);
        const result = await response.json();
        if (result.ok !== true || !cookies.has('tt_auth') || !cookies.has('tt_browser')) {
            throw new Error('Server login did not establish authentication and browser ownership');
        }
        return response;
    }
    return { request, login };
}

// All protocol tools wait for a complete SSE data frame before invoking. Network chunks are
// not frame boundaries; comments and CRLF are legal and must not be mistaken for events.ready.
export function createEventReader(response) {
    if (!response.ok || !response.body) throw new Error(`Events failed (HTTP ${response.status})`);
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    async function nextFrame() {
        for (;;) {
            let boundary;
            while ((boundary = /\r?\n\r?\n/.exec(buffer))) {
                const frame = buffer.slice(0, boundary.index);
                buffer = buffer.slice(boundary.index + boundary[0].length);
                const data = frame.split(/\r?\n/).filter(line => line.startsWith('data:'))
                    .map(line => line.slice(5).replace(/^ /, '')).join('\n');
                if (data) return JSON.parse(data);
            }
            const { value, done } = await reader.read();
            if (done) throw new Error('Event stream ended before the expected frame');
            buffer += decoder.decode(value, { stream: true });
            if (buffer.length > 1 << 20) throw new Error('Event stream frame is unbounded');
        }
    }
    return {
        nextFrame,
        cancel: () => reader.cancel(),
        async waitForEnd() { while (!(await reader.read()).done) { /* Logout/close must end the stream. */ } },
    };
}
