#!/usr/bin/env node
/**
 * Fetches and stages self-contained `rclone` and `exiftool` sidecars into
 * `src-tauri/resources/` so TruongSync works out of the box with zero external installs
 * on both macOS and Windows.
 *
 * Security & Portability notes:
 * - Always fetches official pure-Perl Image-ExifTool tarball on macOS/Linux (never copies
 *   unsigned Homebrew XS/Mach-O binaries like Brotli.bundle).
 * - Stages inside `src-tauri/resources/` so atomic rename is always same-filesystem with rollback.
 * - Uses Windows built-in `tar.exe` (zero PowerShell string-interpolation risk).
 * - Verifies staged binaries actually execute cleanly (including OS/arch & version match).
 */
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const rootDir = path.resolve(__dirname, '..');
const resourcesDir = path.join(rootDir, 'src-tauri', 'resources');
const rcloneDir = path.join(resourcesDir, 'rclone');
const exiftoolDir = path.join(resourcesDir, 'exiftool');

const platform = process.platform;
const arch = process.arch;

function ensureDir(dir) {
  fs.mkdirSync(dir, { recursive: true });
}

function downloadFile(url, dest) {
  console.log(`Downloading ${url}...`);
  execFileSync('curl', ['-fsSL', '-L', '-o', dest, url], { stdio: 'inherit' });
}

function extractArchive(archivePath, destDir) {
  if (platform === 'win32') {
    // Built-in bsdtar on Windows 10/11 supports both .zip and .tar.gz with zero shell interpolation.
    execFileSync('tar.exe', ['-xf', archivePath, '-C', destDir], { stdio: 'inherit' });
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
      if (platform !== 'win32') {
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

function verifyRcloneBinary(binPath) {
  if (!fs.existsSync(binPath)) return false;
  try {
    const out = execFileSync(binPath, ['version'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
    const expectedArch = arch === 'arm64' ? 'arm64' : 'amd64';
    return out.includes('rclone v') && out.toLowerCase().includes(expectedArch);
  } catch {
    return false;
  }
}

function verifyExiftoolBinary(binPath) {
  if (!fs.existsSync(binPath)) return null;
  try {
    const ver = execFileSync(binPath, ['-ver'], {
      encoding: 'utf8',
      env: platform === 'darwin' ? { PATH: '/usr/bin:/bin' } : process.env,
      stdio: ['ignore', 'pipe', 'pipe'],
    }).trim();
    return /^\d+\.\d+$/.test(ver) ? ver : null;
  } catch {
    return null;
  }
}

function fetchRclone() {
  const binName = platform === 'win32' ? 'rclone.exe' : 'rclone';
  const target = path.join(rcloneDir, binName);
  if (verifyRcloneBinary(target)) {
    console.log(`rclone already staged and verified at ${target}`);
    return;
  }

  const rArch = arch === 'arm64' ? 'arm64' : 'amd64';
  const osTag = platform === 'darwin' ? 'osx' : platform === 'win32' ? 'windows' : 'linux';
  const url = `https://downloads.rclone.org/rclone-current-${osTag}-${rArch}.zip`;

  ensureDir(resourcesDir);
  const tmpDir = fs.mkdtempSync(path.join(resourcesDir, '.tmp-rclone-'));
  try {
    const zipPath = path.join(tmpDir, 'rclone.zip');
    downloadFile(url, zipPath);
    extractArchive(zipPath, tmpDir);
    const sub = fs
      .readdirSync(tmpDir)
      .find((n) => n.startsWith('rclone-') && fs.statSync(path.join(tmpDir, n)).isDirectory());
    if (!sub) throw new Error('Extracted rclone directory not found');

    const stageDir = path.join(tmpDir, 'stage');
    ensureDir(stageDir);
    const stagedBin = path.join(stageDir, binName);
    fs.copyFileSync(path.join(tmpDir, sub, binName), stagedBin);
    if (platform !== 'win32') {
      fs.chmodSync(stagedBin, 0o755);
    }
    if (!verifyRcloneBinary(stagedBin)) {
      throw new Error(`Downloaded rclone failed architecture/execution verification: ${stagedBin}`);
    }
    swapDirAtomic(stageDir, rcloneDir);
    console.log(`Verified staged rclone -> ${target}`);
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
}

function fetchExiftool() {
  const binName = platform === 'win32' ? 'exiftool.exe' : 'exiftool';
  const target = path.join(exiftoolDir, binName);
  const sentinel =
    platform === 'win32'
      ? path.join(exiftoolDir, 'exiftool_files', 'perl.exe')
      : path.join(exiftoolDir, 'lib', 'Image', 'ExifTool.pm');

  const hasNativeBundle =
    platform !== 'win32' && fs.existsSync(path.join(exiftoolDir, 'lib', 'darwin-thread-multi-2level'));

  const existingVer = fs.existsSync(sentinel) && !hasNativeBundle ? verifyExiftoolBinary(target) : null;
  if (existingVer) {
    console.log(`ExifTool v${existingVer} already staged and verified at ${target}`);
    return;
  }

  ensureDir(resourcesDir);
  const tmpDir = fs.mkdtempSync(path.join(resourcesDir, '.tmp-exiftool-'));
  try {
    const ver = execFileSync('curl', ['-fsSL', 'https://exiftool.org/ver.txt'], {
      encoding: 'utf8',
    }).trim();
    if (!/^\d+\.\d+$/.test(ver)) {
      throw new Error(`Unexpected ExifTool version from ver.txt: ${JSON.stringify(ver)}`);
    }
    const stageDir = path.join(tmpDir, 'stage');
    ensureDir(stageDir);

    if (platform === 'win32') {
      const zipPath = path.join(tmpDir, 'exiftool.zip');
      downloadFile(`https://exiftool.org/exiftool-${ver}_64.zip`, zipPath);
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
      downloadFile(`https://github.com/exiftool/exiftool/archive/refs/tags/${ver}.tar.gz`, tgz);
      extractArchive(tgz, tmpDir);
      const extracted = path.join(tmpDir, `exiftool-${ver}`);
      let script = fs.readFileSync(path.join(extracted, 'exiftool'), 'utf8');
      script = script.replace(/^#!.*$/m, platform === 'darwin' ? '#!/usr/bin/perl' : '#!/usr/bin/env perl');
      const stagedBin = path.join(stageDir, 'exiftool');
      fs.writeFileSync(stagedBin, script);
      fs.chmodSync(stagedBin, 0o755);
      copyDirRecursive(path.join(extracted, 'lib'), path.join(stageDir, 'lib'));
    }

    const stagedTarget = path.join(stageDir, binName);
    const outVer = verifyExiftoolBinary(stagedTarget);
    if (outVer !== ver) {
      throw new Error(`Staged ExifTool version mismatch: expected ${ver}, got ${outVer}`);
    }
    swapDirAtomic(stageDir, exiftoolDir);
    console.log(`Verified staged pure-Perl ExifTool v${outVer} -> ${target}`);
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
}

fetchRclone();
fetchExiftool();
console.log('All TruongSync sidecars staged and verified.');
