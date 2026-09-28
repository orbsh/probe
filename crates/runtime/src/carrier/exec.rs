//! BGI + exec carriers (ADR-0035): out-of-process booths in two shapes
//! that do NOT share a protocol — the fcgi/cgi split, named precisely.
//!
//! **bgi (Booth Gateway Interface — resident, framed)**: the child lives
//! as long as the residency and speaks the line protocol — request
//! frames in, result frames out, ctx round trips inline (host frames the
//! parent answers). The author's code runs a loop: its own, or a shim
//! the probe ships per language (the nushell fifo adapter — the
//! "fcgi-adapts-cgi" move). Frame line:
//!   parent → child: {"id":N,"kind":"call|iterate_start|iterate_next|iterate_dispose","event":"...","op":"start|next|dispose","args":...,"stream_id":"..."}
//!   child → parent: {"host":{"op":"ctx_invoke","args":...}} → parent answers {"host_reply":{"ok":...}}
//!   child → parent: {"result":...} (terminates one request; success only — failures ride the outer Result, ADR-0012)
//! One outstanding host call at a time (synchronous-by-contract, like
//! every bridge). The source string IS the argv (whitespace-split — the
//! spawn spec; remote content-addressed binaries are a recorded
//! residual). bwrap policy wraps the spawn (mount policy before exec —
//! the child's capability surface stays the two pipes plus the jail).
//!
//! **exec (bare one-shot)**: no protocol. Spawn, write the whole request
//! as one JSON document on stdin (`{"handler": "<event>", "args":
//! <value>}`), close, read stdout to EOF, parse it as the result value.
//! The php-fpm lineage is exact and on purpose: nothing survives between
//! calls — no residency, so no iterate (an error value, like Rust
//! bodies), no ctx seam (there is no channel to hang it on), no sweep
//! (no child can die that anyone promised to keep). This is the SKILL
//! shape, and nushell's landing until its bgi fifo adapter ships (the
//! user's ruling 2026-09-28): a script that reads its stdin at EOF is
//! pure cgi — the loop never has to exist.
//!
//! ADR-0035's "B is A-without-a-loop" phrasing is superseded by this
//! file's shape: B is NOT a degraded protocol — it has no protocol.

use super::session::{ResidentSession, StreamOp};
use super::HostBridge;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

// --------------------------------------------------------------- bgi --

/// One resident bgi session: the spawn spec plus the live framed child.
pub struct BgiSession {
    /// Host functions for the child's ctx round trips, named like every
    /// other bridge's ("ctx_invoke", "ctx_iter_start", …) — the child
    /// sends them inside a host frame.
    host: HostBridge,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    reader: Option<BufReader<std::process::ChildStdout>>,
    next_id: u64,
}

impl BgiSession {
    /// Spawn and keep (no sandbox).
    pub fn spawn(argv: &[String], host: Option<&HostBridge>) -> Result<Self> {
        Self::spawn_wrapped(argv, host, &crate::sandbox::SandboxPolicy::None)
    }

    /// Spawn under the sandbox policy (bwrap wraps argv, §header).
    pub fn spawn_wrapped(
        argv: &[String],
        host: Option<&HostBridge>,
        policy: &crate::sandbox::SandboxPolicy,
    ) -> Result<Self> {
        let (child, stdin, reader) = spawn_child(argv, policy)?;
        Ok(Self {
            host: host.cloned().unwrap_or_default(),
            child: Some(child),
            stdin: Some(stdin),
            reader: Some(reader),
            next_id: 0,
        })
    }

    /// Liveness for the registry sweep (Sessions::sweep_dead): a child
    /// that exited is a broken cache entry — evict so the next call
    /// cold-starts.
    ///
    /// NOT `Child::try_wait`: without a reaping parent it leaves the
    /// exited child as a zombie whose pid never resolves, so a crashed
    /// child would read alive forever. WNOHANG waitpid reaps directly —
    /// 0 still-alive, negative ESRCH (already reaped), other = exited.
    pub fn is_alive(&mut self) -> bool {
        match self.child.as_mut() {
            None => false,
            Some(child) => {
                let pid = libc::pid_t::try_from(child.id()).unwrap_or(-1);
                let res = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
                res == 0
            }
        }
    }

    /// The child pid (test surface: crash-the-child-behind-the-registry
    /// scenarios need to signal it externally).
    pub fn child_pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// One request frame in, one result out, servicing host frames in
    /// between (the child blocks on its ctx call; we answer and keep
    /// reading until the result arrives).
    fn exchange(&mut self, mut frame: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        frame["id"] = Value::from(id);
        {
            let stdin = self
                .stdin
                .as_mut()
                .ok_or_else(|| anyhow!("bgi carrier: stdin closed"))?;
            writeln!(stdin, "{frame}")
                .map_err(|e| anyhow!("bgi carrier: write request: {e}"))?;
            stdin.flush()?;
        }
        loop {
            let mut line = String::new();
            let reader = self
                .reader
                .as_mut()
                .ok_or_else(|| anyhow!("bgi carrier: no stdout"))?;
            let n = reader
                .read_line(&mut line)
                .map_err(|e| anyhow!("bgi carrier: read: {e}"))?;
            if n == 0 {
                self.teardown();
                return Err(anyhow!("bgi carrier: child closed stdout (exited or crashed)"));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: Value = serde_json::from_str(trimmed)
                .map_err(|e| anyhow!("bgi carrier: malformed frame {trimmed}: {e}"))?;
            if let Some(host) = msg.get("host") {
                // One ctx round trip: run the named host fn, answer on
                // stdin. Unknown op = an error value back into the child.
                let op = host.get("op").and_then(|v| v.as_str()).unwrap_or_default();
                let args = host.get("args").cloned().unwrap_or(Value::Null);
                let outcome = match self.host.functions.get(op) {
                    Some(f) => (f)(args),
                    None => Err(anyhow!("bgi carrier: unknown host op '{op}'")),
                };
                let reply = match outcome {
                    Ok(v) => serde_json::json!({"host_reply": {"ok": v}}),
                    Err(e) => serde_json::json!({"host_reply": {"error": e.to_string()}}),
                };
                let stdin = self
                    .stdin
                    .as_mut()
                    .ok_or_else(|| anyhow!("bgi carrier: stdin closed"))?;
                writeln!(stdin, "{reply}")
                    .map_err(|e| anyhow!("bgi carrier: write host_reply: {e}"))?;
                stdin.flush()?;
                continue;
            }
            if let Some(result) = msg.get("result") {
                return Ok(result.clone());
            }
            return Err(anyhow!("bgi carrier: unexpected frame {msg}"));
        }
    }

    /// End the child: drop stdin (the loop's EOF), reap with a kill guard.
    fn teardown(&mut self) {
        self.stdin = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.reader = None;
    }
}

impl ResidentSession for BgiSession {
    /// The source was consumed by spawn (the argv); the child's module
    /// state is its own.
    fn load(&mut self, _source: &str) -> Result<()> {
        Ok(())
    }

    fn call(&mut self, handler: &str, args: &Value) -> Result<Value> {
        self.exchange(serde_json::json!({
            "kind": "call", "event": handler, "args": args,
        }))
    }

    /// ADR-0034 over the bgi seam: the stream verbs are request frames —
    /// the child keeps whatever guard state or generator its language
    /// offers, the envelope rule is the ADR's, unchanged.
    fn iterate(&mut self, op: StreamOp) -> Result<Value> {
        let (kind, event, op_tag, args, stream_id) = match &op {
            StreamOp::Start { stream_id, handler, args } => {
                ("iterate_start", handler, "start", args, stream_id)
            }
            StreamOp::Next { stream_id, handler, args } => {
                ("iterate_next", handler, "next", args, stream_id)
            }
            StreamOp::Dispose { stream_id, handler, args } => {
                ("iterate_dispose", handler, "dispose", args, stream_id)
            }
        };
        self.exchange(serde_json::json!({
            "kind": kind, "event": event, "op": op_tag,
            "args": args, "stream_id": stream_id,
        }))
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl Drop for BgiSession {
    fn drop(&mut self) {
        self.teardown();
    }
}

// -------------------------------------------------------------- exec --

/// The bare one-shot carrier: NO ResidentSession, NO protocol — the cgi
/// shape. Spawn, feed the whole request as one JSON document on stdin,
/// close, read stdout to EOF as the result. Nothing survives the call
/// (php-fpm semantics, on purpose: the SKILL downgrade and nushell's
/// landing until its bgi adapter ships).
pub struct ExecOneShot {
    argv: Vec<String>,
    policy: crate::sandbox::SandboxPolicy,
}

impl ExecOneShot {
    pub fn new(argv: &[String], policy: &crate::sandbox::SandboxPolicy) -> Self {
        Self { argv: argv.to_vec(), policy: policy.clone() }
    }
    /// One invocation = one process = one JSON in, one JSON out.
    pub fn run(&self, handler: &str, args: &Value) -> Result<Value> {
        let (mut child, stdin, mut stdout) = spawn_child(&self.argv, &self.policy)?;
        let request = serde_json::json!({ "handler": handler, "args": args });
        // Write the whole request, then CLOSE — EOF is the script's cue
        // to run (and nushell's `open /dev/stdin` shape needs exactly
        // this: it delivers at writer-EOF).
        {
            let mut stdin = stdin;
            stdin
                .write_all(request.to_string().as_bytes())
                .map_err(|e| anyhow!("exec carrier: write request: {e}"))?;
            stdin.flush()?;
        } // stdin drops here: the writer closes, the child sees EOF.
        let mut out = String::new();
        stdout
            .read_to_string(&mut out)
            .map_err(|e| anyhow!("exec carrier: read stdout: {e}"))?;
        let status = child
            .wait()
            .map_err(|e| anyhow!("exec carrier: wait: {e}"))?;
        let trimmed = out.trim();
        if trimmed.is_empty() {
            anyhow::bail!(
                "exec carrier: empty stdout (exit {:?}) — a one-shot script must print its result JSON",
                status.code()
            );
        }
        serde_json::from_str(trimmed)
            .map_err(|e| anyhow!("exec carrier: stdout is not one JSON document: {e}"))
    }
}

/// The session-seam wrapper so the dispatch machinery (aura run_job, the
/// remote carrier path, the introspection throwaway) keeps ONE shape:
/// this parks no process between calls — every `call` spawns, feeds,
/// reaps. `iterate` is an error value on principle: a one-shot booth has
/// no residency to hold a stream (same ruling as Rust closure bodies,
/// ADR-0034 — the cgi lineage is stateless by definition, not by
/// omission). Sweep ignores these slots (not a BgiSession: nothing can
/// die that was never promised to live).
pub struct OneShotSession {
    inner: ExecOneShot,
}

impl OneShotSession {
    pub fn new(argv: &[String], policy: &crate::sandbox::SandboxPolicy) -> Self {
        Self { inner: ExecOneShot::new(argv, policy) }
    }
}

impl ResidentSession for OneShotSession {
    /// Nothing to load — the spawn spec is the whole program.
    fn load(&mut self, _source: &str) -> Result<()> {
        Ok(())
    }

    fn call(&mut self, handler: &str, args: &Value) -> Result<Value> {
        self.inner.run(handler, args)
    }

    fn iterate(&mut self, _op: StreamOp) -> Result<Value> {
        Err(anyhow!(
            "exec carrier: one-shot booths carry no stream state (ADR-0035 — \
             the cgi shape is stateless by definition; use bgi for residency) \
             — no stream is started"
        ))
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

// -------------------------------------------------------------- spawn --

/// Fork/exec argv[0] (or the bwrap wrapper) with piped stdio; stderr is
/// inherited (the child's diagnostics reach the probe's log, never the
/// result channel).
fn spawn_child(
    argv: &[String],
    policy: &crate::sandbox::SandboxPolicy,
) -> Result<(
    Child,
    ChildStdin,
    BufReader<std::process::ChildStdout>,
)> {
    let (head, rest) = argv
        .split_first()
        .ok_or_else(|| anyhow!("exec/bgi carrier: empty spawn spec"))?;
    let mut cmd = match policy {
        crate::sandbox::SandboxPolicy::None => {
            let mut c = Command::new(head);
            c.args(rest);
            c
        }
        crate::sandbox::SandboxPolicy::Bubblewrap { allow_write, deny_read, cwd, .. } => {
            let quoted = argv
                .iter()
                .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
                .collect::<Vec<_>>()
                .join(" ");
            let config = sandbox_runtime::config::SandboxRuntimeConfig {
                filesystem: sandbox_runtime::config::FilesystemConfig {
                    allow_write: allow_write.clone(),
                    deny_read: deny_read.clone(),
                    ..Default::default()
                },
                ..Default::default()
            };
            let (wrapped, _) = sandbox_runtime::sandbox::linux::generate_bwrap_command(
                &quoted,
                &config,
                std::path::Path::new(cwd),
                None,
                None,
                0,
                0,
                Some("/bin/bash"),
            )
            .map_err(|e| anyhow!("exec/bgi carrier: bwrap command: {e}"))?;
            let mut c = Command::new("bash");
            c.arg("-c").arg(wrapped);
            c
        }
    };
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow!("exec/bgi carrier: spawn {head}: {e}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("exec/bgi carrier: no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("exec/bgi carrier: no stdout"))?;
    Ok((child, stdin, BufReader::new(stdout)))
}
