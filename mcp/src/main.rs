//! Exposes Chrome bookmarks as MCP tools.
//!
//! An extension cannot listen on a port, so the direction is inverted: this
//! process runs a WebSocket server on loopback and the extension dials in.
//! MCP clients attach over stdio.
//!
//!   MCP client --stdio--> this process <--ws(127.0.0.1)-- Chrome extension
//!
//! Every bookmark change goes through the extension's chrome.bookmarks API.
//! Editing Chrome's Bookmarks file directly gets reverted by Chrome Sync.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;
type Outbox = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>>;

const PROTOCOL: &str = "2024-11-05";

/// Why a call to the extension failed. The distinction that matters is whether
/// the request provably never reached Chrome: only then do we know no bookmark
/// was touched, and only then is it safe to clear the dirty marker.
#[derive(Debug)]
enum CallError {
    /// No extension is attached. The frame was never queued.
    NotConnected,
    /// The connection dropped before the frame could be queued.
    SendFailed,
    /// The reply channel closed; the extension reconnected mid-flight.
    Cancelled,
    /// No reply within the budget. The change may or may not have landed.
    Timeout(u64),
    /// The extension itself reported an error for this request.
    Extension(String),
}

impl CallError {
    /// True when the request demonstrably never reached Chrome.
    fn never_sent(&self) -> bool {
        matches!(self, CallError::NotConnected | CallError::SendFailed)
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::NotConnected => f.write_str(
                "The Chrome extension is not connected. Check that it is loaded, \
                 that the port matches, and that the token matches."),
            CallError::SendFailed => f.write_str(
                "The extension disconnected before the request was sent. Nothing was changed."),
            CallError::Cancelled => f.write_str(
                "The extension reconnected while this request was in flight. \
                 Whether it was applied is unknown - run history_drift before retrying."),
            CallError::Timeout(secs) => write!(f,
                "The extension did not reply within {secs}s. Whether the change was \
                 applied is unknown - run history_drift before retrying."),
            CallError::Extension(msg) => f.write_str(msg),
        }
    }
}

impl From<CallError> for String {
    fn from(e: CallError) -> String { e.to_string() }
}

struct Bridge {
    pending: Pending,
    outbox: Outbox,
    /// Generation of the connection that owns `outbox`, so a closing old
    /// connection cannot tear down a newer one.
    epoch: std::sync::atomic::AtomicU64,
    next_id: Mutex<u64>,
    token: String,
}

impl Bridge {
    /// Sends a command to the extension and waits for its reply. Fails fast
    /// when no extension is attached.
    async fn call(&self, method: &str, params: Value) -> Result<Value, CallError> {
        let n_ops = params.get("ops").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
        let tx = {
            let guard = self.outbox.lock().await;
            guard.clone().ok_or(CallError::NotConnected)?
        };
        let id = {
            let mut n = self.next_id.lock().await;
            *n += 1;
            *n
        };
        let (done_tx, done_rx) = oneshot::channel();
        self.pending.lock().await.insert(id, done_tx);
        let frame = json!({"id": id, "method": method, "params": params}).to_string();
        tx.send(frame).map_err(|_| CallError::SendFailed)?;

        // A batch takes as long as it has items. A flat 20s budget cuts large
        // jobs short while the extension keeps applying them, so a retry would
        // duplicate work.
        let budget = std::time::Duration::from_secs(20 + n_ops as u64 / 5);
        match tokio::time::timeout(budget, done_rx).await {
            Ok(Ok(v)) => {
                if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
                    Err(CallError::Extension(err.to_string()))
                } else {
                    Ok(v.get("result").cloned().unwrap_or(Value::Null))
                }
            }
            Ok(Err(_)) => Err(CallError::Cancelled),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(CallError::Timeout(budget.as_secs()))
            }
        }
    }
}

/// Establishes the shared token, but only when both sides can actually hold it.
///
/// The extension can read a file only from inside its own directory, so the
/// token can be planted only in an unpacked checkout. A Web Store install is
/// read-only: planting is impossible there, and a server that kept a token the
/// extension could never learn would reject it forever. So the rule is simply
/// "a token exists only if both ends can have one" - otherwise the bridge runs
/// without one, which the loopback binding already makes reasonable.
///
/// Set BOOKMARK_BRIDGE_TOKEN to demand a token regardless, and put the same
/// value in the extension's storage through its popup.
fn ensure_token() -> Option<String> {
    let ext = extension_dir()?;
    let home = std::env::var("HOME").ok()?;
    let dir = std::path::Path::new(&home).join(".config/bookmark-bridge");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("token");
    let token = match std::fs::read_to_string(&path) {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            // Read exactly what is needed. /dev/urandom is an endless character
            // device: fs::read would never return, growing its buffer until the
            // machine gave out. Every first run hit that.
            use std::io::Read;
            let mut raw = [0u8; 24];
            std::fs::File::open("/dev/urandom").ok()?.read_exact(&mut raw).ok()?;
            let t: String = raw.iter().map(|b| format!("{b:02x}")).collect();
            std::fs::write(&path, &t).ok()?;
            let _ = std::fs::set_permissions(&path,
                std::os::unix::fs::PermissionsExt::from_mode(0o600));
            t
        }
    };
    // Planting must succeed. If it cannot, no token is better than one only this
    // side knows.
    std::fs::write(ext.join("token.txt"), &token).ok()?;
    Some(token)
}

/// The unpacked extension directory, when there is one. Overridable with
/// BOOKMARK_BRIDGE_EXT_DIR; otherwise inferred from the binary's location in a
/// checkout (`<repo>/mcp/target/release/bookmark-bridge`).
fn extension_dir() -> Option<std::path::PathBuf> {
    let dir = match std::env::var("BOOKMARK_BRIDGE_EXT_DIR") {
        Ok(v) if !v.trim().is_empty() => std::path::PathBuf::from(v),
        _ => std::env::current_exe().ok()?
            .ancestors().nth(4)?.join("extension"),
    };
    dir.is_dir().then_some(dir)
}

fn tools() -> Value {
    // The schema mirrors chrome.bookmarks. No invented concepts.
    json!([
      {"name":"bookmarks_tree","description":"Returns the whole bookmark tree: id, title, url, children.",
       "inputSchema":{"type":"object","properties":{},"additionalProperties":false}},
      {"name":"bookmarks_search","description":"Searches titles and URLs.",
       "inputSchema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}},
      {"name":"bookmarks_children","description":"Returns a folder's direct children.",
       "inputSchema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}},
      {"name":"bookmarks_update","description":"Changes a bookmark or folder title or URL. With expect, the item is left alone and reported as a mismatch when its current value differs.",
       "inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"url":{"type":"string"},"expect":{"type":"object"},"force":{"type":"boolean"}},"required":["id"],"additionalProperties":false}},
      {"name":"bookmarks_move","description":"Moves an item to another folder or position. With expect, the item is left alone when its current value differs.",
       "inputSchema":{"type":"object","properties":{"id":{"type":"string"},"parentId":{"type":"string"},"index":{"type":"integer"},"expect":{"type":"object"},"force":{"type":"boolean"}},"required":["id"],"additionalProperties":false}},
      {"name":"bookmarks_create","description":"Creates a bookmark, or a folder when url is omitted.",
       "inputSchema":{"type":"object","properties":{"parentId":{"type":"string"},"title":{"type":"string"},"url":{"type":"string"},"index":{"type":"integer"}},"required":["parentId","title"],"additionalProperties":false}},
      {"name":"bookmarks_remove","description":"Removes one item. A folder must be empty. Always include parentId in expect: it stops you deleting an item the user has since moved elsewhere.",
       "inputSchema":{"type":"object","properties":{"id":{"type":"string"},"expect":{"type":"object"},"force":{"type":"boolean"}},"required":["id"],"additionalProperties":false}},
      {"name":"bookmarks_batch","description":"Applies many changes in one round trip. ops is an array of {op:\"update\"|\"move\"|\"create\"|\"remove\", ...}; each entry takes the same fields as the matching single tool. Give an entry expect{title?,url?,parentId?} and it is skipped and reported as a mismatch when the live value differs. Use expect so you never overwrite an item the user edited between planning and applying.",
       "inputSchema":{"type":"object","properties":{"ops":{"type":"array","items":{"type":"object"}}},"required":["ops"],"additionalProperties":false}},
      {"name":"history_log","description":"Returns bookmark snapshots, newest first.",
       "inputSchema":{"type":"object","properties":{"limit":{"type":"integer"}},"additionalProperties":false}},
      {"name":"history_restore","description":"Restores titles, URLs and positions to a snapshot. Items present then but missing now are recreated. Items present now but absent then are reported, never deleted. Note: a recreated item gets today's dateAdded - the Chrome API cannot set it. If the original dates matter, tell the user to keep an HTML export as well.",
       "inputSchema":{"type":"object","properties":{"commit":{"type":"string"}},"required":["commit"],"additionalProperties":false}},
      {"name":"bookmarks_removeTree","description":"Removes a folder and everything inside it. Use it when merging folders. history_restore undoes it.",
       "inputSchema":{"type":"object","properties":{"id":{"type":"string"},"expect":{"type":"object"},"force":{"type":"boolean"}},"required":["id"],"additionalProperties":false}},
      {"name":"history_drift","description":"Reports how the tree differs from agent-head, the commit marking your last completed operation. That difference is what someone else changed. Call this before planning anything. changes is the authority; ledger is supporting evidence. When unreliable is true the previous operation's after-snapshot failed - do not undo anything based on this result.",
       "inputSchema":{"type":"object","properties":{"clear":{"type":"boolean"}},"additionalProperties":false}},
      {"name":"bridge_status","description":"Reports the extension connection and how many requests are in flight.",
       "inputSchema":{"type":"object","properties":{},"additionalProperties":false}}
    ])
}

async fn handle_tool(b: &Bridge, name: &str, args: &Value) -> Result<Value, String> {
    if name == "bridge_status" {
        let connected = b.outbox.lock().await.is_some();
        let pending = b.pending.lock().await.len();
        return Ok(json!({"extensionConnected": connected, "pendingRequests": pending}));
    }
    if let Some(cmd) = name.strip_prefix("history_") {
        // restore mutates; it is not a read. It needs the same protection as the
        // other mutating tools so agent-head ends up pointing at the restored
        // state. Without that, the next drift reports the whole restore as a user
        // change and the agent tries to undo it.
        if cmd == "restore" {
            let marker = dirty_marker();
            let _ = std::fs::write(&marker, format!("history_restore {}", compact(args)));
            let out = history(b, cmd, args).await;
            if out.is_ok() {
                if snapshot_and_mark(b, "after: history_restore").await.is_ok() {
                    let _ = std::fs::remove_file(&marker);
                }
            }
            return out;
        }
        return history(b, cmd, args).await;
    }
    let method = name.strip_prefix("bookmarks_").ok_or("Unknown tool.")?;
    // Chrome terminates a service worker whose event handler runs past five
    // minutes, which would leave a batch half applied with its reply lost. Cap
    // the size so that state cannot arise.
    if let Some(n) = args.get("ops").and_then(|v| v.as_array()).map(|a| a.len()) {
        if n > 200 {
            return Err(format!("{n} operations at once is too many. Split into \
                                batches of 200 or fewer: if the service worker is \
                                terminated midway, part of the batch is applied and \
                                the reply is lost."));
        }
    }
    // Every mutating tool records the prior state first, so a mistake can be undone.
    if matches!(method, "update" | "move" | "create" | "remove" | "removeTree" | "batch") {
        let label = format!("before: {name} {}", compact(args));
        // Without a snapshot the change cannot be undone. Refuse rather than make
        // one. Passing force:true is the explicit way to override.
        if let Err(e) = snapshot(b, &label).await {
            if args.get("force").and_then(|v| v.as_bool()) != Some(true) {
                return Err(format!(
                    "Aborted: the snapshot failed, and a change that cannot be \
                     undone is not worth making.\nCause: {e}\nPass force: true to \
                     proceed anyway."));
            }
            eprintln!("[bridge] snapshot failed, proceeding because force was set: {e}");
        }
        // Write a dirty marker immediately before mutating. If the after-snapshot
        // fails the marker survives and drift reports itself unreliable. Without it
        // the next drift mistakes the agent's own work for a user change and tries
        // to undo it.
        let marker = dirty_marker();
        let _ = std::fs::write(&marker, &label);
        let out = b.call(method, args.clone()).await;
        if out.is_ok() {
            // Mark the resulting state as agent-head. With several sessions, git log
            // order cannot identify "the last after", so a dedicated ref is needed.
            match snapshot_and_mark(b, &format!("after: {name}")).await {
                Ok(()) => { let _ = std::fs::remove_file(&marker); }
                Err(e) => eprintln!("[bridge] after-snapshot failed, keeping dirty marker: {e}"),
            }
        } else if out.as_ref().err().map(CallError::never_sent).unwrap_or(false) {
            // It never left this process, so nothing was applied. Clear the marker.
            let _ = std::fs::remove_file(&marker);
        }
        // Any other failure (a timeout, say) leaves it unknown whether the change
        // landed. Keep the marker so the next drift reports itself unreliable.
        return out.map_err(Into::into);
    }
    b.call(method, args.clone()).await.map_err(Into::into)
}

fn compact(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 120 { s.chars().take(117).collect::<String>() + "..." } else { s }
}

/// Runtime state that is not part of a snapshot. It must live outside the git
/// worktree: `git add -A` would otherwise commit the lock file and the dirty
/// marker into every snapshot, and their appearing and disappearing would make
/// "nothing changed, so do not commit" never hold.
fn state_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::Path::new(&home).join(".local/state/bookmark-bridge")
}

/// The git worktree. It holds exactly one file: the bookmark tree.
fn history_dir() -> std::path::PathBuf {
    state_dir().join("history")
}

/// Set while a mutation is in flight. Surviving the operation means the
/// after-snapshot failed and drift cannot be trusted.
fn dirty_marker() -> std::path::PathBuf {
    state_dir().join("agent-head-dirty")
}

/// When agent-head was last moved, in milliseconds. This is the watermark handed to
/// the extension so it reports only what happened after the agent's last completed
/// operation. Zero (the file missing) means "report everything you hold".
fn agent_head_at() -> u64 {
    std::fs::read_to_string(state_dir().join("agent-head-at"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_agent_head_at() {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let _ = std::fs::create_dir_all(state_dir());
    let _ = std::fs::write(state_dir().join("agent-head-at"), ms.to_string());
}

fn git(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C").arg(history_dir())
        .args(args)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

/// Cross-process exclusive lock on the history directory. Several sessions write
/// the same git repository, and an index.lock collision loses a snapshot silently.
struct HistoryLock(std::fs::File);

impl HistoryLock {
    fn acquire() -> Result<Self, String> {
        let dir = state_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let f = std::fs::OpenOptions::new().create(true).write(true)
            .open(dir.join("bridge.lock")).map_err(|e| e.to_string())?;
        // The kernel releases flock when the process dies, so a leftover lock file
        // never blocks the next run.
        let rc = unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&f), libc::LOCK_EX) };
        if rc != 0 { return Err("could not acquire the history lock".into()); }
        Ok(HistoryLock(f))
    }
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&self.0), libc::LOCK_UN) };
    }
}

/// Commits and moves agent-head under one lock. Split them and another session's
/// commit lands in between, leaving the baseline pointing at someone else's state.
async fn snapshot_and_mark(b: &Bridge, label: &str) -> Result<(), String> {
    let tree = b.call("tree", json!({})).await?;
    commit_blocking(tree, label.to_string(), true).await
}

/// Commits the current tree. Commits nothing when nothing changed.
async fn snapshot(b: &Bridge, label: &str) -> Result<(), String> {
    let tree = b.call("tree", json!({})).await?;
    commit_blocking(tree, label.to_string(), false).await
}

/// flock and git block. Holding a runtime thread would stall other sessions' stdio
/// loops too. The tree is already fetched, so nothing here awaits.
async fn commit_blocking(tree: Value, label: String, mark: bool) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let _lock = HistoryLock::acquire()?;
        write_and_commit(&tree, &label)?;
        if mark {
            git(&["update-ref", "refs/heads/agent-head", "HEAD"])?;
            // The ledger watermark moves with the ref, under the same lock.
            write_agent_head_at();
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("the snapshot task was aborted: {e}"))?
}

fn write_and_commit(tree: &Value, label: &str) -> Result<(), String> {
    let dir = history_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    if !dir.join(".git").exists() {
        git(&["init", "-q"])?;
        let _ = git(&["config", "user.name", "bookmark-bridge"]);
        let _ = git(&["config", "user.email", "bookmark-bridge@localhost"]);
    }
    // Earlier versions kept the lock and the marker inside the worktree and
    // committed them. Stop tracking them so old repositories converge on
    // snapshots that contain only the tree.
    for stale in [".bridge.lock", ".agent-head-dirty", "bookmarks.json"] {
        if dir.join(stale).exists() || git(&["ls-files", "--error-unmatch", stale]).is_ok() {
            let _ = git(&["rm", "-q", "--cached", "--ignore-unmatch", stale]);
            let _ = std::fs::remove_file(dir.join(stale));
        }
    }
    let pretty = serde_json::to_string_pretty(&tree).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("tree.json"), pretty).map_err(|e| e.to_string())?;
    git(&["add", "-A"])?;
    // git fails when there is nothing to commit. That is not an error.
    let _ = git(&["commit", "-q", "-m", label]);
    Ok(())
}

async fn history(b: &Bridge, cmd: &str, args: &Value) -> Result<Value, String> {
    match cmd {
        "log" => {
            let n = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20);
            let out = git(&["log", &format!("-{n}"), "--pretty=%h\t%ad\t%s", "--date=format:%m-%d %H:%M"])?;
            let items: Vec<Value> = out.lines().filter_map(|l| {
                let mut it = l.splitn(3, '\t');
                Some(json!({"commit": it.next()?, "at": it.next()?, "label": it.next()?}))
            }).collect();
            Ok(json!({"snapshots": items}))
        }
        "restore" => {
            let c = args.get("commit").and_then(|v| v.as_str()).ok_or("commit is required.")?;
            let raw = git(&["show", &format!("{c}:tree.json")])?;
            let old: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
            snapshot(b, &format!("restore to {c}")).await?;
            let now = b.call("tree", json!({})).await?;
            let (mut want, mut have) = (Vec::new(), Vec::new());
            flatten(&old, None, &mut want);
            flatten(&now, None, &mut have);

            let have_by_id: std::collections::HashMap<&str, &Row> =
                have.iter().map(|r| (r.id.as_str(), r)).collect();

            // When ids are renumbered wholesale (signing in or out, profile
            // recovery, HTML import) every node looks "missing" and a restore would
            // duplicate the entire tree. Barely any overlap means this is not a
            // situation to restore across.
            let overlap = want.iter().filter(|r| have_by_id.contains_key(r.id.as_str())).count();
            if want.len() >= 20 && overlap * 4 < want.len() {
                return Err(format!(
                    "The ids appear to have been reassigned: only {1} of {0} \
                     snapshot items match the current tree.\n\
                     Restoring now would build a full duplicate beside the real tree.\n\
                     This follows signing in or out of Chrome, profile recovery, or a \
                     bookmark import.\n\
                     Import from an HTML backup instead, or inspect the tree and decide \
                     yourself.",
                    want.len(), overlap));
            }
            // A recreated folder gets a new id. Children carrying the old parent id
            // would all fail, so build an old-id -> new-id map as we go and rewrite
            // each parent.
            let mut remap: std::collections::HashMap<String, String> = Default::default();
            let mut ops = Vec::new();
            let mut created = 0usize;

            for r in &want {
                // Chrome refuses to edit or move the permanent folders (bookmarks
                // bar, other, mobile).
                if r.parent == "0" { continue; }
                let parent = remap.get(&r.parent).cloned().unwrap_or_else(|| r.parent.clone());
                match have_by_id.get(r.id.as_str()) {
                    Some(cur) => {
                        // Never send url for a folder; update rejects an empty string.
                        if cur.title != r.title || cur.url != r.url {
                            let mut o = json!({"op":"update","id":r.id,"title":r.title});
                            if let Some(u) = &r.url { o["url"] = json!(u); }
                            ops.push(o);
                        }
                        if cur.parent != parent || cur.index != r.index {
                            ops.push(json!({"op":"move","id":r.id,"parentId":parent,"index":r.index}));
                        }
                    }
                    None => {
                        // Children need this parent's id, so create it now and keep it.
                        let mut o = json!({"op":"create","parentId":parent,"title":r.title,"index":r.index});
                        if let Some(u) = &r.url { o["url"] = json!(u); }
                        let made = b.call("create", o).await?;
                        if let Some(new_id) = made.get("id").and_then(|v| v.as_str()) {
                            remap.insert(r.id.clone(), new_id.to_string());
                            created += 1;
                        }
                    }
                }
            }

            let want_ids: std::collections::HashSet<&str> =
                want.iter().map(|r| r.id.as_str()).collect();
            let extra: Vec<&str> = have.iter().map(|r| r.id.as_str())
                .filter(|i| !want_ids.contains(i)).collect();

            // A worker held past five minutes in one event is terminated, so the
            // restore is chunked too.
            let mut applied_total = 0u64;
            for chunk in ops.chunks(200) {
                let r = b.call("batch", json!({"ops": chunk})).await?;
                applied_total += r.get("applied").and_then(|v| v.as_u64()).unwrap_or(0);
            }
            let applied = json!({"applied": applied_total});
            Ok(json!({"recreated": created, "changed": applied,
                      "notDeleted": extra,
                      "note": "Items absent from the snapshot were not deleted. Review notDeleted and decide yourself."}))
        }
        "drift" => {
            let marker = dirty_marker();
            let mut unreliable = marker.exists();
            let pending = std::fs::read_to_string(&marker).unwrap_or_default();
            let base = git(&["show", "agent-head:tree.json"]).ok();
            // Pass the agent-head watermark instead of clearing the ledger. Clearing
            // on read made drift destructive - look without acting and the evidence
            // was gone - and reset the extension's seq so a watermark could never
            // advance. With `since` the read is idempotent.
            let mut dargs = args.clone();
            if let Some(o) = dargs.as_object_mut() {
                o.insert("since".into(), json!(agent_head_at()));
                // Only an explicit history_drift {clear:true} purges it.
                o.entry("clear").or_insert(json!(false));
            }
            let ledger = b.call("drift", dargs).await.unwrap_or(Value::Null);
            let Some(base) = base else {
                return Ok(json!({"baseline": "none",
                    "note": "No agent baseline exists yet. Your first operation creates one.",
                    "ledger": ledger}));
            };
            let old: Value = serde_json::from_str(&base).map_err(|e| e.to_string())?;
            let now = b.call("tree", json!({})).await?;
            let (mut a, mut c) = (Vec::new(), Vec::new());
            flatten(&old, None, &mut a);
            flatten(&now, None, &mut c);
            let am: std::collections::HashMap<&str, &Row> =
                a.iter().map(|r| (r.id.as_str(), r)).collect();
            let cm: std::collections::HashMap<&str, &Row> =
                c.iter().map(|r| (r.id.as_str(), r)).collect();
            let mut changes = Vec::new();
            for r in &c {
                match am.get(r.id.as_str()) {
                    None => changes.push(json!({"kind":"added","id":r.id,"title":r.title,
                        "url":r.url,"parentId":r.parent})),
                    Some(o) => {
                        if o.title != r.title {
                            changes.push(json!({"kind":"retitled","id":r.id,
                                "from":o.title,"to":r.title}));
                        }
                        // A URL change is separate from a title change. Miss it and the
                        // diff answers "no change" after the user retargets a bookmark.
                        if o.url != r.url {
                            changes.push(json!({"kind":"retargeted","id":r.id,"title":r.title,
                                "from":o.url,"to":r.url}));
                        }
                        if o.parent != r.parent {
                            changes.push(json!({"kind":"moved","id":r.id,"title":r.title,
                                "from":o.parent,"to":r.parent,
                                "storageChanged": o.syncing != r.syncing}));
                        } else if o.syncing != r.syncing {
                            changes.push(json!({"kind":"storage-changed","id":r.id,
                                "title":r.title,"from":o.syncing,"to":r.syncing,
                                "note":"moved between account and local storage"}));
                        } else if o.index != r.index {
                            // Reordering inside one folder. A user's sort-by-name lands here.
                            changes.push(json!({"kind":"reordered","id":r.id,"title":r.title,
                                "from":o.index,"to":r.index}));
                        }
                    }
                }
            }
            for r in &a {
                if !cm.contains_key(r.id.as_str()) {
                    changes.push(json!({"kind":"removed","id":r.id,"title":r.title,
                        "url":r.url,"parentId":r.parent}));
                }
            }
            // A marker with a tree identical to the baseline means the operation never
            // applied. That is now established, so clear it - otherwise one timeout
            // disables drift detection forever.
            if unreliable && changes.is_empty() {
                let _ = std::fs::remove_file(&marker);
                unreliable = false;
            }
            Ok(json!({"unreliable": unreliable, "pendingOp": pending,
                      "count": changes.len(), "changes": changes,
                      "ledger": ledger,
                      "note": if unreliable {
                          "The previous operation's after-snapshot failed. Do not undo anything based on this result."
                      } else { "changes is what the user changed. ledger is supporting evidence." }}))
        }
        _ => Err("Only history_log, history_restore and history_drift exist.".into()),
    }
}

struct Row {
    id: String,
    title: String,
    url: Option<String>,
    parent: String,
    index: u64,
    /// Chrome 138+ keeps separate account and local trees once signed in. An item
    /// crossing between them changed where it is stored, which must be reported
    /// distinctly from a move.
    syncing: Option<bool>,
}

fn flatten(node: &Value, parent: Option<&str>, out: &mut Vec<Row>) {
    if let Some(arr) = node.as_array() {
        for n in arr { flatten(n, parent, out); }
        return;
    }
    let id = node.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(p) = parent {
        if !id.is_empty() {
            out.push(Row {
                id: id.to_string(),
                title: node.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                url: node.get("url").and_then(|v| v.as_str()).map(|s| s.to_string()),
                parent: p.to_string(),
                index: node.get("index").and_then(|v| v.as_u64()).unwrap_or(0),
                syncing: node.get("syncing").and_then(|v| v.as_bool()),
            });
        }
    }
    if let Some(ch) = node.get("children").and_then(|v| v.as_array()) {
        for c in ch { flatten(c, Some(id), out); }
    }
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("BOOKMARK_BRIDGE_PORT").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(8787);
    // The token is generated here and planted in the extension directory, so the
    // user has nothing to copy. A web page cannot read files, so this stops a
    // hostile page dialing ws://127.0.0.1. It does not stop a hostile process on
    // this machine, which can read the file just as easily.
    let token = std::env::var("BOOKMARK_BRIDGE_TOKEN")
        .unwrap_or_else(|_| ensure_token().unwrap_or_default());

    let bridge = Arc::new(Bridge {
        pending: Arc::new(Mutex::new(HashMap::new())),
        outbox: Arc::new(Mutex::new(None)),
        epoch: std::sync::atomic::AtomicU64::new(0),
        next_id: Mutex::new(0),
        token,
    });

    // WebSocket listener, bound to loopback only.
    {
        let b = bridge.clone();
        tokio::spawn(async move {
            // When another MCP process already holds the port, join it and relay.
            // This is how several client sessions share one browser extension.
            let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(l) => l,
                Err(_) => {
                    eprintln!("[bridge] port {port} is held by another bridge; joining as a peer");
                    join_as_peer(b, port).await;
                    return;
                }
            };
            eprintln!("[bridge] listening on ws://127.0.0.1:{port}");
            while let Ok((stream, _)) = listener.accept().await {
                let b = b.clone();
                tokio::spawn(async move { serve_socket(b, stream).await; });
            }
        });
    }

    // MCP stdio loop
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = tokio::io::stdout();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() { continue; }
        let req: Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

        // Notifications get no reply.
        if id.is_none() { continue; }

        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "bookmark-bridge", "version": env!("CARGO_PKG_VERSION")}
            })),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => {
                let p = req.get("params").cloned().unwrap_or(Value::Null);
                let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                let args = p.get("arguments").cloned().unwrap_or(json!({}));
                match handle_tool(&bridge, &name, &args).await {
                    Ok(v) => Ok(json!({"content": [{"type": "text",
                        "text": serde_json::to_string_pretty(&v).unwrap_or_default()}]})),
                    Err(e) => Ok(json!({"content": [{"type": "text", "text": e}], "isError": true})),
                }
            }
            _ => Err(format!("Unsupported method: {method}")),
        };

        let resp = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": e}}),
        };
        let _ = stdout.write_all(format!("{resp}\n").as_bytes()).await;
        let _ = stdout.flush().await;
    }
}

/// Joins an already-running bridge as a peer and forwards this process's requests
/// to it. Reconnects when dropped; takes over as host if the first one dies.
/// This is how several client sessions share one browser extension.
async fn join_as_peer(b: Arc<Bridge>, port: u16) {
    loop {
        let url = format!("ws://127.0.0.1:{port}");
        match tokio_tungstenite::connect_async(&url).await {
            Ok((ws, _)) => {
                let (mut write, mut read) = ws.split();
                let hello = format!("peer:{}", b.token);
                if write.send(tokio_tungstenite::tungstenite::Message::Text(hello)).await.is_err() {
                    continue;
                }
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                *b.outbox.lock().await = Some(tx);
                let writer = tokio::spawn(async move {
                    while let Some(f) = rx.recv().await {
                        if write.send(tokio_tungstenite::tungstenite::Message::Text(f)).await.is_err() {
                            break;
                        }
                    }
                });
                while let Some(Ok(msg)) = read.next().await {
                    let Ok(text) = msg.to_text() else { continue };
                    let Ok(v) = serde_json::from_str::<Value>(text) else { continue };
                    let Some(id) = v.get("id").and_then(|i| i.as_u64()) else { continue };
                    if let Some(s) = b.pending.lock().await.remove(&id) { let _ = s.send(v); }
                }
                writer.abort();
                *b.outbox.lock().await = None;
            }
            Err(_) => {}
        }
        // The host may be gone. Try to claim the port and become the host.
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            eprintln!("[bridge] took over as host");
            while let Ok((stream, _)) = listener.accept().await {
                let b = b.clone();
                tokio::spawn(async move { serve_socket(b, stream).await; });
            }
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn serve_peer(
    b: Arc<Bridge>,
    mut read: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>>,
    mut write: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        tokio_tungstenite::tungstenite::Message>,
) {
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if write.send(tokio_tungstenite::tungstenite::Message::Text(frame)).await.is_err() {
                break;
            }
        }
    });
    while let Some(Ok(msg)) = read.next().await {
        let Ok(text) = msg.to_text() else { continue };
        let Ok(v) = serde_json::from_str::<Value>(text) else { continue };
        let Some(id) = v.get("id").and_then(|i| i.as_u64()) else { continue };
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
        let params = v.get("params").cloned().unwrap_or(json!({}));
        let b2 = b.clone();
        let tx = out_tx.clone();
        tokio::spawn(async move {
            let reply = match b2.call(&method, params).await {
                Ok(r) => json!({"id": id, "result": r}),
                Err(e) => json!({"id": id, "error": e.to_string()}),
            };
            let _ = tx.send(reply.to_string());
        });
    }
    writer.abort();
    eprintln!("[bridge] peer MCP process disconnected");
}

async fn serve_socket(b: Arc<Bridge>, stream: tokio::net::TcpStream) {
    // A web page can dial ws://127.0.0.1 too. Browsers always attach an Origin, so
    // http(s) origins are rejected here. The extension sends chrome-extension://
    // and a CLI sends no Origin at all.
    let mut rejected = false;
    let ws = match tokio_tungstenite::accept_hdr_async(stream, |req: &tokio_tungstenite::tungstenite::handshake::server::Request, res| {
        if let Some(o) = req.headers().get("origin").and_then(|v| v.to_str().ok()) {
            if o.starts_with("http://") || o.starts_with("https://") {
                rejected = true;
            }
        }
        Ok(res)
    }).await {
        Ok(w) => w, Err(_) => return,
    };
    if rejected {
        eprintln!("[bridge] rejected a connection from a web origin");
        return;
    }
    let (mut write, mut read) = ws.split();

    // The first frame is "<role>:<token>", where role is ext (the extension) or
    // peer (another MCP process). An older extension sends only the token, so a
    // missing role means ext. With no token configured the value is not checked:
    // binding to loopback is taken as sufficient. Set BOOKMARK_BRIDGE_TOKEN to also
    // exclude other processes on this machine.
    let hello = match read.next().await {
        Some(Ok(msg)) => msg.to_text().unwrap_or("").trim().to_string(),
        _ => String::new(),
    };
    let (role, tok) = match hello.split_once(':') {
        Some((r, t)) if r == "ext" || r == "peer" => (r.to_string(), t.to_string()),
        _ => ("ext".to_string(), hello.clone()),
    };
    let authed = b.token.is_empty() || tok == b.token;
    if !authed {
        let _ = write.send(tokio_tungstenite::tungstenite::Message::Text("unauthorized".into())).await;
        eprintln!("[bridge] closed a connection: token mismatch");
        return;
    }
    eprintln!("[bridge] extension connected");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // Capture this connection's own sender now. Re-reading the global outbox later
    // would pick up a sender from a connection that arrived meanwhile and ping the
    // wrong socket.
    let ping_tx = tx.clone();
    if role == "peer" {
        eprintln!("[bridge] another MCP process attached as a peer");
        serve_peer(b, read, write).await;
        return;
    }
    let my_epoch = b.epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    *b.outbox.lock().await = Some(tx);

    // Requests sent over the dropped socket are still outstanding. Rather than let
    // them wait out the budget in silence, say immediately that it is unknown
    // whether they applied - a retry can be destructive.
    {
        let mut pend = b.pending.lock().await;
        let stranded: Vec<_> = pend.drain().collect();
        drop(pend);
        for (_, s) in stranded {
            let _ = s.send(json!({"error":
                "The extension reconnected. Whether the previous request applied is \
                 unknown - check with history_drift before retrying."}));
        }
    }

    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if write.send(tokio_tungstenite::tungstenite::Message::Text(frame)).await.is_err() { break; }
        }
    });

    // An MV3 service worker sleeps after roughly 30s idle. WebSocket traffic resets
    // that timer, so poke it periodically to keep it awake. Without this the
    // connection drops during every quiet spell.
    let pinger = tokio::spawn(async move {
        loop {
            // The worker dies at 30s idle. Poke every 15s to leave margin.
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            if ping_tx.send("ping".to_string()).is_err() { break; }
        }
    });

    while let Some(Ok(msg)) = read.next().await {
        let Ok(text) = msg.to_text() else { continue };
        if text == "pong" { continue; }
        let Ok(v) = serde_json::from_str::<Value>(text) else { continue };
        let Some(id) = v.get("id").and_then(|i| i.as_u64()) else { continue };
        if let Some(sender) = b.pending.lock().await.remove(&id) { let _ = sender.send(v); }
    }
    pinger.abort();

    // Clear only while this generation is still current; a newer connection owns it otherwise.
    if b.epoch.load(std::sync::atomic::Ordering::SeqCst) == my_epoch {
        *b.outbox.lock().await = None;
        eprintln!("[bridge] extension disconnected");
    } else {
        eprintln!("[bridge] cleaned up a stale connection (newer one kept)");
    }
    writer.abort();
}
