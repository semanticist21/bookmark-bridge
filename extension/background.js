// The MCP server is the WebSocket server. An extension cannot listen on a port,
// so this side dials out and reconnects when dropped. Chrome lets the service
// worker sleep, so an alarm wakes it again.
let socket = null;
let connecting = false;
let backoff = 1000;       // reconnect delay; reset to 1s on success
let keepTimer = null;
let connectedSince = 0;   // when the socket opened
let lastPing = 0;         // when the last server ping arrived

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
        const fn = HANDLERS[op.op];
        if (!fn) throw new Error(`unknown op: ${op.op}`);
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


function scheduleReconnect() {
  backoff = Math.min(backoff * 2, 30000);
  setTimeout(connect, backoff);
}

// Chrome's docs ask the extension to send a message every 20s as well. Doubling up
// with the server ping keeps the worker from sleeping immediately when no MCP
// process is attached.
function keepAlive() {
  clearInterval(keepTimer);
  keepTimer = setInterval(() => {
    if (socket && socket.readyState === WebSocket.OPEN) {
      try { socket.send('ka'); } catch {}
    } else { clearInterval(keepTimer); }
  }, 20000);
}

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

// The extension directory owns the defaults; the popup owns the overrides.
async function settings() {
  let port = 8787;
  try {
    const cfg = await (await fetch(chrome.runtime.getURL('config.json'))).json();
    if (cfg.port) port = cfg.port;
  } catch {}
  let token = '';
  try {
    token = (await (await fetch(chrome.runtime.getURL('token.txt'))).text()).trim();
  } catch { /* No token file means the server runs without one too. */ }
  const saved = await chrome.storage.local.get(['port', 'enabled', 'token']);
  if (saved.port) port = saved.port;
  if (saved.token) token = saved.token;
  // No token by default; binding to loopback is enough. To use one, set
  // BOOKMARK_BRIDGE_TOKEN on the server and store the same value here.
  return {port, token, enabled: saved.enabled !== false};
}

async function connect() {
  // Only OPEN counts, so a socket stuck at readyState 0 (CONNECTING) cannot block
  // retries forever.
  if (connecting || (socket && socket.readyState === WebSocket.OPEN)) return;
  connecting = true;
  try { await open_socket(); } finally { connecting = false; }
}

async function open_socket() {
  const {port, token, enabled} = await settings();
  if (!enabled) { await setBadge('\u00b7', 'Off'); return; }

  // Handlers look at their own instance (ws), not the global socket. When two
  // sockets overlap, the old one's onclose would null out the live new one and
  // replies would leave over the wrong connection. That is a real failure seen here.
  let ws;
  try { ws = new WebSocket(`ws://127.0.0.1:${port}`); } catch { scheduleReconnect(); return; }
  // A pending socket can die, so every send goes through this.
  const trySend = (v) => {
    try { ws.send(typeof v === 'string' ? v : JSON.stringify(v)); return true; }
    catch { return false; }
  };
  const stale = () => socket !== ws;
  socket = ws;

  await new Promise((done) => {
    ws.onopen = () => {
      if (stale()) return ws.close();
      connectedSince = Date.now(); lastPing = 0;
      backoff = 1000;
      done();                                   // first, so a throwing send cannot strand the latch
      trySend(`ext:${token || ''}`);
      setBadge('', 'Connected');
      keepAlive();
    };
    ws.onclose = () => {
      if (!stale()) {
        socket = null; connectedSince = 0; setBadge('\u00b7', 'Disconnected');
        scheduleReconnect();      // waiting for the alarm (up to 60s) leaves the bridge dead
      }
      done();
    };
    ws.onerror = () => { setBadge('\u00b7', 'Server not found'); };
    ws.onmessage = async (ev) => {
      if (stale()) return ws.close();
      if (ev.data === 'unauthorized') { setBadge('!', 'Token mismatch'); return; }
      if (ev.data === 'ping') { lastPing = Date.now(); trySend('pong'); return; }
      let req;
      try { req = JSON.parse(ev.data); } catch { return; }
      const fn = HANDLERS[req.method];
      if (!fn) return trySend({id: req.id, error: `unknown: ${req.method}`});
      try {
        // Events keep arriving briefly after an op finishes. Dropping the flag at
        // once would record agent changes as user, so there is a grace period.
        // Depth alone is not enough: with overlapping ops, the first to finish would
        // let its grace timer mark another op's events as user. So depth drops
        // immediately and a timestamp covers the trailing events.
        agentDepth++;
        let result;
        try { result = await fn(req.params || {}); }
        finally {
          agentDepth--;
          agentUntil = Math.max(agentUntil, Date.now() + 3000);
        }
        trySend({id: req.id, result: result ?? null});
      } catch (e) {
        trySend({id: req.id, error: String((e && e.message) || e)});
      }
    };
  });
}

async function setBadge(text, title) {
  await chrome.action.setBadgeText({text});
  await chrome.action.setTitle({title: `Bridgey — ${title}`});
}

// The alarm is registered unconditionally at top level. Registering it only inside
// onInstalled means it is not re-registered after a reload and the worker stays
// asleep - another real failure seen here.
// The popup must not trust the action title: when the worker dies the title stays
// "Connected". It asks the live worker directly, and no reply is itself the proof
// that the bridge is down.
chrome.runtime.onMessage.addListener((msg, _sender, reply) => {
  if (msg === 'reconnect') {
    if (socket) { socket.close(); socket = null; }
    connect(); reply({ok: true}); return true;
  }
  if (msg !== 'state') return false;
  reply({open: !!socket && socket.readyState === WebSocket.OPEN, connectedSince, lastPing});
  return true;
});

chrome.alarms.create('keepalive', {periodInMinutes: 0.5});
chrome.runtime.onStartup.addListener(connect);
chrome.runtime.onInstalled.addListener(connect);
chrome.alarms.onAlarm.addListener(connect);
chrome.storage.onChanged.addListener((changes, area) => {
  // The ledger writes to the same storage area. Without a key filter the socket
  // would tear down and reconnect on every bookmark the user edits. Reconnect only
  // when a setting actually changed.
  if (area !== 'local') return;
  if (!('port' in changes || 'token' in changes || 'enabled' in changes)) return;
  const old = socket; socket = null;
  if (old) old.close();
  connect();
});
connect();
