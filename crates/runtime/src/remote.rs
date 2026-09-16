//! The remote wrapper (Phase 3): outbound WS to the control plane, register
//! presence + capability, then serve pushed-down calls against the resident
//! carrier sessions. No inbound ports — the probe always dials out; the
//! connection carries both directions. Long-polling is the degraded form and
//! is NOT implemented here (the WS loop is the contract).

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use probe_config::ProbeConfig;
use probe_protocol::{CodePayload, Frame, ToolCall, ToolResult};
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
) -> Result<()> {
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

    // Task loop: calls arrive pushed down the connection. Same-node
    // seriality comes free from the per-instance slot locks in `Sessions`;
    // concurrent calls on distinct instances are not parallelized here (one
    // in-flight call per connection — the control plane opens more
    // connections for concurrency).
    while let Some(msg) = stream.next().await {
        let Message::Text(text) = msg? else { continue };
        let frame: Frame = serde_json::from_str(&text)?;
        let Frame::Call(call) = frame else {
            anyhow::bail!("unexpected frame from control plane: {frame:?}");
        };
        let result = execute_call(config, sessions, &call).await;
        let reply = Frame::Result(ToolResult {
            call_id: call.call_id.clone(),
            outcome: result,
        });
        sink.send(Message::Text(serde_json::to_string(&reply)?)).await?;
    }
    Ok(())
}

/// Execute one ToolCall against the resident sessions. Phase 4 adds the
/// `link` payload form; today only inline bytes are consumed (a `link`
/// payload is an explicit error, not a silent fetch — the probe initiates
/// no fetch of its own).
async fn execute_call(
    config: &ProbeConfig,
    sessions: &Sessions,
    call: &ToolCall,
) -> Result<serde_json::Value, String> {
    execute_call_inner(config, sessions, call)
        .await
        .map_err(|e| e.to_string())
}

async fn execute_call_inner(
    config: &ProbeConfig,
    sessions: &Sessions,
    call: &ToolCall,
) -> Result<serde_json::Value> {
    let CodePayload::Inline { bytes } = &call.code else {
        anyhow::bail!(
            "link code payloads arrive with Phase 4 (chunked WS delivery); got Link"
        );
    };
    let source = String::from_utf8(bytes.clone())
        .context("inline code is not valid UTF-8")?;

    // Session key: one resident VM per (node, tool). Args carry no
    // partition here — the control plane's actor model owns partitioning
    // and addresses this node as one actor per tool.
    let key = format!("probe/{}/{}", config.capabilities.node_alias, call.tool);
    // with_session is sync (CPU-bound VM work); keep it off the async
    // reactor with spawn_blocking. Nushell's PTY pump yields in poll(),
    // so it never starves the thread.
    let sessions = sessions.clone();
    let language = call.language.clone();
    let handler = call.tool.clone();
    let args = call.args.clone();
    let node_alias = config.capabilities.node_alias.clone();
    tokio::task::spawn_blocking(move || {
        sessions.with_session(
            &format!("probe/{node_alias}/{handler}"),
            &language,
            &source,
            None::<&HostBridge>,
            |s| s.call(&handler, &args),
        )
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("session task join: {e}")))
}

/// Top-level loop: dial, register, serve; reconnect with backoff on drop.
pub async fn run(config: ProbeConfig) -> Result<()> {
    let credential = std::env::var(&config.credential_env)
        .with_context(|| format!("credential env var {} not set", config.credential_env))?;
    let sessions = Sessions::new();
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        match serve_connection(&config.control_plane_url, &credential, &config, &sessions).await {
            Ok(()) => anyhow::bail!("control plane closed the connection cleanly"),
            Err(e) => eprintln!("connection lost: {e:#}; reconnecting in {:?}", backoff),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(std::time::Duration::from_secs(60));
    }
}
