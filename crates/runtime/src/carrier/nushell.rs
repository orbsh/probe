//! Nushell carrier: subprocess execution, CGI-shaped.
//!
//! Data contract is identical to the in-process carriers: JSON args in,
//! JSON result out. The operation source is materialized as a module file;
//! a generated wrapper imports it, reads the JSON from stdin (`$in`),
//! calls the declared entry function, and serializes the result back.
//! Nushell absent from PATH = clear error. No bash anywhere: nu runs the
//! wrapper directly.
//!
//! Nu conventions for operations:
//! - the entry function is exported by name (`export def execute [args]`);
//!   a bare `main` is not addressable through module import, so a `main`
//!   entry is rejected with a message telling the op to name its function
//! - args arrive as one parsed value (record/list/...), not a string
//! - the return value is structured; `to json --raw` serializes it

use super::{ExecRequest, ExecResult};
use serde_json::Value;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinguish concurrent executions in the temp dir.
static SEQ: AtomicU64 = AtomicU64::new(0);

pub fn execute(req: ExecRequest) -> ExecResult {
    let entry = req.entry.unwrap_or("execute");
    if entry == "main" {
        anyhow::bail!(
            "nushell operations must export a named function (e.g. `execute`); \
             `main` is not addressable through module import"
        );
    }

    let which = Command::new("nu").arg("--version").output();
    if which.is_err() {
        anyhow::bail!("nushell not found in PATH: the node does not carry the nu carrier");
    }

    let id = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("probe-op-{id}-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let module_path = dir.join("operation.nu");

    // Cleanup on all exits.
    struct Cleanup(std::path::PathBuf);
    impl std::ops::Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _guard = Cleanup(dir.clone());

    std::fs::write(&module_path, req.source)?;

    let wrapper = format!(
        "use '{}' *\n\ndef __probe_wrap [args] {{\n  {entry} $args | to json --raw\n}}\n__probe_wrap ($in | from json)\n",
        module_path.display()
    );
    let wrapper_path = dir.join("wrapper.nu");
    std::fs::write(&wrapper_path, wrapper)?;

    let args_json = serde_json::to_string(req.args)?;
    let out = Command::new("nu")
        .arg("--stdin")
        .arg(&wrapper_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("nushell spawn: {e}"))
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .expect("stdin piped")
                .write_all(args_json.as_bytes())?;
            child.wait_with_output().map_err(Into::into)
        })?;

    if !out.status.success() {
        anyhow::bail!(
            "nushell operation failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim())
        .map_err(|e| anyhow::anyhow!("nushell result not JSON ({e}): {}", stdout.trim()))
}
