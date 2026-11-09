//! End-to-end tests against the real binary.
//!
//! Each test runs the MCP proxy and browser-owned host under a temporary user
//! profile. A fake Chrome extension speaks Native Messaging and keeps its
//! bookmarks in memory.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};

const ORIGIN: &str = "chrome-extension://ekokljgbbianihlpahackkfdendobngd/";
const PROFILE_ID: &str = "00112233445566778899aabbccddeeff";
const WAIT: Duration = Duration::from_secs(30);

fn legacy_state_root(profile: &Path) -> PathBuf {
    if cfg!(windows) { profile.join("bookmark-bridge") }
    else { profile.join(".local/state/bookmark-bridge") }
}

fn state_root(profile: &Path) -> PathBuf {
    legacy_state_root(profile).join("native-v2").join(PROFILE_ID)
}

fn process_for(profile: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bookmark-bridge"));
    command
        .env("HOME", profile)
        .env("LOCALAPPDATA", profile)
        .env("APPDATA", profile)
        .env("BOOKMARK_BRIDGE_SKIP_REGISTRATION", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    command
}

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Server {
    _process: Process,
    stdin: ChildStdin,
    replies: Receiver<Value>,
    id: u64,
    profile: Arc<tempdir::TempDir>,
}

impl Server {
    fn start() -> Server {
        let profile = tempdir::TempDir::new("bb-test").expect("temp profile");
        Server::start_in(Arc::new(profile))
    }

    fn start_in(profile: Arc<tempdir::TempDir>) -> Server {
        let mut child = process_for(profile.path()).spawn().expect("spawn MCP proxy");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (send, replies) = mpsc::channel();
        thread::spawn(move || {
            for line in stdout.lines() {
                let reply = serde_json::from_str(&line.unwrap()).expect("parse MCP reply");
                if send.send(reply).is_err() {
                    break;
                }
            }
        });
        Server { _process: Process(child), stdin, replies, id: 0, profile }
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let req = json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params});
        writeln!(self.stdin, "{req}").expect("write request");
        self.stdin.flush().unwrap();
        let reply = self.replies.recv_timeout(WAIT).expect("MCP reply");
        assert_eq!(reply["id"], self.id, "{reply}");
        reply
    }

    fn status(&mut self, connected: bool) {
        let (status, is_err) = self.tool("bridge_status", json!({}));
        assert!(!is_err, "{status}");
        assert_eq!(status["extensionConnected"], connected, "{status}");
        assert_eq!(status["transport"], "native", "{status}");
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

/// The bookmark tree the fake extension serves, and the log of what it was asked.
#[derive(Default)]
struct Fake {
    /// id -> (title, url, parent, index)
    nodes: Vec<(String, String, Option<String>, String, u64)>,
    calls: Vec<String>,
    next_id: u64,
    received_chunks: usize,
    disconnect_after: Option<&'static str>,
    ignore_health: bool,
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

fn write_frame(mut writer: impl Write, message: &Value) {
    let body = serde_json::to_vec(message).unwrap();
    writer.write_all(&u32::try_from(body.len()).unwrap().to_ne_bytes()).unwrap();
    writer.write_all(&body).unwrap();
    writer.flush().unwrap();
}

fn read_frame(reader: &mut impl Read) -> Option<Value> {
    let mut header = [0; 4];
    if reader.read(&mut header[..1]).unwrap() == 0 {
        return None;
    }
    reader.read_exact(&mut header[1..]).unwrap();
    let len = u32::from_ne_bytes(header) as usize;
    assert!(len <= 1024 * 1024, "Chrome would reject a {len}-byte frame");
    let mut body = vec![0; len];
    reader.read_exact(&mut body).unwrap();
    Some(serde_json::from_slice(&body).expect("parse Native Messaging frame"))
}

fn start_host(profile: &Path) -> (Process, ChildStdin, Receiver<Value>) {
    let mut child = process_for(profile).arg(ORIGIN).spawn().expect("spawn Chrome host");
    let stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let (send, messages) = mpsc::channel();
    thread::spawn(move || {
        while let Some(message) = read_frame(&mut stdout) {
            if send.send(message).is_err() {
                break;
            }
        }
    });
    (Process(child), stdin, messages)
}

/// Runs a fake extension against the Chrome host until the guard is dropped.
struct Extension {
    process: Option<Process>,
    fake: Arc<Mutex<Fake>>,
    task: Option<JoinHandle<()>>,
}

impl Extension {
    fn attach(server: &Server) -> Extension {
        let (process, mut stdin, messages) = start_host(server.profile.path());
        write_frame(&mut stdin, &json!({"type": "prepare", "profileId": PROFILE_ID}));
        let prepared = messages.recv_timeout(WAIT).expect("host preparation");
        assert_eq!(prepared["type"], "prepared", "{prepared}");
        write_frame(&mut stdin, &json!({"type": "activate"}));
        let ready = messages.recv_timeout(WAIT).expect("host activation");
        assert_eq!(ready["type"], "ready", "{ready}");

        let fake = Arc::new(Mutex::new(Fake::seeded()));
        let mine = fake.clone();
        let task = thread::spawn(move || {
            while let Ok(message) = messages.recv() {
                let request = match message["type"].as_str() {
                    Some("request") => message["request"].clone(),
                    Some("requestChunk") => {
                        let id = message["id"].as_u64().unwrap();
                        let total = message["total"].as_u64().unwrap();
                        assert!(total > 1, "{message}");
                        assert_eq!(message["index"], 0);
                        let mut body = message["data"].as_str().unwrap().to_owned();
                        for index in 1..total {
                            let chunk = messages.recv_timeout(WAIT).expect("next request chunk");
                            assert_eq!(chunk["type"], "requestChunk");
                            assert_eq!(chunk["id"], id);
                            assert_eq!(chunk["index"], index);
                            assert_eq!(chunk["total"], total);
                            body.push_str(chunk["data"].as_str().unwrap());
                        }
                        mine.lock().unwrap().received_chunks += total as usize;
                        let request: Value = serde_json::from_str(&body).expect("reassembled request");
                        assert_eq!(request["id"], id);
                        request
                    }
                    _ => panic!("unexpected host message: {message}"),
                };
                let id = request["id"].as_u64().unwrap();
                let method = request["method"].as_str().unwrap();
                if method == "__bridgey_health" {
                    if !mine.lock().unwrap().ignore_health {
                        write_frame(&mut stdin, &json!({"id": id, "result": true}));
                    }
                    continue;
                }
                let mut fake = mine.lock().unwrap();
                let result = fake.handle(method, &request["params"]);
                if fake.disconnect_after == Some(method) {
                    // Simulate Chrome applying a write and disappearing before its reply.
                    break;
                }
                drop(fake);
                let reply = match result {
                    Ok(r) => json!({"id": id, "result": r}),
                    Err(e) => json!({"id": id, "error": e}),
                };
                write_frame(&mut stdin, &reply);
            }
        });
        Extension { process: Some(process), fake, task: Some(task) }
    }
}

impl Drop for Extension {
    fn drop(&mut self) {
        drop(self.process.take());
        if let Some(task) = self.task.take() {
            if let Err(error) = task.join() {
                if !thread::panicking() {
                    std::panic::resume_unwind(error);
                }
            }
        }
    }
}

#[test]
fn speaks_mcp_and_lists_every_tool() {
    let mut s = Server::start();
    let init = s.rpc("initialize", json!({}));
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init["result"]["serverInfo"]["name"], "bookmark-bridge");
    assert_eq!(init["result"]["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));

    let list = s.rpc("tools/list", json!({}));
    let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter()
        .map(|t| t["name"].as_str().unwrap()).collect();
    for expected in ["bookmarks_tree", "bookmarks_search", "bookmarks_children",
                     "bookmarks_find_duplicates",
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

#[test]
fn finds_exact_url_duplicates_within_the_requested_scope_without_writing() {
    let mut s = Server::start();
    let ext = Extension::attach(&s);
    {
        let mut fake = ext.fake.lock().unwrap();
        fake.nodes.extend([
            ("20".into(), "Nested".into(), None, "10".into(), 2),
            ("21".into(), "Nested copy".into(), Some("https://example.com/a".into()), "20".into(), 0),
            ("30".into(), "Other".into(), None, "1".into(), 1),
            ("31".into(), "Other copy".into(), Some("https://example.com/a".into()), "30".into(), 0),
            ("32".into(), "Different URL".into(), Some("https://example.com/a/".into()), "30".into(), 1),
            ("40".into(), "Empty".into(), None, "1".into(), 2),
        ]);
    }

    let (global, is_err) = s.tool("bookmarks_find_duplicates", json!({}));
    assert!(!is_err, "{global}");
    assert_eq!(global, json!({"groups": [{"url": "https://example.com/a", "bookmarks": [
        {"id": "11", "title": "Docs", "parentId": "10"},
        {"id": "21", "title": "Nested copy", "parentId": "20"},
        {"id": "31", "title": "Other copy", "parentId": "30"}
    ]}]}));

    let (scoped, is_err) = s.tool("bookmarks_find_duplicates", json!({"folderId": "10"}));
    assert!(!is_err, "{scoped}");
    let mut expected = global;
    expected["groups"][0]["bookmarks"].as_array_mut().unwrap().pop();
    assert_eq!(scoped, expected);
    assert_eq!(s.tool("bookmarks_find_duplicates", json!({"folderId": "40"})),
        (json!({"groups": []}), false));
    for id in ["11", "missing"] {
        let (msg, is_err) = s.tool("bookmarks_find_duplicates", json!({"folderId": id}));
        assert!(is_err && msg == "No bookmark folder with that ID.", "{msg}");
    }
    for args in [json!({"folderId": 10}), json!({"folder_id": "10"})] {
        assert!(s.tool("bookmarks_find_duplicates", args).1);
    }
    assert_eq!(ext.fake.lock().unwrap().calls, ["tree"; 5]);
    assert!(!state_root(s.profile.path()).join("history").exists());
}

#[test]
fn refuses_to_mutate_while_no_extension_is_attached() {
    let mut s = Server::start();
    let (status, _) = s.tool("bridge_status", json!({}));
    assert_eq!(status["extensionConnected"], false);

    let (msg, is_err) = s.tool("bookmarks_update", json!({"id": "11", "title": "x"}));
    assert!(is_err, "a mutation with no extension must fail");
    let text = msg.as_str().unwrap_or_default();
    assert!(text.contains("not connected"), "unexpected message: {text}");
}

#[test]
fn applies_a_change_and_snapshots_around_it() {
    let mut s = Server::start();
    let ext = Extension::attach(&s);

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

#[test]
fn skips_an_item_whose_value_moved_under_the_plan() {
    let mut s = Server::start();
    let ext = Extension::attach(&s);

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

#[test]
fn creating_the_same_item_twice_makes_one() {
    let mut s = Server::start();
    let ext = Extension::attach(&s);
    let before = ext.fake.lock().unwrap().nodes.len();

    let args = json!({"parentId": "10", "title": "Inbox"});
    let (first, _) = s.tool("bookmarks_create", args.clone());
    let (second, _) = s.tool("bookmarks_create", args);

    assert_eq!(second["deduped"], true, "a repeat create must not duplicate");
    assert_eq!(first["id"], second["id"]);
    assert_eq!(ext.fake.lock().unwrap().nodes.len(), before + 1);
}

#[test]
fn rejects_a_batch_too_large_for_one_service_worker_event() {
    let mut s = Server::start();
    let _ext = Extension::attach(&s);

    let ops: Vec<Value> = (0..201)
        .map(|i| json!({"op": "create", "parentId": "10", "title": format!("n{i}")}))
        .collect();
    let (msg, is_err) = s.tool("bookmarks_batch", json!({"ops": ops}));
    assert!(is_err, "an oversized batch must be refused, not half applied");
    assert!(msg.as_str().unwrap_or_default().contains("200"));
}

#[test]
fn restores_a_deleted_subtree_under_new_ids() {
    let mut s = Server::start();
    let ext = Extension::attach(&s);

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

#[test]
fn reports_the_users_own_changes_as_drift() {
    let mut s = Server::start();
    let ext = Extension::attach(&s);

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

#[test]
fn sessions_share_chrome_status_and_serialize_mutations() {
    let mut first = Server::start();
    let mut second = Server::start_in(first.profile.clone());
    first.status(false);
    second.status(false);

    let ext = Extension::attach(&first);
    first.status(true);
    second.status(true);
    let other = thread::spawn(move || {
        let (out, is_err) = second.tool("bookmarks_update", json!({"id": "12", "title": "Second"}));
        assert!(!is_err, "{out}");
        second
    });
    let (out, is_err) = first.tool("bookmarks_update", json!({"id": "11", "title": "First"}));
    assert!(!is_err, "{out}");
    let mut second = other.join().unwrap();
    {
        let fake = ext.fake.lock().unwrap();
        assert_eq!(fake.get("11").unwrap().1, "First");
        assert_eq!(fake.get("12").unwrap().1, "Second");
        assert_eq!(fake.calls, ["tree", "update", "tree", "tree", "update", "tree"]);
    }

    drop(ext);
    first.status(false);
    second.status(false);
}

#[test]
fn keeps_runtime_state_out_of_the_snapshots() {
    let mut s = Server::start();
    let legacy_history = legacy_state_root(s.profile.path()).join("history");
    std::fs::create_dir_all(&legacy_history).unwrap();
    let legacy_tree = legacy_history.join("tree.json");
    std::fs::write(&legacy_tree, b"legacy bookmarks").unwrap();
    let _ext = Extension::attach(&s);
    let (out, is_err) = s.tool("bookmarks_update", json!({"id": "11", "title": "Native"}));
    assert!(!is_err, "{out}");

    // A snapshot is the bookmark tree and nothing else. The lock file and the
    // dirty marker must not ride along, or an unchanged tree still commits.
    let history = state_root(s.profile.path()).join("history");
    let tracked = Command::new("git").arg("-C").arg(&history).arg("ls-files")
        .output().expect("git ls-files");
    let files: Vec<&str> = std::str::from_utf8(&tracked.stdout).unwrap()
        .lines().collect();
    assert_eq!(files, vec!["tree.json"], "snapshots carry runtime state: {files:?}");
    assert_eq!(std::fs::read(legacy_tree).unwrap(), b"legacy bookmarks");
}

#[test]
fn prepares_a_profile_without_exposing_tools_before_activation() {
    let mut s = Server::start();
    let (_process, mut stdin, messages) = start_host(s.profile.path());
    write_frame(&mut stdin, &json!({"type": "prepare", "profileId": PROFILE_ID}));
    let prepared = messages.recv_timeout(WAIT).expect("host preparation");
    assert_eq!(prepared["type"], "prepared", "{prepared}");
    s.status(false);
}

#[test]
fn reports_disconnected_when_the_host_lives_but_chrome_does_not_respond() {
    let mut s = Server::start();
    let ext = Extension::attach(&s);
    ext.fake.lock().unwrap().ignore_health = true;
    let (status, is_err) = s.tool("bridge_status", json!({}));
    assert!(!is_err, "{status}");
    assert_eq!(status["extensionConnected"], false, "{status}");
    assert_eq!(status["reason"], "extension_not_responding", "{status}");

    ext.fake.lock().unwrap().ignore_health = false;
    s.status(true);
}

#[test]
fn another_chrome_profile_cannot_replace_the_active_one() {
    let mut s = Server::start();
    let ext = Extension::attach(&s);
    let (_process, mut stdin, messages) = start_host(s.profile.path());
    let other_profile = "10112233445566778899aabbccddeeff";
    write_frame(&mut stdin, &json!({"type": "prepare", "profileId": other_profile}));
    let rejected = messages.recv_timeout(WAIT).expect("host rejection");
    assert_eq!(rejected["type"], "error", "{rejected}");
    assert_eq!(rejected["reason"], "profile_in_use", "{rejected}");
    s.status(true);
    let (out, is_err) = s.tool("bookmarks_update", json!({"id": "11", "title": "First profile"}));
    assert!(!is_err, "{out}");
    assert_eq!(ext.fake.lock().unwrap().get("11").unwrap().1, "First profile");
}

#[test]
fn large_unicode_request_crosses_native_frame_limit_and_applies_once() {
    let mut s = Server::start();
    let ext = Extension::attach(&s);
    let title = "문서 \"".repeat(160_000);
    assert!(serde_json::to_vec(&title).unwrap().len() > 1024 * 1024);

    let (out, is_err) = s.tool("bookmarks_update", json!({"id": "11", "title": title}));
    assert!(!is_err, "{out}");
    let fake = ext.fake.lock().unwrap();
    assert!(fake.received_chunks > 1);
    assert!(fake.get("11").unwrap().1 == title);
    assert_eq!(fake.calls, ["tree", "update", "tree"]);
    assert!(!state_root(s.profile.path()).join("agent-head-dirty").exists());
}

#[test]
fn an_interrupted_mutation_is_not_replayed_and_keeps_its_backup() {
    let mut s = Server::start();
    let ext = Extension::attach(&s);
    ext.fake.lock().unwrap().disconnect_after = Some("update");
    let (out, is_err) = s.tool("bookmarks_update", json!({"id": "11", "title": "Applied before loss"}));
    assert!(is_err && out.as_str().unwrap_or_default().contains("unknown"), "{out}");
    let fake = ext.fake.lock().unwrap();
    assert_eq!(fake.get("11").unwrap().1, "Applied before loss");
    assert_eq!(fake.calls, ["tree", "update"]);
    let state = state_root(s.profile.path());
    assert!(state.join("agent-head-dirty").is_file());
    let snapshot = Command::new("git").arg("-C").arg(state.join("history"))
        .args(["show", "HEAD:tree.json"]).output().unwrap();
    assert!(snapshot.status.success(), "{:?}", snapshot.stderr);
    let snapshot = String::from_utf8(snapshot.stdout).unwrap();
    assert!(snapshot.contains("Docs"));
    assert!(!snapshot.contains("Applied before loss"));
}
