//! Bridgey v2 MCP proxy and bookmark tools executed by the Chrome native host.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex};

mod native;
mod registration;

static STATE_DIR: OnceLock<PathBuf> = OnceLock::new();

const PROTOCOL: &str = "2024-11-05";

/// Why a call to the extension failed. The distinction that matters is whether
/// the request provably never reached Chrome: only then do we know no bookmark
/// was touched, and only then is it safe to clear the dirty marker.
#[derive(Debug)]
enum CallError {
    /// The connection dropped before the frame could be queued.
    SendFailed,
    /// The request exceeds the bridge limit and was never queued.
    TooLarge,
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
        matches!(self, CallError::SendFailed | CallError::TooLarge)
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::SendFailed => f.write_str(
                "The extension disconnected before the request was sent. Nothing was changed."),
            CallError::TooLarge => f.write_str("The request exceeds Bridgey's 64 MiB limit."),
            CallError::Cancelled => f.write_str(
                "The extension disconnected while this request was in flight. \
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

struct PendingCall {
    sender: oneshot::Sender<Value>,
    mutates: bool,
}

struct Bridge {
    pending: Mutex<HashMap<u64, PendingCall>>,
    outbox: tokio::sync::mpsc::UnboundedSender<(u64, String)>,
    next_id: std::sync::atomic::AtomicU64,
}

fn is_bookmark_mutation(method: &str) -> bool {
    matches!(method, "update" | "move" | "create" | "remove" | "removeTree" | "batch")
}

impl Bridge {
    async fn has_pending_mutation(&self) -> bool {
        self.pending.lock().await.values().any(|call| call.mutates)
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, CallError> {
        let n_ops = params.get("ops").and_then(Value::as_array).map_or(0, Vec::len);
        self.call_with_budget(method, params, Duration::from_secs(20 + n_ops as u64 / 5)).await
    }

    async fn call_with_budget(
        &self, method: &str, params: Value, budget: Duration,
    ) -> Result<Value, CallError> {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let frame = json!({"id": id, "method": method, "params": params}).to_string();
        if frame.len() > native::CHROME_INPUT_LIMIT {
            return Err(CallError::TooLarge);
        }
        let (done_tx, done_rx) = oneshot::channel();
        let mutates = is_bookmark_mutation(method);
        self.pending.lock().await.insert(id, PendingCall { sender: done_tx, mutates });
        if self.outbox.send((id, frame)).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(CallError::SendFailed);
        }
        match tokio::time::timeout(budget, done_rx).await {
            Ok(Ok(v)) => {
                if let Some(err) = v.get("error").and_then(Value::as_str) {
                    Err(CallError::Extension(err.to_string()))
                } else {
                    Ok(v.get("result").cloned().unwrap_or(Value::Null))
                }
            }
            Ok(Err(_)) => Err(CallError::Cancelled),
            Err(_) => {
                // The caller timed out, but Chrome can still be changing bookmarks.
                if !mutates {
                    self.pending.lock().await.remove(&id);
                }
                Err(CallError::Timeout(budget.as_secs()))
            }
        }
    }
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
      {"name":"bookmarks_find_duplicates","description":"Lists bookmarks with exactly the same URL, with their IDs, titles and parent folder IDs. Optionally scan only a folder and its descendants. Does not change bookmarks.",
       "inputSchema":{"type":"object","properties":{"folderId":{"type":"string","minLength":1}},"additionalProperties":false}},
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
        let pending = b.pending.lock().await.len();
        let connected = matches!(
            b.call_with_budget("__bridgey_health", json!({}), Duration::from_secs(2)).await,
            Ok(Value::Bool(true))
        );
        let mut status = json!({
            "extensionConnected": connected, "pendingRequests": pending, "transport": "native"
        });
        if !connected {
            status["reason"] = json!("extension_not_responding");
        }
        return Ok(status);
    }
    let mutates = name == "history_restore"
        || name.strip_prefix("bookmarks_").is_some_and(is_bookmark_mutation);
    if mutates && b.has_pending_mutation().await {
        return Err("A previous bookmark change is still running in Chrome. Wait for it to finish, then run history_drift before retrying.".into());
    }
    if let Some(cmd) = name.strip_prefix("history_") {
        // restore mutates; it is not a read. It needs the same protection as the
        // other mutating tools so agent-head ends up pointing at the restored
        // state. Without that, the next drift reports the whole restore as a user
        // change and the agent tries to undo it.
        if cmd == "restore" {
            let marker = dirty_marker();
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
    if method == "find_duplicates" {
        let args = args.as_object().ok_or("Expected an object with optional folderId.")?;
        if args.keys().any(|key| key != "folderId") {
            return Err("Only folderId is accepted.".into());
        }
        let folder_id = args.get("folderId").map(|id| {
            id.as_str().filter(|id| !id.is_empty()).ok_or("folderId must be a non-empty string.")
        }).transpose()?;
        let tree = b.call("tree", json!({})).await?;
        return find_duplicates(&tree, folder_id);
    }
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
    if mutates {
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
        std::fs::write(&marker, &label)
            .map_err(|e| format!("Could not record the pending change: {e}"))?;
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
fn select_profile(profile: &str) -> Result<(), String> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
    } else {
        std::env::var_os("HOME").map(|dir| PathBuf::from(dir).join(".local/state"))
    }.filter(|path| path.is_absolute()).ok_or("The user's state directory is unavailable.")?;
    let dir = base.join("bookmark-bridge").join("native-v2").join(profile);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    STATE_DIR.set(dir).map_err(|_| "The profile is already selected.".into())
}

fn state_dir() -> &'static Path {
    STATE_DIR.get().expect("profile selected before accepting tool calls")
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
    let out = std::process::Command::new(registration::git_program())
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
        // The OS releases the lock when the process dies, so a leftover lock file
        // never blocks the next run.
        fs4::fs_std::FileExt::lock_exclusive(&f)
            .map_err(|e| format!("could not acquire the history lock: {e}"))?;
        Ok(HistoryLock(f))
    }
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        let _ = fs4::fs_std::FileExt::unlock(&self.0);
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
    if !tree.as_array().is_some_and(|roots| !roots.is_empty()) {
        return Err("Chrome did not return a bookmark tree.".into());
    }
    let dir = history_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    if !dir.join(".git").exists() {
        git(&["init", "-q"])?;
        let _ = git(&["config", "user.name", "bookmark-bridge"]);
        let _ = git(&["config", "user.email", "bookmark-bridge@localhost"]);
    }
    let pretty = serde_json::to_string_pretty(&tree).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("tree.json"), pretty).map_err(|e| e.to_string())?;
    git(&["add", "--", "tree.json"])?;
    if !git(&["diff", "--cached", "--name-only", "--", "tree.json"])?.is_empty() {
        git(&["commit", "-q", "-m", label, "--", "tree.json"])?;
    }
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
            let raw = git(&["show", "--end-of-options", &format!("{c}:tree.json")])?;
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
            std::fs::write(dirty_marker(), format!("history_restore {}", compact(args)))
                .map_err(|e| format!("Could not record the pending restore: {e}"))?;
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
            let unsettled = b.has_pending_mutation().await;
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
                // Keep the evidence while Chrome can still be changing bookmarks.
                if unsettled {
                    o.insert("clear".into(), json!(false));
                }
            }
            let ledger = b.call("drift", dargs).await.unwrap_or(Value::Null);
            let Some(base) = base else {
                return Ok(json!({"baseline": "none", "unreliable": unreliable, "pendingOp": pending,
                    "note": if unreliable {
                        "A change may have applied before the first baseline was saved. Review the before-snapshot with history_log."
                    } else {
                        "No agent baseline exists yet. Your first operation creates one."
                    },
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
            // Only a settled operation can be compared against the baseline.
            if unreliable && !unsettled && changes.is_empty() {
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

fn find_duplicates(tree: &Value, folder_id: Option<&str>) -> Result<Value, String> {
    let roots = tree.as_array().ok_or("The extension did not return a bookmark tree.")?;
    let subtree = if let Some(id) = folder_id {
        let mut pending: Vec<_> = roots.iter().collect();
        loop {
            let node = pending.pop().ok_or("No bookmark folder with that ID.")?;
            if node["id"].as_str() == Some(id) && node.get("url").is_none() {
                break node;
            }
            if let Some(children) = node["children"].as_array() {
                pending.extend(children);
            }
        }
    } else {
        tree
    };

    let mut rows = Vec::new();
    flatten(subtree, None, &mut rows);
    let mut by_url: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for row in &rows {
        if let Some(url) = row.url.as_deref().filter(|url| !url.is_empty()) {
            by_url.entry(url).or_default().push(row);
        }
    }
    let groups: Vec<_> = by_url.into_iter().filter(|(_, rows)| rows.len() > 1).map(|(url, rows)| {
        let bookmarks: Vec<_> = rows.iter().map(|row| {
            json!({"id": row.id, "title": row.title, "parentId": row.parent})
        }).collect();
        json!({"url": url, "bookmarks": bookmarks})
    }).collect();
    Ok(json!({"groups": groups}))
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    if let Some(command) = args.next() {
        let result = if command == "register-native" {
            match args.next() {
                Some(id) if args.next().is_none() => registration::register_unpacked(&id),
                None => registration::register(),
                _ => Err("Usage: bookmark-bridge register-native [extension-id]".into()),
            }
        } else if registration::check_origin(&command) {
            native::run_host().await
        } else {
            Err("Unknown command or unauthorized extension origin.".into())
        };
        if let Err(e) = &result {
            eprintln!("[bridgey] {e}");
        }
        // Tokio stdin may still be blocked after the browser drops its port.
        std::process::exit(i32::from(result.is_err()));
    }

    let registration_error = registration::register().err();
    if let Some(e) = &registration_error {
        eprintln!("[bridgey] native host registration failed: {e}");
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
                match native::proxy_tool(&name, args, registration_error.as_deref()).await {
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
