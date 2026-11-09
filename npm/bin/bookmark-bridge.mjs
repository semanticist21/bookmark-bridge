#!/usr/bin/env node
// Verifies and unpacks the bundled server, then hands over to it.
// An MCP client launches this over stdio, so nothing may be written to
// stdout that is not protocol: progress goes to stderr.
import {spawn} from 'node:child_process';
import {createHash} from 'node:crypto';
import {chmod, mkdir, mkdtemp, readFile, rename, rm, stat} from 'node:fs/promises';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const pkg = JSON.parse(await readFile(join(here, '..', 'package.json'), 'utf8'));
const REPO = 'semanticist21/bookmark-bridge';

const TARGETS = {
  'darwin-arm64': 'aarch64-apple-darwin',
  'darwin-x64': 'x86_64-apple-darwin',
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'linux-arm64': 'aarch64-unknown-linux-gnu',
  'win32-x64': 'x86_64-pc-windows-msvc',
};

const EXE = process.platform === 'win32' ? '.exe' : '';

function die(message) {
  process.stderr.write(`bookmark-bridge: ${message}\n`);
  process.exit(1);
}

const key = `${process.platform}-${process.arch}`;
const target = TARGETS[key];
if (!target) {
  die(`no prebuilt server for ${key}. Build from source: https://github.com/${REPO}`);
}

// Cached per version, so upgrading the package unpacks a matching server rather
// than silently reusing an old one.
const cacheDir = join(here, '..', '.bin', pkg.version);
const binary = join(cacheDir, `bookmark-bridge${EXE}`);

async function exists(path) {
  try { await stat(path); return true; } catch { return false; }
}

async function install() {
  const assets = join(here, '..', 'release-assets');
  const name = `bookmark-bridge-${target}.tar.gz`;
  const sums = await readFile(join(assets, 'SHA256SUMS'), 'utf8');
  const expected = sums.split('\n').map(line => line.trim().split(/\s+/))
    .find(([, file]) => file === name)?.[0];
  if (!expected) throw new Error(`no checksum bundled for ${name}`);
  const archive = join(assets, name);
  const actual = createHash('sha256').update(await readFile(archive)).digest('hex');
  if (actual !== expected) throw new Error(`checksum mismatch for ${name}`);

  await mkdir(dirname(cacheDir), {recursive: true});
  const staging = await mkdtemp(join(dirname(cacheDir), '.install-'));
  try {
    await new Promise((resolve, reject) => {
      const tar = spawn('tar', ['-xzf', archive, '-C', staging], {stdio: 'inherit'});
      tar.on('error', reject);
      tar.on('exit', (code) => code === 0 ? resolve() : reject(new Error(`tar exited ${code}`)));
    });

    const unpacked = join(staging, `bookmark-bridge-${target}`);
    if (process.platform !== 'win32') await chmod(join(unpacked, `bookmark-bridge${EXE}`), 0o755);
    // Publish the whole directory. Another client may already have installed it.
    try { await rename(unpacked, cacheDir); }
    catch (e) { if (!(await exists(binary))) throw e; }
  } finally {
    await rm(staging, {recursive: true, force: true});
  }
}

if (!(await exists(binary))) {
  try { await install(); }
  catch (e) { die(`install failed: ${e.message}`); }
}

const child = spawn(binary, process.argv.slice(2), {stdio: 'inherit'});
child.on('error', (e) => die(`could not start the server: ${e.message}`));
child.on('exit', (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  else process.exit(code ?? 0);
});
