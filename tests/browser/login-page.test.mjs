import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';
import { Window } from 'happy-dom';

const template = readFileSync(new URL('../../src-tauri/crates/tt-server/src/login.html', import.meta.url), 'utf8');
const json = (status, data) => ({ status, ok: status >= 200 && status < 300, json: async () => data });
// Drain resolved fetch/body promises, without waiting on an intentionally gated request.
const settle = () => new Promise(resolve => setImmediate(resolve));

function openPage(context, { mode = 'login', authenticated = false, status, login, logout } = {}) {
    const window = new Window({
        url: `http://localhost/${mode}?next=https://outside.invalid/`,
        settings: { disableJavaScriptEvaluation: true, disableCSSFileLoading: true, disableJavaScriptFileLoading: true },
    });
    context.after(() => window.happyDOM.close());
    window.document.write(template.replaceAll('__MODE__', mode).replaceAll('__NONCE__', 'test-nonce'));
    const calls = [];
    const navigations = [];
    const fetch = async (url, options = {}) => {
        calls.push({ url, options });
        switch (url) {
            case '/__tt/auth/status': return status ? status() : json(200, { authenticated });
            case '/__tt/auth/login':
                assert.ok(login, 'Unexpected login');
                return login();
            case '/__tt/auth/logout':
                assert.ok(logout, 'Unexpected logout');
                return logout();
            default: throw new Error(`Unexpected request: ${url}`);
        }
    };
    const sandbox = vm.createContext({ document: window.document, fetch, location: { replace: path => navigations.push(path) } });
    for (const script of window.document.querySelectorAll('script')) {
        vm.runInContext(script.textContent, sandbox, { filename: 'login.html' });
    }
    const element = id => window.document.getElementById(id);
    const submit = () => element('login-form').dispatchEvent(new window.Event('submit', { cancelable: true, bubbles: true }));
    const fill = () => {
        element('username').value = ' existing-user ';
        element('password').value = 'existing-password';
    };
    return { window, element, calls, navigations, submit, fill };
}

test('real login template submits exact credentials once and only enters the root after confirmed success', async context => {
    const pending = Promise.withResolvers();
    const page = openPage(context, { login: () => pending.promise });
    await settle();
    page.fill();
    page.element('show-password').checked = true;
    page.element('show-password').dispatchEvent(new page.window.Event('change'));
    assert.equal(page.element('password').type, 'text');
    page.submit();
    page.submit();
    assert.equal(page.element('login-fields').disabled, true);
    assert.equal(page.element('login-form').getAttribute('aria-busy'), 'true');
    const posts = page.calls.filter(call => call.url === '/__tt/auth/login');
    assert.equal(posts.length, 1);
    assert.equal(posts[0].options.method, 'POST');
    assert.equal(posts[0].options.credentials, 'same-origin');
    assert.equal(posts[0].options.headers['Content-Type'], 'application/json');
    assert.deepEqual(JSON.parse(posts[0].options.body), { username: ' existing-user ', password: 'existing-password' });
    assert.deepEqual(page.navigations, []);
    pending.resolve(json(200, { ok: true }));
    await settle();
    assert.deepEqual(page.navigations, ['/']);
    assert.equal(page.element('password').value, '');
    page.submit();
    assert.equal(page.calls.filter(call => call.url === '/__tt/auth/login').length, 1, 'success remains disabled until navigation');
});

test('failed credentials, rate limits, malformed success and network failures never navigate', async context => {
    const serverError = '<img src=x onerror="alert(1)">用户名或密码错误';
    const cases = [
        { name: '401 server error is text', result: () => json(401, { error: serverError }), expected: serverError },
        { name: '429 rate limit', result: () => json(429, { error: '尝试过于频繁，请稍后再试。' }), expected: '尝试过于频繁，请稍后再试。' },
        { name: '200 without confirmation', result: () => json(200, { ok: false }) },
        { name: 'invalid JSON', result: () => ({ status: 200, ok: true, json: async () => { throw new SyntaxError('Invalid JSON'); } }) },
        { name: 'network error', result: () => { throw new TypeError('Failed to fetch'); } },
    ];
    for (const scenario of cases) {
        await context.test(scenario.name, async child => {
            const page = openPage(child, { login: scenario.result });
            await settle();
            page.fill();
            page.submit();
            await settle();
            assert.deepEqual(page.navigations, []);
            assert.equal(page.element('login-fields').disabled, false);
            assert.equal(page.element('message').dataset.error, 'true');
            assert.ok(page.element('message').textContent.length > 0);
            if (scenario.expected) assert.equal(page.element('message').textContent, scenario.expected);
            assert.equal(page.element('message').children.length, 0, 'server errors must not create HTML');
        });
    }
});

test('authenticated login stays accessible and offers explicit logout without booting an application session', async context => {
    const page = openPage(context, { authenticated: true, logout: () => json(204, null) });
    await settle();
    assert.deepEqual(page.navigations, []);
    assert.deepEqual(page.calls.map(call => call.url), ['/__tt/auth/status']);
    assert.equal(page.calls[0].options.credentials, 'same-origin');
    assert.equal(page.element('login-form').hidden, true);
    assert.equal(page.element('authenticated-actions').hidden, false);
    assert.equal(page.element('authenticated-actions').querySelector('a').getAttribute('href'), '/');
    page.element('logout-button').click();
    await settle();
    assert.equal(page.element('login-form').hidden, false);
    assert.equal(page.element('authenticated-actions').hidden, true);
    assert.deepEqual(page.navigations, []);
});

test('logout page only sends logout on active confirmation and then offers the login entry', async context => {
    const pending = Promise.withResolvers();
    const page = openPage(context, { mode: 'logout', authenticated: true, logout: () => pending.promise });
    await settle();
    assert.deepEqual(page.calls.map(call => call.url), ['/__tt/auth/status']);
    assert.equal(page.element('authenticated-actions').hidden, false);
    page.element('logout-button').click();
    page.element('logout-button').click();
    const posts = page.calls.filter(call => call.url === '/__tt/auth/logout');
    assert.equal(posts.length, 1);
    assert.equal(posts[0].options.method, 'POST');
    assert.equal(posts[0].options.credentials, 'same-origin');
    assert.equal(posts[0].options.headers['Content-Type'], 'application/json');
    assert.deepEqual(JSON.parse(posts[0].options.body), {});
    pending.resolve(json(204, null));
    await settle();
    assert.equal(page.element('authenticated-actions').hidden, true);
    assert.equal(page.element('login-form').hidden, true);
    assert.equal(page.element('login-link').hidden, false);
    assert.equal(page.element('login-link').getAttribute('href'), '/login');
    assert.deepEqual(page.navigations, []);
});

test('unauthenticated logout only offers login; uncertain initial status blocks credential submission until checked', async context => {
    const logoutPage = openPage(context, { mode: 'logout' });
    await settle();
    assert.equal(logoutPage.element('login-link').hidden, false);
    assert.equal(logoutPage.element('authenticated-actions').hidden, true);
    assert.deepEqual(logoutPage.calls.map(call => call.url), ['/__tt/auth/status']);

    let online = false;
    const loginPage = openPage(context, { status: () => {
        if (!online) throw new TypeError('Offline');
        return json(200, { authenticated: false });
    } });
    await settle();
    loginPage.fill();
    loginPage.submit();
    assert.equal(loginPage.element('login-form').hidden, true);
    assert.equal(loginPage.element('retry-status').hidden, false);
    assert.deepEqual(loginPage.calls.map(call => call.url), ['/__tt/auth/status']);
    online = true;
    loginPage.element('retry-status').click();
    await settle();
    assert.equal(loginPage.element('login-form').hidden, false);
    assert.equal(loginPage.element('retry-status').hidden, true);
});
