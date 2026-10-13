import assert from 'node:assert/strict';
import test from 'node:test';
import { createBrowserRuntime } from './runtime.mjs';

// happy-dom has no layout: provide the scrollport geometry and resize delivery,
// while using the actual application, composer observer, projection and scroll writer.
test('composer expansion cannot scroll backwards between appending a message and the tail frame', async context => {
    const { window, load, startHost } = createBrowserRuntime();
    context.after(() => window.happyDOM.close());
    const observers = [];
    window.ResizeObserver = class {
        constructor(callback) { this.callback = callback; this.targets = new Set(); observers.push(this); }
        observe(target) { this.targets.add(target); }
        unobserve(target) { this.targets.delete(target); }
        disconnect() { this.targets.clear(); }
    };
    const supports = window.CSS.supports.bind(window.CSS);
    window.CSS.supports = (...args) => args[0] === 'field-sizing' ? true : supports(...args);
    await startHost();
    const main = (await load('script.js')).namespace;
    const ross = (await load('scripts/RossAscends-mods.js')).namespace;
    const { power_user } = (await load('scripts/power-user.js')).namespace;
    power_user.auto_connect = false;
    power_user.auto_scroll_chat_to_bottom = true;

    const root = window.document.getElementById('chat');
    let height = 500;
    let contentHeight = 3000;
    let top = contentHeight - height;
    Object.defineProperties(root, {
        clientHeight: { configurable: true, get: () => height },
        // Borders are not part of the scroll viewport.
        offsetHeight: { configurable: true, get: () => height + 2 },
        scrollHeight: { configurable: true, get: () => contentHeight },
        scrollTop: {
            configurable: true, get: () => top,
            set: value => { top = Math.max(0, Math.min(Number(value), contentHeight - height)); },
        },
    });
    ross.initRossMods();
    const resize = observers.filter(observer => observer.targets.has(root));
    assert.equal(resize.length, 1);
    const deliverResize = () => resize[0].callback([{ target: root }]);
    const frames = new Map();
    let frameId = 0;
    window.requestAnimationFrame = callback => { frames.set(++frameId, callback); return frameId; };
    window.cancelAnimationFrame = id => frames.delete(id);
    const flushFrame = () => {
        const callbacks = [...frames.values()]; frames.clear();
        for (const callback of callbacks) callback(0);
    };

    // The empty composer has made the viewport taller, but the new message has
    // already increased the content extent before resize delivery. The send's
    // normal tail scroll is still waiting for its frame.
    height = 650;
    contentHeight = 3300;
    const message = { name: 'User', is_user: true, is_system: false, mes: 'Anonymous new message', extra: {} };
    main.chat.push(message);
    main.addOneMessage(message);
    assert.equal(main.getChatScrollTop(), 2500);
    deliverResize();
    assert.equal(main.getChatScrollTop(), 2500, 'resize must not reveal earlier floors before the send frame');
    flushFrame();
    assert.equal(main.getChatScrollTop(), 2650, 'the existing send frame still reaches the new tail');

    root.scrollTop = 1200;
    height = 750;
    deliverResize();
    assert.equal(main.getChatScrollTop(), 1200, 'expansion preserves the top of a historical reading position');
    height = 600;
    deliverResize();
    assert.equal(main.getChatScrollTop(), 1350, 'shrinking preserves the existing lower-edge compensation');

    // Welcome content is not a message viewport and must never receive that compensation.
    const welcome = window.document.createElement('section'); welcome.className = 'welcomePanel'; root.append(welcome);
    height = 500;
    deliverResize();
    assert.equal(main.getChatScrollTop(), 1350);
});
