const $ = (id) => document.getElementById(id);
const save = () => chrome.storage.local.set({
  port: Number($('port').value) || 8787,
  enabled: $('enabled').checked,
});

const light = (id, state, text) => {
  $('dot' + id).dataset.s = state;
  $('v' + id).textContent = text;
};

// The setup hint belongs on screen only while there is nothing to talk to.
// Showing it once connected would be noise.
const hint = (show) => { $('hint').hidden = !show; };

const ago = (t) => {
  if (!t) return '—';
  const s = Math.round((Date.now() - t) / 1000);
  return s < 60 ? `${s}s ago` : `${Math.round(s / 60)}m ago`;
};

async function refresh() {
  const {enabled} = await chrome.storage.local.get('enabled');
  if (enabled === false) {
    light('Worker', 'off', '—');
    light('Socket', 'off', 'Off');
    hint(false);
    return;
  }
  // A reply means the worker is alive. No reply means it is not.
  let st = null;
  try { st = await chrome.runtime.sendMessage('state'); } catch { st = null; }
  if (!st) {
    light('Worker', 'warn', 'Asleep');
    light('Socket', 'warn', 'Disconnected');
    hint(true);
    return;
  }
  light('Worker', 'on', 'Running');
  if (st.open) {
    const up = st.connectedSince ? ago(st.connectedSince).replace(' ago', '') : '—';
    light('Socket', 'on', `Up ${up} · ping ${ago(st.lastPing)}`);
    hint(false);
  } else {
    light('Socket', 'warn', 'Disconnected');
    hint(true);
  }
}

(async () => {
  const s = await chrome.storage.local.get(['port', 'enabled']);
  $('port').value = s.port || 8787;
  $('enabled').checked = s.enabled !== false;
  // Flush the checked state before transitions turn on, or it animates anyway.
  void $('enabled').offsetWidth;
  document.body.dataset.ready = '';
  refresh();
  setInterval(refresh, 1000);
  $('reconnect').addEventListener('click', async () => {
    try { await chrome.runtime.sendMessage('reconnect'); } catch {}
    setTimeout(refresh, 400);
  });
  // Reload the extension from here instead of going to chrome://extensions.
  $('reload').addEventListener('click', () => chrome.runtime.reload());
  $('port').addEventListener('change', save);
  $('enabled').addEventListener('change', () => { save(); refresh(); });
})();
