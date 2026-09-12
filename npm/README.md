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

Two pieces: this server, and the Chrome extension that dials into it.

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

**2. Install the extension.** From the Chrome Web Store, or load `extension/`
at `chrome://extensions` with developer mode on.

That is all of it. The extension's popup goes green once the two find each other.

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

`cd mcp && cargo test` runs the real binary against a fake extension: a
WebSocket client holding an in-memory tree. The destructive paths are covered
without a browser, including restoring a deleted subtree under new ids, refusing
to mutate when no snapshot can be taken, and two sessions sharing one extension
through the peer relay.

Two release-blocking defects came out of writing these, both invisible on a
machine that had already been set up once.

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
`127.0.0.1` and turns away handshakes carrying an `http(s)` origin, so a web page
cannot reach it. Nothing goes anywhere else. The token stops a page; it does not
stop another process on this machine, which can read the token file just as
easily.

## License

MIT. See [LICENSE](LICENSE).
