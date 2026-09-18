//! JSON-RPC stdio transport for local MCP servers.

use serde_json::{json, Value};
use std::{collections::HashMap, io, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot, Mutex},
};

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("MCP process I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("MCP process returned malformed JSON-RPC: {0}")]
    Json(#[from] serde_json::Error),
    #[error("MCP process exited")]
    Closed,
    #[error("MCP request failed ({code}): {message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
}

#[derive(Debug)]
struct Inner {
    stdin: Mutex<ChildStdin>,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value, TransportError>>>>,
    notifications: mpsc::Sender<Value>,
    terminal_error: Mutex<Option<TransportError>>,
}

/// A local MCP server speaking newline-delimited JSON-RPC over stdin/stdout.
///
/// A single reader task owns stdout and dispatches responses by request id;
/// stderr is drained separately so a verbose server cannot deadlock.
#[derive(Debug)]
pub struct LocalMcpTransport {
    inner: Arc<Inner>,
    notifications: Mutex<mpsc::Receiver<Value>>,
    child: Mutex<Option<Child>>,
}

impl LocalMcpTransport {
    pub async fn spawn(mut command: Command) -> Result<Self, TransportError> {
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("missing MCP stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing MCP stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing MCP stderr"))?;
        let (notification_tx, notification_rx) = mpsc::channel(64);
        let inner = Arc::new(Inner {
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            notifications: notification_tx,
            terminal_error: Mutex::new(None),
        });

        let reader_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let result = loop {
                match lines.next_line().await {
                    Ok(Some(line)) if line.trim().is_empty() => continue,
                    Ok(Some(line)) => {
                        let message: Value = match serde_json::from_str(&line) {
                            Ok(value) => value,
                            Err(error) => break Err(TransportError::Json(error)),
                        };
                        if let Some(id) = message.get("id").and_then(request_key) {
                            if let Some(sender) = reader_inner.pending.lock().await.remove(&id) {
                                let value = if let Some(error) = message.get("error") {
                                    Err(TransportError::Rpc {
                                        code: error
                                            .get("code")
                                            .and_then(Value::as_i64)
                                            .unwrap_or(-32000),
                                        message: error
                                            .get("message")
                                            .and_then(Value::as_str)
                                            .unwrap_or("MCP error")
                                            .to_owned(),
                                        data: error.get("data").cloned(),
                                    })
                                } else {
                                    Ok(message.get("result").cloned().unwrap_or(Value::Null))
                                };
                                let _ = sender.send(value);
                            }
                        } else {
                            let _ = reader_inner.notifications.send(message).await;
                        }
                    }
                    Ok(None) => break Err(TransportError::Closed),
                    Err(error) => break Err(TransportError::Io(error)),
                }
            };
            *reader_inner.terminal_error.lock().await = Some(match &result {
                Ok(()) => TransportError::Closed,
                Err(error) => error.clone_error(),
            });
            let mut pending = reader_inner.pending.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err(match &result {
                    Ok(()) => TransportError::Closed,
                    Err(error) => error.clone_error(),
                }));
            }
        });
        tokio::spawn(async move {
            let mut stderr = BufReader::new(stderr).lines();
            while stderr.next_line().await.ok().flatten().is_some() {}
        });
        Ok(Self {
            inner,
            notifications: Mutex::new(notification_rx),
            child: Mutex::new(Some(child)),
        })
    }

    pub async fn request(
        &self,
        id: impl Into<String>,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, TransportError> {
        let id = id.into();
        let (sender, receiver) = oneshot::channel();
        self.inner.pending.lock().await.insert(id.clone(), sender);
        if let Some(error) = self.inner.terminal_error.lock().await.as_ref() {
            self.inner.pending.lock().await.remove(&id);
            return Err(error.clone_error());
        }
        let mut message = json!({"jsonrpc":"2.0", "id":id, "method":method});
        if let Some(params) = params {
            message["params"] = params;
        }
        if let Err(error) = self.write(message).await {
            self.inner.pending.lock().await.remove(&id);
            return Err(error);
        }
        receiver.await.unwrap_or(Err(TransportError::Closed))
    }

    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), TransportError> {
        let mut message = json!({"jsonrpc":"2.0", "method":method});
        if let Some(params) = params {
            message["params"] = params;
        }
        self.write(message).await.map(|_| ())
    }

    pub async fn next_notification(&self) -> Option<Value> {
        self.notifications.lock().await.recv().await
    }

    /// PID of the managed child, while it is still present.
    pub async fn pid(&self) -> Option<u32> {
        self.child.lock().await.as_ref().and_then(Child::id)
    }

    async fn write(&self, message: Value) -> Result<(), TransportError> {
        let mut stdin = self.inner.stdin.lock().await;
        let mut bytes = serde_json::to_vec(&message)?;
        bytes.push(b'\n');
        stdin.write_all(&bytes).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<(), TransportError> {
        self.inner.stdin.lock().await.shutdown().await.ok();
        if let Some(mut child) = self.child.lock().await.take() {
            if child.try_wait()?.is_none() {
                child.kill().await?;
            }
            let _ = child.wait().await?;
        }
        Ok(())
    }
}

fn request_key(id: &Value) -> Option<String> {
    match id {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

trait CloneError {
    fn clone_error(&self) -> TransportError;
}
impl CloneError for TransportError {
    fn clone_error(&self) -> TransportError {
        match self {
            TransportError::Io(e) => TransportError::Io(io::Error::new(e.kind(), e.to_string())),
            TransportError::Json(e) => TransportError::Json(serde_json::Error::io(io::Error::new(
                io::ErrorKind::InvalidData,
                e.to_string(),
            ))),
            TransportError::Closed => TransportError::Closed,
            TransportError::Rpc {
                code,
                message,
                data,
            } => TransportError::Rpc {
                code: *code,
                message: message.clone(),
                data: data.clone(),
            },
        }
    }
}
