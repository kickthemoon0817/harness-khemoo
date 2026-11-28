//! Asking many devices at once, and dispatching to one of them.

use fleet_proto::output::{
    DeviceError, DeviceReport, DeviceSession, OUTPUT_VERSION, SessionsOutput, StatusOutput,
};
use fleet_proto::{DispatchSpec, ErrorCode, Role, Session, SessionId, StatusReport, WireError};
use futures_util::stream::{self, StreamExt};

use crate::client::Client;
use crate::placement;
use crate::registry::{Device, Timeouts};

/// How many devices are asked at once. Each query holds a socket or an ssh child, so a
/// large registry is walked in waves rather than all at once.
pub const MAX_IN_FLIGHT: usize = 16;

/// Runs `f` for every device with at most [`MAX_IN_FLIGHT`] in flight, results in the
/// order of `devices`.
async fn each<'a, T, F, Fut>(devices: &[&'a Device], f: F) -> Vec<T>
where
    F: Fn(&'a Device) -> Fut,
    Fut: Future<Output = T>,
{
    stream::iter(devices.iter().copied())
        .map(f)
        .buffered(MAX_IN_FLIGHT)
        .collect()
        .await
}

/// `fleet status`: every device's status and sessions. A device that fails is reported
/// with its error; the others are unaffected.
pub async fn status(devices: &[&Device], timeouts: Timeouts, now: u64) -> StatusOutput {
    let reports = each(devices, |device| async move {
        let answer = async {
            let mut client = Client::connect(device, timeouts, Role::Monitor).await?;
            let status = client.status().await?;
            let (sessions, truncated) = client.sessions().await?;
            Ok::<_, WireError>((status, sessions, truncated))
        }
        .await;
        let (ok, error, status, sessions, sessions_truncated) = match answer {
            Ok((status, sessions, truncated)) => (true, None, Some(status), sessions, truncated),
            Err(e) => (false, Some(e), None, Vec::new(), false),
        };
        DeviceReport {
            name: device.name.clone(),
            transport: device.transport.kind(),
            ok,
            error,
            status,
            sessions,
            sessions_truncated,
        }
    })
    .await;
    StatusOutput {
        version: OUTPUT_VERSION,
        generated_at: now,
        devices: reports,
    }
}

/// `fleet sessions`: every device's sessions, by device then start time.
pub async fn sessions(
    devices: &[&Device],
    timeouts: Timeouts,
    now: u64,
    running_only: bool,
) -> SessionsOutput {
    let answers = each(devices, |device| async move {
        let answer = async {
            let mut client = Client::connect(device, timeouts, Role::Monitor).await?;
            client.sessions().await
        }
        .await;
        (device, answer)
    })
    .await;
    let mut out = SessionsOutput {
        version: OUTPUT_VERSION,
        generated_at: now,
        sessions: Vec::new(),
        errors: Vec::new(),
        truncated: Vec::new(),
    };
    for (device, answer) in answers {
        match answer {
            Ok((mut list, truncated)) => {
                list.retain(|s| !running_only || s.state.is_running());
                list.sort_by_key(|s: &Session| s.started_at);
                out.sessions
                    .extend(list.into_iter().map(|session| DeviceSession {
                        device: device.name.clone(),
                        session,
                    }));
                if truncated {
                    out.truncated.push(device.name.clone());
                }
            }
            Err(error) => out.errors.push(DeviceError {
                device: device.name.clone(),
                error,
            }),
        }
    }
    out
}

/// A dispatch that a device accepted, with the dispatch-role connection it went over,
/// on which the caller may go on to `wait`.
pub struct Dispatched<'a> {
    pub device: &'a Device,
    pub session: SessionId,
    pub client: Client,
}

pub async fn dispatch_to(
    device: &Device,
    timeouts: Timeouts,
    spec: DispatchSpec,
) -> Result<Dispatched<'_>, WireError> {
    let mut client = Client::connect(device, timeouts, Role::Dispatch).await?;
    let session = client.dispatch(spec).await?;
    Ok(Dispatched {
        device,
        session,
        client,
    })
}

/// `--device auto`: asks every `auto` device for its status, ranks them with
/// [`placement::rank`], and dispatches to each in turn until one accepts. On failure the
/// error carries the device it is about, when there is one.
pub async fn dispatch_auto<'a>(
    devices: &[&'a Device],
    timeouts: Timeouts,
    spec: &DispatchSpec,
) -> Result<Dispatched<'a>, (Option<&'a Device>, WireError)> {
    let candidates: Vec<&Device> = devices.iter().copied().filter(|d| d.auto).collect();
    let probes: Vec<Option<StatusReport>> = each(&candidates, |device| async move {
        let mut client = Client::connect(device, timeouts, Role::Monitor)
            .await
            .ok()?;
        client.status().await.ok()
    })
    .await;
    let refs: Vec<Option<&StatusReport>> = probes.iter().map(Option::as_ref).collect();
    let order = placement::rank(&refs, &spec.adapter, spec.model.as_ref()).map_err(|code| {
        let what = match &spec.model {
            Some(m) => format!("adapter {} with model {m}", spec.adapter),
            None => format!("adapter {}", spec.adapter),
        };
        let message = match code {
            ErrorCode::Unreachable => "no auto device answered".to_owned(),
            ErrorCode::Busy => format!("every auto device with {what} is at capacity"),
            _ => format!("no auto device offers {what}"),
        };
        (None, WireError::new(code, message))
    })?;
    let (last, rest) = order
        .split_last()
        .expect("rank returns at least one device");
    for &i in rest {
        let device = candidates[i];
        let refused = async {
            let Ok(mut client) = Client::connect(device, timeouts, Role::Dispatch).await else {
                return None;
            };
            match client.dispatch(spec.clone()).await {
                Ok(session) => Some(Ok(Dispatched {
                    device,
                    session,
                    client,
                })),
                Err(e) if is_refusal(&e) => None,
                Err(e) => Some(Err((Some(device), e))),
            }
        };
        if let Some(outcome) = refused.await {
            return outcome;
        }
    }
    let device = candidates[*last];
    dispatch_to(device, timeouts, spec.clone())
        .await
        .map_err(|e| (Some(device), e))
}

/// Whether a failed dispatch certainly started nothing: the device answered with a refusal.
/// A timeout, a lost connection or a garbled answer after the request went out may hide a
/// session that did start, so trying another device could run the agent twice.
fn is_refusal(e: &WireError) -> bool {
    !matches!(
        e.code,
        ErrorCode::Timeout | ErrorCode::Unreachable | ErrorCode::Protocol | ErrorCode::Internal
    )
}
