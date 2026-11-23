const $ = (id) => document.getElementById(id);
const STORE_ID = 'ekokljgbbianihlpahackkfdendobngd';
const REASONS = {
  host_not_registered: 'Start the Native MCP server once to register its host, then reconnect.',
  host_unavailable: 'The Native host is unavailable. Start the Native MCP server and reconnect.',
  host_timeout: 'The Native host did not respond in time. Check the setup guide and reconnect.',
  profile_in_use: 'An older Bridgey host already has a connection. Update your MCP server and reconnect.',
  profile_id_in_use: 'Bridgey is already connected with this profile ID.',
  host_upgrade_required: 'Start the updated MCP server, then reconnect Bridgey in the profile still using the older host.',
  protocol_error: 'The Native host sent an unexpected response. Update it and reconnect.',
  busy: 'Wait for the current bookmark request to finish, then try again.'
};

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

function showHint(reason) {
  $('hint').hidden = !reason;
  if (!reason) return;
  setText('hintText', REASONS[reason] || REASONS.host_unavailable);
  const setup = ['host_unavailable', 'host_not_registered'].includes(reason);
  $('setupCommand').hidden = !setup;
}

async function refresh() {
  if (refreshing) return;
  refreshing = true;
  try {
    const saved = await chrome.storage.local.get('enabled');
    const enabled = saved.enabled !== false;
    const st = await chrome.runtime.sendMessage('state').catch(() => null);
    $('reconnect').disabled = !enabled;

    light('Worker', enabled ? (st ? 'on' : 'warn') : 'off', enabled ? (st ? 'Running' : 'Asleep') : '—');
    if (!enabled) light('Socket', 'off', 'Off');
    else if (st?.phase === 'connected') light('Socket', 'on', 'Connected');
    else if (st?.phase === 'connecting') light('Socket', 'warn', 'Connecting…');
    else light('Socket', 'warn', 'Disconnected');

    showHint(enabled ? (notice || st?.reason || (!st ? 'host_unavailable' : '')) : '');
  } finally {
    refreshing = false;
  }
}

(async () => {
  const saved = await chrome.storage.local.get('enabled');
  $('enabled').checked = saved.enabled !== false;
  // Flush the checked state before enabling transitions.
  void $('enabled').offsetWidth;
  document.body.dataset.ready = '';
  refresh();
  setInterval(refresh, 1000);
  $('reconnect').addEventListener('click', async () => {
    const result = await chrome.runtime.sendMessage('reconnect').catch(() => null);
    notice = result ? result.reason : 'host_unavailable';
    refresh();
  });
  $('reload').addEventListener('click', () => chrome.runtime.reload());
  if (chrome.runtime.id !== STORE_ID) {
    $('unpacked').hidden = false;
    setText('unpackedCommand', 'npx -y bookmark-bridge register-native ' + chrome.runtime.id);
  }
  for (const button of document.querySelectorAll('[data-guide]')) {
    button.addEventListener('click', async () => {
      $('guide').showModal();
      const id = await chrome.runtime.sendMessage('profile-id').catch(() => null);
      if (id) {
        setText('profileId', id);
        $('profileSetup').hidden = false;
      }
    });
  }
  $('guideClose').addEventListener('click', () => $('guide').close());
  for (const button of document.querySelectorAll('.copy button')) {
    button.addEventListener('click', async () => {
      const copied = await navigator.clipboard.writeText(button.parentElement.querySelector('code').textContent)
        .then(() => true, () => false);
      button.textContent = copied ? 'Copied' : 'Select and copy';
      setTimeout(() => { button.textContent = 'Copy'; }, 1500);
    });
  }
  $('enabled').addEventListener('change', async () => {
    notice = undefined;
    await chrome.storage.local.set({enabled: $('enabled').checked});
    refresh();
  });
})();
