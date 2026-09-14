//! Probe registration and capability configuration.
//!
//! Registration credential = user credential (outbound connection carries
//! "whose machine am I"). The capability list is written into the user's
//! namespace registry by the control plane; this file is the Probe-side
//! declaration of the same surface.

use serde::{Deserialize, Serialize};

/// Per-probe capability surface: the operations this node accepts.
///
/// This is the ONLY skill-level containment layer (no per-skill container
/// boundary beneath it), so the corresponding check must be enforced at
/// execution, not just honored at dispatch-declaration. No credential fields
/// by design: skills never touch credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CapabilitySurface {
    /// Filesystem paths the skill may read/write. Prefix-scoped; empty = no fs.
    pub fs_scope: Vec<String>,
    /// Whether the skill may execute host commands.
    pub command_exec: bool,
    /// Network egress policy for skill execution.
    pub network: NetworkPolicy,
    /// Optional host alias used in target resolution
    /// (`probe:<node_alias>:<capability>`).
    pub node_alias: String,
}

impl Default for CapabilitySurface {
    fn default() -> Self {
        Self {
            fs_scope: Vec::new(),
            command_exec: false,
            network: NetworkPolicy::None,
            node_alias: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    /// No outbound network from skills.
    #[default]
    None,
    /// Explicit allowlist of host:port targets.
    Allow(Vec<String>),
    /// Unrestricted (single-user nodes where the user accepted the trade).
    Open,
}

/// Probe startup configuration (Phase 3): where to register and what to claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeConfig {
    /// Control plane endpoint for the outbound registration connection
    /// (WS preferred; long-polling is the degraded implementation).
    pub control_plane_url: String,
    /// User credential used as the registration credential. Provided by the
    /// environment (single source of truth outside config files).
    pub credential_env: String,
    pub capabilities: CapabilitySurface,
}
