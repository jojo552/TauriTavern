import test from 'node:test';
import assert from 'node:assert/strict';
import { createBrowserFileTransfer } from '../src/web/file-transfer.js';

function browser() {
    const clicks = [];
    const nodes = [];
    const revoked = [];
    const deferred = [];
    let clickError;
    const document = {
        baseURI: 'http://localhost:8080/',
        body: { append(node) { nodes.push(node); node.attached = true; } },
        createElement(tag) {
            const node = new EventTarget();
            node.tag = tag;
            node.files = [];
            node.remove = () => { node.attached = false; };
            node.click = () => {
                clicks.push(node);
                if (clickError) throw clickError;
            };
            return node;
        },
    };
    const api = createBrowserFileTransfer({
        document,
        urlApi: { createObjectURL: () => 'blob:bytes', revokeObjectURL: url => revoked.push(url) },
        schedule: callback => deferred.push(callback),
    });
    return { api, nodes, clicks, revoked, deferred, failClick: error => { clickError = error; } };
}

test('file selection returns the original browser Files with names and bytes, and cleans its input', async () => {
    const { api, nodes, clicks } = browser();
    const selected = api.pickFiles({ kind: 'character-card', multiple: true });
    assert.equal(clicks.length, 1, 'opens synchronously in the user gesture');
    const input = nodes[0];
    assert.equal(input.accept, '.json,.png');
    assert.equal(input.multiple, true);
    const file = new File(['{"name":"Alice"}'], 'Alice%20.json', { type: 'application/json' });
    input.files = [file];
    input.dispatchEvent(new Event('change'));
    const files = await selected;
    assert.equal(files[0], file);
    assert.equal(await files[0].text(), '{"name":"Alice"}');
    assert.equal(input.attached, false);
});

test('user cancellation, caller abortion and picker failure have distinct observable results', async () => {
    const { api, nodes, failClick } = browser();
    const cancelled = api.pickFiles({ kind: 'skill-archive' });
    nodes[0].dispatchEvent(new Event('cancel'));
    assert.equal(await cancelled, null);
    assert.equal(nodes[0].attached, false);

    const controller = new AbortController();
    const aborted = api.pickFiles({ kind: 'data-archive', signal: controller.signal });
    const reason = new Error('Caller left import flow');
    controller.abort(reason);
    await assert.rejects(aborted, error => error === reason);
    assert.equal(nodes[1].attached, false);

    const failure = new Error('Picker unavailable');
    failClick(failure);
    await assert.rejects(api.pickFiles({ kind: 'character-card' }), error => error === failure);
    assert.equal(nodes[2].attached, false);
});

test('Blob downloads hand off to a native anchor without claiming disk delivery, and release object URLs', () => {
    const { api, clicks, revoked, deferred } = browser();
    assert.deepEqual(api.downloadBlob(new Blob(['archive']), 'export.zip'), { status: 'handed-off' });
    assert.equal(clicks[0].href, 'blob:bytes');
    assert.equal(clicks[0].download, 'export.zip');
    assert.equal(clicks[0].attached, false);
    assert.deepEqual(revoked, []);
    deferred[0]();
    assert.deepEqual(revoked, ['blob:bytes']);
});

test('download failures remain failures and host downloads cannot escape the same origin', () => {
    const { api, clicks, revoked, failClick } = browser();
    for (const url of ['https://other.example/export', 'file:///etc/passwd', 'javascript:alert(1)']) {
        assert.throws(() => api.downloadUrl(url, 'export.zip'), /same-origin/);
    }
    assert.equal(clicks.length, 0);
    assert.deepEqual(api.downloadUrl('/__tt/download/opaque', 'export.zip'), { status: 'handed-off' });
    assert.equal(clicks[0].href, 'http://localhost:8080/__tt/download/opaque');
    const failure = new Error('Download blocked');
    failClick(failure);
    assert.throws(() => api.downloadBlob(new Blob(['x']), 'x.txt'), error => error === failure);
    assert.deepEqual(revoked, ['blob:bytes']);
    assert.equal(clicks[1].attached, false);
});
