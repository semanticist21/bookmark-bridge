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

1. Build the server: `cd mcp && cargo build --release`
2. Register it: `claude mcp add bookmark-bridge -- <repo>/mcp/target/release/bookmark-bridge`
3. Load `extension/` at `chrome://extensions` with developer mode on.

There is nothing to configure. From a checkout, the server writes a token to
`~/.config/bookmark-bridge/token` and copies it into `extension/`, so both ends
have it. A Web Store install is read-only and cannot receive one, so the server
runs without a token there and relies on the loopback binding — a token only one
side knows would reject the extension forever. Set `BOOKMARK_BRIDGE_TOKEN` to
demand one regardless, and paste the same value into the extension's storage.
`BOOKMARK_BRIDGE_PORT` changes the port.

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
