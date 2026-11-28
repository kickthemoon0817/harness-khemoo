//! One handshaken connection to a device, and the requests the hub makes on it.

use std::num::NonZeroU64;
use std::time::Duration;

use fleet_proto::{
    Connection, DispatchSpec, ErrorCode, ProtoError, Request, RequestBody, RequestId, Response,
    ResponseBody, Role, Session, SessionId, SessionState, StatusReport, WireError,
};
use tokio::time::timeout;

use crate::registry::{Device, Timeouts};
use crate::transport::{self, Stream};

pub struct Client {
    conn: Connection<Stream>,
    last_id: u64,
    timeouts: Timeouts,
}

/// One item of a `logs` or `wait` stream. Empty keep-alive events are never returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chunk {
    Data(String),
    End(SessionState),
}

fn protocol(msg: impl Into<String>) -> WireError {
    WireError::new(ErrorCode::Protocol, msg)
}

fn unexpected(body: &ResponseBody) -> WireError {
    let kind = match body {
        ResponseBody::Status(_) => "status",
        ResponseBody::Sessions { .. } => "sessions",
        ResponseBody::Dispatched { .. } => "dispatched",
        ResponseBody::Cancelled { .. } => "cancelled",
        ResponseBody::Event { .. } => "event",
        ResponseBody::End { .. } => "end",
        ResponseBody::Error(_) => "error",
    };
    protocol(format!("unexpected {kind} frame"))
}

impl Client {
    pub async fn connect(
        device: &Device,
        timeouts: Timeouts,
        role: Role,
    ) -> Result<Self, WireError> {
        let conn = transport::connect(device, &timeouts, role).await?;
        Ok(Self {
            conn,
            last_id: 0,
            timeouts,
        })
    }

    pub async fn status(&mut self) -> Result<StatusReport, WireError> {
        match self.call(RequestBody::Status {}).await? {
            ResponseBody::Status(report) => Ok(*report),
            other => Err(unexpected(&other)),
        }
    }

    /// The device's sessions and whether it truncated the list.
    pub async fn sessions(&mut self) -> Result<(Vec<Session>, bool), WireError> {
        match self.call(RequestBody::Sessions {}).await? {
            ResponseBody::Sessions {
                sessions,
                truncated,
            } => Ok((sessions, truncated)),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn dispatch(&mut self, spec: DispatchSpec) -> Result<SessionId, WireError> {
        match self.call(RequestBody::Dispatch(spec)).await? {
            ResponseBody::Dispatched { session } => Ok(session),
            other => Err(unexpected(&other)),
        }
    }

    pub async fn cancel(&mut self, session: SessionId) -> Result<SessionId, WireError> {
        let asked = session.clone();
        match self.call(RequestBody::Cancel { session }).await? {
            ResponseBody::Cancelled { session } if session == asked => Ok(session),
            ResponseBody::Cancelled { .. } => Err(protocol("cancelled another session")),
            other => Err(unexpected(&other)),
        }
    }

    /// Starts a `logs` or `wait` stream; read it with [`Client::next_chunk`] until
    /// [`Chunk::End`] or an error.
    pub async fn stream(&mut self, body: RequestBody) -> Result<(), WireError> {
        self.send(body).await
    }

    pub async fn next_chunk(&mut self) -> Result<Chunk, WireError> {
        loop {
            match self.recv(self.timeouts.stream_idle).await? {
                ResponseBody::Event { data } if data.is_empty() => {}
                ResponseBody::Event { data } => return Ok(Chunk::Data(data)),
                ResponseBody::End { state } => return Ok(Chunk::End(state)),
                other => return Err(unexpected(&other)),
            }
        }
    }

    async fn call(&mut self, body: RequestBody) -> Result<ResponseBody, WireError> {
        self.send(body).await?;
        self.recv(self.timeouts.request).await
    }

    async fn send(&mut self, body: RequestBody) -> Result<(), WireError> {
        self.last_id += 1;
        let id: RequestId =
            NonZeroU64::new(self.last_id).ok_or_else(|| protocol("request ids exhausted"))?;
        let request = Request { id, body };
        match timeout(self.timeouts.request, self.conn.send(&request)).await {
            Ok(Ok(())) => Ok(()),
            // A server refusing the connection sends an error with id 0 and closes, which can
            // make our write fail; that error is the real reason, so read it if it is there.
            Ok(Err(e)) => match self.recv(self.timeouts.request).await {
                Err(device_error)
                    if !matches!(
                        device_error.code,
                        ErrorCode::Unreachable | ErrorCode::Timeout | ErrorCode::Protocol
                    ) =>
                {
                    Err(device_error)
                }
                _ => Err(WireError::new(
                    ErrorCode::Unreachable,
                    format!("connection lost: {e}"),
                )),
            },
            Err(_) => Err(transport::no_answer(self.timeouts.request)),
        }
    }

    /// The next frame answering the current request, which is `last_id`. An `error` frame,
    /// for the request or for the whole connection (id 0), becomes `Err`.
    async fn recv(&mut self, wait: Duration) -> Result<ResponseBody, WireError> {
        let response: Response = match timeout(wait, self.conn.recv()).await {
            Err(_) => return Err(transport::no_answer(wait)),
            Ok(Ok(Some(r))) => r,
            Ok(Ok(None)) => {
                return Err(WireError::new(
                    ErrorCode::Unreachable,
                    "the device closed the connection",
                ));
            }
            Ok(Err(e @ (ProtoError::Decode(_) | ProtoError::TooLarge))) => {
                return Err(protocol(e.to_string()));
            }
            Ok(Err(e)) => {
                return Err(WireError::new(
                    ErrorCode::Unreachable,
                    format!("connection lost: {e}"),
                ));
            }
        };
        match response {
            Response {
                body: ResponseBody::Error(e),
                id,
            } if id == 0 || id == self.last_id => Err(e),
            Response { id, .. } if id != self.last_id => Err(protocol(format!(
                "answer to request {id}, expected {}",
                self.last_id
            ))),
            Response { body, .. } => Ok(body),
        }
    }
}
