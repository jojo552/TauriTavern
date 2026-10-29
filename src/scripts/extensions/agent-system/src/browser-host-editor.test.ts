import { afterEach, beforeEach, expect, rstest, test } from '@rstest/core';
import { EditorView } from '@codemirror/view';

import * as editorRuntime from '../../../tauri/codemirror-editor.js';

// The shared editor runtime has no test project of its own; Agent System's CodeMirrorTextarea is one of its users.
const { loadBundle } = rstest.hoisted(() => ({
    loadBundle: rstest.fn<() => Promise<typeof import('../../../../lib-bundle-editor.js')>>(),
}));
rstest.mock('../../../../lib.js', () => ({ getCodeMirrorEditor: loadBundle }));
rstest.mock('../../../popup.js', () => ({ callGenericPopup: rstest.fn(), POPUP_TYPE: { TEXT: 1 } }));
rstest.mock('../../../i18n.js', () => ({ t: (strings: TemplateStringsArray) => strings.join('') }));
rstest.mock('../../../utils.js', () => ({ copyText: rstest.fn() }));

beforeEach(() => {
    Object.defineProperty(window, '__TAURITAVERN_HOST__', { configurable: true, value: { platform: 'browser', kind: 'web' } });
    editorRuntime.initializeCodeMirrorEditor({ codemirror_editor_enabled: true });
    loadBundle.mockImplementation(() => import('../../../../lib-bundle-editor.js'));
});

afterEach(() => {
    Reflect.deleteProperty(navigator, 'userAgentData');
    document.body.replaceChildren();
});

function setClientHints(value: { brands: { brand: string, version: string }[] } | undefined): void {
    Object.defineProperty(navigator, 'userAgentData', { configurable: true, value });
}

async function mount(text: string) {
    const source = document.createElement('textarea');
    source.value = text;
    document.body.append(source);
    const editor = await editorRuntime.mountCodeMirrorEditor(source);
    if (!editor) throw new Error('expected editor');
    return editor;
}

function editorView(root: ParentNode | null): EditorView {
    const dom = root?.querySelector<HTMLElement>('.cm-editor');
    const view = dom ? EditorView.findFromDOM(dom) : null;
    if (!view) throw new Error('expected CodeMirror view');
    return view;
}

test('a Chromium browser host mounts an editable CodeMirror in a shadow root', async () => {
    setClientHints({ brands: [{ brand: 'Google Chrome', version: '130' }, { brand: 'Chromium', version: '130' }] });
    const editor = await mount('Chromium entry');

    const view = editorView(editor.wrapper.shadowRoot);
    expect(view.state.doc.toString()).toBe('Chromium entry');
    expect(view.contentDOM.getAttribute('contenteditable')).toBe('true');
    editor.destroy();
});

test('a browser host without client hints, as in WebKit and Gecko, mounts an editable CodeMirror in the page', async () => {
    setClientHints(undefined);
    const editor = await mount('WebKit entry');

    expect(editor.wrapper.shadowRoot).toBeNull();
    const view = editorView(editor.wrapper);
    expect(view.state.doc.toString()).toBe('WebKit entry');
    expect(view.contentDOM.getAttribute('contenteditable')).toBe('true');
    editor.destroy();
});
