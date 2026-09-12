//! End-to-end tests against the real binary.
//!
//! Each test spawns `bookmark-bridge` with its own HOME and port, speaks MCP over
//! the child's stdio, and plays the Chrome extension with a WebSocket client. The
//! fake extension keeps an in-memory tree, so the destructive paths can be
//! exercised without a browser and without touching anyone's bookmarks.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};

/// Ports are leased from a high range so concurrently running tests never collide.
static NEXT_PORT: AtomicU16 = AtomicU16::new(18787);
fn lease_port() -> u16 {
    NEXT_PORT.fetch_add(1, Ordering::SeqCst)
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
    id: u64,
    #[allow(dead_code)]
    home: tempdir::TempDir,
    port: u16,
}

impl Server {
    fn start() -> Server {
        Server::start_on(lease_port())
    }

    fn start_on(port: u16) -> Server {
        let home = tempdir::TempDir::new("bb-test").expect("temp home");
        let mut child = Command::new(env!("CARGO_BIN_EXE_bookmark-bridge"))
            .env("HOME", home.path())
            .env("BOOKMARK_BRIDGE_PORT", port.to_string())
            .env("BOOKMARK_BRIDGE_TOKEN", "")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bookmark-bridge");
        let stdin = child.stdin.take().unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Server { child, stdin, out, id: 0, home, port }
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let req = json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params});
        writeln!(self.stdin, "{req}").expect("write request");
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.out.read_line(&mut line).expect("read reply");
        serde_json::from_str(&line).expect("parse reply")
    }

    /// Calls a tool and returns its decoded payload plus whether it was an error.
    fn tool(&mut self, name: &str, args: Value) -> (Value, bool) {
        let r = self.rpc("tools/call", json!({"name": name, "arguments": args}));
        let result = &r["result"];
        let text = result["content"][0]["text"].as_str().unwrap_or("").to_string();
        let is_err = result["isError"].as_bool().unwrap_or(false);
        (serde_json::from_str(&text).unwrap_or(Value::String(text)), is_err)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The bookmark tree the fake extension serves, and the log of what it was asked.
#[derive(Default)]
struct Fake {
    /// id -> (title, url, parent, index)
    nodes: Vec<(String, String, Option<String>, String, u64)>,
    calls: Vec<String>,
    next_id: u64,
}

impl Fake {
    fn seeded() -> Fake {
        let mut f = Fake { next_id: 100, ..Default::default() };
        f.nodes.push(("10".into(), "Bar".into(), None, "1".into(), 0));
        f.nodes.push(("11".into(), "Docs".into(), Some("https://example.com/a".into()), "10".into(), 0));
        f.nodes.push(("12".into(), "Notes".into(), Some("https://example.com/b".into()), "10".into(), 1));
        f
    }

    fn tree(&self) -> Value {
        // Mirrors chrome.bookmarks.getTree: a single root whose children nest.
        fn children(f: &Fake, parent: &str) -> Vec<Value> {
            f.nodes.iter().filter(|n| n.3 == parent).map(|n| {
                let mut o = json!({"id": n.0, "title": n.1, "index": n.4, "parentId": n.3});
                if let Some(u) = &n.2 { o["url"] = json!(u); }
                let kids = children(f, &n.0);
                if !kids.is_empty() { o["children"] = json!(kids); }
                o
            }).collect()
        }
        json!([{"id": "0", "title": "", "children": [
            {"id": "1", "title": "root", "index": 0, "parentId": "0",
             "children": children(self, "1")}
        ]}])
    }

    fn get(&self, id: &str) -> Option<&(String, String, Option<String>, String, u64)> {
        self.nodes.iter().find(|n| n.0 == id)
    }

    /// The compare-and-swap the real extension performs before every mutation.
    fn mismatch(&self, p: &Value) -> Option<Value> {
        let expect = p.get("expect")?.as_object()?;
        let id = p.get("id")?.as_str()?;
        let cur = self.get(id)?;
        let actual = |k: &str| match k {
            "title" => Some(cur.1.clone()),
            "url" => cur.2.clone(),
            "parentId" => Some(cur.3.clone()),
            _ => None,
        };
        let bad: Vec<Value> = expect.iter()
            .filter(|(k, v)| actual(k).as_deref() != v.as_str())
            .map(|(k, v)| json!({"field": k, "expected": v, "actual": actual(k)}))
            .collect();
        if bad.is_empty() { None } else { Some(json!({"skipped": true, "id": id, "mismatch": bad})) }
    }

    fn apply(&mut self, op: &str, p: &Value) -> Result<Value, String> {
        let id = || p.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        match op {
            "update" => {
                let target = id();
                let n = self.nodes.iter_mut().find(|n| n.0 == target).ok_or("no such id")?;
                if let Some(t) = p.get("title").and_then(|v| v.as_str()) { n.1 = t.into(); }
                if let Some(u) = p.get("url").and_then(|v| v.as_str()) { n.2 = Some(u.into()); }
                Ok(json!({"id": target}))
            }
            "move" => {
                let target = id();
                let n = self.nodes.iter_mut().find(|n| n.0 == target).ok_or("no such id")?;
                if let Some(pa) = p.get("parentId").and_then(|v| v.as_str()) { n.3 = pa.into(); }
                if let Some(i) = p.get("index").and_then(|v| v.as_u64()) { n.4 = i; }
                Ok(json!({"id": target}))
            }
            "create" => {
                let parent = p.get("parentId").and_then(|v| v.as_str()).unwrap_or("1").to_string();
                let title = p.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let url = p.get("url").and_then(|v| v.as_str()).map(|s| s.to_string());
                // Idempotent create, exactly as the extension does it.
                if p.get("ifAbsent").and_then(|v| v.as_bool()) != Some(false) {
                    if let Some(dup) = self.nodes.iter()
                        .find(|n| n.3 == parent && n.1 == title && n.2 == url) {
                        return Ok(json!({"id": dup.0, "deduped": true}));
                    }
                }
                self.next_id += 1;
                let new = self.next_id.to_string();
                let index = p.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                self.nodes.push((new.clone(), title, url, parent, index));
                Ok(json!({"id": new}))
            }
            "remove" | "removeTree" => {
                let target = id();
                self.nodes.retain(|n| n.0 != target && n.3 != target);
                Ok(Value::Null)
            }
            other => Err(format!("unknown op: {other}")),
        }
    }

    fn handle(&mut self, method: &str, p: &Value) -> Result<Value, String> {
        self.calls.push(method.to_string());
        match method {
            "tree" => Ok(self.tree()),
            "children" => Ok(json!([])),
            "search" => Ok(json!([])),
            "drift" => Ok(json!({"since": p.get("since").cloned().unwrap_or(json!(0)),
                                 "count": 0, "changes": [], "dropped": 0})),
            "batch" => {
                let mut applied = 0u64;
                let (mut skipped, mut failed, mut deduped) = (vec![], vec![], vec![]);
                for op in p.get("ops").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
                    let name = op.get("op").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if let Some(m) = self.mismatch(&op) { skipped.push(m); continue; }
                    match self.apply(&name, &op) {
                        Ok(r) if r.get("deduped").is_some() => deduped.push(r),
                        Ok(_) => applied += 1,
                        Err(e) => failed.push(json!({"op": name, "error": e})),
                    }
                }
                Ok(json!({"applied": applied, "deduped": deduped,
                          "skipped": skipped, "failed": failed}))
            }
            m => {
                if let Some(mm) = self.mismatch(p) { return Ok(mm); }
                self.apply(m, p)
            }
        }
    }
}

/// Runs a fake extension against the server until the returned guard is dropped.
struct Extension {
    fake: Arc<Mutex<Fake>>,
    task: tokio::task::JoinHandle<()>,
}

impl Extension {
    async fn attach(port: u16, role: &str) -> Extension {
        let fake = Arc::new(Mutex::new(Fake::seeded()));
        let url = format!("ws://127.0.0.1:{port}");
        let hello = format!("{role}:");
        // The server may not be listening the instant the process starts.
        let mut ws = None;
        for _ in 0..80 {
            match tokio_tungstenite::connect_async(&url).await {
                Ok((s, _)) => { ws = Some(s); break; }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            }
        }
        let ws = ws.expect("extension could not reach the bridge");
        let (mut write, mut read) = ws.split();
        write.send(tokio_tungstenite::tungstenite::Message::Text(hello)).await.unwrap();

        let mine = fake.clone();
        let task = tokio::spawn(async move {
            while let Some(Ok(msg)) = read.next().await {
                let Ok(text) = msg.to_text() else { continue };
                if text == "ping" {
                    let _ = write.send(tokio_tungstenite::tungstenite::Message::Text("pong".into())).await;
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(text) else { continue };
                let Some(id) = v.get("id").and_then(|i| i.as_u64()) else { continue };
                let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
                let params = v.get("params").cloned().unwrap_or(json!({}));
                let reply = match mine.lock().unwrap().handle(&method, &params) {
                    Ok(r) => json!({"id": id, "result": r}),
                    Err(e) => json!({"id": id, "error": e}),
                };
                if write.send(tokio_tungstenite::tungstenite::Message::Text(reply.to_string()))
                    .await.is_err() { break; }
            }
        });
        Extension { fake, task }
    }
}

impl Drop for Extension {
    fn drop(&mut self) { self.task.abort(); }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speaks_mcp_and_lists_every_tool() {
    let mut s = Server::start();
    let init = s.rpc("initialize", json!({}));
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init["result"]["serverInfo"]["name"], "bookmark-bridge");

    let list = s.rpc("tools/list", json!({}));
    let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter()
        .map(|t| t["name"].as_str().unwrap()).collect();
    for expected in ["bookmarks_tree", "bookmarks_search", "bookmarks_children",
                     "bookmarks_update", "bookmarks_move", "bookmarks_create",
                     "bookmarks_remove", "bookmarks_removeTree", "bookmarks_batch",
                     "history_log", "history_restore", "history_drift", "bridge_status"] {
        assert!(names.contains(&expected), "{expected} missing from tools/list");
    }
    // Every description is what an agent reads as the spec. None may be empty,
    // and the release is English-only.
    for t in list["result"]["tools"].as_array().unwrap() {
        let d = t["description"].as_str().unwrap();
        assert!(!d.is_empty(), "{} has no description", t["name"]);
        assert!(!d.chars().any(|c| ('\u{ac00}'..='\u{d7a3}').contains(&c)),
                "{} still has a non-English description", t["name"]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuses_to_mutate_while_no_extension_is_attached() {
    let mut s = Server::start();
    let (status, _) = s.tool("bridge_status", json!({}));
    assert_eq!(status["extensionConnected"], false);

    let (msg, is_err) = s.tool("bookmarks_update", json!({"id": "11", "title": "x"}));
    assert!(is_err, "a mutation with no extension must fail");
    let text = msg.as_str().unwrap_or_default();
    assert!(text.contains("not connected"), "unexpected message: {text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn applies_a_change_and_snapshots_around_it() {
    let mut s = Server::start();
    let ext = Extension::attach(s.port, "ext").await;

    let (status, _) = s.tool("bridge_status", json!({}));
    assert_eq!(status["extensionConnected"], true);

    let (_, is_err) = s.tool("bookmarks_update", json!({"id": "11", "title": "Docs (tidied)"}));
    assert!(!is_err);
    assert_eq!(ext.fake.lock().unwrap().get("11").unwrap().1, "Docs (tidied)");

    // A before-snapshot and an after-snapshot, so the change can be rolled back.
    let (log, _) = s.tool("history_log", json!({}));
    let labels: Vec<String> = log["snapshots"].as_array().unwrap().iter()
        .map(|c| c["label"].as_str().unwrap_or("").to_string()).collect();
    assert!(labels.iter().any(|l| l.starts_with("before: bookmarks_update")), "{labels:?}");
    assert!(labels.iter().any(|l| l.starts_with("after: bookmarks_update")), "{labels:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skips_an_item_whose_value_moved_under_the_plan() {
    let mut s = Server::start();
    let ext = Extension::attach(s.port, "ext").await;

    // The user renamed it after the plan was made.
    ext.fake.lock().unwrap().nodes.iter_mut().find(|n| n.0 == "11").unwrap().1 = "Docs, mine".into();

    let (out, is_err) = s.tool("bookmarks_batch", json!({"ops": [
        {"op": "update", "id": "11", "title": "Reference", "expect": {"title": "Docs"}},
        {"op": "update", "id": "12", "title": "Reading",   "expect": {"title": "Notes"}}
    ]}));
    assert!(!is_err);
    assert_eq!(out["applied"], 1, "only the untouched item may be applied");
    assert_eq!(out["skipped"].as_array().unwrap().len(), 1);
    assert_eq!(out["skipped"][0]["id"], "11");
    // The user's title survives; the other one is applied.
    assert_eq!(ext.fake.lock().unwrap().get("11").unwrap().1, "Docs, mine");
    assert_eq!(ext.fake.lock().unwrap().get("12").unwrap().1, "Reading");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creating_the_same_item_twice_makes_one() {
    let mut s = Server::start();
    let ext = Extension::attach(s.port, "ext").await;
    let before = ext.fake.lock().unwrap().nodes.len();

    let args = json!({"parentId": "10", "title": "Inbox"});
    let (first, _) = s.tool("bookmarks_create", args.clone());
    let (second, _) = s.tool("bookmarks_create", args);

    assert_eq!(second["deduped"], true, "a repeat create must not duplicate");
    assert_eq!(first["id"], second["id"]);
    assert_eq!(ext.fake.lock().unwrap().nodes.len(), before + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_a_batch_too_large_for_one_service_worker_event() {
    let mut s = Server::start();
    let _ext = Extension::attach(s.port, "ext").await;

    let ops: Vec<Value> = (0..201)
        .map(|i| json!({"op": "create", "parentId": "10", "title": format!("n{i}")}))
        .collect();
    let (msg, is_err) = s.tool("bookmarks_batch", json!({"ops": ops}));
    assert!(is_err, "an oversized batch must be refused, not half applied");
    assert!(msg.as_str().unwrap_or_default().contains("200"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restores_a_deleted_subtree_under_new_ids() {
    let mut s = Server::start();
    let ext = Extension::attach(s.port, "ext").await;

    // Take a snapshot by making any change, then delete a folder outright.
    let (_, _) = s.tool("bookmarks_update", json!({"id": "11", "title": "Docs"}));
    let (log, _) = s.tool("history_log", json!({}));
    let good = log["snapshots"][0]["commit"].as_str().unwrap().to_string();

    let (_, is_err) = s.tool("bookmarks_removeTree", json!({"id": "10"}));
    assert!(!is_err);
    assert!(ext.fake.lock().unwrap().get("10").is_none());

    let (out, is_err) = s.tool("history_restore", json!({"commit": good}));
    assert!(!is_err, "restore failed: {out}");
    assert!(out["recreated"].as_u64().unwrap() >= 3,
            "the folder and its children should come back: {out}");
    let f = ext.fake.lock().unwrap();
    assert!(f.nodes.iter().any(|n| n.1 == "Bar"));
    assert!(f.nodes.iter().any(|n| n.1 == "Docs"));
    assert!(f.nodes.iter().any(|n| n.1 == "Notes"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reports_the_users_own_changes_as_drift() {
    let mut s = Server::start();
    let ext = Extension::attach(s.port, "ext").await;

    // Establish agent-head.
    s.tool("bookmarks_update", json!({"id": "11", "title": "Docs"}));
    let (clean, _) = s.tool("history_drift", json!({}));
    assert_eq!(clean["count"], 0, "the agent's own work is not drift: {clean}");

    // Now the user edits the tree behind the agent's back.
    ext.fake.lock().unwrap().nodes.iter_mut().find(|n| n.0 == "12").unwrap().1 = "Renamed by me".into();

    let (drift, _) = s.tool("history_drift", json!({}));
    assert_eq!(drift["count"], 1, "{drift}");
    assert_eq!(drift["changes"][0]["kind"], "retitled");
    assert_eq!(drift["changes"][0]["to"], "Renamed by me");
    assert_eq!(drift["unreliable"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_session_relays_through_the_first() {
    let port = lease_port();
    let mut host = Server::start_on(port);
    let ext = Extension::attach(port, "ext").await;
    // The host must own the port before the peer starts, or both become hosts.
    let (status, _) = host.tool("bridge_status", json!({}));
    assert_eq!(status["extensionConnected"], true);

    let mut peer = Server::start_on(port);
    // The peer reaches the same extension through the host.
    for _ in 0..40 {
        let (s, _) = peer.tool("bridge_status", json!({}));
        if s["extensionConnected"] == true { break; }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let (s, _) = peer.tool("bridge_status", json!({}));
    assert_eq!(s["extensionConnected"], true, "the peer never attached");

    let (out, is_err) = peer.tool("bookmarks_update", json!({"id": "12", "title": "From the peer"}));
    assert!(!is_err, "peer write failed: {out}");
    assert_eq!(ext.fake.lock().unwrap().get("12").unwrap().1, "From the peer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keeps_runtime_state_out_of_the_snapshots() {
    let mut s = Server::start();
    let _ext = Extension::attach(s.port, "ext").await;
    s.tool("bookmarks_update", json!({"id": "11", "title": "Docs"}));

    // A snapshot is the bookmark tree and nothing else. The lock file and the
    // dirty marker must not ride along, or an unchanged tree still commits.
    let history = s.home.path().join(".local/state/bookmark-bridge/history");
    let tracked = Command::new("git").arg("-C").arg(&history).arg("ls-files")
        .output().expect("git ls-files");
    let files: Vec<&str> = std::str::from_utf8(&tracked.stdout).unwrap()
        .lines().collect();
    assert_eq!(files, vec!["tree.json"], "snapshots carry runtime state: {files:?}");
}
