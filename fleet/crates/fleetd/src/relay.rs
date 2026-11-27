//! `fleetd --stdio`: the ssh transport's relay from stdin and stdout to the daemon's socket.

use std::path::Path;

use fleet_proto::{Connection, ErrorCode, PROTOCOL_VERSION, ProtoError, Response, Role};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::server::send_hello;

/// Relays one connection from `client` (stdin and stdout, under ssh) to the daemon at
/// `socket`, asking for at most `cap`. Returns the process exit code: 0 once either side
/// has closed, 1 if the relay could not be set up.
pub async fn run<T: AsyncRead + AsyncWrite + Unpin>(client: T, socket: &Path, cap: Role) -> i32 {
    let mut client = Connection::new(client);
    let asked = match client.recv_hello().await {
        Ok(hello) => hello.role,
        Err(ProtoError::VersionMismatch { theirs, .. }) => {
            let msg = format!("fleetd speaks protocol {PROTOCOL_VERSION}, the client {theirs}");
            refuse(&mut client, ErrorCode::VersionMismatch, &msg).await;
            return 1;
        }
        Err(ProtoError::Decode(_)) => {
            refuse(
                &mut client,
                ErrorCode::BadRequest,
                "the hello did not decode",
            )
            .await;
            return 1;
        }
        Err(ProtoError::TooLarge) => {
            refuse(&mut client, ErrorCode::TooLarge, "the frame exceeds 1 MiB").await;
            return 1;
        }
        Err(_) => return 1,
    };
    let Ok(stream) = UnixStream::connect(socket).await else {
        refuse(&mut client, ErrorCode::Unavailable, "no fleetd is running").await;
        return 1;
    };
    let mut daemon = Connection::new(stream);
    let granted = match daemon.client_hello(asked.min(cap)).await {
        Ok(role) => role,
        Err(ProtoError::VersionMismatch { theirs, .. }) => {
            let msg = format!("the relay speaks protocol {PROTOCOL_VERSION}, the daemon {theirs}");
            refuse(&mut client, ErrorCode::VersionMismatch, &msg).await;
            return 1;
        }
        Err(_) => {
            refuse(
                &mut client,
                ErrorCode::Unavailable,
                "the daemon closed the handshake",
            )
            .await;
            return 1;
        }
    };
    if !send_hello(&mut client, granted).await {
        return 1;
    }
    tracing::info!(role = %granted, "stdio relay connected");

    let (client, client_early) = client.into_parts();
    let (daemon, daemon_early) = daemon.into_parts();
    let (mut client_rx, mut client_tx) = tokio::io::split(client);
    let (mut daemon_rx, mut daemon_tx) = daemon.into_split();
    let forward = async {
        daemon_tx.write_all(&client_early).await?;
        tokio::io::copy(&mut client_rx, &mut daemon_tx).await
    };
    let back = async {
        client_tx.write_all(&daemon_early).await?;
        let n = tokio::io::copy(&mut daemon_rx, &mut client_tx).await?;
        client_tx.flush().await?;
        Ok::<_, std::io::Error>(n)
    };
    tokio::select! {
        _ = forward => {}
        _ = back => {}
    }
    0
}

/// Answers a client the relay cannot serve at all (its config does not load), so the hub
/// reports why instead of a broken handshake.
pub async fn refuse_client<T: AsyncRead + AsyncWrite + Unpin>(
    client: T,
    code: ErrorCode,
    message: &str,
) {
    let mut client = Connection::new(client);
    if !matches!(
        client.recv_hello().await,
        Err(ProtoError::Io(_) | ProtoError::Closed)
    ) {
        refuse(&mut client, code, message).await;
    }
}

async fn refuse<T: AsyncRead + AsyncWrite + Unpin>(
    client: &mut Connection<T>,
    code: ErrorCode,
    message: &str,
) {
    if send_hello(client, Role::Monitor).await {
        let _ = client.send(&Response::error(0, code, message)).await;
    }
}
