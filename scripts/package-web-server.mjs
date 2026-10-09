#!/usr/bin/env node
// Assemble the immutable browser-host release: built frontend, packaged resources, binary.
// The resource layout mirrors the native bundle (tauri.conf.json resources + build.rs virtual
// paths), so DirectoryBundledResources reads the same relative paths as the desktop host.

import fs from 'node:fs/promises';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const [binary, output] = process.argv.slice(2);
if (!binary || !output) {
    console.error('usage: node scripts/package-web-server.mjs <tt-server binary> <output dir>');
    process.exit(2);
}

const out = path.resolve(output);
await fs.rm(out, { recursive: true, force: true });
await fs.mkdir(path.join(out, 'resources', 'frontend-extensions'), { recursive: true });

const src = path.join(repoRoot, 'src');
await fs.access(path.join(src, 'dist')).catch(() => {
    throw new Error('src/dist is missing; run `pnpm run web:build` first');
});
await fs.cp(src, path.join(out, 'web'), { recursive: true });
await fs.cp(path.join(repoRoot, 'default'), path.join(out, 'resources', 'default'), { recursive: true });
await fs.cp(path.join(src, 'scripts', 'templates'), path.join(out, 'resources', 'frontend-templates'), { recursive: true });

const extensionsRoot = path.join(src, 'scripts', 'extensions');
for (const extension of await fs.readdir(extensionsRoot, { withFileTypes: true })) {
    if (!extension.isDirectory()) continue;
    const files = await fs.readdir(path.join(extensionsRoot, extension.name), { withFileTypes: true });
    const templates = files.filter(file => file.isFile() && file.name.toLowerCase().endsWith('.html'));
    if (templates.length === 0) continue;
    const target = path.join(out, 'resources', 'frontend-extensions', extension.name);
    await fs.mkdir(target, { recursive: true });
    for (const file of templates) {
        await fs.copyFile(path.join(extensionsRoot, extension.name, file.name), path.join(target, file.name));
    }
}

await fs.copyFile(binary, path.join(out, 'tt-server'));
await fs.chmod(path.join(out, 'tt-server'), 0o755);
console.log(`Packaged browser host into ${out}`);
