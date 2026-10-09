import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { Window } from 'happy-dom';
import { installHostIdentity } from '../helpers/host-identity.mjs';

// Drive init.js itself, with only the three heavyweight application imports replaced.
test('browser readiness failure stops startup and replaces the preloader with a safe reloadable error', async () => {
    const window = new Window({ url: 'http://localhost/' });
    installHostIdentity({ platform: 'browser', kind: 'web' }, window);
    window.document.body.innerHTML = '<div id="preloader"></div>';
    const cause = new Error('SSE session mismatch <img src=x onerror=alert(1)>');
    const calls = [];
    const modules = new Map();
    let finish;
    const failed = new Promise(resolve => { finish = resolve; });
    window.console.error = () => {};
    const append = window.document.body.appendChild.bind(window.document.body);
    window.document.body.appendChild = element => {
        const result = append(element);
        finish();
        return result;
    };
    function getModule(url) {
        const key = url.href;
        if (!modules.has(key)) {
            modules.set(key, new vm.SourceTextModule(readFileSync(url, 'utf8'), {
                context: window,
                identifier: key,
                importModuleDynamically: async specifier => {
                    calls.push(specifier);
                    const module = new vm.SyntheticModule([], function () {
                        if (specifier === './tauri-main.js') {
                            window.__TAURITAVERN_MAIN_READY__ = Promise.reject(cause);
                        }
                    }, { context: window });
                    await module.link(() => {});
                    await module.evaluate();
                    return module;
                },
            }));
        }
        return modules.get(key);
    }
    try {
        const entry = getModule(new URL('../../src/init.js', import.meta.url));
        await entry.link((specifier, owner) => getModule(new URL(specifier, owner.identifier)));
        await entry.evaluate();
        await failed;
        assert.deepEqual(calls, ['./lib.js', './tauri-main.js']);
        assert.equal(window.document.getElementById('preloader'), null);
        const panel = window.document.querySelector('[role="alert"]');
        assert.ok(panel.textContent.includes(cause.message));
        assert.equal(panel.querySelector('img'), null, 'error text is not interpreted as HTML');
        assert.ok(panel.textContent.includes('Startup has stopped'));
        const reload = panel.querySelector('button');
        assert.equal(reload.textContent, 'Reload');
        assert.equal(window.document.activeElement, reload);
        let reloads = 0;
        window.location.reload = () => { reloads += 1; };
        reload.click();
        assert.equal(reloads, 1, 'recovery is an explicit page reload, not a replayed command');
    } finally {
        await window.happyDOM.close();
    }
});
