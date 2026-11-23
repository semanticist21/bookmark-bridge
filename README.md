# Bookmark Bridge

An MCP client can read and reorganize your Chrome bookmarks through this.
Every change lands in git first, so anything can be rolled back.

Chrome starts the Bridgey host through Native Messaging. An MCP client runs
the server over stdio, and the server talks to the host over an authenticated
loopback connection.

## Install

1. Install [Bridgey from the Chrome Web Store](https://chromewebstore.google.com/detail/ekokljgbbianihlpahackkfdendobngd).
2. Add the server to your MCP client. You need Node.js 18 or later and Git on
   the client's PATH. The npm package includes verified binaries for macOS,
   Linux and Windows.

   **Claude Code**
   ```sh
   claude mcp add bookmark-bridge -- npx -y bookmark-bridge
   ```

   **Codex** (~/.codex/config.toml)
   ```toml
   [mcp_servers.bookmark-bridge]
   command = "npx"
   args = ["-y", "bookmark-bridge"]
   ```

   **Other MCP clients**
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
3. Restart your MCP client so the server can register the host with Chrome.
   Open the Bridgey popup and click **Reconnect**. When it shows
   **Bridge Connected**, call `bridge_status` from your MCP client.

The first server startup installs the native host outside the npm cache and
registers it with Chrome. After upgrading from 3.0, update and restart every
MCP client that runs Bridgey. Click **Reconnect** in the previously connected
Chrome profile before reconnecting the others.

### Multiple Chrome profiles

Open Bridgey → **How to set up** in each Chrome profile and copy its profile
ID under **Multiple Chrome profiles**. Give each profile its own MCP server
entry and pass its ID to `--profile`. For example, in Codex:

```toml
[mcp_servers.bookmark-bridge-work]
command = "npx"
args = ["-y", "bookmark-bridge", "--profile", "<work profile ID>"]

[mcp_servers.bookmark-bridge-personal]
command = "npx"
args = ["-y", "bookmark-bridge", "--profile", "<personal profile ID>"]
```

Add only the intended entry to any MCP client configuration that should
access a single profile. `bridge_status` reports the selected ID. A session
stays bound to that profile through disconnects and reconnects.

The original command works when Bridgey knows one profile. Once it has seen
more than one, new sessions need `--profile`, even when only one profile is
online. If reinstalling the extension creates a new ID, copy it again and
update the corresponding server entry.

### Unpacked development extension

In `chrome://extensions`, enable Developer mode and load the `extension/`
directory. Copy the extension ID and run
`npx -y bookmark-bridge register-native <extension-id>` before reconnecting.
For a non-default Chrome user data directory on macOS and Linux, set
`BOOKMARK_BRIDGE_CHROME_USER_DATA_DIR` to its absolute path when registering.

To run from source, build with `cargo build --release --locked --manifest-path mcp/Cargo.toml`
and point the MCP client at the resulting binary. The binary accepts the same
`register-native` command.

## Safety model

Three mechanisms, in the order they engage.

**Snapshots.** Every mutating call commits the whole tree to a git repository under
`bookmark-bridge/native-v2/<profileId>/history` in the user state directory,
before and after. A failed
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

The extension uses Chrome's `bookmarks`, `storage` and `nativeMessaging`
permissions. Chrome restricts access to the host by extension ID. Bridgey
keeps bookmark data on this computer.

## License

MIT. See [LICENSE](LICENSE).
