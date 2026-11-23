use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;

use crate::{handle_tool, profile_state_root, registration, select_profile, tools, Bridge};

pub const CHROME_INPUT_LIMIT: usize = 64 * 1024 * 1024;
const CHROME_OUTPUT_LIMIT: usize = 1024 * 1024;
const IPC_LIMIT: usize = 128 * 1024 * 1024;
const AUTH_LIMIT: usize = 1024;
const CHUNK_BYTES: usize = 128 * 1024;
const CONNECT_BUDGET: Duration = Duration::from_secs(2);
const NATIVE_STARTUP_BUDGET: Duration = Duration::from_secs(8);
const IPC_PROTOCOL: u8 = 3;
type Secret = [u8; 32];
type AuthMac = Hmac<Sha256>;

#[derive(Deserialize, Serialize)]
struct Endpoint {
    protocol: u8,
    profile_id: String,
    port: u16,
    secret: Secret,
}

pub fn valid_profile_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn known_profiles() -> io::Result<Vec<String>> {
    // History directories survive an offline host and are already present on 3.0.
    let path = profile_state_root().map_err(io::Error::other)?;
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut profiles = Vec::new();
    for entry in entries {
        let entry = entry?;
        if let Ok(id) = entry.file_name().into_string() {
            if valid_profile_id(&id) {
                if !entry.file_type()?.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Bridgey profile state is not a directory.",
                    ));
                }
                profiles.push(id);
            }
        }
    }
    Ok(profiles)
}

fn random_secret() -> Result<Secret, String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes)
}

async fn read_frame(input: &mut (impl AsyncRead + Unpin), limit: usize) -> io::Result<Value> {
    let mut header = [0; 4];
    input.read_exact(&mut header).await?;
    let len = u32::from_ne_bytes(header) as usize;
    if len == 0 || len > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid message length.",
        ));
    }
    let mut bytes = vec![0; len];
    input.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn write_frame(
    output: &mut (impl AsyncWrite + Unpin),
    value: &Value,
    limit: usize,
) -> io::Result<()> {
    write_payload(output, &serde_json::to_vec(value)?, limit).await
}

async fn write_payload(
    output: &mut (impl AsyncWrite + Unpin),
    bytes: &[u8],
    limit: usize,
) -> io::Result<()> {
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Message exceeds the size limit.",
        ));
    }
    output
        .write_all(&(bytes.len() as u32).to_ne_bytes())
        .await?;
    output.write_all(bytes).await?;
    output.flush().await
}

fn proof(secret: &Secret, role: &[u8], client: &Secret, host: &Secret) -> AuthMac {
    let mut mac = AuthMac::new_from_slice(secret).expect("HMAC accepts a 32-byte key");
    mac.update(b"bridgey-native-v3/");
    mac.update(role);
    mac.update(client);
    mac.update(host);
    mac
}

fn auth_bytes(message: &Value, key: &str) -> Result<Secret, String> {
    serde_json::from_value(message[key].clone()).map_err(|_| "Invalid authentication frame.".into())
}

async fn authenticate_client(socket: &mut TcpStream, endpoint: &Endpoint) -> Result<(), String> {
    let client = random_secret()?;
    write_frame(
        socket,
        &json!({"protocol": IPC_PROTOCOL, "profileId": endpoint.profile_id, "nonce": client}),
        AUTH_LIMIT,
    )
    .await
    .map_err(|e| e.to_string())?;
    let reply = read_frame(socket, AUTH_LIMIT)
        .await
        .map_err(|e| e.to_string())?;
    let host = auth_bytes(&reply, "nonce")?;
    proof(&endpoint.secret, b"host", &client, &host)
        .verify_slice(&auth_bytes(&reply, "proof")?)
        .map_err(|_| "Native host authentication failed.")?;
    let response: Secret = proof(&endpoint.secret, b"client", &client, &host)
        .finalize()
        .into_bytes()
        .into();
    write_frame(socket, &json!({"proof": response}), AUTH_LIMIT)
        .await
        .map_err(|e| e.to_string())?;
    let ack = read_frame(socket, AUTH_LIMIT)
        .await
        .map_err(|e| e.to_string())?;
    if ack["authenticated"] != true {
        return Err("Native host rejected the MCP client.".into());
    }
    Ok(())
}

async fn authenticate_host(socket: &mut TcpStream, endpoint: &Endpoint) -> Result<(), String> {
    let hello = read_frame(socket, AUTH_LIMIT)
        .await
        .map_err(|e| e.to_string())?;
    if hello["protocol"] != IPC_PROTOCOL {
        return Err("Unsupported local protocol.".into());
    }
    if hello["profileId"] != endpoint.profile_id {
        return Err("MCP client selected a different Chrome profile.".into());
    }
    let client = auth_bytes(&hello, "nonce")?;
    let host = random_secret()?;
    let response: Secret = proof(&endpoint.secret, b"host", &client, &host)
        .finalize()
        .into_bytes()
        .into();
    write_frame(
        socket,
        &json!({"nonce": host, "proof": response}),
        AUTH_LIMIT,
    )
    .await
    .map_err(|e| e.to_string())?;
    let reply = read_frame(socket, AUTH_LIMIT)
        .await
        .map_err(|e| e.to_string())?;
    proof(&endpoint.secret, b"client", &client, &host)
        .verify_slice(&auth_bytes(&reply, "proof")?)
        .map_err(|_| "MCP client authentication failed.")?;
    write_frame(socket, &json!({"authenticated": true}), AUTH_LIMIT)
        .await
        .map_err(|e| e.to_string())
}

fn offline_status(reason: &str, profile: Option<&str>) -> Value {
    json!({
        "extensionConnected": false, "pendingRequests": 0, "transport": "native",
        "reason": reason, "profileId": profile
    })
}

fn offline(
    name: &str,
    profile: Option<&str>,
    reason: &str,
    error: impl Into<String>,
) -> Result<Value, String> {
    if name == "bridge_status" {
        Ok(offline_status(reason, profile))
    } else {
        Err(error.into())
    }
}

pub async fn proxy_tool(
    selected_profile: &mut Option<String>,
    name: &str,
    args: Value,
    registration_error: Option<&str>,
) -> Result<Value, String> {
    let unavailable = |profile| {
        match registration_error {
        Some(e) => offline(name, profile, "registration_failed", format!("Native host registration failed: {e}")),
        None => offline(
            name, profile, "native_host_unavailable",
            "The Chrome extension is not connected in Native mode. Start the updated MCP server and choose Reconnect in the selected Chrome profile.",
        ),
    }
    };
    if selected_profile.is_none() {
        match known_profiles().as_deref() {
            Ok([id]) => *selected_profile = Some(id.to_owned()),
            Ok([]) => return unavailable(None),
            Ok(_) => return offline(
                name, None, "profile_selection_required",
                "Bridgey found multiple Chrome profiles. Use --profile <id> in your MCP server arguments. Copy each ID from How to set up in the Bridgey popup.",
            ),
            Err(e) => return offline(
                name, None, "profile_discovery_failed",
                format!("Could not read Bridgey's Chrome profiles: {e}"),
            ),
        }
    }
    let profile = selected_profile.as_deref().unwrap();
    let connection = async {
        let path = registration::root()?
            .join("profiles")
            .join(profile)
            .join("endpoint.json");
        let bytes = fs::read(path).map_err(|e| e.to_string())?;
        let endpoint: Endpoint = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if endpoint.protocol != IPC_PROTOCOL || endpoint.profile_id != profile {
            return Err("Invalid Bridgey endpoint for the selected Chrome profile.".into());
        }
        let mut socket = TcpStream::connect(("127.0.0.1", endpoint.port))
            .await
            .map_err(|e| e.to_string())?;
        authenticate_client(&mut socket, &endpoint).await?;
        Ok::<_, String>(socket)
    };
    let mut socket = match timeout(CONNECT_BUDGET, connection).await {
        Ok(Ok(socket)) => socket,
        _ => return unavailable(Some(profile)),
    };
    let request =
        serde_json::to_vec(&json!({"name": name, "args": args})).map_err(|e| e.to_string())?;
    if request.len() > IPC_LIMIT {
        return Err("The tool request exceeds Bridgey's local message limit.".into());
    }
    let response = async {
        write_payload(&mut socket, &request, IPC_LIMIT).await?;
        read_frame(&mut socket, IPC_LIMIT).await
    };
    let response = if name == "bridge_status" {
        match timeout(Duration::from_secs(3), response).await {
            Ok(response) => response,
            Err(_) => return Ok(offline_status("native_host_not_responding", Some(profile))),
        }
    } else {
        response.await
    };
    let reply = match response {
        Ok(reply) => reply,
        Err(_) if name == "bridge_status" => {
            return Ok(offline_status("native_host_disconnected", Some(profile)))
        }
        Err(_) => return Err(
            "The Native host disconnected during the request. Whether a change was applied is unknown. Check history_drift before retrying.".into()
        ),
    };
    if let Some(error) = reply["error"].as_str() {
        return Err(error.into());
    }
    let mut result = reply
        .get("result")
        .cloned()
        .ok_or("Invalid response from the Native host.")?;
    if name == "bridge_status" {
        result
            .as_object_mut()
            .ok_or("Invalid status response from the Native host.")?
            .insert("profileId".into(), json!(profile));
    }
    Ok(result)
}

async fn serve_client(
    mut socket: TcpStream,
    endpoint: &Endpoint,
    bridge: &Bridge,
    execution: &Mutex<()>,
) -> Result<(), String> {
    timeout(CONNECT_BUDGET, authenticate_host(&mut socket, endpoint))
        .await
        .map_err(|e| e.to_string())??;
    let request = timeout(CONNECT_BUDGET, read_frame(&mut socket, IPC_LIMIT))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let name = request["name"].as_str().unwrap_or_default();
    let result = if !tools()
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == name)
    {
        Err("Unknown tool.".into())
    } else if name == "bridge_status" {
        handle_tool(bridge, name, &request["args"]).await
    } else {
        // A proxy sends exactly one request and keeps the socket open for its reply.
        let mut extra = [0u8; 1];
        let _guard = tokio::select! {
            biased;
            _ = socket.read(&mut extra) => return Ok(()),
            guard = execution.lock() => guard,
        };
        handle_tool(bridge, name, &request["args"]).await
    };
    let response = match result {
        Ok(result) => json!({"result": result}),
        Err(error) => json!({"error": error}),
    };
    write_frame(&mut socket, &response, IPC_LIMIT)
        .await
        .map_err(|e| e.to_string())
}

async fn write_request(
    output: &mut (impl AsyncWrite + Unpin),
    id: u64,
    request: &str,
) -> io::Result<()> {
    const PREFIX: &[u8] = b"{\"type\":\"request\",\"request\":";
    if PREFIX.len() + request.len() + 1 <= CHROME_OUTPUT_LIMIT {
        let mut bytes = Vec::with_capacity(PREFIX.len() + request.len() + 1);
        bytes.extend_from_slice(PREFIX);
        bytes.extend_from_slice(request.as_bytes());
        bytes.push(b'}');
        return write_payload(output, &bytes, CHROME_OUTPUT_LIMIT).await;
    }
    let mut chunks = Vec::new();
    let mut rest = request;
    while !rest.is_empty() {
        let mut end = rest.len().min(CHUNK_BYTES);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (chunk, next) = rest.split_at(end);
        chunks.push(chunk);
        rest = next;
    }
    for (index, data) in chunks.iter().enumerate() {
        write_frame(output, &json!({
            "type": "requestChunk", "id": id, "index": index, "total": chunks.len(), "data": data,
        }), CHROME_OUTPUT_LIMIT).await?;
    }
    Ok(())
}

struct HostLock {
    endpoint: PathBuf,
    _file: File,
    _legacy_lock: File,
}

fn open_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
}

impl Drop for HostLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.endpoint);
    }
}

async fn native_error(output: &mut (impl AsyncWrite + Unpin), reason: &str) -> Result<(), String> {
    write_frame(
        output,
        &json!({"type": "error", "reason": reason}),
        CHROME_OUTPUT_LIMIT,
    )
    .await
    .map_err(|e| e.to_string())
}

pub async fn run_host() -> Result<(), String> {
    let mut input = BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    let prepare = timeout(NATIVE_STARTUP_BUDGET, read_frame(&mut input, AUTH_LIMIT))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let profile = prepare["profileId"].as_str().unwrap_or_default();
    if prepare["type"] != "prepare" || !valid_profile_id(profile) {
        return native_error(&mut output, "protocol_error").await;
    }
    let setup = async {
        let root = registration::root()?;
        // A 3.0 host takes this lock exclusively and can still use the same history.
        let legacy_lock = open_lock(&root.join("host.lock")).map_err(|e| e.to_string())?;
        if !fs4::fs_std::FileExt::try_lock_shared(&legacy_lock).map_err(|e| e.to_string())? {
            return Ok::<_, String>(Err("host_upgrade_required"));
        }
        let _ = fs::remove_file(root.join("endpoint.json"));
        let runtime = root.join("profiles").join(profile);
        fs::create_dir_all(&runtime).map_err(|e| e.to_string())?;
        let lock = open_lock(&runtime.join("host.lock")).map_err(|e| e.to_string())?;
        if !fs4::fs_std::FileExt::try_lock_exclusive(&lock).map_err(|e| e.to_string())? {
            return Ok(Err("profile_id_in_use"));
        }
        let guard = HostLock {
            endpoint: runtime.join("endpoint.json"),
            _file: lock,
            _legacy_lock: legacy_lock,
        };
        let _ = fs::remove_file(&guard.endpoint);
        select_profile(profile)?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let endpoint = Endpoint {
            protocol: IPC_PROTOCOL,
            profile_id: profile.into(),
            port,
            secret: random_secret()?,
        };
        Ok(Ok((guard, listener, endpoint)))
    }
    .await;
    let (guard, listener, endpoint) = match setup {
        Ok(Ok(setup)) => setup,
        Ok(Err(reason)) => return native_error(&mut output, reason).await,
        Err(e) => {
            native_error(&mut output, "host_unavailable").await?;
            return Err(e);
        }
    };
    write_frame(
        &mut output,
        &json!({"type": "prepared"}),
        CHROME_OUTPUT_LIMIT,
    )
    .await
    .map_err(|e| e.to_string())?;
    let activate = timeout(NATIVE_STARTUP_BUDGET, read_frame(&mut input, AUTH_LIMIT))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    if activate["type"] != "activate" {
        return native_error(&mut output, "protocol_error").await;
    }
    if let Err(e) = registration::write_private(
        &guard.endpoint,
        &serde_json::to_vec(&endpoint).map_err(|e| e.to_string())?,
    ) {
        native_error(&mut output, "host_unavailable").await?;
        return Err(e);
    }
    write_frame(&mut output, &json!({"type": "ready"}), CHROME_OUTPUT_LIMIT)
        .await
        .map_err(|e| e.to_string())?;

    let (outbox, mut outgoing) = mpsc::unbounded_channel();
    let bridge = std::sync::Arc::new(Bridge {
        pending: Mutex::new(Default::default()),
        outbox,
        next_id: AtomicU64::new(1),
    });
    let execution = std::sync::Arc::new(Mutex::new(()));
    let read = async {
        loop {
            let mut reply = match read_frame(&mut input, CHROME_INPUT_LIMIT).await {
                Ok(reply) => reply,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e.to_string()),
            };
            if let Some(id) = reply["id"].as_u64() {
                if reply.get("result").is_some() == reply["error"].is_string() {
                    reply = json!({"error": "Malformed extension response. Check history_drift before retrying a change."});
                }
                if let Some(call) = bridge.pending.lock().await.remove(&id) {
                    let _ = call.sender.send(reply);
                }
            }
        }
    };
    let write = async {
        while let Some((id, request)) = outgoing.recv().await {
            write_request(&mut output, id, &request)
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    };
    let endpoint = std::sync::Arc::new(endpoint);
    let serve = async {
        loop {
            let (socket, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let bridge = bridge.clone();
            let execution = execution.clone();
            let endpoint = endpoint.clone();
            tokio::spawn(async move {
                let _ = serve_client(socket, &endpoint, &bridge, &execution).await;
            });
        }
    };
    tokio::select! { result = read => result, result = write => result, result = serve => result }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_endpoints_cannot_complete_authentication() {
        let profile = "0123456789abcdef0123456789abcdef";
        for (client_profile, client_secret) in [
            (profile, [2; 32]),
            ("fedcba9876543210fedcba9876543210", [1; 32]),
        ] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let endpoint = Endpoint {
                protocol: IPC_PROTOCOL,
                profile_id: client_profile.into(),
                port: listener.local_addr().unwrap().port(),
                secret: client_secret,
            };
            let host = Endpoint {
                secret: [1; 32],
                profile_id: profile.into(),
                ..endpoint
            };
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                authenticate_host(&mut socket, &host).await
            });
            let mut socket = TcpStream::connect(("127.0.0.1", endpoint.port))
                .await
                .unwrap();
            assert!(authenticate_client(&mut socket, &endpoint).await.is_err());
            drop(socket);
            assert!(server.await.unwrap().is_err());
        }
    }
}
