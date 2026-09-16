//! Nushell PTY residency carrier (Phase 2.6 for nu): a long-lived nu REPL
//! per actor instance in a pseudo-terminal. One `source` loads the module;
//! each call addresses the handler by name (multi-entry). Cross-call state
//! lives in `$env` variables. Results travel via the filesystem, not the
//! PTY stream — the stream is too noisy (echo, prompt redraws, OSC) to parse.

use anyhow::Result;
use serde_json::Value;

/// One resident nu REPL session (a pseudo-terminal master + nu child).
pub struct NushellSession {
    fd: i32,
    pid: i32,
}

impl NushellSession {
    /// Spawn `nu --no-config-file` in a PTY and wait for the first prompt.
    pub fn spawn() -> anyhow::Result<Self> {
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
                libc::execlp(b"nu\0".as_ptr() as *const libc::c_char, b"nu\0".as_ptr() as *const libc::c_char, b"--no-config-file\0".as_ptr() as *const libc::c_char, std::ptr::null::<libc::c_char>());
                libc::_exit(127);
            }
        }
        unsafe { libc::close(slave) };
        let session = Self { fd: master, pid };
        session.pump(2.5); // banner + first prompt
        Ok(session)
    }

    /// Load the actor module (defines the handler functions).
    pub fn load(&mut self, script_path: &str) {
        self.send(&format!("source '{}'\r\n", script_path));
        self.pump(1.5);
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

        // Poll for the result file, keeping the session responsive.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut raw: Option<String> = None;
        while raw.is_none() {
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("nushell session: result timeout for handler '{handler}'");
            }
            if let Ok(text) = std::fs::read_to_string(&result_path) {
                raw = Some(text);
            } else {
                self.pump(0.1);
            }
        }
        let raw = raw.unwrap();
        let _ = std::fs::remove_file(&args_path);
        let _ = std::fs::remove_file(&result_path);
        let _ = std::fs::remove_file(&wrapper_path);
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
