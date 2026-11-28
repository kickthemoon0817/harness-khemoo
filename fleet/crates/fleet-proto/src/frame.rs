use std::io;

use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Framed, LengthDelimitedCodec, LengthDelimitedCodecError};

use crate::hello::{Hello, Role};
use crate::{MAX_FRAME_LEN, PROTOCOL_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("i/o: {0}")]
    Io(#[from] io::Error),
    /// A frame over [`MAX_FRAME_LEN`]. Inbound, the stream cannot be resynchronised, so the
    /// connection must be closed.
    #[error("frame exceeds {MAX_FRAME_LEN} bytes")]
    TooLarge,
    #[error("undecodable frame: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("unencodable message: {0}")]
    Encode(#[source] serde_json::Error),
    /// The peer closed the connection where a frame was required.
    #[error("connection closed")]
    Closed,
    #[error("protocol version mismatch: ours {ours}, peer's {theirs}")]
    VersionMismatch { ours: u32, theirs: u32 },
}

/// A framed connection: each frame is a u32 big-endian length then that many bytes of one
/// JSON document. Generic over the byte stream, so unix sockets, TLS streams and an ssh
/// child's stdio (joined with `tokio::io::join`) all carry the same frames.
pub struct Connection<T> {
    framed: Framed<T, LengthDelimitedCodec>,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Connection<T> {
    pub fn new(io: T) -> Self {
        let codec = LengthDelimitedCodec::builder()
            .big_endian()
            .length_field_type::<u32>()
            .max_frame_length(MAX_FRAME_LEN)
            .new_codec();
        Self {
            framed: Framed::new(io, codec),
        }
    }

    pub async fn send<M: Serialize>(&mut self, msg: &M) -> Result<(), ProtoError> {
        let bytes = serde_json::to_vec(msg).map_err(ProtoError::Encode)?;
        if bytes.len() > MAX_FRAME_LEN {
            return Err(ProtoError::TooLarge);
        }
        self.framed.send(bytes.as_slice()).await?;
        Ok(())
    }

    /// The next message, or `None` when the peer closed the connection between frames.
    pub async fn recv<M: DeserializeOwned>(&mut self) -> Result<Option<M>, ProtoError> {
        match self.framed.next().await {
            None => Ok(None),
            Some(Err(e)) if is_oversize(&e) => Err(ProtoError::TooLarge),
            Some(Err(e)) => Err(ProtoError::Io(e)),
            Some(Ok(frame)) => serde_json::from_slice(&frame)
                .map(Some)
                .map_err(ProtoError::Decode),
        }
    }

    /// Runs the client side of the handshake and returns the role the server granted.
    /// The client must not send a request before this returns.
    pub async fn client_hello(&mut self, role: Role) -> Result<Role, ProtoError> {
        self.send(&Hello {
            protocol_version: PROTOCOL_VERSION,
            role,
        })
        .await?;
        let theirs = self.recv_hello().await?;
        Ok(theirs.role)
    }

    /// Runs the server side of the handshake. `max_role` is what the transport
    /// authenticated; the grant is the lesser of it and the role the client asked for.
    /// The server's hello is sent even on a version mismatch, granting `monitor`, so the
    /// client learns which version it reached; the caller then sends a `version_mismatch`
    /// error with id 0 and closes the connection.
    pub async fn server_hello(&mut self, max_role: Role) -> Result<Role, ProtoError> {
        let theirs = match self.recv_hello().await {
            Err(e @ ProtoError::VersionMismatch { .. }) => {
                self.send(&Hello {
                    protocol_version: PROTOCOL_VERSION,
                    role: Role::Monitor,
                })
                .await?;
                return Err(e);
            }
            other => other?,
        };
        let granted = theirs.role.min(max_role);
        self.send(&Hello {
            protocol_version: PROTOCOL_VERSION,
            role: granted,
        })
        .await?;
        Ok(granted)
    }

    /// Reads a hello, comparing the version before anything else so that a later peer's
    /// hello, whose other fields may not decode here, still reports the mismatch.
    async fn recv_hello(&mut self) -> Result<Hello, ProtoError> {
        #[derive(serde::Deserialize)]
        struct Version {
            protocol_version: u32,
        }
        let value: serde_json::Value = self.recv().await?.ok_or(ProtoError::Closed)?;
        let Version { protocol_version } =
            Version::deserialize(&value).map_err(ProtoError::Decode)?;
        check_version(protocol_version)?;
        Hello::deserialize(&value).map_err(ProtoError::Decode)
    }

    /// The underlying stream, and any bytes already read past the last frame, which a
    /// relay must forward before copying the stream.
    pub fn into_parts(self) -> (T, BytesMut) {
        let parts = self.framed.into_parts();
        (parts.io, parts.read_buf)
    }
}

fn check_version(theirs: u32) -> Result<(), ProtoError> {
    if theirs == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtoError::VersionMismatch {
            ours: PROTOCOL_VERSION,
            theirs,
        })
    }
}

fn is_oversize(e: &io::Error) -> bool {
    matches!(e.get_ref(), Some(inner) if inner.is::<LengthDelimitedCodecError>())
}
