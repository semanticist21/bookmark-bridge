use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::PathBuf;
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

use crate::{handle_tool, registration, select_profile, tools, Bridge};

pub const CHROME_INPUT_LIMIT: usize = 64 * 1024 * 1024;
const CHROME_OUTPUT_LIMIT: usize = 1024 * 1024;
const IPC_LIMIT: usize = 128 * 1024 * 1024;
const AUTH_LIMIT: usize = 1024;
const CHUNK_BYTES: usize = 128 * 1024;
const CONNECT_BUDGET: Duration = Duration::from_secs(2);
const NATIVE_STARTUP_BUDGET: Duration = Duration::from_secs(8);
type Secret = [u8; 32];
type AuthMac = Hmac<Sha256>;

#[derive(Deserialize, Serialize)]
struct Endpoint {
    protocol: u8,
    port: u16,
    secret: Secret,
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
    mac.update(b"bridgey-native-v2/");
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
    write_frame(socket, &json!({"protocol": 2, "nonce": client}), AUTH_LIMIT)
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

async fn authenticate_host(socket: &mut TcpStream, secret: &Secret) -> Result<(), String> {
    let hello = read_frame(socket, AUTH_LIMIT)
        .await
        .map_err(|e| e.to_string())?;
    if hello["protocol"] != 2 {
        return Err("Unsupported local protocol.".into());
    }
    let client = auth_bytes(&hello, "nonce")?;
    let host = random_secret()?;
    let response: Secret = proof(secret, b"host", &client, &host)
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
    proof(secret, b"client", &client, &host)
        .verify_slice(&auth_bytes(&reply, "proof")?)
        .map_err(|_| "MCP client authentication failed.")?;
    write_frame(socket, &json!({"authenticated": true}), AUTH_LIMIT)
        .await
        .map_err(|e| e.to_string())
}

fn offline_status(reason: &str) -> Value {
    json!({
        "extensionConnected": false, "pendingRequests": 0, "transport": "native", "reason": reason
    })
}

pub async fn proxy_tool(
    name: &str,
    args: Value,
    registration_error: Option<&str>,
) -> Result<Value, String> {
    let connection = async {
        let path = registration::root()?.join("endpoint.json");
        let bytes = fs::read(path).map_err(|e| e.to_string())?;
        let endpoint: Endpoint = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if endpoint.protocol != 2 {
            return Err("Unsupported Bridgey host protocol.".into());
        }
        let mut socket = TcpStream::connect(("127.0.0.1", endpoint.port))
            .await
            .map_err(|e| e.to_string())?;
        authenticate_client(&mut socket, &endpoint).await?;
        Ok::<_, String>(socket)
    };
    let mut socket = match timeout(CONNECT_BUDGET, connection).await {
        Ok(Ok(socket)) => socket,
        _ if name == "bridge_status" => {
            return Ok(offline_status(if registration_error.is_some() {
                "registration_failed"
            } else {
                "native_host_unavailable"
            }));
        }
        _ => return Err(match registration_error {
            Some(e) => format!("Native host registration failed: {e}"),
            None => "The Chrome extension is not connected in Native mode. Open Bridgey's toolbar popup and choose Switch to Native or Reconnect.".into(),
        }),
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
            Err(_) => return Ok(offline_status("native_host_not_responding")),
        }
    } else {
        response.await
    };
    let reply = match response {
        Ok(reply) => reply,
        Err(_) if name == "bridge_status" => return Ok(offline_status("native_host_disconnected")),
        Err(_) => return Err(
            "The Native host disconnected during the request. Whether a change was applied is unknown. Check history_drift before retrying.".into()
        ),
    };
    if let Some(error) = reply["error"].as_str() {
        return Err(error.into());
    }
    reply
        .get("result")
        .cloned()
        .ok_or("Invalid response from the Native host.".into())
}

async fn serve_client(
    mut socket: TcpStream,
    secret: &Secret,
    bridge: &Bridge,
    execution: &Mutex<()>,
) -> Result<(), String> {
    timeout(CONNECT_BUDGET, authenticate_host(&mut socket, secret))
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
    if prepare["type"] != "prepare"
        || profile.len() != 32
        || !profile
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return native_error(&mut output, "protocol_error").await;
    }
    let setup = async {
        let root = registration::root()?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("host.lock"))
            .map_err(|e| e.to_string())?;
        if !fs4::fs_std::FileExt::try_lock_exclusive(&lock).map_err(|e| e.to_string())? {
            return Ok::<_, String>(None);
        }
        let guard = HostLock {
            endpoint: root.join("endpoint.json"),
            _file: lock,
        };
        let _ = fs::remove_file(&guard.endpoint);
        select_profile(profile)?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let endpoint = Endpoint {
            protocol: 2,
            port,
            secret: random_secret()?,
        };
        Ok(Some((guard, listener, endpoint)))
    }
    .await;
    let (guard, listener, endpoint) = match setup {
        Ok(Some(setup)) => setup,
        Ok(None) => return native_error(&mut output, "profile_in_use").await,
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
    let serve = async {
        loop {
            let (socket, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let bridge = bridge.clone();
            let execution = execution.clone();
            let secret = endpoint.secret;
            tokio::spawn(async move {
                let _ = serve_client(socket, &secret, &bridge, &execution).await;
            });
        }
    };
    tokio::select! { result = read => result, result = write => result, result = serve => result }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_or_forged_endpoints_cannot_complete_authentication() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let endpoint = Endpoint {
            protocol: 2,
            port: listener.local_addr().unwrap().port(),
            secret: [1; 32],
        };
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            authenticate_host(&mut socket, &[2; 32]).await
        });
        let mut socket = TcpStream::connect(("127.0.0.1", endpoint.port))
            .await
            .unwrap();
        assert!(authenticate_client(&mut socket, &endpoint).await.is_err());
        drop(socket);
        assert!(server.await.unwrap().is_err());
    }
}
