#!/usr/bin/env node
// Fetches the prebuilt server for this platform on first run, then hands over to
// it. An MCP client launches this over stdio, so nothing may be written to
// stdout that is not protocol: progress goes to stderr.
import {spawn} from 'node:child_process';
import {createHash} from 'node:crypto';
import {createWriteStream} from 'node:fs';
import {chmod, mkdir, mkdtemp, readFile, rename, rm, stat} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {dirname, join} from 'node:path';
import {pipeline} from 'node:stream/promises';
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

// Cached per version, so upgrading the package fetches a matching server rather
// than silently reusing an old one.
const cacheDir = join(here, '..', '.bin', pkg.version);
const binary = join(cacheDir, `bookmark-bridge${EXE}`);

async function exists(path) {
  try { await stat(path); return true; } catch { return false; }
}

async function fetchOrDie(url, what) {
  const res = await fetch(url, {redirect: 'follow'});
  if (!res.ok) die(`could not download ${what} (HTTP ${res.status})\n  ${url}`);
  return res;
}

async function install() {
  const tag = `v${pkg.version}`;
  const name = `bookmark-bridge-${target}.tar.gz`;
  const base = `https://github.com/${REPO}/releases/download/${tag}`;

  process.stderr.write(`bookmark-bridge: fetching ${tag} for ${key}\n`);
  const sums = (await (await fetchOrDie(`${base}/SHA256SUMS`, 'checksums')).text());
  const line = sums.split('\n').find((l) => l.trim().endsWith(name));
  if (!line) die(`no checksum published for ${name} in ${tag}`);
  const expected = line.trim().split(/\s+/)[0];

  const staging = await mkdtemp(join(tmpdir(), 'bookmark-bridge-'));
  const archive = join(staging, name);
  try {
    await pipeline(
      (await fetchOrDie(`${base}/${name}`, 'the server')).body,
      createWriteStream(archive),
    );

    // Never run a binary that does not match what the release published.
    const actual = createHash('sha256').update(await readFile(archive)).digest('hex');
    if (actual !== expected) {
      die(`checksum mismatch for ${name}\n  expected ${expected}\n  got      ${actual}`);
    }

    await new Promise((resolve, reject) => {
      const tar = spawn('tar', ['-xzf', archive, '-C', staging], {stdio: 'inherit'});
      tar.on('error', reject);
      tar.on('exit', (code) => code === 0 ? resolve() : reject(new Error(`tar exited ${code}`)));
    });

    await mkdir(dirname(cacheDir), {recursive: true});
    // Rename is atomic, so two clients starting at once cannot see a half-written
    // directory.
    await rm(cacheDir, {recursive: true, force: true});
    await rename(join(staging, `bookmark-bridge-${target}`), cacheDir);
    // Windows has no execute bit; the archive already carries it elsewhere.
    if (process.platform !== 'win32') await chmod(binary, 0o755);
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
