//! Nushell resident carrier: one nu REPL per booth instance in a PTY.
//! Wraps the low-level `NushellSession` (openpty/fork, reedline CPR
//! answering, file-based result protocol). The old one-shot subprocess
//! path is gone — resident sessions ARE the nushell carrier.

use super::session::ResidentSession;
use super::nushell_session::NushellSession;
use anyhow::Result;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Resident nu session: spawned eagerly (REPL up), script loaded on
/// `load()`, handlers addressed by name on `call()`. Env-var state
/// (`$env.*` written via `def --env` handlers) persists across calls.
pub struct NushellResident {
    session: NushellSession,
    module_path: std::path::PathBuf,
    dir: std::path::PathBuf,
    /// The ctx bridge materialized as nu commands (`bridge.nu`), loaded
    /// before the booth module so handlers resolve them.
    bridge: Option<std::sync::Arc<super::HostBridge>>,
    bridge_path: std::path::PathBuf,
}

impl NushellResident {
    pub fn new(policy: &crate::sandbox::SandboxPolicy, host: Option<std::sync::Arc<super::HostBridge>>) -> Result<Self> {
        // With a Bubblewrap policy the session dir MUST live inside the
        // sandbox's writable view: /tmp is tmpfs-mounted (files written
        // outside vanish), so the per-session dir goes under the policy's
        // cwd (bind-mounted by the wrapper). Unwrapped = temp dir.
        let (session, dir) = match policy {
            crate::sandbox::SandboxPolicy::None => {
                let session = NushellSession::spawn(policy)?;
                let id = SEQ.fetch_add(1, Ordering::Relaxed);
                let dir = std::env::temp_dir()
                    .join(format!("probe-nu-res-{}-{}", std::process::id(), id));
                std::fs::create_dir_all(&dir)?;
                (session, dir)
            }
            crate::sandbox::SandboxPolicy::Bubblewrap { cwd, .. } => {
                let id = SEQ.fetch_add(1, Ordering::Relaxed);
                let dir = std::path::PathBuf::from(cwd)
                    .join(format!("probe-nu-res-{}-{}", std::process::id(), id));
                std::fs::create_dir_all(&dir)?;
                let session = NushellSession::spawn(policy)?;
                (session, dir)
            }
        };
        let bridge = host;
        let mut session = session;
        if let Some(b) = &bridge {
            session.set_bridge(b.clone(), dir.clone());
        }
        Ok(Self {
            session,
            module_path: dir.join("operation.nu"),
            bridge_path: dir.join("bridge.nu"),
            dir,
            bridge,
        })
    }

    /// Materialize the ctx bridge as nu custom commands: each host fn
    /// becomes `ctx-<name>` (nu forbids dots in command names). The nu
    /// side writes a request file, polls for the response, and returns
    /// the value — the Rust poll loop (inside `call`) answers requests
    /// via `sweep_bridge_requests`. One writer per session (the slot
    /// lock serializes calls), so the fixed seq keeps req/resp paired.
    fn materialize_bridge(&self) -> Result<()> {
        let Some(bridge) = &self.bridge else { return Ok(()) };
        let dir = &self.dir;
        let mut script = String::from(
            "# ctx bridge: host functions as file-round-trip commands\n",
        );
        for name in bridge.functions.keys() {
            let cmd = name.replace(['.', '_'], "-"); // nu forbids dots; underscores → dashes for the conventional ctx_* names
            script.push_str(&format!(
                r#"export def "{cmd}" [args] {{
    let req = {{ fn: "{name}", arg: $args }}
    let seq = (random uuid)
    let req_path = '{dir}/req-' + $seq + '.json'
    let resp_path = '{dir}/resp-' + $seq + '.json'
    $req | to json --raw | save --force $req_path
    mut waited = 0
    while (not ($resp_path | path exists)) and ($waited < 300) {{
        sleep 100ms
        $waited = $waited + 1
    }}
    if (not ($resp_path | path exists)) {{
        error make {{ msg: "ctx bridge timeout for {name}" }}
    }}
    let resp = (open $resp_path)
    rm $resp_path
    if ($resp.__error? != null) {{
        error make {{ msg: $resp.__error }}
    }}
    $resp.value
}}
"#,
                dir = dir.display(),
                name = name,
                cmd = cmd,
            ));
        }
        std::fs::write(&self.bridge_path, script)?;
        Ok(())
    }
}

impl ResidentSession for NushellResident {
    fn load(&mut self, source: &str) -> Result<()> {
        std::fs::write(&self.module_path, source)?;
        // Bridge first: handlers reference ctx-* commands.
        if self.bridge.is_some() {
            self.materialize_bridge()?;
            self.session.load(self.bridge_path.to_str().unwrap())?;
        }
        self.session.load(self.module_path.to_str().unwrap())
    }

    fn call(&mut self, handler: &str, args: &Value) -> Result<Value> {
        self.session.call(handler, args)
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl Drop for NushellResident {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
