// Each Chrome profile has one active transport. Legacy is the default for
// existing installs and the user must explicitly switch to Native Messaging.
let connection = null;
let generation = 0;
let connecting = false;
let retryTimer;
let backoff = 1000;
let migration = null;
const state = {mode: 'legacy', phase: 'connecting', reason: '', connectedSince: 0, lastPing: 0};
const NATIVE_HOST = 'com.kobbokkom.bridgey';
const NATIVE_PERMISSION = {permissions: ['nativeMessaging']};
const NATIVE_TIMEOUT = 6000;
const CHUNK_BYTES = 128 * 1024;
const MAX_REQUEST_BYTES = 64 * 1024 * 1024;

const HANDLERS = {
  tree:     ()  => chrome.bookmarks.getTree(),
  search:   (p) => chrome.bookmarks.search(p.query),
  children: (p) => chrome.bookmarks.getChildren(p.id),
  update:   async (p) => (await guard(p)) || chrome.bookmarks.update(p.id, pick(p, ['title', 'url'])),
  move:     async (p) => (await guard(p)) || chrome.bookmarks.move(p.id, pick(p, ['parentId', 'index'])),
  create:   async (p) => {
    // So a retry after a timeout does not duplicate: if the same item already
    // exists under this parent, return that one.
    if (p.ifAbsent !== false && p.parentId) {
      const sibs = await chrome.bookmarks.getChildren(p.parentId);
      const dup = sibs.find((c) => c.title === p.title && (c.url || '') === (p.url || ''));
      if (dup) return {...dup, deduped: true};
    }
    return chrome.bookmarks.create(pick(p, ['parentId', 'title', 'url', 'index']));
  },
  remove:   async (p) => (await guard(p)) || chrome.bookmarks.remove(p.id),
  // Removes a folder with its children. remove only takes empty folders, so this
  // is what folder merging needs.
  removeTree: async (p) => (await guard(p)) || chrome.bookmarks.removeTree(p.id),
  // What the user changed since the agent last ran - the equivalent of git status.
  drift: async (p) => {
    const store = await chrome.storage.local.get([LEDGER_KEY, DROPPED_KEY]);
    const log = store[LEDGER_KEY] || [];
    // The server passes the timestamp of its agent-head, so reading is idempotent:
    // ask twice and get the same answer. Clearing on read used to lose changes
    // whenever a caller looked without acting, and reset seq so the watermark
    // could not advance.
    const since = p.since || 0;
    const all = log.filter((e) => e.at > since);
    const items = all.filter((e) => e.who !== 'agent');
    const byAgent = all.length - items.length;
    // The ledger is capped, so it can silently forget. Say so rather than let the
    // caller read an empty list as "nothing happened".
    const dropped = store[DROPPED_KEY] || 0;
    if (p.clear) {
      await chrome.storage.local.set({[LEDGER_KEY]: [], [DROPPED_KEY]: 0});
    }
    return {since, count: items.length, agentAttributed: byAgent,
            dropped, retained: log.length, capacity: LEDGER_MAX, changes: items};
  },
  // Many at once. One round trip per call is unusable at hundreds of items.
  batch: async (p) => {
    const applied = [], skipped = [], failed = [], deduped = [];
    for (const op of p.ops || []) {
      try {
        if (!Object.hasOwn(HANDLERS, op.op)) throw new Error(`unknown op: ${op.op}`);
        const fn = HANDLERS[op.op];
        // Compare-and-swap. A current value different from the one planned against
        // is someone else's change, so leave it alone. Without expect, apply as is.
        const g = await guard(op);
        if (g) { skipped.push(g); continue; }
        const r = await fn(op);
        // A deduped create is not an application; counting it would make applied lie.
        if (r && r.deduped) { deduped.push({title: op.title, existingId: r.id}); continue; }
        applied.push(op.id ?? op.title ?? op.op);
      } catch (e) {
        failed.push({op: op.op, id: op.id ?? null, error: String(e && e.message || e)});
      }
    }
    return {applied: applied.length, deduped, skipped, failed};
  },
};

// Record who changed what: events raised while an agent op runs are agent, the
// rest are user. The worker dies at 30s idle, so this cannot live in memory - it
// is appended to storage.
let agentDepth = 0;
let agentUntil = 0;   // treat events up to this time as agent (absorbs trailing
                      // events from overlapping ops)
const LEDGER_KEY = 'ledger';
const DROPPED_KEY = 'ledgerDropped';
const LEDGER_MAX = 2000;

let noteChain = Promise.resolve();
function note(kind, detail) {
  // Attribution is decided at the moment the event fires. Reading it as the queue
  // drains would misclassify: when hundreds of events are backed up - a folder
  // deletion, say - and an agent op starts meanwhile, a whole batch of user changes
  // becomes agent and is dropped. The largest deletion goes missing the hardest.
  // So nothing is dropped. A misclassification would be a loss, so every event is
  // kept with its who and the server does the filtering. This is what used to make
  // user changes within 3s of an agent op vanish entirely.
  const who = (agentDepth > 0 || Date.now() < agentUntil) ? 'agent' : 'user';
  noteChain = noteChain.then(() => noteOne(who, kind, detail)).catch(() => {});
  return noteChain;
}

async function noteOne(who, kind, detail) {
  const store = await chrome.storage.local.get([LEDGER_KEY, DROPPED_KEY]);
  const log = store[LEDGER_KEY] || [];
  const seq = (log.length ? (log[log.length - 1].seq || 0) : 0) + 1;
  log.push({seq, at: Date.now(), who, kind, ...detail});
  const write = {[LEDGER_KEY]: log};
  if (log.length > LEDGER_MAX) {
    // Losing the oldest entries is acceptable; losing them silently is not.
    // drift reports this count so a caller knows the ledger is incomplete.
    const cut = log.length - LEDGER_MAX;
    log.splice(0, cut);
    write[DROPPED_KEY] = (store[DROPPED_KEY] || 0) + cut;
  }
  await chrome.storage.local.set(write);
}

chrome.bookmarks.onCreated.addListener((id, n) =>
  note('created', {id, title: n.title, url: n.url ?? null, parentId: n.parentId}));
chrome.bookmarks.onChanged.addListener((id, info) =>
  note('changed', {id, ...info}));
chrome.bookmarks.onMoved.addListener((id, info) =>
  note('moved', {id, from: info.oldParentId, to: info.parentId, index: info.index}));
chrome.bookmarks.onRemoved.addListener((id, info) => {
  // Deleting a folder fires one event for the top node only. Without expanding the
  // children the deletion is under-reported, which is exactly the shape of a whole
  // folder going away.
  const out = [];
  (function flat(n, parent) {
    if (!n) return;
    out.push({id: n.id, title: n.title ?? null, url: n.url ?? null, parentId: parent});
    (n.children || []).forEach((c) => flat(c, n.id));
  })(info.node, info.parentId);
  out.forEach((n) => note('removed', n));
});
// Sorting a folder by name fires only this event. Miss it and reordering is invisible.
chrome.bookmarks.onChildrenReordered.addListener((id, info) =>
  // childIds is the new order. Without it the event says "something moved" and
  // nothing about what the order became, which cannot be reviewed or undone.
  note('reordered', {id, childIds: (info && info.childIds) || null}));


const norm = (x) => String(x ?? '').normalize('NFC');

async function guard(p) {
  if (!p || !p.expect || !p.id) return null;
  const [cur] = await chrome.bookmarks.get(p.id);
  const mism = Object.entries(p.expect)
    // The same characters in different Unicode normal forms compare unequal. This
    // really happens with Hangul and combining marks, and produces an invisible
    // false mismatch, so normalize both sides before comparing.
    .filter(([k, v]) => norm(cur[k]) !== norm(v))
    .map(([k, v]) => ({field: k, expected: v, actual: cur[k] ?? null}));
  return mism.length ? {skipped: true, id: p.id, mismatch: mism} : null;
}

const pick = (o, keys) => Object.fromEntries(
  keys.filter((k) => o[k] !== undefined).map((k) => [k, o[k]]));

// The extension directory owns the Legacy defaults; the popup owns overrides.
async function legacySettings(saved) {
  let port = 8787;
  try {
    const cfg = await (await fetch(chrome.runtime.getURL('config.json'))).json();
    if (cfg.port) port = cfg.port;
  } catch {}
  let token = '';
  try {
    token = (await (await fetch(chrome.runtime.getURL('token.txt'))).text()).trim();
  } catch { /* No token file means the server runs without one too. */ }
  return {port: saved.port || port, token: saved.token || token};
}

const readSettings = () => chrome.storage.local.get(['port', 'enabled', 'token', 'connectionMode', 'nativeProfileId']);
const savedMode = (saved) => saved.connectionMode === 'native' ? 'native' : 'legacy';

let profileReady;
function profileId(saved) {
  if (/^[a-f0-9]{32}$/.test(saved.nativeProfileId)) return saved.nativeProfileId;
  if (!profileReady) {
    const id = crypto.randomUUID().replaceAll('-', '');
    profileReady = chrome.storage.local.set({nativeProfileId: id}).then(() => id).catch((error) => {
      profileReady = undefined;
      throw error;
    });
  }
  return profileReady;
}

function setStatus(phase, reason = '') {
  if (phase === 'connected' && state.phase !== 'connected') {
    state.connectedSince = Date.now();
    backoff = 1000;
  }
  if (phase !== 'connected') {
    state.connectedSince = 0;
    state.lastPing = 0;
  }
  Object.assign(state, {phase, reason});
  const badge = phase === 'connected' ? '' : reason ? '!' : '·';
  chrome.action.setBadgeText({text: badge}).catch(() => {});
  chrome.action.setTitle({title: 'Bridgey — ' + phase}).catch(() => {});
}

function stopConnection() {
  generation++;
  connecting = false;
  clearTimeout(retryTimer);
  retryTimer = undefined;
  const previous = connection;
  connection = null;
  previous?.close();
}

function retry() {
  if (retryTimer || migration) return;
  retryTimer = setTimeout(() => {
    retryTimer = undefined;
    connect();
  }, backoff);
  backoff = Math.min(backoff * 2, 30000);
}

function disconnected(transport, reason) {
  if (connection !== transport) return;
  connection = null;
  transport.close();
  setStatus('disconnected', reason);
  retry();
}

async function connect() {
  if (connecting || connection || migration) return;
  const attempt = generation;
  connecting = true;
  try {
    const saved = await readSettings();
    if (attempt !== generation) return;
    state.mode = savedMode(saved);
    if (saved.enabled === false) { setStatus('off'); return; }

    setStatus('connecting', state.reason);
    if (state.mode === 'legacy') {
      const settings = await legacySettings(saved);
      if (attempt === generation) openLegacy(settings);
    } else {
      if (!(await chrome.permissions.contains(NATIVE_PERMISSION))) {
        if (attempt === generation) setStatus('disconnected', 'permission_required');
        return;
      }
      if (attempt !== generation) return;
      const id = await profileId(saved);
      if (attempt !== generation) return;
      const transport = openNative(id);
      connection = transport;
      try {
        await transport.prepare();
        await transport.activate();
        if (connection === transport) {
          transport.accept();
          setStatus('connected');
        }
      } catch (error) {
        disconnected(transport, error.message);
      }
    }
  } catch {
    if (attempt === generation) {
      setStatus('disconnected', state.mode === 'native' ? 'host_unavailable' : 'legacy_unreachable');
      retry();
    }
  } finally {
    if (attempt === generation) connecting = false;
  }
}

function restart() {
  stopConnection();
  backoff = 1000;
  if (migration) {
    migration.cancelled = true;
    migration.candidate?.close();
  } else {
    connect();
  }
}

function openLegacy({port, token}) {
  const ws = new WebSocket('ws://127.0.0.1:' + port);
  let keepalive;
  const timeout = setTimeout(() => disconnected(transport, 'legacy_timeout'), 6000);
  const transport = {
    close() {
      clearTimeout(timeout);
      clearInterval(keepalive);
      ws.close();
    }
  };
  const send = (value) => {
    if (connection !== transport || ws.readyState !== WebSocket.OPEN) return;
    try { ws.send(typeof value === 'string' ? value : JSON.stringify(value)); } catch {}
  };
  connection = transport;
  ws.onopen = () => {
    if (connection !== transport) return;
    clearTimeout(timeout);
    send('ext:' + token);
    setStatus('connected');
    keepalive = setInterval(() => send('ka'), 20000);
  };
  ws.onclose = () => disconnected(transport, 'legacy_unreachable');
  ws.onmessage = ({data}) => {
    if (connection !== transport) return;
    if (data === 'unauthorized') {
      disconnected(transport, 'token_mismatch');
    } else if (data === 'ping') {
      state.lastPing = Date.now();
      send('pong');
    } else {
      let request;
      try { request = JSON.parse(data); } catch { return; }
      dispatch(request, send);
    }
  };
}

const validId = (id) => Number.isSafeInteger(id) && id >= 0;
async function dispatch(request, send) {
  if (!validId(request?.id)) return;
  const {id, method, params} = request;
  if (method === '__bridgey_health') { send({id, result: true}); return; }
  if (typeof method !== 'string' || !Object.hasOwn(HANDLERS, method)) {
    send({id, error: 'Unknown bookmark method.'});
    return;
  }
  // Bookmark events can arrive after a request completes. Keep the grace
  // period for trailing events and track overlapping operations independently.
  agentDepth++;
  try {
    const result = await HANDLERS[method](params || {});
    send({id, result: result ?? null});
  } catch (error) {
    send({id, error: String(error?.message || error)});
  } finally {
    agentDepth--;
    agentUntil = Math.max(agentUntil, Date.now() + 3000);
  }
}

function openNative(id) {
  const port = chrome.runtime.connectNative(NATIVE_HOST);
  let pending;
  let closed = false;
  let ready = false;
  let accepting = false;
  let queuedRequest;
  let chunk;
  const encoder = new TextEncoder();

  function complete(reason) {
    if (!pending) return;
    const {resolve, reject, timer} = pending;
    pending = undefined;
    clearTimeout(timer);
    if (reason) reject(new Error(reason));
    else resolve();
  }

  const transport = {
    prepare: () => exchange({type: 'prepare', profileId: id}, 'prepared'),
    activate: () => exchange({type: 'activate'}, 'ready'),
    accept() {
      accepting = true;
      if (queuedRequest) dispatch(queuedRequest, send);
      queuedRequest = undefined;
    },
    close(reason = 'host_unavailable') {
      if (closed) return;
      closed = true;
      chunk = undefined;
      queuedRequest = undefined;
      complete(reason);
      try { port.disconnect(); } catch {}
    }
  };

  function fail(reason) {
    transport.close(reason);
    disconnected(transport, reason);
  }

  function exchange(message, expected) {
    if (closed) return Promise.reject(new Error('host_unavailable'));
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => fail('host_timeout'), NATIVE_TIMEOUT);
      pending = {expected, resolve, reject, timer};
      try { port.postMessage(message); }
      catch { fail('host_unavailable'); }
    });
  }

  function send(value) {
    if (closed || connection !== transport) return;
    try { port.postMessage(value); } catch { fail('host_unavailable'); }
  }

  function dispatchWhenCommitted(request) {
    if (accepting) {
      dispatch(request, send);
    } else if (request.method === '__bridgey_health') {
      send({id: request.id, result: false});
    } else if (!queuedRequest) {
      // The host serializes bookmark calls. One may arrive while the new mode
      // is being saved, after the host has already advertised readiness.
      queuedRequest = request;
    } else {
      send({id: request.id, error: 'Bridgey Native is still switching modes.'});
    }
  }

  function handleChunk(message) {
    const {id, index, total, data} = message;
    const invalid = () => {
      const ids = new Set([chunk?.id, id]);
      chunk = undefined;
      for (const requestId of ids) {
        if (validId(requestId)) send({id: requestId, error: 'Invalid chunked request.'});
      }
    };
    const size = typeof data === 'string' && data.length <= CHUNK_BYTES ? encoder.encode(data).length : 0;
    // UTF-8 boundaries can leave up to three bytes unused in a slice.
    if (!validId(id) || !Number.isInteger(index) || !Number.isInteger(total) ||
        total < 1 || total > Math.ceil(MAX_REQUEST_BYTES / (CHUNK_BYTES - 3)) ||
        index < 0 || index >= total || size < 1 || size > CHUNK_BYTES) {
      invalid();
      return;
    }
    if (!chunk && index === 0) chunk = {id, total, parts: [], bytes: 0};
    if (!chunk || chunk.id !== id || chunk.total !== total || chunk.parts.length !== index ||
        chunk.bytes + size > MAX_REQUEST_BYTES) {
      invalid();
      return;
    }
    chunk.parts.push(data);
    chunk.bytes += size;
    if (chunk.parts.length < total) return;

    let request;
    try { request = JSON.parse(chunk.parts.join('')); } catch {}
    if (request?.id !== id || typeof request.method !== 'string') { invalid(); return; }
    chunk = undefined;
    dispatchWhenCommitted(request);
  }

  port.onDisconnect.addListener(() => {
    const message = chrome.runtime.lastError?.message || '';
    fail(message.includes('not found') || message.includes('forbidden') ? 'host_not_registered' : 'host_unavailable');
  });
  port.onMessage.addListener((message) => {
    if (closed) return;
    if (message?.type === 'error') {
      const reasons = ['profile_in_use', 'host_unavailable', 'protocol_error'];
      fail(reasons.includes(message.reason) ? message.reason : 'protocol_error');
      return;
    }
    if (pending && message?.type === pending.expected) {
      if (message.type === 'ready') ready = true;
      complete();
      return;
    }
    if (message?.type !== 'request' && message?.type !== 'requestChunk') {
      fail('protocol_error');
      return;
    }
    if (!ready || connection !== transport) {
      const id = message.request?.id ?? message.id;
      if (validId(id)) send({id, error: 'Bridgey Native is not ready.'});
      return;
    }
    if (message.type === 'requestChunk') {
      handleChunk(message);
    } else {
      if (chunk) {
        send({id: chunk.id, error: 'Interrupted chunked request.'});
        chunk = undefined;
      }
      if (!validId(message.request?.id)) { fail('protocol_error'); return; }
      dispatchWhenCommitted(message.request);
    }
  });
  return transport;
}

async function chooseMode(mode) {
  if (mode !== 'native' && mode !== 'legacy') return {ok: false, reason: 'protocol_error'};
  if (migration) return {ok: false, reason: 'busy'};
  const change = {candidate: null, cancelled: false};
  migration = change;
  let switched = false;
  try {
    const saved = await readSettings();
    if (change.cancelled) return {ok: false, reason: 'cancelled'};
    if (savedMode(saved) === mode) {
      stopConnection();
      return {ok: true};
    }
    if (agentDepth) return {ok: false, reason: 'busy'};

    if (mode === 'native') {
      if (saved.enabled === false) return {ok: false, reason: 'enable_first'};
      if (!(await chrome.permissions.contains(NATIVE_PERMISSION))) {
        return {ok: false, reason: 'permission_required'};
      }
      const id = await profileId(saved);
      if (change.cancelled) return {ok: false, reason: 'cancelled'};
      change.candidate = openNative(id);
      await change.candidate.prepare();
      if (agentDepth || change.cancelled) return {ok: false, reason: agentDepth ? 'busy' : 'cancelled'};
      // The old connection stays usable through prepare. Retire it before the
      // host publishes its endpoint, so both routes cannot accept writes.
      stopConnection();
      setStatus('connecting');
      connection = change.candidate;
      await change.candidate.activate();
      if (change.cancelled) return {ok: false, reason: 'cancelled'};
      if (connection !== change.candidate) throw new Error(state.reason || 'host_unavailable');
      await chrome.storage.local.set({connectionMode: 'native'});
      if (change.cancelled || connection !== change.candidate) {
        await chrome.storage.local.set({connectionMode: 'legacy'});
        return {ok: false, reason: change.cancelled ? 'cancelled' : state.reason || 'host_unavailable'};
      }
      state.mode = 'native';
      connection.accept();
      setStatus('connected');
    } else {
      stopConnection();
      setStatus('connecting');
      await chrome.storage.local.set({connectionMode: 'legacy'});
    }
    switched = true;
    return {ok: true};
  } catch (error) {
    const reasons = ['profile_in_use', 'host_unavailable', 'host_timeout', 'host_not_registered', 'protocol_error'];
    return {ok: false, reason: change.cancelled ? 'cancelled' :
      reasons.includes(error.message) ? error.message : 'mode_change_failed'};
  } finally {
    if (change.candidate && !switched) {
      if (connection === change.candidate) stopConnection();
      else change.candidate.close();
    }
    migration = null;
    if (!connection) {
      setStatus('connecting');
      connect();
    }
  }
}

chrome.runtime.onMessage.addListener((msg, _sender, reply) => {
  if (msg === 'reconnect') {
    restart();
    reply({ok: true});
  } else if (msg === 'state') {
    reply({...state, switching: Boolean(migration)});
  } else if (msg?.type === 'mode') {
    chooseMode(msg.mode).then(reply);
    return true;
  }
  return false;
});

chrome.runtime.onStartup.addListener(connect);
chrome.runtime.onInstalled.addListener(connect);
chrome.storage.onChanged.addListener((changes, area) => {
  if (area === 'local' && ('port' in changes || 'token' in changes || 'enabled' in changes)) restart();
});
chrome.permissions.onRemoved.addListener(({permissions}) => {
  if (permissions?.includes('nativeMessaging') && (state.mode === 'native' || migration)) restart();
});
connect();
