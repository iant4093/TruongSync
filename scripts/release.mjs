#!/usr/bin/env node
// scripts/release.mjs
// One-command release build:
//   1. stages self-contained rclone + ExifTool sidecars
//   2. builds with Tauri, signing the updater archive with your private key
//   3. writes latest.json (the manifest the in-app updater reads)
//
// Usage: npm run app:build [-- --notes "What changed"] [-- <extra tauri build args>]
// Signing key: $TAURI_SIGNING_PRIVATE_KEY, or ~/.tauri/truongsync.key (created by `tauri signer generate`).

import { spawnSync } from 'node:child_process';
import { existsSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const notesIdx = args.indexOf('--notes');
const notes = notesIdx >= 0 ? args.splice(notesIdx, 2)[1] : '';

const conf = JSON.parse(readFileSync(join(root, 'src-tauri', 'tauri.conf.json'), 'utf8'));
const cfgIdx = args.indexOf('--config') >= 0 ? args.indexOf('--config') : args.indexOf('-c');
let override = {};
if (cfgIdx >= 0 && args[cfgIdx + 1]) {
  const cfgArg = args[cfgIdx + 1].trim();
  if (cfgArg.startsWith('{')) {
    override = JSON.parse(cfgArg);
  } else if (existsSync(resolve(root, cfgArg))) {
    override = JSON.parse(readFileSync(resolve(root, cfgArg), 'utf8'));
  }
}
const version = override.version ?? conf.version;
const endpoint = override.plugins?.updater?.endpoints?.[0] ?? conf.plugins?.updater?.endpoints?.[0] ?? '';

const targetIdx = args.indexOf('--target') >= 0 ? args.indexOf('--target') : args.indexOf('-t');
const targetTriple = targetIdx >= 0 ? args[targetIdx + 1] : null;

// 1a. Fetch bundled rclone + ExifTool sidecars BEFORE loading the release signing private key
const sidecarEnv = { ...process.env };
delete sidecarEnv.TAURI_SIGNING_PRIVATE_KEY;
delete sidecarEnv.TAURI_SIGNING_PRIVATE_KEY_PATH;
delete sidecarEnv.TAURI_SIGNING_PRIVATE_KEY_PASSWORD;
const sidecarArgs = [join(root, 'scripts', 'fetch-sidecars.mjs')];
if (targetTriple) {
  sidecarArgs.push('--target', targetTriple);
}
const fetchSidecars = spawnSync(process.execPath, sidecarArgs, {
  cwd: root,
  env: sidecarEnv,
  stdio: 'inherit',
});
if (fetchSidecars.status !== 0) process.exit(fetchSidecars.status ?? 1);

// 1b. Signing key for updater artifacts (respects either TAURI_SIGNING_PRIVATE_KEY or TAURI_SIGNING_PRIVATE_KEY_PATH)
const env = { ...process.env };
env.TS_BUILD_VERSION ??= version;
if (!env.TAURI_SIGNING_PRIVATE_KEY && !env.TAURI_SIGNING_PRIVATE_KEY_PATH) {
  const keyFile = join(homedir(), '.tauri', 'truongsync.key');
  if (!existsSync(keyFile)) {
    console.error('[release] No signing key. Run: npx tauri signer generate -w ~/.tauri/truongsync.key');
    process.exit(1);
  }
  env.TAURI_SIGNING_PRIVATE_KEY = readFileSync(keyFile, 'utf8');
}
env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD ??= '';

// Run the local Tauri CLI through Node: `npx` is `npx.cmd` on Windows, which spawnSync cannot
// start without a shell.
const tauriCli = join(root, 'node_modules', '@tauri-apps', 'cli', 'tauri.js');
const build = spawnSync(process.execPath, [tauriCli, 'build', ...args], { cwd: root, env, stdio: 'inherit' });
if (build.status !== 0) process.exit(build.status ?? 1);

// 2. latest.json — upload it plus the .tar.gz/.exe next to it to the GitHub release v<version>.
const bundleCandidates = targetTriple
  ? [
      join(root, 'target', targetTriple, 'release', 'bundle'),
      join(root, 'src-tauri', 'target', targetTriple, 'release', 'bundle'),
    ]
  : [
      join(root, 'target', 'release', 'bundle'),
      join(root, 'src-tauri', 'target', 'release', 'bundle'),
    ];
const bundleDir = bundleCandidates.find((dir) => existsSync(dir)) ?? bundleCandidates[0];
const platformKey = targetTriple
  ? `${targetTriple.includes('apple-darwin') ? 'darwin' : targetTriple.includes('windows') ? 'windows' : 'linux'}-${
      targetTriple.startsWith('aarch64') ? 'aarch64' : 'x86_64'
    }`
  : `${process.platform === 'darwin' ? 'darwin' : process.platform === 'win32' ? 'windows' : 'linux'}-${
      process.arch === 'arm64' ? 'aarch64' : 'x86_64'
    }`;
const downloadBase = endpoint.includes('/releases/latest/download/')
  ? endpoint.replace(/\/releases\/latest\/download\/.*$/, `/releases/download/v${version}/`)
  : '';

const archives = [];
for (const sub of ['macos', 'nsis', 'msi', 'appimage']) {
  const dir = join(bundleDir, sub);
  if (!existsSync(dir)) continue;
  for (const f of readdirSync(dir)) {
    if (f.endsWith('.sig')) {
      const sigPath = join(dir, f);
      archives.push({
        file: f.slice(0, -4),
        dir,
        sig: readFileSync(sigPath, 'utf8').trim(),
        mtimeMs: statSync(sigPath).mtimeMs,
      });
    }
  }
}
if (!archives.length) {
  console.warn('[release] No signed updater archives found (is createUpdaterArtifacts enabled?)');
  process.exit(0);
}
// Prefer newest archive matching the built version (NSIS on Windows, never MSI).
archives.sort((a, b) => b.mtimeMs - a.mtimeMs);
const nsisArchives = archives.filter((a) => a.dir.endsWith('nsis'));
const pool = nsisArchives.length ? nsisArchives : archives;
const pick = pool.find((a) => a.file.includes(version)) ?? pool[0];
const manifest = {
  version,
  notes: notes || `TruongSync ${version}`,
  pub_date: new Date().toISOString(),
  platforms: {
    [platformKey]: { signature: pick.sig, url: `${downloadBase}${encodeURIComponent(pick.file)}` },
  },
};
const out = join(bundleDir, 'latest.json');
writeFileSync(out, JSON.stringify(manifest, null, 2) + '\n');
console.log(`\n[release] ${pick.file} (+ .sig) and latest.json are in ${bundleDir}`);
console.log(`[release] Upload both to the GitHub release "v${version}".`);
