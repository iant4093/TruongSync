#!/usr/bin/env node
/**
 * Fetches and stages self-contained `rclone` and `exiftool` sidecars into
 * `src-tauri/resources/` so TruongSync works out of the box with zero external installs
 * on both macOS and Windows.
 *
 * Security & Portability notes:
 * - Pins exact versions and SHA-256 digests for every archive, verifying SHA-256 BEFORE
 *   extracting or executing any downloaded file (rejecting tampered mirrors and SourceForge
 *   HTTP 200 HTML interstitials cleanly).
 * - Always fetches official pure-Perl Image-ExifTool tarball on macOS/Linux (never copies
 *   unsigned Homebrew XS/Mach-O binaries like Brotli.bundle).
 * - Supports `--target <rust-triple>` for cross-architecture builds (e.g. `x86_64-apple-darwin`).
 * - Stages inside `src-tauri/resources/` so atomic rename is always same-filesystem with rollback.
 * - Uses Windows built-in `System32\tar.exe` (zero PowerShell string-interpolation risk).
 */
import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const rootDir = path.resolve(__dirname, '..');
const resourcesDir = path.join(rootDir, 'src-tauri', 'resources');
const rcloneDir = path.join(resourcesDir, 'rclone');
const exiftoolDir = path.join(resourcesDir, 'exiftool');

const RCLONE_VERSION = 'v1.76.0';
const RCLONE_SHA256 = {
  'osx-arm64': 'e72dc90b70f4947a7b72b61bde4a4fbba5777b2874fb448951ffdde67ebd0ab7',
  'osx-amd64': '4f5835733d52b818340f97b1fc0348a70461534d123363a183ca2b660219595a',
  'windows-amd64': '8b4fd2737ade4e70c057ef4a49deb7cffe94b0fb9d7587c189d149823f21a1dc',
  'linux-amd64': 'b00f00b3266c3764413afe599d3ec316f6f43b3e0ea8a0bc1d186d2133e46df7',
};

const EXIFTOOL_VERSION = '13.64';
const EXIFTOOL_SHA256 = {
  source: '6ed24bfbe1ce38d0e7800f8e4132f9bfce2a29f4256d924d0458f59fd64b513a',
  win64: '72c29c44d1e860eebcaefa71692d979af9858fe9a6d7d791a5e7f1dac77804af',
};

const cliArgs = process.argv.slice(2);
const targetIdx = cliArgs.indexOf('--target');
const targetTriple = targetIdx >= 0 ? cliArgs[targetIdx + 1] : null;

const hostPlatform = process.platform;
const hostArch = process.arch;

const targetPlatform = targetTriple
  ? targetTriple.includes('apple-darwin')
    ? 'darwin'
    : targetTriple.includes('windows')
      ? 'win32'
      : 'linux'
  : hostPlatform;

const targetArch = targetTriple
  ? targetTriple.startsWith('aarch64')
    ? 'arm64'
    : 'x64'
  : hostArch;

const canExecuteTarget =
  targetPlatform === hostPlatform &&
  (targetArch === hostArch || (hostPlatform === 'darwin' && hostArch === 'arm64' && targetArch === 'x64'));

function ensureDir(dir) {
  fs.mkdirSync(dir, { recursive: true });
}

function sha256File(filePath) {
  const hash = crypto.createHash('sha256');
  hash.update(fs.readFileSync(filePath));
  return hash.digest('hex');
}

function downloadFile(url, dest) {
  console.log(`Downloading ${url}...`);
  execFileSync('curl', ['-fsSL', '-L', '--retry', '3', '--retry-delay', '2', '-A', 'curl/8.7.1', '-o', dest, url], {
    stdio: 'inherit',
  });
}

function downloadVerified(urls, dest, expectedSha256) {
  let lastErr;
  for (const url of urls) {
    try {
      fs.rmSync(dest, { force: true });
      downloadFile(url, dest);
      const actual = sha256File(dest);
      if (actual !== expectedSha256) {
        fs.rmSync(dest, { force: true });
        throw new Error(
          `SHA-256 mismatch for ${url}: expected ${expectedSha256}, got ${actual}`,
        );
      }
      return;
    } catch (err) {
      lastErr = err;
      console.warn(`Mirror failed (${err.message}); trying next mirror if available...`);
    }
  }
  throw lastErr;
}

function extractArchive(archivePath, destDir) {
  if (hostPlatform === 'win32') {
    // Invoke Windows System32 bsdtar explicitly so Git Bash PATH never resolves GNU tar.exe.
    const winTar = path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'tar.exe');
    execFileSync(winTar, ['-xf', archivePath, '-C', destDir], { stdio: 'inherit' });
  } else if (archivePath.endsWith('.zip')) {
    execFileSync('unzip', ['-qo', archivePath, '-d', destDir], { stdio: 'inherit' });
  } else {
    execFileSync('tar', ['-xzf', archivePath, '-C', destDir], { stdio: 'inherit' });
  }
}

function copyDirRecursive(src, dst) {
  ensureDir(dst);
  for (const entry of fs.readdirSync(src, { withFileTypes: true })) {
    const s = path.join(src, entry.name);
    const d = path.join(dst, entry.name);
    if (entry.isDirectory()) {
      copyDirRecursive(s, d);
    } else if (entry.isFile()) {
      fs.copyFileSync(s, d);
      if (hostPlatform !== 'win32') {
        const mode = fs.statSync(s).mode & 0o111 ? 0o755 : 0o644;
        fs.chmodSync(d, mode);
      }
    }
  }
}

/**
 * Same-filesystem atomic directory swap with backup & rollback so an interrupted run
 * never leaves `targetDir` missing or half-populated.
 */
function swapDirAtomic(stageDir, targetDir) {
  ensureDir(path.dirname(targetDir));
  fs.writeFileSync(path.join(stageDir, '.gitkeep'), '');
  const backupDir = `${targetDir}.bak-${process.pid}`;
  fs.rmSync(backupDir, { recursive: true, force: true });
  const hadTarget = fs.existsSync(targetDir);
  if (hadTarget) {
    fs.renameSync(targetDir, backupDir);
  }
  try {
    fs.renameSync(stageDir, targetDir);
  } catch (err) {
    if (hadTarget && !fs.existsSync(targetDir) && fs.existsSync(backupDir)) {
      fs.renameSync(backupDir, targetDir);
    }
    throw err;
  }
  if (hadTarget) {
    fs.rmSync(backupDir, { recursive: true, force: true });
  }
}

function minimalVerifyEnv() {
  if (hostPlatform === 'win32') {
    return {
      SystemRoot: process.env.SystemRoot || 'C:\\Windows',
      TEMP: process.env.TEMP || 'C:\\Windows\\Temp',
      TMP: process.env.TMP || 'C:\\Windows\\Temp',
      PATH: process.env.PATH || '',
    };
  }
  return { PATH: '/usr/bin:/bin' };
}

function verifyRcloneBinary(binPath, expectedArchKey) {
  if (!fs.existsSync(binPath)) return false;
  if (!canExecuteTarget) {
    return fs.statSync(binPath).size > 1_000_000;
  }
  try {
    const out = execFileSync(binPath, ['version'], {
      encoding: 'utf8',
      env: minimalVerifyEnv(),
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    return out.includes(`rclone ${RCLONE_VERSION}`) && out.toLowerCase().includes(expectedArchKey);
  } catch {
    return false;
  }
}

function verifyExiftoolBinary(binPath) {
  if (!fs.existsSync(binPath)) return null;
  if (targetPlatform !== hostPlatform) {
    return EXIFTOOL_VERSION;
  }
  try {
    const ver = execFileSync(binPath, ['-config', '', '-ver'], {
      encoding: 'utf8',
      env: minimalVerifyEnv(),
      stdio: ['ignore', 'pipe', 'pipe'],
    }).trim();
    return ver === EXIFTOOL_VERSION ? ver : null;
  } catch {
    return null;
  }
}

function fetchRclone() {
  const binName = targetPlatform === 'win32' ? 'rclone.exe' : 'rclone';
  const rArch = targetArch === 'arm64' ? 'arm64' : 'amd64';
  const osTag = targetPlatform === 'darwin' ? 'osx' : targetPlatform === 'win32' ? 'windows' : 'linux';
  const key = `${osTag}-${rArch}`;
  const expectedSha = RCLONE_SHA256[key];
  if (!expectedSha) {
    throw new Error(`No pinned rclone SHA-256 configured for target "${key}"`);
  }

  const target = path.join(rcloneDir, binName);
  const stampFile = path.join(rcloneDir, '.sidecar-stamp');
  const expectedStamp = `rclone:${RCLONE_VERSION}:${key}:${expectedSha}`;
  if (
    fs.existsSync(stampFile) &&
    fs.readFileSync(stampFile, 'utf8').trim() === expectedStamp &&
    verifyRcloneBinary(target, rArch)
  ) {
    console.log(`rclone ${RCLONE_VERSION} (${key}) already staged and verified at ${target}`);
    return;
  }

  const archiveName = `rclone-${RCLONE_VERSION}-${key}.zip`;
  const urls = [
    `https://downloads.rclone.org/${RCLONE_VERSION}/${archiveName}`,
    `https://github.com/rclone/rclone/releases/download/${RCLONE_VERSION}/${archiveName}`,
  ];

  ensureDir(resourcesDir);
  const tmpDir = fs.mkdtempSync(path.join(resourcesDir, '.tmp-rclone-'));
  try {
    const zipPath = path.join(tmpDir, 'rclone.zip');
    downloadVerified(urls, zipPath, expectedSha);
    extractArchive(zipPath, tmpDir);
    const sub = fs
      .readdirSync(tmpDir)
      .find((n) => n.startsWith('rclone-') && fs.statSync(path.join(tmpDir, n)).isDirectory());
    if (!sub) throw new Error('Extracted rclone directory not found');

    const stageDir = path.join(tmpDir, 'stage');
    ensureDir(stageDir);
    const stagedBin = path.join(stageDir, binName);
    fs.copyFileSync(path.join(tmpDir, sub, binName), stagedBin);
    if (hostPlatform !== 'win32') {
      fs.chmodSync(stagedBin, 0o755);
    }
    if (!verifyRcloneBinary(stagedBin, rArch)) {
      throw new Error(`Downloaded rclone failed architecture/version verification: ${stagedBin}`);
    }
    fs.writeFileSync(path.join(stageDir, '.sidecar-stamp'), `${expectedStamp}\n`);
    swapDirAtomic(stageDir, rcloneDir);
    console.log(`Verified staged rclone ${RCLONE_VERSION} (${key}) -> ${target}`);
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
}

function fetchExiftool() {
  const binName = targetPlatform === 'win32' ? 'exiftool.exe' : 'exiftool';
  const target = path.join(exiftoolDir, binName);
  const sentinel =
    targetPlatform === 'win32'
      ? path.join(exiftoolDir, 'exiftool_files', 'perl.exe')
      : path.join(exiftoolDir, 'lib', 'Image', 'ExifTool.pm');

  const hasNativeBundle =
    targetPlatform !== 'win32' &&
    fs.existsSync(path.join(exiftoolDir, 'lib', 'darwin-thread-multi-2level'));

  const expectedSha = targetPlatform === 'win32' ? EXIFTOOL_SHA256.win64 : EXIFTOOL_SHA256.source;
  const stampFile = path.join(exiftoolDir, '.sidecar-stamp');
  const expectedStamp = `exiftool:${EXIFTOOL_VERSION}:${targetPlatform}:${expectedSha}`;

  const existingVer =
    fs.existsSync(sentinel) &&
    !hasNativeBundle &&
    fs.existsSync(stampFile) &&
    fs.readFileSync(stampFile, 'utf8').trim() === expectedStamp
      ? verifyExiftoolBinary(target)
      : null;
  if (existingVer === EXIFTOOL_VERSION) {
    console.log(`ExifTool v${existingVer} already staged and verified at ${target}`);
    return;
  }

  ensureDir(resourcesDir);
  const tmpDir = fs.mkdtempSync(path.join(resourcesDir, '.tmp-exiftool-'));
  try {
    const stageDir = path.join(tmpDir, 'stage');
    ensureDir(stageDir);

    if (targetPlatform === 'win32') {
      const zipPath = path.join(tmpDir, 'exiftool.zip');
      downloadVerified(
        [
          `https://sourceforge.net/projects/exiftool/files/exiftool-${EXIFTOOL_VERSION}_64.zip/download`,
          `https://exiftool.org/exiftool-${EXIFTOOL_VERSION}_64.zip`,
          `https://oliverbetz.de/cms/files/Artikel/ExifTool-for-Windows/exiftool-${EXIFTOOL_VERSION}_64.zip`,
        ],
        zipPath,
        EXIFTOOL_SHA256.win64,
      );
      extractArchive(zipPath, tmpDir);
      const sub = fs
        .readdirSync(tmpDir)
        .find((n) => n.startsWith('exiftool-') && fs.statSync(path.join(tmpDir, n)).isDirectory());
      const srcDir = sub ? path.join(tmpDir, sub) : tmpDir;
      const exeSrc = fs.existsSync(path.join(srcDir, 'exiftool(-k).exe'))
        ? path.join(srcDir, 'exiftool(-k).exe')
        : path.join(srcDir, 'exiftool.exe');
      if (!fs.existsSync(exeSrc)) {
        throw new Error(`Expected exiftool.exe not found inside ${srcDir}`);
      }
      const filesDir = path.join(srcDir, 'exiftool_files');
      if (!fs.existsSync(filesDir)) {
        throw new Error(`Mandatory Windows exiftool_files/ directory missing in ${srcDir}`);
      }
      fs.copyFileSync(exeSrc, path.join(stageDir, 'exiftool.exe'));
      copyDirRecursive(filesDir, path.join(stageDir, 'exiftool_files'));
    } else {
      const tgz = path.join(tmpDir, 'exiftool.tar.gz');
      downloadVerified(
        [`https://github.com/exiftool/exiftool/archive/refs/tags/${EXIFTOOL_VERSION}.tar.gz`],
        tgz,
        EXIFTOOL_SHA256.source,
      );
      extractArchive(tgz, tmpDir);
      const extracted = path.join(tmpDir, `exiftool-${EXIFTOOL_VERSION}`);
      let script = fs.readFileSync(path.join(extracted, 'exiftool'), 'utf8');
      script = script.replace(
        /^#!.*$/m,
        targetPlatform === 'darwin' ? '#!/usr/bin/perl' : '#!/usr/bin/env perl',
      );
      const stagedBin = path.join(stageDir, 'exiftool');
      fs.writeFileSync(stagedBin, script);
      if (hostPlatform !== 'win32') {
        fs.chmodSync(stagedBin, 0o755);
      }
      copyDirRecursive(path.join(extracted, 'lib'), path.join(stageDir, 'lib'));
    }

    const stagedTarget = path.join(stageDir, binName);
    const outVer = verifyExiftoolBinary(stagedTarget);
    if (outVer !== EXIFTOOL_VERSION) {
      throw new Error(`Staged ExifTool version mismatch: expected ${EXIFTOOL_VERSION}, got ${outVer}`);
    }
    fs.writeFileSync(path.join(stageDir, '.sidecar-stamp'), `${expectedStamp}\n`);
    swapDirAtomic(stageDir, exiftoolDir);
    console.log(`Verified staged ExifTool v${outVer} -> ${target}`);
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
}

fetchRclone();
fetchExiftool();
console.log('All TruongSync sidecars staged and verified.');
