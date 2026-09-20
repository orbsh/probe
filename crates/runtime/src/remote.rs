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
use probe_protocol::{CodePayload, Frame, HostCall, HostFrame, HostOp, KvFrame, ToolCall, ToolResult};
use crate::carrier::session::Sessions;
use crate::carrier::HostBridge;
use crate::kv_executor::{KvRegistry, KvReply};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;

/// Serve one connection until it drops. Reconnect loop lives in `run`.
async fn serve_connection(
    ws_url: &str,
    credential: &str,
    config: &ProbeConfig,
    sessions: &Sessions,
    registry: &Arc<KvRegistry>,
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
                // One session task per call; host calls it makes run
                // concurrently over the same writer channel.
                tokio::spawn(async move {
                    let result = execute_call(&config, &sessions, &call, &tx, &pending_host).await;
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
            Frame::Kv(kv) => {
                let tx = tx.clone();
                let registry = Arc::clone(registry);
                // Engine work is blocking (one frame = one WAL commit):
                // spawn_blocking keeps the reader loop free, the same
                // isolation the tool-call path uses. No read/write fork —
                // every op takes this one path.
                tokio::spawn(async move {
                    // The dispatch closure takes ownership of the request; the
                    // outer copies serve only the panic fallback below.
                    let fallback = Frame::KvRefused {
                        executor: kv.executor.clone(),
                        kv_id: kv.kv_id.clone(),
                        reason: "executor dispatch panicked".into(),
                    };
                    let reply = tokio::task::spawn_blocking(move || {
                        // Refusal is answered, never dropped: the reason now
                        // travels on the wire (its own frame type), so it is
                        // not duplicated into a local log.
                        let outcome = registry.dispatch(&kv.executor, &kv.frame);
                        match outcome {
                            KvReply::Response(bytes) => Frame::Kv(KvFrame {
                                executor: kv.executor,
                                kv_id: kv.kv_id,
                                frame: bytes,
                            }),
                            KvReply::Refused(why) => Frame::KvRefused {
                                executor: kv.executor,
                                kv_id: kv.kv_id,
                                reason: why.to_string(),
                            },
                        }
                    })
                    .await;
                    // A panicked dispatch task leaves the sender waiting on a
                    // reply nobody sends — answer the refusal it deserved.
                    let _ = tx.send(reply.unwrap_or(fallback));
                });
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
) -> Result<serde_json::Value, String> {
    execute_call_inner(config, sessions, call, tx, pending_host)
        .await
        .map_err(|e| e.to_string())
}

async fn execute_call_inner(
    config: &ProbeConfig,
    sessions: &Sessions,
    call: &ToolCall,
    tx: &tokio::sync::mpsc::UnboundedSender<Frame>,
    pending_host: &Arc<PendingHost>,
) -> Result<serde_json::Value> {
    // Two payload forms (Phase 4): inline bytes ride the frame; link
    // payloads are fetched by content-hash URL — the URL is its own
    // invalidation policy, the hash verifies the bytes (zero cache: the
    // probe holds nothing between calls and initiates no fetch of its own
    // beyond the declared one).
    let bytes = match &call.code {
        CodePayload::Inline { bytes } => bytes.clone(),
        CodePayload::Link { url, expected_sha256, .. } => fetch_link(url, expected_sha256)?,
    };
    let source = String::from_utf8(bytes)
        .context("code payload is not valid UTF-8")?;

    // Session key: one resident VM per (node, tool). Args carry no
    // partition here — the control plane's actor model owns partitioning
    // and addresses this node as one actor per tool.
    // Host bridge: ctx ops ride Frame::Host over the same connection, each
    // with a unique host_call_id; the closure awaits the correlated reply.
    let bridge = build_host_bridge(call.call_id.clone(), &tx, pending_host);
    let key = format!("probe/{}/{}", config.capabilities.node_alias, call.tool);
    let sessions = sessions.clone();
    let language = call.language.clone();
    let handler = call.tool.clone();
    let args = call.args.clone();
    let node_alias = config.capabilities.node_alias.clone();
    let sandbox = sandbox_policy_for(config);
    tokio::task::spawn_blocking(move || {
        sessions.with_session(
            &format!("probe/{node_alias}/{handler}"),
            &language,
            &source,
            Some(&bridge),
            &sandbox,
            |s: &mut dyn crate::carrier::session::ResidentSession| {
                s.call(&handler, &args)
            },
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
    for name in ["ctx_state_get", "ctx_state_set", "ctx_state_delete", "ctx_invoke"] {
        let tx = tx.clone();
        let pending = pending.clone();
        let call_id = call_id.clone();
        let f: crate::carrier::HostFn = Arc::new(move |arg: serde_json::Value| {
            let (op_tx, op_rx) = tokio::sync::oneshot::channel();
            let host_call_id = format!("host-{}", std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?.as_nanos());
            let op = match name {
                "ctx_state_get" => {
                    // Steel passes a JSON string arg; python passes the
                    // decoded value. Normalize: field name from string or
                    // object {"field": ...}.
                    let field = match &arg {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Object(m) => m
                            .get("field")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        _ => arg.to_string(),
                    };
                    HostOp::StateGet { field }
                }
                "ctx_state_set" => {
                    let m = match &arg {
                        serde_json::Value::String(s) => serde_json::from_str::<serde_json::Value>(s)
                            .unwrap_or(serde_json::Value::Null),
                        other => other.clone(),
                    };
                    let field = m
                        .get("field")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let value = m.get("value").cloned().unwrap_or(serde_json::Value::Null);
                    HostOp::StateSet { field, value }
                }
                "ctx_state_delete" => {
                    let field = match &arg {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Object(m) => m
                            .get("field")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        _ => arg.to_string(),
                    };
                    HostOp::StateDelete { field }
                }
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



/// Fetch a Link payload: GET the URL, verify sha256 against the expected
/// hash (hex). The content-hash URL means a mismatch is either tampering
/// or a stale resolution — both are errors, never a silent accept.
fn fetch_link(url: &str, expected_sha256: &str) -> anyhow::Result<Vec<u8>> {
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
    // KV executors are opened once per process, before dialing: a
    // declaration that cannot be served is a startup failure, not a
    // per-frame surprise (the same fail-closed stance as the sandbox check).
    let registry = Arc::new(KvRegistry::open(&config.kv_executors)?);
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        match serve_connection(
            &config.control_plane_url,
            &credential,
            &config,
            &sessions,
            &registry,
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
