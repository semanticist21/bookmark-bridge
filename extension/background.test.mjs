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

function extension(saved = {}, permission = true) {
  const ports = [];
  const sockets = [];
  const queries = [];
  const timers = new Map();
  const runtime = {onMessage: event(), onStartup: event(), onInstalled: event()};
  const storageChanged = event();
  const permissionsRemoved = event();
  let nextTimer = 0;
  let nativeCommit;
  let bookmarkUpdate;

  class Socket {
    static OPEN = 1;
    readyState = 0;
    sent = [];
    constructor(url) { this.url = url; sockets.push(this); }
    open() { this.readyState = Socket.OPEN; this.onopen?.(); }
    close() { this.readyState = 3; this.onclose?.(); }
    message(data) { this.onmessage?.({data}); }
    send(data) { this.sent.push(data); }
  }

  const bookmarkEvents = Object.fromEntries(['onCreated', 'onChanged', 'onMoved', 'onRemoved', 'onChildrenReordered']
    .map((name) => [name, event()]));
  runInNewContext(source, {
    WebSocket: Socket,
    TextEncoder,
    crypto: {randomUUID: () => '01234567-89ab-cdef-0123-456789abcdef'},
    fetch: async (name) => ({json: async () => ({}), text: async () => name === 'token.txt' ? 'file-token' : ''}),
    setTimeout: (callback, delay) => {
      timers.set(++nextTimer, {callback, delay});
      return nextTimer;
    },
    clearTimeout: (id) => timers.delete(id),
    setInterval: () => 0,
    clearInterval: () => {},
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
      permissions: {contains: async () => permission, onRemoved: permissionsRemoved},
      runtime: {
        ...runtime,
        getURL: (name) => name,
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
          set: async (patch) => {
            if (patch.connectionMode === 'native') await nativeCommit;
            Object.assign(saved, patch);
          }
        },
        onChanged: storageChanged
      }
    }
  }, {filename: 'background.js'});
  return {
    ports, sockets, queries, saved, timers,
    send(message) { return new Promise((resolve) => runtime.onMessage.emit(message, {}, resolve)); },
    pauseNativeCommit() {
      let resume;
      nativeCommit = new Promise((resolve) => { resume = resolve; });
      return resume;
    },
    pauseBookmarkUpdate() {
      let resume;
      bookmarkUpdate = new Promise((resolve) => { resume = resolve; });
      return resume;
    },
    wake() { runtime.onStartup.emit(); },
    revoke() { permission = false; permissionsRemoved.emit({permissions: ['nativeMessaging']}); }
  };
}

test('Legacy remains the default and a failed Native prepare leaves the existing socket in use', async () => {
  const ext = extension({port: 9876, token: 'saved-token'});
  await tick();
  assert.equal(ext.sockets.length, 1);
  const [legacy] = ext.sockets;
  assert.equal(legacy.url, 'ws://127.0.0.1:9876');
  legacy.open();
  assert.deepEqual(legacy.sent, ['ext:saved-token']);

  const change = ext.send({type: 'mode', mode: 'native'});
  await tick();
  const [native] = ext.ports;
  assert.equal(native.name, 'com.kobbokkom.bridgey');
  assert.deepEqual(native.sent, [{type: 'prepare', profileId}]);
  assert.equal(legacy.readyState, 1);
  native.onMessage.emit({type: 'error', reason: 'profile_in_use'});
  assert.deepEqual(copy(await change), {ok: false, reason: 'profile_in_use'});
  assert.equal(ext.saved.connectionMode, undefined);
  assert.equal(legacy.readyState, 1);
  assert.equal((await ext.send('state')).phase, 'connected');
});

test('Native commits only after readiness and uses the same bookmark dispatcher for chunked requests', async () => {
  const ext = extension();
  await tick();
  const [legacy] = ext.sockets;
  legacy.open();

  const change = ext.send({type: 'mode', mode: 'native'});
  await tick();
  const [native] = ext.ports;
  native.onMessage.emit({type: 'prepared'});
  await tick();
  assert.equal(legacy.readyState, 3);
  assert.deepEqual(native.sent.at(-1), {type: 'activate'});
  assert.equal(ext.saved.connectionMode, undefined);
  native.onMessage.emit({type: 'request', request: {id: 1, method: 'search', params: {query: 'before ready'}}});
  assert.deepEqual(native.sent.at(-1), {id: 1, error: 'Bridgey Native is not ready.'});
  const resume = ext.pauseNativeCommit();
  native.onMessage.emit({type: 'ready'});
  await tick();
  native.onMessage.emit({type: 'request', request: {id: 5, method: 'search', params: {query: 'during commit'}}});
  assert.deepEqual(ext.queries, []);
  assert.equal(ext.saved.connectionMode, undefined);
  resume();
  assert.deepEqual(copy(await change), {ok: true});
  await tick();
  assert.deepEqual(native.sent.at(-1), {id: 5, result: [{id: '11'}]});
  assert.deepEqual(ext.queries, ['during commit']);
  assert.equal(ext.saved.connectionMode, 'native');
  assert.equal(ext.saved.nativeProfileId, profileId);
  assert.equal((await ext.send('state')).phase, 'connected');

  native.onMessage.emit({type: 'request', request: {id: 2, method: '__bridgey_health'}});
  assert.deepEqual(native.sent.at(-1), {id: 2, result: true});
  const data = JSON.stringify({id: 3, method: 'search', params: {query: '한글'}});
  native.onMessage.emit({type: 'requestChunk', id: 3, index: 0, total: 2, data: data.slice(0, 20)});
  assert.deepEqual(ext.queries, ['during commit']);
  native.onMessage.emit({type: 'requestChunk', id: 3, index: 1, total: 2, data: data.slice(20)});
  await tick();
  assert.deepEqual(ext.queries, ['during commit', '한글']);
  assert.deepEqual(native.sent.at(-1), {id: 3, result: [{id: '11'}]});

  native.onMessage.emit({type: 'requestChunk', id: 4, index: 1, total: 2, data});
  assert.deepEqual(native.sent.at(-1), {id: 4, error: 'Invalid chunked request.'});
  assert.deepEqual(ext.queries, ['during commit', '한글']);

  ext.revoke();
  await tick();
  assert.equal(native.closed, true);
  assert.equal(ext.sockets.length, 1);
  assert.equal(ext.saved.connectionMode, 'native');
  assert.equal((await ext.send('state')).reason, 'permission_required');

  assert.deepEqual(copy(await ext.send({type: 'mode', mode: 'legacy'})), {ok: true});
  await tick();
  assert.equal(ext.saved.connectionMode, 'legacy');
  assert.equal(ext.sockets.length, 2);
});

test('failed Native activation resumes Legacy and committed Native startup never falls back', async () => {
  const ext = extension();
  await tick();
  const change = ext.send({type: 'mode', mode: 'native'});
  await tick();
  const [native] = ext.ports;
  native.onMessage.emit({type: 'prepared'});
  await tick();
  assert.equal(ext.sockets[0].readyState, 3);
  const timeout = [...ext.timers.values()].find(({delay}) => delay === 6000);
  timeout.callback();
  assert.deepEqual(copy(await change), {ok: false, reason: 'host_timeout'});
  await tick();
  assert.equal(ext.saved.connectionMode, undefined);
  assert.equal(ext.sockets.length, 2);

  const committed = extension({connectionMode: 'native', nativeProfileId: profileId});
  await tick();
  committed.ports[0].disconnect();
  await tick();
  assert.equal(committed.saved.connectionMode, 'native');
  assert.equal(committed.sockets.length, 0);
  assert.equal((await committed.send('state')).reason, 'host_unavailable');
});

test('permission revoked during initial Native persistence cancels opt-in and discards queued work', async () => {
  const ext = extension();
  await tick();
  const change = ext.send({type: 'mode', mode: 'native'});
  await tick();
  const [native] = ext.ports;
  native.onMessage.emit({type: 'prepared'});
  await tick();
  const resume = ext.pauseNativeCommit();
  native.onMessage.emit({type: 'ready'});
  await tick();
  native.onMessage.emit({type: 'request', request: {id: 1, method: 'search', params: {query: 'discard me'}}});
  ext.revoke();
  resume();
  assert.deepEqual(copy(await change), {ok: false, reason: 'cancelled'});
  await tick();
  assert.deepEqual(ext.queries, []);
  assert.equal(ext.saved.connectionMode, 'legacy');
  assert.equal(ext.sockets.length, 2);
});

test('Native reconnect waits for a Chrome bookmark update after its original host disconnects', async () => {
  const ext = extension({connectionMode: 'native', nativeProfileId: profileId});
  await tick();
  const [native] = ext.ports;
  native.onMessage.emit({type: 'prepared'});
  await tick();
  native.onMessage.emit({type: 'ready'});
  await tick();

  const finish = ext.pauseBookmarkUpdate();
  native.onMessage.emit({type: 'request', request: {id: 1, method: 'update', params: {id: '11', title: 'New'}}});
  assert.deepEqual(copy(await ext.send({type: 'mode', mode: 'native'})), {ok: false, reason: 'busy'});
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
