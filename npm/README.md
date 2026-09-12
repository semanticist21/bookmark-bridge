# Bookmark Bridge

Lets an MCP client read and reorganize Chrome bookmarks, with every change
recorded in git so it can be rolled back.

```
MCP client --stdio--> bookmark-bridge --ws(127.0.0.1)-- Chrome extension --chrome.bookmarks--> Chrome
```

The extension is the WebSocket client; it dials out because an extension cannot
listen on a port. A second MCP process that cannot bind the port joins the first
as a peer and relays through it, so several sessions share one browser.

## Install

Two pieces: this server, and a Chrome extension that dials into it.

**1. Register the server with your MCP client.** It is fetched on first run; no
Rust needed. macOS and Linux only — the server uses `flock` and POSIX file modes.

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

It is macOS and Linux only. Do not build from source; the npx package fetches
a prebuilt binary.
```

</details>

Your client starts and stops the server for you; there is no daemon to run.

**2. Install the extension.** From the Chrome Web Store, or load `extension/`
at `chrome://extensions` with developer mode on.

That is the whole setup. The extension shows a green light in its popup once the
two find each other.

<details>
<summary>From source instead</summary>

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

## Safety model

Three independent mechanisms, in the order they engage:

**Snapshots.** Every mutating call commits the whole tree to a git repository at
`~/.local/state/bookmark-bridge/history` before and after the change. If the
before-snapshot fails, the change is refused — a mutation that cannot be undone
is not worth making. Pass `force: true` to override.

**Compare-and-swap.** Any operation may carry `expect: {title?, url?, parentId?}`.
If the current value differs, that operation alone is skipped and reported with
the actual value. This is what protects a plan from a user edit that landed
between planning and applying.

**Drift detection.** `history_drift` compares the live tree against `agent-head`,
the commit marking the agent's last completed operation. The difference is what
someone else changed. The extension additionally records bookmark events tagged
`agent` or `user`; that ledger is evidence, and the diff is the authority.

## Known limits

These are real. Read them before trusting the output.

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

`cd mcp && cargo test` spawns the real binary, plays the extension with a
WebSocket client holding an in-memory tree, and drives it over MCP. It covers the
protocol handshake, compare-and-swap skipping, idempotent create, the batch size
cap, refusing to mutate without a snapshot, restoring a deleted subtree under new
ids, drift reporting only the user's changes, and two sessions sharing one
extension through the peer relay.

## Tools

| | |
| --- | --- |
| `bookmarks_tree` `_search` `_children` | read |
| `bookmarks_update` `_move` `_create` `_remove` `_removeTree` | single change, each accepts `expect` |
| `bookmarks_batch` | up to 200 changes at once, per-item `expect` |
| `history_log` `_restore` `_drift` | snapshots, rollback, what changed since the agent last ran |
| `bridge_status` | extension connection |

## Privacy

The extension holds `bookmarks`, `storage` and `alarms`. It connects only to
`127.0.0.1` and rejects WebSocket handshakes carrying an `http(s)` origin, so a
web page cannot reach it. Nothing is sent anywhere else. The token stops a page
from connecting; it does not stop another process on the same machine, which can
read the token file just as easily.

## License

MIT. See [LICENSE](LICENSE).
