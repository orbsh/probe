//! The probe binary: load config, run the remote wrapper (Phase 3).

use probe_config::ProbeConfig;

fn main() -> anyhow::Result<()> {
    // Config path: argv[1] or ./probe.json. Single-node simplicity: no
    // discovery layer, the operator points the probe at its control plane.
    let path = std::env::args().nth(1).unwrap_or_else(|| "probe.json".into());
    let raw = std::fs::read_to_string(&path)?;
    let config: ProbeConfig = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("probe config {path}: {e}"))?;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(probe_runtime::remote::run(config))
}
