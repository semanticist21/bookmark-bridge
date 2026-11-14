# Bridgey privacy policy

Bridgey does not collect bookmark data, use
analytics, or send bookmarks to the developer. It does not require an account
or a hosted service.

Bridgey uses Chrome's `bookmarks` permission to read and edit bookmarks at the
request of your MCP client. Chrome uses the `nativeMessaging` permission to
start the Bridgey host and connect to it on your computer. The host passes
tool results to the local MCP client over an authenticated loopback connection.
How your MCP client uses those results is governed by its own settings and
privacy policy.

The extension uses `storage` for its connection settings, a random local
profile ID, and a capped record of recent bookmark changes.
The server saves Git snapshots on your computer so you can undo changes.
Snapshots are stored under
`bookmark-bridge/native-v2/<profileId>/history` in your local state directory.
On macOS and Linux that directory is under `~/.local/state`; on Windows it is
under `%LOCALAPPDATA%`. Uninstalling the extension removes its browser storage.
You can delete the snapshot directories separately to remove local history.

Bridgey does not sell or share bookmark data. For questions, contact
semanticist0@gmail.com.

[Source code](https://github.com/semanticist21/bookmark-bridge)
