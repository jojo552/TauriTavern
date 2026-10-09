import { extractErrorText } from './tauri/main/kernel/host-error-response.js';

// Available before lib.js and script.js: startup failure must not depend on the UI it failed to load.
export function showStartupError(error) {
    document.getElementById('preloader')?.remove();
    document.getElementById('tt-startup-error')?.remove();
    const panel = document.createElement('section');
    panel.id = 'tt-startup-error';
    panel.setAttribute('role', 'alert');
    panel.style.cssText = 'position:fixed;inset:0;z-index:2147483647;overflow:auto;padding:24px;background:#202020;color:#fff;font:16px/1.5 system-ui;';

    const title = document.createElement('h1');
    title.textContent = 'TauriTavern could not start';
    const detail = document.createElement('pre');
    detail.style.cssText = 'white-space:pre-wrap;overflow-wrap:anywhere;';
    detail.textContent = extractErrorText(error);
    const guidance = document.createElement('p');
    guidance.textContent = 'Startup has stopped. Resolve the error, then reload to try again.';
    const reload = document.createElement('button');
    reload.type = 'button';
    reload.textContent = 'Reload';
    reload.addEventListener('click', () => window.location.reload());
    panel.append(title, detail, guidance, reload);
    document.body.appendChild(panel);
    reload.focus();
}
