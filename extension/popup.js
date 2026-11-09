const $ = (id) => document.getElementById(id);
const NATIVE_PERMISSION = {permissions: ['nativeMessaging']};
const REASONS = {
  legacy_unreachable: 'Start the Legacy MCP server and check that its port matches this popup.',
  legacy_timeout: 'The Legacy connection timed out. Check the port and reconnect.',
  token_mismatch: 'The Legacy server rejected the token. Check both token settings.',
  permission_required: 'Allow Native Messaging when you reconnect.',
  host_not_registered: 'Start the Native MCP server once to register its host, then reconnect.',
  host_unavailable: 'The Native host is unavailable. Start the Native MCP server and reconnect.',
  host_timeout: 'The Native host did not respond in time. Check the setup guide and reconnect.',
  profile_in_use: 'Another browser profile is using the Native host. Disconnect it first.',
  protocol_error: 'The Native host sent an unexpected response. Update it and reconnect.',
  busy: 'Wait for the current bookmark request to finish, then try again.',
  cancelled: 'The connection settings changed. Try again.',
  enable_first: 'Turn on Bridgey before switching to Native Messaging.',
  mode_change_failed: 'Bridgey could not save the new mode. Try again.'
};

let applying = false;
let refreshing = false;
let notice;

function setText(id, value) {
  const element = $(id);
  if (element.textContent !== value) element.textContent = value;
}

function light(id, status, text) {
  $('dot' + id).dataset.s = status;
  setText('v' + id, text);
}

function showHint(reason, mode) {
  $('hint').hidden = !reason;
  if (!reason) return;
  setText('hintText', REASONS[reason] || REASONS.mode_change_failed);
  const setup = ['legacy_unreachable', 'host_unavailable', 'host_not_registered'].includes(reason);
  $('setupCommand').hidden = !setup;
  if (setup) setText('setupCommand', 'npx -y bookmark-bridge' + (mode === 'native' ? '@native' : ''));
}

async function refresh() {
  if (refreshing) return;
  refreshing = true;
  try {
    const saved = await chrome.storage.local.get(['enabled', 'connectionMode']);
    const mode = saved.connectionMode === 'native' ? 'native' : 'legacy';
    const enabled = saved.enabled !== false;
    const st = await chrome.runtime.sendMessage('state').catch(() => null);
    const switching = applying || st?.switching;
    if (!applying) $('mode').value = mode;
    $('mode').disabled = switching || !enabled;
    $('enabled').disabled = switching;
    $('port').disabled = switching;
    $('portLabel').hidden = $('portField').hidden = mode !== 'legacy';
    $('reconnect').disabled = switching || !enabled;

    light('Worker', enabled ? (st ? 'on' : 'warn') : 'off', enabled ? (st ? 'Running' : 'Asleep') : '—');
    if (!enabled) light('Socket', 'off', 'Off');
    else if (switching) light('Socket', 'warn', 'Switching…');
    else if (st?.phase === 'connected') light('Socket', 'on', 'Connected');
    else if (st?.phase === 'connecting') light('Socket', 'warn', 'Connecting…');
    else light('Socket', 'warn', 'Disconnected');

    const reason = enabled ? (notice?.reason || st?.reason || (!st && mode === 'legacy' ? 'legacy_unreachable' : '')) : '';
    showHint(reason, notice?.mode || mode);
  } finally {
    refreshing = false;
  }
}

async function changeMode(mode) {
  if (applying) return;
  applying = true;
  $('mode').disabled = true;
  $('reconnect').disabled = true;
  $('enabled').disabled = true;
  $('port').disabled = true;
  notice = undefined;
  try {
    // Chrome requires the request to begin in this popup's user gesture.
    if (mode === 'native' && !(await chrome.permissions.request(NATIVE_PERMISSION))) {
      notice = {mode, reason: 'permission_required'};
      return;
    }
    const result = await chrome.runtime.sendMessage({type: 'mode', mode});
    if (!result?.ok) notice = {mode, reason: result?.reason || 'mode_change_failed'};
  } catch {
    notice = {mode, reason: 'mode_change_failed'};
  } finally {
    applying = false;
    refresh();
  }
}

(async () => {
  const saved = await chrome.storage.local.get(['port', 'enabled']);
  $('port').value = saved.port || 8787;
  $('enabled').checked = saved.enabled !== false;
  // Flush the checked state before enabling transitions.
  void $('enabled').offsetWidth;
  document.body.dataset.ready = '';
  refresh();
  setInterval(refresh, 1000);
  $('mode').addEventListener('change', () => changeMode($('mode').value));
  $('reconnect').addEventListener('click', async () => {
    if ($('mode').value === 'native') {
      await changeMode('native');
    } else {
      notice = undefined;
      await chrome.runtime.sendMessage('reconnect').catch(() => {});
      refresh();
    }
  });
  $('reload').addEventListener('click', () => chrome.runtime.reload());
  $('port').addEventListener('change', () => {
    const port = $('port').valueAsNumber;
    if ($('port').reportValidity()) chrome.storage.local.set({port: Number.isNaN(port) ? 8787 : port});
  });
  $('enabled').addEventListener('change', async () => {
    notice = undefined;
    await chrome.storage.local.set({enabled: $('enabled').checked});
    refresh();
  });
})();
