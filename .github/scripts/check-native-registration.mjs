import assert from 'node:assert/strict';
import {execFileSync, spawnSync} from 'node:child_process';
import {existsSync, mkdtempSync, readFileSync, rmSync, statSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {isAbsolute, join, resolve} from 'node:path';

const windows = process.platform === 'win32';
const hostName = 'com.kobbokkom.bridgey';
const storeOrigin = 'chrome-extension://ekokljgbbianihlpahackkfdendobngd/';
const devId = 'abcdefghijklmnopabcdefghijklmnop';
const exe = `bookmark-bridge${windows ? '.exe' : ''}`;
const binary = resolve('mcp', 'target', 'debug', exe);
const version = JSON.parse(readFileSync('npm/package.json', 'utf8')).version;
const key = `HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts\\${hostName}`;
const reg = (...args) => spawnSync(join(process.env.SystemRoot, 'System32', 'reg.exe'),
  [...args, '/reg:32'], {encoding: 'utf8'});

// Never replace a real registration when this check is run on Windows locally.
if (windows) {
  const existing = reg('QUERY', key);
  assert.equal(existing.error, undefined);
  assert.notEqual(existing.status, 0, 'Bridgey is already registered under HKCU');
}

const home = mkdtempSync(join(tmpdir(), 'bridgey-register-'));
const chrome = join(home, 'Chrome profile root');
const env = {
  ...process.env,
  HOME: home,
  APPDATA: home,
  LOCALAPPDATA: home,
  BOOKMARK_BRIDGE_CHROME_USER_DATA_DIR: chrome,
};
delete env.BOOKMARK_BRIDGE_SKIP_REGISTRATION;
const root = join(home, {
  darwin: 'Library/Application Support/Bridgey/native-v2',
  linux: '.local/share/bridgey/native-v2',
  win32: 'Bridgey/native-v2',
}[process.platform]);
const manifestPath = join(windows ? root : join(chrome, 'NativeMessagingHosts'), `${hostName}.json`);
const hostPath = join(root, `bookmark-bridge-${version}${windows ? '.exe' : ''}`);
const register = (...args) => execFileSync(binary, ['register-native', ...args], {cwd: home, env});
const manifest = () => JSON.parse(readFileSync(manifestPath, 'utf8'));
const registeredPath = () => reg('QUERY', key, '/ve').stdout?.split(/\s+REG_SZ\s+/)[1]?.trim();

try {
  register();
  const production = manifest();
  assert.equal(production.name, hostName);
  assert.equal(production.type, 'stdio');
  assert.deepEqual(production.allowed_origins, [storeOrigin]);
  assert.equal(resolve(production.path), hostPath);
  assert.ok(readFileSync(hostPath).equals(readFileSync(binary)));
  const git = readFileSync(join(root, 'git-path'), 'utf8');
  assert.ok(isAbsolute(git) && existsSync(git));

  register(devId);
  assert.deepEqual(manifest().allowed_origins, [storeOrigin, `chrome-extension://${devId}/`]);
  if (windows) {
    assert.equal(resolve(registeredPath() || ''), manifestPath);
  } else {
    for (const [path, mode] of [[root, 0o700], [manifestPath, 0o600], [hostPath, 0o700]]) {
      assert.equal(statSync(path).mode & 0o777, mode);
    }
  }
  const timestamps = () => [manifestPath, hostPath].map(path => statSync(path, {bigint: true}).mtimeNs);
  const first = timestamps();
  register();
  assert.deepEqual(timestamps(), first);
  assert.notEqual(spawnSync(binary, ['register-native', 'invalid'], {cwd: home, env}).status, 0);
  assert.deepEqual(timestamps(), first);
  console.log('Native host registration, allowlist, executable, Git path and idempotence verified.');
} finally {
  const registryPath = windows && registeredPath();
  if (registryPath && resolve(registryPath) === manifestPath) {
    const result = reg('DELETE', key, '/f');
    assert.equal(result.status, 0, result.stderr);
  }
  rmSync(home, {recursive: true, force: true});
}
