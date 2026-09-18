//! Resident VM sessions: one long-lived execution context per language,
//! loaded once, called per event until evicted. Replaces one-shot execution
//! (fresh VM per call) — cross-call state survives inside the session.

use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::HostBridge;

/// One resident session for an actor instance. Loaded once (source in),
/// then each call addresses a handler by name with JSON args and gets a
/// JSON result. Session state (defined vars, loaded code) persists across
/// calls; eviction = drop.
pub trait ResidentSession: Send {
    /// Load the actor module (defines handlers / declarations).
    fn load(&mut self, source: &str) -> Result<()>;
    /// Invoke one handler by name with parsed JSON args.
    fn call(&mut self, handler: &str, args: &Value) -> Result<Value>;
}

/// Per-instance slot: the session plus its own lock. The lock is held only
/// for the duration of one call on THAT instance — other instances proceed
/// concurrently, and host functions that call back into other instances
/// (ctx_invoke) never contend on a global lock.
type Slot = Mutex<Box<dyn ResidentSession>>;

/// Session registry: per actor-instance key, one live session.
#[derive(Default, Clone)]
pub struct Sessions {
    map: Arc<Mutex<HashMap<String, Arc<Slot>>>>,
}

impl Sessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get or create the resident session for `key` and run `f` on it.
    ///
    /// Locking: the registry lock is taken only for map get/insert; the
    /// per-slot lock serializes calls to the SAME instance (session state
    /// is not thread-safe and calls to one instance must order anyway).
    /// `load` runs at cold start inside the slot lock; later calls reuse
    /// the loaded session.
    pub fn with_session(
        &self,
        key: &str,
        language: &str,
        source: &str,
        host: Option<&HostBridge>,
        sandbox: &crate::sandbox::SandboxPolicy,
        f: impl FnOnce(&mut dyn ResidentSession) -> Result<Value>,
    ) -> Result<Value> {
        let slot = {
            let mut map = self.map.lock().unwrap();
            match map.get(key) {
                Some(slot) => slot.clone(),
                None => {
                    // Cold start (or after eviction): spawn + load while
                    // holding the registry lock — spawn errors surface as
                    // errors, never panics, and the failed entry is not
                    // cached.
                    let mut session = spawn_session(language, host, sandbox)?;
                    session.load(source)?;
                    let slot = Arc::new(Mutex::new(session));
                    map.insert(key.to_string(), slot.clone());
                    slot
                }
            }
        };
        let mut session = slot.lock().unwrap();
        f(session.as_mut())
    }

    /// Evict (drop) the session — instance idle-expiry or hot replacement.
    /// Waits for an in-flight call on the slot to finish before dropping.
    pub fn evict(&self, key: &str) {
        self.map.lock().unwrap().remove(key);
    }
}

fn spawn_session(
    language: &str,
    host: Option<&HostBridge>,
    sandbox: &crate::sandbox::SandboxPolicy,
) -> Result<Box<dyn ResidentSession>> {
    Ok(match language {
        #[cfg(feature = "steel")]
        "steel" => Box::new(super::steel::SteelSession::new(host)),
        #[cfg(feature = "python")]
        "python" => Box::new(super::python::PythonSession::new(host)?),
        #[cfg(feature = "nushell")]
        "nushell" => Box::new(super::nushell::NushellResident::new(sandbox)?),
        other => anyhow::bail!("language not resident-carried by this probe build: {other}"),
    })
}

pub type DynSession = Box<dyn ResidentSession>;
