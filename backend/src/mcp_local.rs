//! Local MCP process manager, ported from pinned Plexus
//! `services/mcp-local/mcp-process-manager.ts`.
//!
//! A `local_http` server is `<launcher> <package> [args…]` (bunx/uvx) that
//! serves HTTP MCP on `127.0.0.1:<port><path>`. Operations per server are
//! serialized; `stop`/`restart` bump a lifecycle version that cancels an
//! in-flight startup. Readiness is any HTTP status below 500.
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{watch, Mutex};

const MAX_LOG_LINES: usize = 500;
const SAFE_INHERITED_ENV_KEYS: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "XDG_CACHE_HOME",
    "BUN_INSTALL",
    "UV_CACHE_DIR",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Parsed `local_http` config (`LocalHttpMcpServerConfigSchema` output).
#[derive(Debug, Clone, PartialEq)]
pub struct LocalConfig {
    pub launcher: String,
    pub package: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub port: u16,
    pub path: String,
    pub startup_timeout_ms: u64,
}

impl LocalConfig {
    pub fn from_view(view: &Value) -> Option<Self> {
        if view.get("mode").and_then(Value::as_str) != Some("local_http") {
            return None;
        }
        let strings = |key: &str| {
            view.get(key)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut env: Vec<(String, String)> = view
            .get("env")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        env.sort();
        Some(Self {
            launcher: view.get("launcher")?.as_str()?.to_owned(),
            package: view
                .get("package")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            args: strings("args"),
            env,
            port: view.get("port").and_then(Value::as_u64).unwrap_or(0) as u16,
            path: view
                .get("path")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .unwrap_or("/mcp")
                .to_owned(),
            startup_timeout_ms: view
                .get("startup_timeout_ms")
                .and_then(Value::as_u64)
                .filter(|t| *t > 0)
                .unwrap_or(30_000),
        })
    }
    pub fn local_url(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port, self.path)
    }
}

struct ProcessState {
    status: &'static str,
    child: Option<Arc<Mutex<Child>>>,
    pid: Option<u32>,
    fingerprint: Option<LocalConfig>,
    logs: Vec<String>,
    last_error: Option<String>,
    started_at: Option<String>,
    exited_at: Option<String>,
    lifecycle_version: u64,
    cancel: Option<watch::Sender<bool>>,
}

impl Default for ProcessState {
    fn default() -> Self {
        Self {
            status: "stopped",
            child: None,
            pid: None,
            fingerprint: None,
            logs: vec![],
            last_error: None,
            started_at: None,
            exited_at: None,
            lifecycle_version: 0,
            cancel: None,
        }
    }
}

#[derive(Default)]
pub struct McpProcessManager {
    states: StdMutex<HashMap<String, Arc<StdMutex<ProcessState>>>>,
    operations: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn append_log(state: &mut ProcessState, line: &str) {
    state.logs.push(format!("{} {line}", now_iso()));
    if state.logs.len() > MAX_LOG_LINES {
        let excess = state.logs.len() - MAX_LOG_LINES;
        state.logs.drain(..excess);
    }
}

impl McpProcessManager {
    fn state(&self, name: &str) -> Arc<StdMutex<ProcessState>> {
        self.states
            .lock()
            .unwrap()
            .entry(name.to_owned())
            .or_default()
            .clone()
    }
    fn operation_lock(&self, name: &str) -> Arc<Mutex<()>> {
        self.operations
            .lock()
            .unwrap()
            .entry(name.to_owned())
            .or_default()
            .clone()
    }

    pub fn status(&self, name: &str, config: Option<&LocalConfig>) -> Value {
        let state = self.state(name);
        let s = state.lock().unwrap();
        json!({
            "serverName": name, "status": s.status, "pid": s.pid,
            "url": config.map(LocalConfig::local_url),
            "lastError": s.last_error, "startedAt": s.started_at, "exitedAt": s.exited_at,
        })
    }
    pub fn logs(&self, name: &str) -> Vec<String> {
        self.state(name).lock().unwrap().logs.clone()
    }

    /// `ensureRunning`: start unless already running with the same config.
    pub async fn ensure_running(
        self: &Arc<Self>,
        name: &str,
        config: &LocalConfig,
    ) -> Result<(), String> {
        let version = self.state(name).lock().unwrap().lifecycle_version;
        let lock = self.operation_lock(name);
        let _guard = lock.lock().await;
        self.ensure_running_internal(name, config, version).await
    }

    pub async fn start(
        self: &Arc<Self>,
        name: &str,
        config: &LocalConfig,
    ) -> Result<Value, String> {
        let version = self.state(name).lock().unwrap().lifecycle_version;
        let lock = self.operation_lock(name);
        let _guard = lock.lock().await;
        self.ensure_running_internal(name, config, version).await?;
        Ok(self.status(name, Some(config)))
    }

    pub async fn stop(self: &Arc<Self>, name: &str) -> Result<Value, String> {
        {
            let state = self.state(name);
            let mut s = state.lock().unwrap();
            s.lifecycle_version += 1;
            if let Some(cancel) = &s.cancel {
                let _ = cancel.send(true);
            }
        }
        let lock = self.operation_lock(name);
        let _guard = lock.lock().await;
        self.stop_internal(name).await
    }

    pub async fn restart(
        self: &Arc<Self>,
        name: &str,
        config: &LocalConfig,
    ) -> Result<Value, String> {
        let version = {
            let state = self.state(name);
            let mut s = state.lock().unwrap();
            s.lifecycle_version += 1;
            if let Some(cancel) = &s.cancel {
                let _ = cancel.send(true);
            }
            s.lifecycle_version
        };
        let lock = self.operation_lock(name);
        let _guard = lock.lock().await;
        self.stop_internal(name).await?;
        self.ensure_running_internal(name, config, version).await?;
        Ok(self.status(name, Some(config)))
    }

    async fn ensure_running_internal(
        self: &Arc<Self>,
        name: &str,
        config: &LocalConfig,
        version: u64,
    ) -> Result<(), String> {
        let state = self.state(name);
        let (running, has_child) = {
            let s = state.lock().unwrap();
            if s.lifecycle_version != version {
                return Err("Local MCP startup cancelled".into());
            }
            (
                s.status == "running" && s.fingerprint.as_ref() == Some(config),
                s.child.is_some(),
            )
        };
        if has_child {
            if running {
                return Ok(());
            }
            self.stop_internal(name).await?;
        }
        if state.lock().unwrap().lifecycle_version != version {
            return Err("Local MCP startup cancelled".into());
        }
        let (tx, rx) = watch::channel(false);
        state.lock().unwrap().cancel = Some(tx);
        let result = self.start_internal(name, config, rx).await;
        state.lock().unwrap().cancel = None;
        result
    }

    async fn stop_internal(&self, name: &str) -> Result<Value, String> {
        let state = self.state(name);
        let child = state.lock().unwrap().child.clone();
        if let Some(child) = child {
            let mut guard = child.lock().await;
            #[cfg(unix)]
            if let Some(pid) = guard.id() {
                unsafe {
                    libc_kill(pid as i32, 15);
                }
            }
            let exited = tokio::time::timeout(Duration::from_millis(3000), guard.wait())
                .await
                .is_ok();
            if !exited {
                let _ = guard.start_kill();
                if tokio::time::timeout(Duration::from_millis(1000), guard.wait())
                    .await
                    .is_err()
                {
                    let mut s = state.lock().unwrap();
                    s.status = "failed";
                    s.last_error = Some("Local MCP process did not exit after SIGKILL".into());
                    append_log(
                        &mut s,
                        "Failed to stop process: Local MCP process did not exit after SIGKILL",
                    );
                    return Err("Local MCP process did not exit after SIGKILL".into());
                }
            }
            let mut s = state.lock().unwrap();
            if s.child.as_ref().is_some_and(|c| Arc::ptr_eq(c, &child)) {
                s.child = None;
                s.pid = None;
            }
        }
        {
            let mut s = state.lock().unwrap();
            s.status = "stopped";
            s.last_error = None;
            s.exited_at = Some(now_iso());
        }
        Ok(self.status(name, None))
    }

    async fn start_internal(
        self: &Arc<Self>,
        name: &str,
        config: &LocalConfig,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<(), String> {
        let state = self.state(name);
        let args: Vec<String> = std::iter::once(config.package.clone())
            .chain(config.args.iter().cloned())
            .map(|a| {
                a.replace("{{PORT}}", &config.port.to_string())
                    .replace("{{HOST}}", "127.0.0.1")
            })
            .collect();
        {
            let mut s = state.lock().unwrap();
            s.status = "starting";
            s.last_error = None;
            s.exited_at = None;
            s.fingerprint = Some(config.clone());
            let command_line = std::iter::once(config.launcher.clone())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" ");
            append_log(&mut s, &format!("Starting: {command_line}"));
        }
        let mut command = Command::new(&config.launcher);
        command
            .args(&args)
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for key in SAFE_INHERITED_ENV_KEYS {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        for (k, v) in &config.env {
            command.env(k, v);
        }
        command
            .env("PORT", config.port.to_string())
            .env("HOST", "127.0.0.1");
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let mut s = state.lock().unwrap();
                s.status = "failed";
                s.last_error = Some(error.to_string());
                append_log(&mut s, &format!("Spawn failed: {error}"));
                return Err(error.to_string());
            }
        };
        let pid = child.id();
        for (stream, label) in [
            (
                child
                    .stdout
                    .take()
                    .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
                "stdout",
            ),
            (
                child
                    .stderr
                    .take()
                    .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
                "stderr",
            ),
        ] {
            if let Some(stream) = stream {
                let state = state.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stream).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        append_log(
                            &mut state.lock().unwrap(),
                            &format!("[{label}] {}", line.trim_end()),
                        );
                    }
                });
            }
        }
        let child = Arc::new(Mutex::new(child));
        {
            let mut s = state.lock().unwrap();
            s.child = Some(child.clone());
            s.pid = pid;
            s.started_at = Some(now_iso());
        }
        // Exit watcher: a process that dies on its own records its exit code.
        {
            let state = state.clone();
            let child = child.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let status = {
                        let mut guard = child.lock().await;
                        guard.try_wait()
                    };
                    match status {
                        Ok(Some(status)) => {
                            let code = status
                                .code()
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "null".into());
                            let mut s = state.lock().unwrap();
                            if s.child.as_ref().is_some_and(|c| Arc::ptr_eq(c, &child)) {
                                if let Some(cancel) = &s.cancel {
                                    let _ = cancel.send(true);
                                }
                                s.child = None;
                                s.pid = None;
                                s.status = if status.success() {
                                    "stopped"
                                } else {
                                    "failed"
                                };
                                s.last_error = (!status.success())
                                    .then(|| format!("Process exited with code {code}"));
                                s.exited_at = Some(now_iso());
                            }
                            append_log(&mut s, &format!("Process exited with code {code}"));
                            break;
                        }
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
            });
        }
        let ready = wait_for_ready(config, &mut cancel).await;
        let still_ours = state
            .lock()
            .unwrap()
            .child
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(c, &child));
        let result: Result<(), String> = match ready {
            Ok(()) if still_ours => {
                let mut s = state.lock().unwrap();
                s.status = "running";
                append_log(&mut s, &format!("Ready at {}", config.local_url()));
                return Ok(());
            }
            Ok(()) => Err("Local MCP process exited before readiness".to_string()),
            Err(error) => Err(if *cancel.borrow() && !still_ours {
                "Local MCP process exited before readiness".to_string()
            } else {
                error
            }),
        };
        let error = result.unwrap_err();
        {
            let mut s = state.lock().unwrap();
            s.status = "failed";
            s.last_error = Some(error.clone());
            append_log(&mut s, &format!("Startup failed: {error}"));
        }
        if let Err(stop_error) = self.stop_internal(name).await {
            let mut s = state.lock().unwrap();
            s.status = "failed";
            s.last_error = Some(format!("{error}; cleanup failed: {stop_error}"));
            return Err(error);
        }
        let mut s = state.lock().unwrap();
        s.status = "failed";
        s.last_error = Some(error.clone());
        Err(error)
    }
}

async fn wait_for_ready(
    config: &LocalConfig,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    let url = config.local_url();
    let total = Duration::from_millis(config.startup_timeout_ms);
    let deadline = tokio::time::Instant::now() + total;
    let client = reqwest::Client::new();
    let mut last_error = String::new();
    while tokio::time::Instant::now() < deadline {
        if *cancel.borrow() {
            return Err("Local MCP startup cancelled".into());
        }
        let remaining = deadline - tokio::time::Instant::now();
        let attempt = (total / 4).max(Duration::from_millis(1000)).min(remaining);
        tokio::select! {
            result = client.get(&url).timeout(attempt).send() => match result {
                Ok(response) if response.status().as_u16() < 500 => return Ok(()),
                Ok(response) => last_error = format!("HTTP {}", response.status().as_u16()),
                Err(error) => last_error = if error.is_timeout() { "Readiness request timed out".into() } else { error.to_string() },
            },
            _ = cancel.changed() => return Err("Local MCP startup cancelled".into()),
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(remaining.min(Duration::from_millis(500))) => {}
            _ = cancel.changed() => return Err("Local MCP startup cancelled".into()),
        }
    }
    if *cancel.borrow() {
        return Err("Local MCP startup cancelled".into());
    }
    Err(format!(
        "Local MCP server did not become ready: {last_error}"
    ))
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, signal: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, signal);
}

impl std::fmt::Debug for McpProcessManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("McpProcessManager")
    }
}
