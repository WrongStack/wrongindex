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

/// Longest wait for a service to answer a new connection's handshake.
pub const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(20);
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
        Self::connect_within(endpoint, token, client_name, HANDSHAKE_DEADLINE).await
    }

    /// [`Self::connect_with`], the whole handshake bounded by `deadline`. A
    /// service holds its endpoint from the moment it starts, before it
    /// accepts its first client (it takes back its indexes first), and a
    /// wedged one may never answer: without a bound every caller - the CLI's
    /// `--wait-seconds`, the desktop's Start - waited as long as that took.
    pub async fn connect_within(
        endpoint: &str,
        token: &str,
        client_name: &str,
        deadline: Duration,
    ) -> Result<Self, CallError> {
        match tokio::time::timeout(deadline, Self::handshake(endpoint, token, client_name)).await {
            Ok(result) => result,
            Err(_) => Err(CallError::Unavailable(format!(
                "{endpoint} did not answer the handshake within {} s (the service may still be starting)",
                deadline.as_secs()
            ))),
        }
    }

    async fn handshake(endpoint: &str, token: &str, client_name: &str) -> Result<Self, CallError> {
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

    /// Turn this connection into an event stream (`events.subscribe`).
    /// Returns the acknowledgement (`instanceId`, `currentSeq`, `gap`) and the
    /// stream; the connection can no longer make calls.
    pub async fn subscribe(
        mut self,
        since_seq: Option<u64>,
        since_instance_id: Option<&str>,
        project_id: Option<&str>,
    ) -> Result<(Value, EventStream), CallError> {
        let ack = self
            .call_raw(
                wrongindex_contracts::method::EVENTS_SUBSCRIBE,
                project_id,
                serde_json::json!({
                    "sinceSeq": since_seq,
                    "sinceInstanceId": since_instance_id,
                }),
                None,
            )
            .await?;
        Ok((
            ack,
            EventStream {
                stream: self.stream,
            },
        ))
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

/// Server-pushed events after `events.subscribe`. Each item is one event
/// object (`seq`, `instanceId`, `kind`, `projectId?`, `data`); a `stream.gap`
/// event means history was lost and the caller must take a snapshot.
pub struct EventStream {
    stream: Stream,
}

impl EventStream {
    /// Next event, or `None` when the daemon closed the stream.
    pub async fn next(&mut self) -> Result<Option<Value>, CallError> {
        Ok(read_frame(&mut self.stream, MAX_FRAME_BYTES).await?)
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use tokio::net::windows::named_pipe::ServerOptions;

    /// A peer that takes the connection and never answers is given up on
    /// within the deadline, as unavailable, and nothing is left waiting.
    #[tokio::test]
    async fn a_service_that_never_answers_the_handshake_is_given_up_on() {
        let name = format!(r"\\.\pipe\wrongindex-test-handshake-{}", std::process::id());
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .unwrap();
        let peer = tokio::spawn(async move {
            server.connect().await.unwrap();
            // Holds the connection; reads nothing, writes nothing.
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(server);
        });
        let started = std::time::Instant::now();
        let result =
            Client::connect_within(&name, "synthetic-token", "t", Duration::from_millis(500)).await;
        let waited = started.elapsed();
        peer.abort();
        match result {
            Err(CallError::Unavailable(why)) => assert!(why.contains("handshake"), "{why}"),
            Err(e) => panic!("expected unavailable, got {e}"),
            Ok(_) => panic!("a peer that never answered was accepted"),
        }
        assert!(waited < Duration::from_secs(5), "waited {waited:?}");
    }
}
