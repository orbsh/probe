//! The remote wrapper (Phase 3): outbound WS to the control plane, register
//! presence + capability, then serve pushed-down calls against the resident
//! carrier sessions. No inbound ports — the probe always dials out; the
//! connection carries both directions. No degraded fallback exists by
//! design: long-polling was explicitly rejected (WS-only is the contract —
//! the control determines compromise, not the industry's conservative
//! defaults).

use anyhow::{Context, Result};
use std::io::Read as _;
use futures_util::{SinkExt, StreamExt};
use probe_config::ProbeConfig;
use probe_protocol::{CodeRef, Frame, HostCall, HostFrame, HostOp, ToolCall, ToolResult};
use std::sync::Mutex as StdMutex;
use crate::carrier::session::Sessions;
use crate::carrier::HostBridge;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;

/// Serve one connection until it drops. Reconnect loop lives in `run`.
async fn serve_connection(
    ws_url: &str,
    credential: &str,
    config: &ProbeConfig,
    sessions: &Sessions,
    code_cache: &Arc<CodeCache>,
) -> Result<()> {
    let pending_host: Arc<PendingHost> = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let (ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .with_context(|| format!("dial control plane {ws_url}"))?;
    let (mut sink, mut stream) = ws.split();

    // Registration: presence + capability list (carried languages). The
    // control plane writes these into the user's namespace registry.
    let register = Frame::Register {
        node_alias: config.capabilities.node_alias.clone(),
        credential: credential.to_string(),
        carriers: config.capabilities.carriers.clone(),
    };
    sink.send(Message::Text(serde_json::to_string(&register)?))
        .await?;

    // Await acceptance before serving calls.
    match stream.next().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<Frame>(&text)? {
            Frame::Registered => {}
            other => anyhow::bail!("expected Registered, got {other:?}"),
        },
        Some(Err(e)) => return Err(e.into()),
        _ => anyhow::bail!("control plane closed before registration"),
    }

    // Bidirectional loop: a writer task drains an mpsc into the sink; the
    // reader dispatches incoming frames. Host calls (ctx bridge) flow
    // probe->control-plane while a ToolCall is executing — the two task
    // structure makes the connection full-duplex at the frame level.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Frame>();
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if sink.send(Message::Text(serde_json::to_string(&frame)?)).await.is_err() {
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    });

    while let Some(msg) = stream.next().await {
        let Message::Text(text) = msg? else { continue };
        let frame: Frame = serde_json::from_str(&text)?;
        match frame {
            Frame::Call(call) => {
                let tx = tx.clone();
                let pending_host = pending_host.clone();
                let config = config.clone();
                let sessions = sessions.clone();
                let code_cache = code_cache.clone();
                // One session task per call; host calls it makes run
                // concurrently over the same writer channel.
                tokio::spawn(async move {
                    let result =
                        execute_call(&config, &sessions, &call, &tx, &pending_host, &code_cache)
                            .await;
                    let _ = tx.send(Frame::Result(ToolResult {
                        call_id: call.call_id.clone(),
                        outcome: result,
                    }));
                });
            }
            Frame::Host(HostFrame::Result(hr)) => {
                let mut pending = pending_host.lock().unwrap();
                if let Some(tx) = pending.remove(&hr.host_call_id) {
                    let _ = tx.send(hr.outcome);
                }
                // Unknown id: the caller timed out — drop the late result.
            }
            other => anyhow::bail!("unexpected frame from control plane: {other:?}"),
        }
    }
    writer.abort();
    Ok(())
}

/// Pending host calls: host_call_id -> reply path.
type PendingHost = std::sync::Mutex<
    std::collections::HashMap<String, tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>>,
>;

/// Execute one ToolCall against the resident sessions. Phase 4 adds the
/// `link` payload form; today only inline bytes are consumed (a `link`
/// payload is an explicit error, not a silent fetch — the probe initiates
/// no fetch of its own).
async fn execute_call(
    config: &ProbeConfig,
    sessions: &Sessions,
    call: &ToolCall,
    tx: &tokio::sync::mpsc::UnboundedSender<Frame>,
    pending_host: &Arc<PendingHost>,
    code_cache: &Arc<CodeCache>,
) -> Result<serde_json::Value, String> {
    execute_call_inner(config, sessions, call, tx, pending_host, code_cache)
        .await
        .map_err(|e| e.to_string())
}

async fn execute_call_inner(
    config: &ProbeConfig,
    sessions: &Sessions,
    call: &ToolCall,
    tx: &tokio::sync::mpsc::UnboundedSender<Frame>,
    pending_host: &Arc<PendingHost>,
    code_cache: &Arc<CodeCache>,
) -> Result<serde_json::Value> {
    // Content-addressed code (ADR-0027): the frame carries a reference,
    // bytes ride the data path. Resolve = per-hash cache hit, else fetch +
    // verify (mismatch = error, never silent). The cache is a discardable
    // hot layer — same legitimacy tier as the resident session; nothing
    // in it would need to be recovered.
    let source = code_cache.resolve(&call.code)?;

    // Args carry no partition here — the control plane's actor model owns
    // partitioning; the probe only keys residency by the caller's session
    // identity (below).
    // Host bridge: ctx ops ride Frame::Host over the same connection, each
    // with a unique host_call_id; the closure awaits the correlated reply.
    let bridge = build_host_bridge(call.call_id.clone(), &tx, pending_host);
    // Residency: the caller's session identity keys the resident runtime —
    // calls sharing a `session` share VM/module state, different ones never
    // do. The node alias in the key is only for readability (the registry is
    // process-local). `entry` names the point the delivered code exposes.
    let key = format!("probe/{}/{}", config.capabilities.node_alias, call.session);
    let sessions = sessions.clone();
    let language = call.language.clone();
    let entry = call.entry.clone();
    let args = call.args.clone();
    let sandbox = sandbox_policy_for(config);
    tokio::task::spawn_blocking(move || {
        sessions.with_session(
            &key,
            &language,
            &source,
            Some(&bridge),
            &sandbox,
            |s: &mut dyn crate::carrier::session::ResidentSession| s.call(&entry, &args),
        )
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("session task join: {e}")))
}

/// Build a HostBridge whose functions send Frame::Host(Call) over the
/// connection and await the correlated Frame::Host(Result). One JSON arg
/// in, JSON value out — the same marshal contract as in-process bridges.
fn build_host_bridge(
    call_id: String,
    tx: &tokio::sync::mpsc::UnboundedSender<Frame>,
    pending_host: &Arc<PendingHost>,
) -> HostBridge {
    let tx = tx.clone();
    let pending = pending_host.clone();
    let mut bridge = HostBridge::default();
    for name in ["ctx_invoke"] {
        let tx = tx.clone();
        let pending = pending.clone();
        let call_id = call_id.clone();
        let f: crate::carrier::HostFn = Arc::new(move |arg: serde_json::Value| {
            let (op_tx, op_rx) = tokio::sync::oneshot::channel();
            let host_call_id = format!("host-{}", std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?.as_nanos());
            let op = match name {
                "ctx_invoke" => {
                    let m = match &arg {
                        serde_json::Value::String(s) => serde_json::from_str::<serde_json::Value>(s)
                            .unwrap_or(serde_json::Value::Null),
                        other => other.clone(),
                    };
                    let gs = |k: &str| m.get(k).cloned().unwrap_or(serde_json::Value::Null);
                    let gs_s = |k: &str| gs(k).as_str().unwrap_or_default().to_string();
                    HostOp::Invoke {
                        target_type: gs_s("type"),
                        target_key: gs_s("key"),
                        handler: gs_s("handler"),
                        args: gs("args"),
                    }
                }
                _ => unreachable!(),
            };
            pending
                .lock()
                .unwrap()
                .insert(host_call_id.clone(), op_tx);
            tx.send(Frame::Host(HostFrame::Call(HostCall {
                host_call_id: host_call_id.clone(),
                call_id: call_id.clone(),
                op,
            })))
            .map_err(|_| anyhow::anyhow!("connection closed"))?;
            // Block on the reply — host fns are synchronous from the
            // script's perspective; spawn_blocking isolation makes the
            // await of the oneshot's blocking recv safe.
            match op_rx.blocking_recv() {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(e)) => Err(anyhow::anyhow!("{e}")),
                Err(_) => Err(anyhow::anyhow!("host call dropped")),
            }
        });
        bridge.functions.insert(name.to_string(), f);
    }
    bridge
}



/// Per-hash code cache (ADR-0027): resolved bytes keyed by the asserted
/// hash. Content addressing makes eviction semantics trivial — a stale
/// entry is unreachable waste (new code is a new hash, a new key), so the
/// cache grows with distinct code, never with call count. Survives
/// reconnection (lives at `run` scope); a process start begins empty.
pub struct CodeCache {
    entries: StdMutex<std::collections::HashMap<String, Arc<str>>>,
}

impl CodeCache {
    pub fn new() -> Self {
        Self { entries: StdMutex::new(std::collections::HashMap::new()) }
    }

    /// Resolve a reference to source text: cache hit, or fetch + verify +
    /// insert. A hash mismatch is tampering or a stale resolution — an
    /// error, never a silent accept.
    pub fn resolve(&self, code: &CodeRef) -> anyhow::Result<String> {
        if let Some(hit) = self.entries.lock().unwrap().get(&code.sha256) {
            return Ok(hit.to_string());
        }
        let bytes = fetch_verified(&code.url, &code.sha256)?;
        let source = String::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("fetched code is not valid UTF-8"))?;
        self.entries
            .lock()
            .unwrap()
            .insert(code.sha256.clone(), Arc::from(source.as_str()));
        Ok(source)
    }
}

impl Default for CodeCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Fetch by reference: GET the URL, verify sha256 against the expected
/// hash (hex, asserted by the frame — never parsed from the URL).
fn fetch_verified(url: &str, expected_sha256: &str) -> anyhow::Result<Vec<u8>> {
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(30))
        .call()
        .map_err(|e| anyhow::anyhow!("link fetch {url}: {e}"))?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| anyhow::anyhow!("link read {url}: {e}"))?;
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(&bytes);
    let got = hex::encode(hasher.finalize());
    if got != expected_sha256.to_lowercase() {
        anyhow::bail!(
            "link hash mismatch for {url}: expected {expected_sha256}, got {got}"
        );
    }
    Ok(bytes)
}

/// Top-level loop: dial, register, serve; reconnect with backoff on drop.
pub async fn run(config: ProbeConfig) -> Result<()> {
    let credential = std::env::var(&config.credential_env)
        .with_context(|| format!("credential env var {} not set", config.credential_env))?;
    let sessions = Sessions::new();
    let code_cache = Arc::new(CodeCache::new());
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        match serve_connection(
            &config.control_plane_url,
            &credential,
            &config,
            &sessions,
            &code_cache,
        )
        .await
        {
            Ok(()) => anyhow::bail!("control plane closed the connection cleanly"),
            Err(e) => eprintln!("connection lost: {e:#}; reconnecting in {:?}", backoff),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(std::time::Duration::from_secs(60));
    }
}fn sandbox_policy_for(config: &ProbeConfig) -> crate::sandbox::SandboxPolicy {
    use crate::sandbox::SandboxPolicy;
    SandboxPolicy::Bubblewrap {
        allow_write: config.capabilities.fs_scope.clone(),
        deny_read: vec![],
        cwd: std::env::temp_dir().display().to_string(),
        allowed_domains: vec![],
    }
}
