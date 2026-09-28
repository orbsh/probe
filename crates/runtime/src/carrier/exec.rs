//! Exec carrier (ADR-0035): out-of-process booths speaking the line
//! protocol over stdin/stdout. Mode A (resident): the child is spawned
//! once per booth instance and the frame loop lives in the child — this
//! session is its peer, the residency IS the process. Mode B (one-shot):
//! the same frames, the child exits at EOF — a fresh spawn per call, no
//! ctx return path (the SKILL downgrade: invoke only).
//!
//! The seam mirrors the wasm carrier's discipline: JSON frames at the
//! boundary, session state (the child process) persists across calls in
//! mode A, eviction = close stdin + reap. The source string IS the spawn
//! argv (the compiled booth; remote content-addressed delivery of
//! binaries is the follow-on — recorded residual).
//!
//! Frame line:
//!   parent → child: {"id":N,"kind":"call|iterate_start|iterate_next|iterate_dispose","event":"...","op":"start|next|dispose","args":...,"stream_id":"..."}
//!   child → parent: {"host":{"op":"ctx_invoke","args":...}}            (mode A only: a ctx round trip)
//!   parent → child: {"host_reply":{"ok":...}}
//!   child → parent: {"result":...}                                      (terminates one request)
//! One outstanding host call at a time (the script-side ctx fns are
//! synchronous by contract — same rule as every other carrier's bridge).

use super::session::{ResidentSession, StreamOp};
use super::HostBridge;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

/// One exec session: the spawn spec plus the current child (mode A keeps
/// one alive; mode B replaces it per call).
pub struct ExecSession {
    /// The argv, kept so mode B re-spawns identically.
    argv: Vec<String>,
    /// Host functions for the child's ctx round trips (mode A). Named
    /// like every other bridge's ("ctx_invoke", "ctx_iter_start", …) —
    /// the child sends them inside a host frame.
    host: HostBridge,
    /// The sandbox policy the child was spawned under (mode B re-applies).
    policy: crate::sandbox::SandboxPolicy,
    /// ADR-0035 mode B: close stdin after writing the request (EOF is
    /// the child's cue to run — nushell slurps its args at EOF), reap
    /// after the result, re-spawn on the next call.
    one_shot: bool,
    /// The live child. Mode A spawns at construction and keeps it; mode B
    /// spawns per call and clears after (None = "spawn on next call").
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    reader: Option<BufReader<std::process::ChildStdout>>,
    next_id: u64,
}

impl ExecSession {
    /// Mode A: spawn and keep (no sandbox).
    pub fn spawn(argv: &[String], host: Option<&HostBridge>) -> Result<Self> {
        Self::spawn_wrapped(argv, host, &crate::sandbox::SandboxPolicy::None)
    }

    /// Spawn argv[0] directly, or under the sandbox policy: a Bubblewrap
    /// policy wraps the child in `bwrap … <argv>` via bash -c — the same
    /// mount-namespace-before-exec shape the nushell PTY session uses
    /// (sandbox_runtime's generator, fs allow/deny + unshared net). The
    /// child's only capability surface stays the two pipes plus the jail.
    pub fn spawn_wrapped(
        argv: &[String],
        host: Option<&HostBridge>,
        policy: &crate::sandbox::SandboxPolicy,
    ) -> Result<Self> {
        let (child, stdin, reader) = spawn_child(argv, host, policy)?;
        Ok(Self {
            argv: argv.to_vec(),
            host: host.cloned().unwrap_or_default(),
            policy: policy.clone(),
            one_shot: false,
            child: Some(child),
            stdin: Some(stdin),
            reader: Some(reader),
            next_id: 0,
        })
    }

    /// Mode B: the same spawn spec, one-shot semantics — a fresh process
    /// per call, stdin closed at request write, no ctx return path.
    /// The SKILL shape, and the landing spot for runtimes that cannot
    /// block-read a pipe line-by-line (nushell's `input` needs a TTY;
    /// mode A for it waits on upstream).
    pub fn spawn_oneshot(
        argv: &[String],
        host: Option<&HostBridge>,
        policy: &crate::sandbox::SandboxPolicy,
    ) -> Result<Self> {
        let (child, stdin, reader) = spawn_child(argv, host, policy)?;
        Ok(Self {
            argv: argv.to_vec(),
            host: host.cloned().unwrap_or_default(),
            policy: policy.clone(),
            one_shot: true,
            child: Some(child),
            stdin: Some(stdin),
            reader: Some(reader),
            next_id: 0,
        })
    }

    /// Liveness for the registry sweep (Sessions::sweep_dead): a child
    /// that exited is a broken cache entry — evict so the next call
    /// cold-starts a fresh spawn. Mode B is always "alive" between calls
    /// (no child parked is its normal state, not a crash).
    ///
    /// NOT `Child::try_wait`: without a reaping parent it leaves the
    /// exited child as a zombie whose pid never resolves, so a crashed
    /// mode-A child would read alive forever. WNOHANG waitpid reaps
    /// directly — 0 still-alive, negative ESRCH (already reaped), other
    /// = exited.
    pub fn is_alive(&mut self) -> bool {
        if self.one_shot {
            return true;
        }
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

    /// Ensure a live child for the next exchange. Mode A never needs it
    /// after construction (the child is resident); mode B calls this when
    /// the previous one-shot was reaped and cleared.
    fn ensure_child(&mut self) -> Result<()> {
        if self.child.is_some() {
            return Ok(());
        }
        let (child, stdin, reader) =
            spawn_child(&self.argv, Some(&self.host), &self.policy)?;
        self.child = Some(child);
        self.stdin = Some(stdin);
        self.reader = Some(reader);
        Ok(())
    }

    /// One request frame in, one result out, servicing host frames in
    /// between (the child blocks on its ctx call; we answer and keep
    /// reading until the result arrives). Mode B writes the request,
    /// CLOSES stdin (the EOF is the child's cue — its args arrive with
    /// it), then reads the single result; a host frame from a one-shot
    /// child is a contract violation (B is ctx-free, ADR-0035 §2).
    fn exchange(&mut self, mut frame: Value) -> Result<Value> {
        self.ensure_child()?;
        let id = self.next_id;
        self.next_id += 1;
        frame["id"] = Value::from(id);
        {
            let stdin = self.stdin.as_mut().ok_or_else(|| anyhow!("exec carrier: stdin closed"))?;
            writeln!(stdin, "{frame}").map_err(|e| anyhow!("exec carrier: write request: {e}"))?;
            stdin.flush()?;
        }
        if self.one_shot {
            // EOF triggers the child to run; there is no return path.
            self.stdin = None;
        }
        loop {
            let mut line = String::new();
            let reader = self
                .reader
                .as_mut()
                .ok_or_else(|| anyhow!("exec carrier: no stdout"))?;
            let n = reader.read_line(&mut line).map_err(|e| anyhow!("exec carrier: read: {e}"))?;
            if n == 0 {
                // Child closed stdout (exited after its answer — mode B's
                // normal end — or crashed mid-call in mode A).
                self.teardown_child();
                return Err(anyhow!("exec carrier: child closed stdout (exited or crashed)"));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: Value = serde_json::from_str(trimmed)
                .map_err(|e| anyhow!("exec carrier: malformed frame {trimmed}: {e}"))?;
            if let Some(host) = msg.get("host") {
                if self.stdin.is_none() {
                    anyhow::bail!(
                        "exec carrier mode B: child sent a host frame ('{}') but B is ctx-free \
                         (one-shot scripts answer and exit; no stdin return path)",
                        host.get("op").and_then(|v| v.as_str()).unwrap_or("?")
                    );
                }
                // One ctx round trip: run the named host fn, answer on
                // stdin. Unknown op = an error value back into the child.
                let op = host.get("op").and_then(|v| v.as_str()).unwrap_or_default();
                let args = host.get("args").cloned().unwrap_or(Value::Null);
                let outcome = match self.host.functions.get(op) {
                    Some(f) => (f)(args),
                    None => Err(anyhow!("exec carrier: unknown host op '{op}'")),
                };
                let reply = match outcome {
                    Ok(v) => serde_json::json!({"host_reply": {"ok": v}}),
                    Err(e) => serde_json::json!({"host_reply": {"error": e.to_string()}}),
                };
                let stdin = self.stdin.as_mut().unwrap();
                writeln!(stdin, "{reply}").map_err(|e| anyhow!("exec carrier: write host_reply: {e}"))?;
                stdin.flush()?;
                continue;
            }
            if let Some(result) = msg.get("result") {
                let result = result.clone();
                if self.one_shot {
                    // B's contract: the answer is the last word — reap now
                    // (a lingering child is killed) and clear; the next
                    // call re-spawns.
                    self.teardown_child();
                }
                return Ok(result);
            }
            return Err(anyhow!("exec carrier: unexpected frame {msg}"));
        }
    }

    /// End the current child: drop stdin (the loop's EOF), reap with a
    /// kill guard. Shared by mode B's per-call teardown and Drop.
    fn teardown_child(&mut self) {
        self.stdin = None; // EOF for a well-behaved loop
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.reader = None;
    }
}

/// Fork/exec argv[0] (or the bwrap wrapper) with piped stdio. Separated
/// so mode A's construction and mode B's per-call re-spawn share it.
fn spawn_child(
    argv: &[String],
    host: Option<&HostBridge>,
    policy: &crate::sandbox::SandboxPolicy,
) -> Result<(Child, ChildStdin, BufReader<std::process::ChildStdout>)> {
    let _ = host; // the bridge rides the parent's exchange loop, not the child's env
    let (head, rest) = argv
        .split_first()
        .ok_or_else(|| anyhow!("exec carrier: empty spawn spec"))?;
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
            .map_err(|e| anyhow!("exec carrier: bwrap command: {e}"))?;
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
        .map_err(|e| anyhow!("exec carrier: spawn {head}: {e}"))?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("exec carrier: no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("exec carrier: no stdout"))?;
    Ok((child, stdin, BufReader::new(stdout)))
}

impl ResidentSession for ExecSession {
    /// The source was consumed by spawn (the argv). Loading is the
    /// carrier's construction — the child's module state is its own.
    fn load(&mut self, _source: &str) -> Result<()> {
        Ok(())
    }

    fn call(&mut self, handler: &str, args: &Value) -> Result<Value> {
        self.exchange(serde_json::json!({
            "kind": "call", "event": handler, "args": args,
        }))
    }

    /// ADR-0034 over the exec seam: the stream verbs are request frames
    /// (the child keeps whatever guard state or generator its language
    /// offers; the envelope rule is the ADR's, unchanged).
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

impl Drop for ExecSession {
    fn drop(&mut self) {
        self.teardown_child();
    }
}
