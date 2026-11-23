import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import test from 'node:test';
import {runInNewContext} from 'node:vm';

const source = readFileSync(new URL('background.js', import.meta.url), 'utf8');
const tick = () => new Promise(setImmediate);
const copy = (value) => JSON.parse(JSON.stringify(value));
const profileId = '0123456789abcdef0123456789abcdef';

function event() {
  const callbacks = [];
  return {
    addListener(callback) { callbacks.push(callback); },
    emit(...args) { for (const callback of callbacks) callback(...args); }
  };
}

function extension(saved = {}) {
  const ports = [];
  const queries = [];
  const timers = new Map();
  const runtime = {onMessage: event(), onStartup: event(), onInstalled: event()};
  const storageChanged = event();
  let nextTimer = 0;
  let bookmarkUpdate;

  const bookmarkEvents = Object.fromEntries(['onCreated', 'onChanged', 'onMoved', 'onRemoved', 'onChildrenReordered']
    .map((name) => [name, event()]));
  runInNewContext(source, {
    TextEncoder,
    crypto: {randomUUID: () => '01234567-89ab-cdef-0123-456789abcdef'},
    setTimeout: (callback, delay) => {
      timers.set(++nextTimer, {callback, delay});
      return nextTimer;
    },
    clearTimeout: (id) => timers.delete(id),
    chrome: {
      action: {setBadgeText: async () => {}, setTitle: async () => {}},
      bookmarks: {
        ...bookmarkEvents,
        search: async (query) => { queries.push(query); return [{id: '11'}]; },
        update: async (id, patch) => {
          await bookmarkUpdate;
          return {id, ...patch};
        }
      },
      runtime: {
        ...runtime,
        connectNative(name) {
          const port = {
            name, sent: [], closed: false, onMessage: event(), onDisconnect: event(),
            postMessage(value) { this.sent.push(copy(value)); },
            disconnect() {
              if (this.closed) return;
              this.closed = true;
              this.onDisconnect.emit();
            }
          };
          ports.push(port);
          return port;
        }
      },
      storage: {
        local: {
          get: async () => ({...saved}),
          set: async (patch) => { Object.assign(saved, patch); }
        },
        onChanged: storageChanged
      }
    }
  }, {filename: 'background.js'});
  return {
    ports, queries, saved, timers,
    send(message) { return new Promise((resolve) => runtime.onMessage.emit(message, {}, resolve)); },
    pauseBookmarkUpdate() {
      let resume;
      bookmarkUpdate = new Promise((resolve) => { resume = resolve; });
      return resume;
    },
    wake() { runtime.onStartup.emit(); },
    setEnabled(enabled) {
      saved.enabled = enabled;
      storageChanged.emit({enabled: {newValue: enabled}}, 'local');
    }
  };
}

test('Upgraded profiles automatically connect natively and accept requests only after readiness', async () => {
  const ext = extension({connectionMode: 'legacy', port: 9876});
  await tick();
  const [native] = ext.ports;
  assert.equal(native.name, 'com.kobbokkom.bridgey');
  assert.deepEqual(native.sent, [{type: 'prepare', profileId}]);
  assert.equal(ext.saved.nativeProfileId, profileId);

  native.onMessage.emit({type: 'prepared'});
  await tick();
  assert.deepEqual(native.sent.at(-1), {type: 'activate'});
  native.onMessage.emit({type: 'request', request: {id: 1, method: 'search', params: {query: 'before ready'}}});
  assert.deepEqual(native.sent.at(-1), {id: 1, error: 'Bridgey Native is not ready.'});
  assert.deepEqual(ext.queries, []);
  native.onMessage.emit({type: 'ready'});
  await tick();
  assert.equal((await ext.send('state')).phase, 'connected');

  native.onMessage.emit({type: 'request', request: {id: 2, method: '__bridgey_health'}});
  assert.deepEqual(native.sent.at(-1), {id: 2, result: true});
  const data = JSON.stringify({id: 3, method: 'search', params: {query: '한글'}});
  native.onMessage.emit({type: 'requestChunk', id: 3, index: 0, total: 2, data: data.slice(0, 20)});
  assert.deepEqual(ext.queries, []);
  native.onMessage.emit({type: 'requestChunk', id: 3, index: 1, total: 2, data: data.slice(20)});
  await tick();
  assert.deepEqual(ext.queries, ['한글']);
  assert.deepEqual(native.sent.at(-1), {id: 3, result: [{id: '11'}]});

  native.onMessage.emit({type: 'requestChunk', id: 4, index: 1, total: 2, data});
  assert.deepEqual(native.sent.at(-1), {id: 4, error: 'Invalid chunked request.'});
  assert.deepEqual(ext.queries, ['한글']);

  ext.setEnabled(false);
  assert.equal(native.closed, true);
  assert.equal((await ext.send('state')).phase, 'off');
});

test('Native activation times out and a retry uses the existing profile ID', async () => {
  const ext = extension({nativeProfileId: profileId});
  await tick();
  const [native] = ext.ports;
  native.onMessage.emit({type: 'prepared'});
  await tick();
  const timeout = [...ext.timers.values()].find(({delay}) => delay === 6000);
  timeout.callback();
  await tick();
  assert.equal((await ext.send('state')).reason, 'host_timeout');
  const [retry] = ext.timers.values();
  retry.callback();
  await tick();
  assert.deepEqual(ext.ports[1].sent, [{type: 'prepare', profileId}]);
});

test('Setup can read a stable profile ID while the connection is disabled', async () => {
  const ext = extension({enabled: false});
  assert.equal(await ext.send('profile-id'), profileId);
  assert.equal(await ext.send('profile-id'), profileId);
  assert.equal(ext.saved.nativeProfileId, profileId);
  assert.equal(ext.ports.length, 0);
  ext.setEnabled(true);
  await tick();
  assert.deepEqual(ext.ports[0].sent, [{type: 'prepare', profileId}]);
});

test('Native reconnect waits for a Chrome bookmark update after its original host disconnects', async () => {
  const ext = extension({nativeProfileId: profileId});
  await tick();
  const [native] = ext.ports;
  native.onMessage.emit({type: 'prepared'});
  await tick();
  native.onMessage.emit({type: 'ready'});
  await tick();

  const finish = ext.pauseBookmarkUpdate();
  native.onMessage.emit({type: 'request', request: {id: 1, method: 'update', params: {id: '11', title: 'New'}}});
  assert.deepEqual(copy(await ext.send('reconnect')), {ok: false, reason: 'busy'});
  assert.equal(native.closed, false);

  native.disconnect();
  ext.wake();
  await tick();
  assert.equal(ext.ports.length, 1);
  assert.equal(ext.timers.size, 0);

  finish();
  await tick();
  assert.equal(native.sent.some(({id}) => id === 1), false);
  const [retry] = ext.timers.values();
  retry.callback();
  await tick();
  assert.equal(ext.ports.length, 2);
  assert.deepEqual(ext.ports[1].sent, [{type: 'prepare', profileId}]);
});
