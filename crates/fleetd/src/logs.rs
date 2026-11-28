//! The `logs` and `wait` streams: a session log read as `event` frames, then followed.

use std::io::SeekFrom;
use std::sync::Arc;
use std::time::Duration;

use fleet_proto::{ErrorCode, MAX_EVENT_DATA, ResponseBody, SessionId, SessionState, WireError};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;

use crate::dispatch::Sessions;

/// Lossy decoding turns each invalid byte into the 3-byte U+FFFD, so a read this size, plus
/// a carried partial character, always fits in one event.
const READ_CHUNK: usize = MAX_EVENT_DATA / 3 - 3;
const TAIL_BLOCK: u64 = 64 * 1024;
const HEARTBEAT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy)]
pub(crate) enum Start {
    Tail(u32),
    Beginning,
}

/// Sends the stream's frames into `tx`, ending with `end` or `error`. A closed `tx` (the
/// client went away) ends it quietly.
pub(crate) async fn produce(
    sessions: Arc<Sessions>,
    id: SessionId,
    start: Start,
    follow: bool,
    tx: mpsc::Sender<ResponseBody>,
) {
    if let Err(e) = run(&sessions, &id, start, follow, &tx).await {
        let _ = tx.send(ResponseBody::Error(e)).await;
    }
}

async fn run(
    sessions: &Sessions,
    id: &SessionId,
    start: Start,
    follow: bool,
    tx: &mpsc::Sender<ResponseBody>,
) -> Result<(), WireError> {
    let gone = || WireError::new(ErrorCode::NotFound, "no such session");
    let unreadable = |_| WireError::new(ErrorCode::Internal, "could not read the session log");
    let mut handle = sessions.log(id).ok_or_else(gone)?;
    let mut file = File::open(&handle.path).await.map_err(unreadable)?;
    let offset = match start {
        Start::Beginning => 0,
        Start::Tail(lines) => tail_offset(&mut file, lines).await.map_err(unreadable)?,
    };
    file.seek(SeekFrom::Start(offset))
        .await
        .map_err(unreadable)?;

    let mut buf = vec![0u8; READ_CHUNK];
    let mut carry = Utf8Carry::default();
    let mut state = sessions.state(id).ok_or_else(gone)?;
    loop {
        handle.progress.borrow_and_update();
        // The state is read before the log: once it shows the end, every write is in the file.
        if let Some(now) = sessions.state(id) {
            state = now;
        }
        loop {
            let n = file.read(&mut buf).await.map_err(unreadable)?;
            if n == 0 {
                break;
            }
            let data = carry.push(&buf[..n]);
            if !data.is_empty() {
                emit(tx, ResponseBody::Event { data }).await?;
            }
        }
        if !follow || !state.is_running() {
            let data = carry.finish();
            if !data.is_empty() {
                emit(tx, ResponseBody::Event { data }).await?;
            }
            return emit(tx, ResponseBody::End { state }).await;
        }
        tokio::select! {
            changed = handle.progress.changed() => {
                // The session was pruned while followed; report what it last was.
                if changed.is_err() {
                    let state = sessions.state(id).unwrap_or(SessionState::Failed {
                        message: "the session was deleted".to_owned(),
                    });
                    return emit(tx, ResponseBody::End { state }).await;
                }
            }
            () = tokio::time::sleep(HEARTBEAT) => {
                emit(tx, ResponseBody::Event { data: String::new() }).await?;
            }
        }
    }
}

/// A closed channel means the client left; the error stops the stream, and sending it
/// fails just as quietly.
async fn emit(tx: &mpsc::Sender<ResponseBody>, body: ResponseBody) -> Result<(), WireError> {
    tx.send(body)
        .await
        .map_err(|_| WireError::new(ErrorCode::Internal, "the client left"))
}

/// Where the last `lines` lines of the file begin, found by reading backwards one block at
/// a time, so memory stays bounded however long the lines are.
async fn tail_offset(file: &mut File, lines: u32) -> std::io::Result<u64> {
    let len = file.metadata().await?.len();
    let mut seen = 0;
    let mut end = len;
    let mut block = Vec::new();
    while end > 0 {
        let start = end.saturating_sub(TAIL_BLOCK);
        block.resize(usize::try_from(end - start).unwrap_or(0), 0);
        file.seek(SeekFrom::Start(start)).await?;
        file.read_exact(&mut block).await?;
        for (i, &b) in block.iter().enumerate().rev() {
            let at = start + i as u64;
            // A newline that ends the file closes the last line rather than starting one.
            if b == b'\n' && at + 1 != len {
                seen += 1;
                if seen == lines {
                    return Ok(at + 1);
                }
            }
        }
        end = start;
    }
    Ok(0)
}

/// Decodes a byte stream into UTF-8 chunks, holding back a character split across reads and
/// replacing invalid bytes with U+FFFD.
#[derive(Default)]
struct Utf8Carry {
    pending: Vec<u8>,
}

impl Utf8Carry {
    fn push(&mut self, bytes: &[u8]) -> String {
        let mut all = std::mem::take(&mut self.pending);
        all.extend_from_slice(bytes);
        let split = all.len() - incomplete_tail(&all);
        self.pending = all.split_off(split);
        String::from_utf8_lossy(&all).into_owned()
    }

    fn finish(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned()
    }
}

/// How many bytes at the end of `b` begin a UTF-8 character that has not finished yet.
fn incomplete_tail(b: &[u8]) -> usize {
    for back in 1..=b.len().min(3) {
        let byte = b[b.len() - back];
        if byte & 0xC0 == 0x80 {
            continue;
        }
        let need = match byte {
            0xF0.. => 4,
            0xE0.. => 3,
            0xC0.. => 2,
            _ => 1,
        };
        return if need > back { back } else { 0 };
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_split_across_reads_is_kept_whole() {
        let mut c = Utf8Carry::default();
        let bytes = "aé€😀".as_bytes();
        let mut out = String::new();
        for b in bytes {
            out.push_str(&c.push(std::slice::from_ref(b)));
        }
        out.push_str(&c.finish());
        assert_eq!(out, "aé€😀");
    }

    #[test]
    fn invalid_bytes_become_replacement_characters() {
        let mut c = Utf8Carry::default();
        assert_eq!(c.push(b"a\xffb"), "a\u{FFFD}b");
        assert_eq!(c.push(b"\xe2\x82"), "");
        assert_eq!(c.finish(), "\u{FFFD}");
    }

    #[test]
    fn worst_case_event_fits() {
        let mut c = Utf8Carry::default();
        let _ = c.push(&[0xF0, 0x9F, 0x98]);
        let out = c.push(&vec![0xFF; READ_CHUNK]);
        assert!(out.len() <= MAX_EVENT_DATA, "{}", out.len());
    }

    #[tokio::test]
    async fn tail_finds_the_last_lines() {
        let dir = std::env::temp_dir().join(format!("fleetd-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log");
        let body: String = (1..=50).map(|i| i.to_string() + "\n").collect();
        std::fs::write(&path, &body).unwrap();
        let mut f = File::open(&path).await.unwrap();
        let at = tail_offset(&mut f, 3).await.unwrap();
        assert_eq!(&body[usize::try_from(at).unwrap()..], "48\n49\n50\n");
        assert_eq!(tail_offset(&mut f, 500).await.unwrap(), 0);

        std::fs::write(&path, "x\ny").unwrap();
        let mut f = File::open(&path).await.unwrap();
        assert_eq!(tail_offset(&mut f, 1).await.unwrap(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
