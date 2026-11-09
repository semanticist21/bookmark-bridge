# Bookmark Bridge

An MCP client can read and reorganize your Chrome bookmarks through this.
Every change lands in git first, so anything can be rolled back.

```
MCP client --stdio--> bookmark-bridge --ws(127.0.0.1)-- Chrome extension --chrome.bookmarks--> Chrome
```

The extension dials out rather than listening, because an extension cannot open
a port. A second MCP process that finds the port taken joins the first as a peer
and relays through it, so several sessions share one browser.

## Install

The default package uses the existing WebSocket connection. For the opt-in
Native Messaging release, see [Native Messaging](#native-messaging).

**1. Register the server with your MCP client.** It downloads itself on first
run, so you do not need Rust. macOS, Linux and Windows.

<details open>
<summary><b>Claude Code</b></summary>

```sh
claude mcp add bookmark-bridge -- npx -y bookmark-bridge
```

</details>

<details>
<summary><b>Codex</b> — <code>~/.codex/config.toml</code></summary>

```toml
[mcp_servers.bookmark-bridge]
command = "npx"
args = ["-y", "bookmark-bridge"]
```

</details>

<details>
<summary><b>Any other client</b> — the standard entry</summary>

```json
{
  "mcpServers": {
    "bookmark-bridge": {
      "command": "npx",
      "args": ["-y", "bookmark-bridge"]
    }
  }
}
```

Put it wherever your client keeps its MCP servers.

</details>

<details>
<summary><b>Or have your agent do it</b> — paste this</summary>

```
Install the bookmark-bridge MCP server for me.

1. Register it with this client as an MCP server named "bookmark-bridge",
   running: npx -y bookmark-bridge
   Use whatever config file this client uses, and do not disturb the servers
   already there.
2. Restart or reload so the server is picked up, then call its bridge_status
   tool and tell me what it reports.
3. If it says the extension is not connected, tell me to install the Chrome
   extension from https://github.com/semanticist21/bookmark-bridge and then
   check again.

Do not build from source; the npx package fetches a prebuilt binary.
```

</details>

Your client starts and stops it for you. Nothing to leave running. `git` must be
on PATH — the snapshots are a real git repository.

**2. Install the extension.** Get [Bridgey from the Chrome Web Store](https://chromewebstore.google.com/detail/ekokljgbbianihlpahackkfdendobngd)
and leave its mode set to **Legacy (WebSocket)**. For the unpacked legacy
extension, use `extension/` from the `v0.1.2` tag.

That is all of it. The extension's popup goes green once the two find each other.

The extension sleeps when you are not using it. Chrome ends an idle service
worker, and this one is not woken on a timer — tidying bookmarks is occasional
work, and polling a socket nobody is using is what Manifest V3 set out to stop.
It wakes on its own when the browser starts, when a setting changes, when you
touch a bookmark, and when you open its popup. If an agent reports that the
extension is not connected, clicking the toolbar icon reconnects it.

<details>
<summary>Build the legacy server from source</summary>

Use the `v0.1.2` tag for the legacy server.

```sh
cd mcp && cargo build --release
```

Then point the same config entry at the binary instead:
`"command": "<repo>/mcp/target/release/bookmark-bridge"`, with no `args`.

A source checkout also gets a shared token automatically: the server writes one
to `~/.config/bookmark-bridge/token` and copies it into `extension/`, so both
ends have it. A Web Store install is read-only and cannot receive one, so the
server runs without a token there and relies on the loopback binding — a token
only one side knows would reject the extension forever. Set
`BOOKMARK_BRIDGE_TOKEN` to demand one regardless, and put the same value in the
extension's storage. `BOOKMARK_BRIDGE_PORT` changes the port.

</details>

## Native Messaging

The v2 server is available on the npm `native` channel. The standard
`bookmark-bridge` command still installs the legacy server.

1. Open `chrome://extensions` and check the Bridgey extension version. Native
   Messaging requires **2.0.0 or later**. If you have an older version, keep
   using the legacy MCP command or [load the unpacked extension](#unpacked-development-extension)
   before changing your MCP configuration.
2. In your MCP client, change the command arguments from `-y bookmark-bridge`
   to `-y bookmark-bridge@native`, then restart its MCP connection. With Claude
   Code, the command for a new entry is
   `claude mcp add bookmark-bridge -- npx -y bookmark-bridge@native`.
3. Open Bridgey's popup and choose **Native Messaging**. Approve Chrome's
   permission prompt. If setup fails, the extension stays in Legacy mode.
   Restore the legacy MCP command to reconnect.
4. Run `bridge_status` from your MCP client to confirm the connection.

The Native npm package contains the prebuilt servers and their checksums.
The first server startup installs the native host in a user data directory
outside the npm cache and registers it with Chrome. Chrome launches that host
when the extension connects. The host and the MCP process communicate over an
authenticated loopback connection. Keep `git` on the MCP client's PATH. The
server remembers its absolute path so the Chrome-launched host can use it.

Bridgey serves one Native Messaging browser profile at a time. Each profile's
v2 snapshots live under `native-v2/<profileId>` inside the existing history
state directory. Legacy snapshots remain available in Legacy mode.

To switch back, choose **Legacy (WebSocket)** if the popup shows a mode
selector, restore the MCP command arguments to `-y bookmark-bridge`, and
restart its MCP connection.
Restarting Chrome or reconnecting the popup refreshes the browser connection.
Restart the MCP connection after updating the server to refresh its tool list.

### Unpacked development extension

At `chrome://extensions`, enable Developer mode and load the `extension/`
directory from this repository at `v2.0.0` or later. Copy the unpacked
extension's ID and run `npx -y bookmark-bridge@native register-native <extension-id>`
before selecting Native Messaging. The host remembers this ID alongside the
published one.
On macOS and Linux, set `BOOKMARK_BRIDGE_CHROME_USER_DATA_DIR` to the absolute
user data directory when registering for a Chrome test profile outside the
default location.

## Safety model

Three mechanisms, in the order they engage.

**Snapshots.** Every mutating call commits the whole tree to a git repository at
`~/.local/state/bookmark-bridge/history`, before and after. A failed
before-snapshot refuses the change: a mutation nobody can undo is not worth
making. `force: true` overrides that.

**Compare-and-swap.** Any operation may carry `expect: {title?, url?, parentId?}`.
If the live value differs, that one operation is skipped and reported with what
was actually there. This is what survives a user edit landing between planning
and applying.

**Drift detection.** `history_drift` diffs the live tree against `agent-head`,
the commit marking the agent's last finished operation. Whatever differs is
someone else's work. The extension also logs bookmark events tagged `agent` or
`user`; that ledger is evidence, the diff is the authority.

## Known limits

Read these before trusting the output.

- **Reading list is invisible.** `chrome.readingList` is a separate API whose
  items never appear in the bookmark tree. Neither layer sees them.
- **"Remove all user bookmarks" fires no extension event.** Chromium has a
  standing TODO for it. The diff still catches it; the ledger will be empty.
- **The ledger misses whatever happens while Chrome or the extension is not
  running** — changes synced from another device while the browser was closed,
  an extension reload, an update. The diff catches all of these on the next call.
- **Sync-originated changes do fire events** and are attributed to the user,
  which is correct. Each sync batch also emits `onImportBegan`/`onImportEnded`,
  so those events cannot distinguish an HTML import from a routine sync.
- **`dateAdded` cannot be restored.** Anything `history_restore` recreates gets
  today's date; the Chrome API offers no way to set it. Export an HTML backup if
  the original dates matter.
- **Restore refuses when ids look reassigned.** Signing in or out, profile
  recovery and HTML import all renumber bookmarks; restoring across that boundary
  would build a duplicate tree beside the real one, so it stops instead.
- **Order within a folder is restored approximately.** Indices are applied
  sequentially, and each move shifts its siblings.

## Tests

`cd mcp && cargo test --locked` exercises the MCP server against a fake browser
extension with an in-memory tree. The test harness uses temporary state and
does not read your real Chrome bookmarks.

## Tools

| | |
| --- | --- |
| `bookmarks_tree` `_search` `_children` | read |
| `bookmarks_find_duplicates` | groups identical URLs and lists each bookmark's ID, title and parent folder ID; pass `folderId` to scan a folder and its descendants |
| `bookmarks_update` `_move` `_create` `_remove` `_removeTree` | single change, each accepts `expect` |
| `bookmarks_batch` | up to 200 changes at once, per-item `expect` |
| `history_log` `_restore` `_drift` | snapshots, rollback, what changed since the agent last ran |
| `bridge_status` | extension connection |

## Privacy

The extension holds `bookmarks` and `storage`. Native Messaging also requires
an optional permission and uses Chrome's extension allowlist. Legacy mode uses
`127.0.0.1` and rejects handshakes carrying a web origin. Both transports keep
bookmark data on this computer.

## License

MIT. See [LICENSE](LICENSE).
