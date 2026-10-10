import assert from 'node:assert/strict';
import { test } from 'node:test';
import { readFile } from 'node:fs/promises';
import { runInNewContext } from 'node:vm';
import { Window } from 'happy-dom';
import ts from 'typescript';
import { installHostIdentity, HOSTS } from './helpers/host-identity.mjs';
import { resolveTauriTavernSettingsCapabilities } from '../src/scripts/tauri/setting/setting-panel/settings-view-model.js';

import {
    getExtensionMenuShortcutIds,
    setExtensionMenuShortcutIds,
} from '../src/scripts/tauri/setting/extension-menu-shortcuts.js';

test('menu rendering and its manager use host capabilities while preserving unavailable saved choices', async () => {
    const source = await readFile(new URL('../src/scripts/tauri/setting/extension-menu-shortcuts.js', import.meta.url), 'utf8');
    const { outputText } = ts.transpileModule(source, {
        compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
    });
    const previousWindow = globalThis.window;
    const dom = new Window();
    globalThis.window = dom;
    try {
        for (const host of [HOSTS.windows, { platform: 'browser', kind: 'web' }, HOSTS.ios]) {
            const restore = installHostIdentity(host);
            const allIds = ['sync', 'frontend-logs', 'backend-logs', 'llm-api-logs', 'reload-frontend'];
            dom.__TAURITAVERN__ = host.platform === 'ios'
                ? { iosPolicy: { scope: 'ios', capabilities: { sync: { lan: false } } } }
                : undefined;
            dom.document.body.innerHTML = '<div id="extensionsMenu"></div>';
            let customInputs;
            const api = {};
            const modules = {
                './setting-panel/settings-view-model.js': { resolveTauriTavernSettingsCapabilities },
                '../../extensions.js': { ensureExtensionsUiReady: async () => {}, showHideExtensionsMenu: () => {} },
                '../../keyboard.js': { makeKeyboardInteractable: () => {} },
                '../../i18n.js': { translate: key => key },
                './setting-panel/popup-utils.js': { runOrPopup: callback => callback() },
                '../../popup.js': { POPUP_RESULT: { AFFIRMATIVE: 1 }, POPUP_TYPE: { CONFIRM: 1 } },
                './panel-popup.js': {
                    createTauriTavernPanelPopup: (_content, _type, _value, options) => {
                        customInputs = options.customInputs;
                        return {
                            show: async () => 1,
                            inputResults: new Map(customInputs.map(input => [input.id, true])),
                        };
                    },
                },
            };
            runInNewContext(outputText, {
                exports: api,
                require: specifier => {
                    assert.ok(specifier in modules, specifier);
                    return modules[specifier];
                },
                document: dom.document,
                HTMLElement: dom.HTMLElement,
                localStorage: dom.localStorage,
                window: dom,
            });
            try {
                api.setExtensionMenuShortcutIds(allIds);
                await api.renderExtensionMenuShortcuts();
                // Only sync depends on the host: log panels are available everywhere.
                const expected = host.platform === 'windows' ? allIds : allIds.slice(1);
                const rendered = () => Array.from(dom.document.querySelectorAll('[role="button"]'), item => item.id);
                assert.deepEqual(rendered(), expected.map(id => 'tauritavern_shortcut_' + id));
                await api.openExtensionMenuShortcutsManager();
                assert.deepEqual(Array.from(customInputs, input => input.id), expected.map(id => 'tauritavern_shortcut_toggle_' + id));
                assert.deepEqual(Array.from(api.getExtensionMenuShortcutIds()), allIds);
                assert.deepEqual(rendered(), expected.map(id => 'tauritavern_shortcut_' + id));
            } finally {
                restore();
            }
        }
    } finally {
        globalThis.window = previousWindow;
        await dom.happyDOM.close();
    }
});

test('quick access selection defaults to Sync and preserves an explicit empty selection', () => {
    const values = new Map();
    const originalStorage = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
    Object.defineProperty(globalThis, 'localStorage', {
        configurable: true,
        value: {
            getItem: key => values.get(key) ?? null,
            setItem: (key, value) => values.set(key, String(value)),
        },
    });

    try {
        assert.deepEqual(getExtensionMenuShortcutIds(), ['sync']);

        setExtensionMenuShortcutIds([]);
        assert.deepEqual(getExtensionMenuShortcutIds(), []);

        assert.throws(() => setExtensionMenuShortcutIds(['unknown']), /unsupported shortcut: unknown/);
    } finally {
        if (originalStorage) {
            Object.defineProperty(globalThis, 'localStorage', originalStorage);
        } else {
            delete globalThis.localStorage;
        }
    }
});
