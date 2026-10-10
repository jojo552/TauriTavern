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
    window.__TAURITAVERN_STARTUP_STAGE__ = 'host-readiness';
    const calls = [];
    const reported = [];
    let completeCopy;
    let copyFailure = null;
    const modules = new Map();
    let finish;
    const failed = new Promise(resolve => { finish = resolve; });
    window.console.error = (...args) => reported.push(args);
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
                    const exports = specifier === './tauri-bridge.js' ? ['writeClipboardText'] : [];
                    const module = new vm.SyntheticModule(exports, function () {
                        if (specifier === './tauri-bridge.js') {
                            this.setExport('writeClipboardText', text => {
                                completeCopy(text);
                                return copyFailure ? Promise.reject(copyFailure) : Promise.resolve();
                            });
                        }
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
        const root = panel.shadowRoot;
        const details = root.querySelector('pre').textContent;
        assert.ok(details.includes(cause.message));
        assert.ok(details.includes('Stage: host-readiness'));
        assert.ok(details.includes(cause.stack));
        assert.equal(root.querySelector('img'), null, 'error text is not interpreted as HTML');
        assert.ok(root.textContent.includes('Startup has stopped'));
        assert.equal(panel.querySelector('button'), null, 'failure controls are isolated from page selectors');
        assert.equal(panel.style.all, 'initial', 'inherited page styles cannot suppress the failure host');
        assert.ok(reported.some(args => args.includes(cause)), 'the original startup error is still reported');
        const reload = root.querySelector('button[name=reload]');
        assert.equal(reload.textContent, 'Reload');
        assert.equal(root.activeElement, reload);
        const copy = root.querySelector('button[name=copy]');
        const copied = new Promise(resolve => { completeCopy = resolve; });
        copy.click();
        assert.equal(await copied, details, 'copy uses the same stage, message and stack as the panel');
        await window.happyDOM.waitUntilComplete();
        assert.equal(root.querySelector('[role=status]').textContent, 'Copied!');
        assert.deepEqual(calls, ['./lib.js', './tauri-main.js', './tauri-bridge.js'], 'the clipboard bridge loads only after clicking Copy');
        copyFailure = new Error('Clipboard permission denied');
        const denied = new Promise(resolve => { completeCopy = resolve; });
        copy.click();
        await denied;
        await window.happyDOM.waitUntilComplete();
        assert.ok(root.querySelector('[role=status]').textContent.includes(copyFailure.message));
        assert.equal(copy.disabled, false);
        assert.equal(window.document.getElementById('tt-startup-error'), panel, 'a copy failure does not replace recovery UI');
        let reloads = 0;
        window.location.reload = () => { reloads += 1; };
        reload.click();
        assert.equal(reloads, 1, 'recovery is an explicit page reload, not a replayed command');
    } finally {
        await window.happyDOM.close();
    }
});
