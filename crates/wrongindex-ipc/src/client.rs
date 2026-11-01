//! Rust IPC client shared by the CLI and the desktop bridge.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use wrongindex_contracts::{
    AuthRequest, AuthResponse, ErrorBody, ErrorCode, Hello, MAX_FRAME_BYTES, PROTOCOL_MAJOR,
    PROTOCOL_MINOR, Request, Response,
};

use crate::endpoint::{DataRoot, Discovery};
use crate::frame::{FrameError, read_frame, write_frame};

#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// No daemon is reachable for this data root (missing/stale discovery).
    #[error("WrongIndex service is not running: {0}")]
    Unavailable(String),
    #[error("transport: {0}")]
    Transport(#[from] FrameError),
    #[error("{0}")]
    Remote(ErrorBody),
    #[error("unexpected response: {0}")]
    Protocol(String),
}

impl CallError {
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            CallError::Remote(e) => Some(e.code),
            _ => None,
        }
    }
}

#[cfg(windows)]
type Stream = tokio::net::windows::named_pipe::NamedPipeClient;
#[cfg(unix)]
type Stream = tokio::net::UnixStream;

pub struct Client {
    stream: Stream,
    hello: Hello,
    next_id: AtomicU64,
}

impl Client {
    /// Discover the daemon for `root`, connect, and authenticate.
    pub async fn connect(root: &DataRoot, client_name: &str) -> Result<Self, CallError> {
        let discovery = Discovery::load(root).map_err(|e| {
            CallError::Unavailable(format!(
                "no discovery metadata in {}: {e}",
                root.path().display()
            ))
        })?;
        Self::connect_with(&discovery.info.endpoint, &discovery.token, client_name).await
    }

    /// Connect with an explicit endpoint and token (tests and diagnostics).
    pub async fn connect_with(
        endpoint: &str,
        token: &str,
        client_name: &str,
    ) -> Result<Self, CallError> {
        let mut stream = open(endpoint).await?;
        let auth = AuthRequest {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            token: token.to_string(),
            client_name: client_name.to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            supported_profile_ids: vec![wrongindex_contracts::BASELINE_PROFILE_ID.to_string()],
        };
        write_frame(&mut stream, &auth, MAX_FRAME_BYTES).await?;
        let reply: AuthResponse = read_frame(&mut stream, MAX_FRAME_BYTES)
            .await?
            .ok_or_else(|| CallError::Protocol("connection closed during authentication".into()))?;
        match reply {
            AuthResponse::Ok { hello } => Ok(Self {
                stream,
                hello,
                next_id: AtomicU64::new(1),
            }),
            AuthResponse::Error { error } => Err(CallError::Remote(error)),
        }
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    /// Send one request and wait for its response.
    pub async fn call_raw(
        &mut self,
        method: &str,
        project_id: Option<&str>,
        params: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Value, CallError> {
        let request_id = format!("r{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let req = Request {
            protocol: PROTOCOL_MAJOR,
            request_id: request_id.clone(),
            method: method.to_string(),
            project_id: project_id.map(str::to_string),
            params,
            idempotency_key: idempotency_key.map(str::to_string),
        };
        write_frame(&mut self.stream, &req, MAX_FRAME_BYTES).await?;
        let resp: Response = read_frame(&mut self.stream, MAX_FRAME_BYTES)
            .await?
            .ok_or_else(|| CallError::Protocol("connection closed before response".into()))?;
        if resp.request_id != request_id {
            return Err(CallError::Protocol(format!(
                "response id {} does not match request {request_id}",
                resp.request_id
            )));
        }
        match (resp.result, resp.error) {
            (_, Some(err)) => Err(CallError::Remote(err)),
            (Some(v), None) => Ok(v),
            (None, None) => Ok(Value::Null),
        }
    }

    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &mut self,
        method: &str,
        project_id: Option<&str>,
        params: &P,
    ) -> Result<R, CallError> {
        let params =
            serde_json::to_value(params).map_err(|e| CallError::Protocol(e.to_string()))?;
        let v = self.call_raw(method, project_id, params, None).await?;
        serde_json::from_value(v).map_err(|e| CallError::Protocol(e.to_string()))
    }
}

#[cfg(windows)]
async fn open(endpoint: &str) -> Result<Stream, CallError> {
    crate::win::connect(endpoint, Duration::from_secs(5))
        .await
        .map_err(|e| CallError::Unavailable(format!("cannot open {endpoint}: {e}")))
}

/// Unix sockets are milestone M8; the client side is wired so portable crates
/// build, but no daemon listens on Unix yet.
#[cfg(unix)]
async fn open(endpoint: &str) -> Result<Stream, CallError> {
    let _ = Duration::ZERO;
    tokio::net::UnixStream::connect(endpoint)
        .await
        .map_err(|e| CallError::Unavailable(format!("cannot open {endpoint}: {e}")))
}
