import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { runInNewContext } from 'node:vm';
import ts from 'typescript';

async function loadFeedback() {
    const source = await readFile(new URL('../src/scripts/download-feedback.js', import.meta.url), 'utf8');
    const { outputText } = ts.transpileModule(source, {
        compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
    });
    const feedback = {};
    runInNewContext(outputText, {
        exports: feedback,
        require: specifier => {
            assert.equal(specifier, './i18n.js');
            return { t: String.raw };
        },
    });
    return feedback;
}

test('data archive export button reports browser handoff, native delivery and cancellation distinctly', async () => {
    const feedback = await loadFeedback();
    const source = await readFile(new URL('../src/scripts/extensions/data-migration/index.js', import.meta.url), 'utf8');
    const { outputText } = ts.transpileModule(source, {
        compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
    });
    for (const result of [{ status: 'handed-off' }, { delivered: true }, { delivered: false }]) {
        const shown = [];
        const handlers = new Map();
        const state = new Map();
        const requests = [];
        const toastr = Object.fromEntries(['info', 'success', 'error', 'warning'].map(kind => [
            kind, (message, title) => shown.push({ kind, message, title }),
        ]));
        let initialized;
        const $ = selector => {
            const element = state.get(selector) ?? { text: '', disabled: false };
            state.set(selector, element);
            return {
                append: () => {},
                text: value => { element.text = value; },
                prop: (key, value) => { element[key] = value; },
                hide: () => {},
                show: () => {},
                on: (event, handler) => handlers.set(selector + ':' + event, handler),
            };
        };
        const modules = {
            '../../extensions.js': { renderExtensionTemplateAsync: async () => '<section />' },
            '../../i18n.js': { t: String.raw, translate: key => key },
            '../../popup.js': { Popup: {} },
            '../../download-feedback.js': {
                showExportSuccessToast: delivery => feedback.showExportSuccessToast(delivery, { toastrInstance: toastr }),
            },
            '../../tauritavern/ios-policy.js': { getActiveIosPolicyActivationReport: () => null },
            '../../../tauri/main/services/lifecycle/lifecycle-flush-service.js': { flushLifecycleState: async () => {} },
            '../../../tauri-bridge.js': { isWebHost: () => result.status === 'handed-off' },
            '../../../web/file-transfer.js': {},
            '../../../tauri/main/services/files/file-staging-service.js': {},
        };
        runInNewContext(outputText, {
            exports: {},
            require: specifier => {
                assert.ok(specifier in modules, specifier);
                return modules[specifier];
            },
            $,
            jQuery: ready => { initialized = ready(); },
            toastr,
            fetch: async url => {
                requests.push(url);
                const payload = url.endsWith('/export') ? { job_id: 'archive-job' }
                    : url.includes('/job?id=') ? { state: 'completed' } : result;
                return { ok: true, json: async () => payload };
            },
        });
        await initialized;
        await handlers.get('#data_migration_export_button:click')();
        assert.deepEqual(requests, [
            '/api/extensions/data-migration/export',
            '/api/extensions/data-migration/job?id=archive-job',
            '/api/extensions/data-migration/export/save',
        ]);
        assert.equal(state.get('#data_migration_export_button').disabled, false);
        const status = state.get('#data_migration_status').text;
        const delivered = shown.filter(toast => toast.kind === 'success');
        if (result.status === 'handed-off') {
            assert.equal(status, 'Download started');
            assert.equal(delivered.length, 0);
            assert.ok(shown.some(toast => toast.kind === 'info' && toast.message.includes('cannot be confirmed')));
        } else if (result.delivered) {
            assert.equal(status, 'Export completed');
            assert.equal(delivered.length, 1);
        } else {
            assert.equal(status, 'Export cancelled');
            assert.equal(delivered.length, 0);
        }
    }
});

test('export feedback confirms native delivery only, and reports browser handoff without claiming a save', async () => {
    const { showExportSuccessToast } = await loadFeedback();
    const shown = [];
    const toastrInstance = {
        success: message => shown.push(['success', message]),
        info: message => shown.push(['info', message]),
    };

    showExportSuccessToast({ delivered: true }, { toastrInstance });
    showExportSuccessToast({ delivered: false }, { toastrInstance });
    showExportSuccessToast({ status: 'handed-off' }, { toastrInstance });

    assert.deepEqual(shown.map(([kind]) => kind), ['success', 'info']);
    assert.equal(shown[0][1], 'Export completed.');
    assert.match(shown[1][1], /cannot be confirmed/);
});
