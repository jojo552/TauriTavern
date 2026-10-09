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
