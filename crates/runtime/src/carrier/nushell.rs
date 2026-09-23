//! Nushell resident carrier: one nu REPL per actor instance in a PTY.
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
}

impl NushellResident {
    pub fn new(policy: &crate::sandbox::SandboxPolicy) -> Result<Self> {
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
        Ok(Self {
            session,
            module_path: dir.join("operation.nu"),
            dir,
        })
    }
}

impl ResidentSession for NushellResident {
    fn load(&mut self, source: &str) -> Result<()> {
        std::fs::write(&self.module_path, source)?;
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
