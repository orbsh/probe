//! Nushell PTY residency carrier (Phase 2.6 for nu): a long-lived nu REPL
//! per booth instance in a pseudo-terminal. One `source` loads the module;
//! each call addresses the handler by name (multi-entry). Cross-call state
//! lives in `$env` variables. Results travel via the filesystem, not the
//! PTY stream — the stream is too noisy (echo, prompt redraws, OSC) to parse.

use anyhow::Result;
use serde_json::Value;

/// One resident nu REPL session (a pseudo-terminal master + nu child).
pub struct NushellSession {
    fd: i32,
    pid: i32,
    /// The ctx bridge (nushell carrier): host functions answered through
    /// the session directory's request/response files. `None` = pure
    /// session (no bridge materialized, no sweeping).
    pub bridge: Option<std::sync::Arc<super::HostBridge>>,
    /// The session directory (bridge.nu + req/resp files live here).
    pub dir: Option<std::path::PathBuf>,
}

impl NushellSession {
    /// Spawn `nu --no-config-file` in a PTY and wait for the first prompt.
    /// A Bubblewrap policy wraps the child process: the forked child execs
    /// `bash -c <bwrap …>` so the whole REPL lives inside the sandbox
    /// (mount namespace set up before nu starts; no per-syscall cost).
    pub fn spawn(policy: &crate::sandbox::SandboxPolicy) -> anyhow::Result<Self> {
        let mut master: libc::c_int = 0;
        let mut slave: libc::c_int = 0;
        if unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null()) } != 0 {
            anyhow::bail!("openpty failed");
        }
        // Set a small window: keeps redraw noise down.
        let ws = libc::winsize { ws_row: 40, ws_col: 120, ws_xpixel: 0, ws_ypixel: 0 };
        unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &ws) };

        let pid = unsafe { libc::fork() };
        if pid < 0 {
            anyhow::bail!("fork failed");
        }
        if pid == 0 {
            unsafe {
                libc::setsid();
                libc::ioctl(slave, libc::TIOCSCTTY, 0);
                libc::dup2(slave, 0);
                libc::dup2(slave, 1);
                libc::dup2(slave, 2);
                if slave > 2 { libc::close(slave); }
                libc::close(master);
                match policy {
                    crate::sandbox::SandboxPolicy::None => {
                        libc::execlp(c"nu".as_ptr() as *const libc::c_char, c"nu".as_ptr() as *const libc::c_char, c"--no-config-file".as_ptr() as *const libc::c_char, std::ptr::null::<libc::c_char>());
                    }
                    crate::sandbox::SandboxPolicy::Bubblewrap { allow_write, deny_read, cwd, .. } => {
                        // bwrap mount policy from the sandbox config: fs
                        // allow/deny lists map to bind mounts, network is
                        // unshared (--unshare-net). Domain allowlists (proxy
                        // filtering) are a Phase 5 refinement.
                        let config = sandbox_runtime::config::SandboxRuntimeConfig {
                            filesystem: sandbox_runtime::config::FilesystemConfig {
                                allow_write: allow_write.clone(),
                                deny_read: deny_read.clone(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };
                        let (wrapped, _) = sandbox_runtime::sandbox::linux::generate_bwrap_command(
                            "nu --no-config-file",
                            &config,
                            std::path::Path::new(cwd),
                            None,
                            None,
                            0,
                            0,
                            Some("/bin/bash"),
                        ).expect("bwrap command");
                        let c0 = std::ffi::CString::new(wrapped).unwrap();
                        libc::execlp(c"bash".as_ptr() as *const libc::c_char, c"bash".as_ptr() as *const libc::c_char, c"-c".as_ptr() as *const libc::c_char, c0.as_ptr(), std::ptr::null::<libc::c_char>());
                    }
                }
                libc::_exit(127);
            }
        }
        unsafe { libc::close(slave) };
        let session = Self { fd: master, pid, bridge: None, dir: None };
        session.pump(2.5); // banner + first prompt
        Ok(session)
    }

    /// Attach the ctx bridge: the session dir where req/resp files live
    /// and the host functions the sweep answers.
    pub fn set_bridge(&mut self, bridge: std::sync::Arc<super::HostBridge>, dir: std::path::PathBuf) {
        self.bridge = Some(bridge);
        self.dir = Some(dir);
    }

    /// Load the booth module (defines the handler functions).
    pub fn load(&mut self, script_path: &str) -> Result<()> {
        self.send(&format!("source '{}'\r\n", script_path));
        self.pump(1.5);
        Ok(())
    }

    /// Invoke one handler with parsed JSON args; returns the JSON result.
    ///
    /// Per call: materialize a wrapper script, `source` it in the resident
    /// session. The wrapper reads args from a file, calls the handler by
    /// name, and saves the JSON result to a file. Completion is detected by
    /// polling the result file; the PTY stream is only drained (discarding
    /// output, answering reedline cursor queries) to keep the REPL alive.
    pub fn call(&mut self, handler: &str, args: &Value) -> Result<Value> {
        let args_json = serde_json::to_string(args)?;
        let marker = format!(
            "__R{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.subsec_nanos()
        );
        let dir = std::env::temp_dir().join(format!("probe-nu-{}-{}", std::process::id(), self.pid));
        std::fs::create_dir_all(&dir)?;
        let args_path = dir.join(format!("{marker}.args.json"));
        let result_path = dir.join(format!("{marker}.result.json"));
        std::fs::write(&args_path, args_json)?;

        let wrapper = format!(
            "$env.__PROBE_ARGS = (open '{}'); let __r = ({handler} $env.__PROBE_ARGS | to json --raw); $__r | save --force '{}'\n",
            args_path.display(),
            result_path.display()
        );
        let wrapper_path = dir.join(format!("{marker}.nu"));
        std::fs::write(&wrapper_path, wrapper)?;
        self.send(&format!("source '{}'\r\n", wrapper_path.display()));

        // Poll for the result file, keeping the session responsive and
        // answering ctx-bridge request files (a handler blocked inside a
        // ctx-invoke poll is exactly the case the bridge exists for).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut raw: Option<String> = None;
        while raw.is_none() {
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("nushell session: result timeout for handler '{handler}'");
            }
            if let Ok(text) = std::fs::read_to_string(&result_path) {
                raw = Some(text);
            } else {
                self.sweep_bridge_requests();
                self.pump(0.1);
            }
        }
        let raw = raw.unwrap();
        let _ = std::fs::remove_file(&args_path);
        let _ = std::fs::remove_file(&result_path);
        let _ = std::fs::remove_file(&wrapper_path);
        // The result file appears while the REPL is still finishing the
        // wrapper (prompt redraw + the `ESC[6n` cursor query it answers
        // against). Returning now leaves that query unanswered and the
        // NEXT call's `source` line lands in a half-drawn prompt — the
        // second bridge turn then never executes (locked by nu_twocall).
        // Drain until the prompt is back before handing the session over.
        self.pump_quiet(1.0);
        Ok(serde_json::from_str(raw.trim())?)
    }

    fn send(&self, s: &str) {
        let _ = unsafe { libc::write(self.fd, s.as_bytes().as_ptr() as *const libc::c_void, s.len()) };
    }

    /// Drain the PTY stream for `t` seconds, discarding output but answering
    /// reedline cursor-position queries (`ESC[6n`) so the REPL never blocks.
    fn pump(&self, t: f64) {
        let mut buf = vec![0u8; 65536];
        let end = std::time::Instant::now() + std::time::Duration::from_secs_f64(t);
        while std::time::Instant::now() < end {
            if poll_fd(self.fd, 0.2) {
                match read_fd(self.fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if find_subslice(&buf[..n], b"\x1b[6n").is_some() {
                            self.send("\x1b[1;1R");
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }

    /// Drain until the stream goes quiet (prompt redraw finished), so the
    /// session is handed back with the REPL idle. `t` bounds the wait.
    fn pump_quiet(&self, t: f64) {
        let mut buf = vec![0u8; 65536];
        let end = std::time::Instant::now() + std::time::Duration::from_secs_f64(t);
        while std::time::Instant::now() < end {
            if poll_fd(self.fd, 0.15) {
                match read_fd(self.fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if find_subslice(&buf[..n], b"\x1b[6n").is_some() {
                            self.send("\x1b[1;1R");
                        }
                    }
                    Err(_) => break,
                }
            } else {
                break; // 150ms of silence: the prompt is settled
            }
        }
    }

    /// Answer pending ctx-bridge requests: for each `req-*.json` in the
    /// session dir, look up the named HostFn, run it, write the reply to
    /// `resp-<same>.json`, remove the request. Runs inside `call`'s poll
    /// loop — the nu script is blocked polling its resp file, the slot
    /// lock is ours, and HostFn calls may re-enter the realm (spawn_blocking
    /// contract), so there is no deadlock.
    fn sweep_bridge_requests(&self) {
        let Some(dir) = &self.dir else { return };
        let Some(bridge) = &self.bridge else { return };
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = match name.to_str() {
                Some(n) if n.starts_with("req-") && n.ends_with(".json") => n,
                _ => continue,
            };
            let resp_name = name.replacen("req-", "resp-", 1);
            let resp_path = dir.join(&resp_name);
            if resp_path.exists() {
                continue; // already answered this pass
            }
            let Ok(body) = std::fs::read_to_string(entry.path()) else { continue };
            let parsed: serde_json::Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(e) => {
                    let _ = std::fs::write(&resp_path, serde_json::json!({ "__error": format!("bad request: {e}") }).to_string());
                    continue;
                }
            };
            let fn_name = parsed.get("fn").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let arg = parsed.get("arg").cloned().unwrap_or(serde_json::Value::Null);
            let outcome = match bridge.functions.get(&fn_name) {
                Some(f) => f(arg).map_err(|e| format!("{e:#}")),
                None => Err(format!("unknown host function '{fn_name}'")),
            };
            let reply = match outcome {
                Ok(v) => serde_json::json!({ "value": v }),
                Err(e) => serde_json::json!({ "__error": e }),
            };
            let _ = std::fs::write(&resp_path, reply.to_string());
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

impl Drop for NushellSession {
    fn drop(&mut self) {
        self.send("exit\r\n");
        self.pump(0.5);
        unsafe { libc::kill(self.pid, libc::SIGTERM) };
        unsafe { libc::close(self.fd) };
    }
}

fn poll_fd(fd: i32, timeout_s: f64) -> bool {
    let mut fds = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let n = unsafe { libc::poll(&mut fds, 1, (timeout_s * 1000.0) as i32) };
    n > 0 && (fds.revents & libc::POLLIN) != 0
}

fn read_fd(fd: i32, buf: &mut [u8]) -> std::io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
