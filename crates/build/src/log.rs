//! Bounded access to files named by a builder's run summary.
use base64::{Engine, engine::general_purpose::STANDARD};
use hgl_ai_bus::Bus;
use hgl_ai_proto::build::{Summary, check_build_id, new_build_id, reply_seat};
use hgl_ai_proto::{Kind, Note, Seat};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path};
use std::time::Duration;

pub const MAX: u64 = 64 << 10;
#[derive(Serialize, Deserialize)]
struct Request {
    id: String,
    step: Option<String>,
    bytes: Option<String>,
    reply: Seat,
}
#[derive(Serialize, Deserialize)]
struct Response {
    data: Option<String>,
    error: Option<String>,
}

pub fn address(label: &str) -> Result<Seat, hgl_ai_proto::ProtoError> {
    Seat::new(format!("logs-{label}"))
}

pub fn read(
    dir: &Path,
    summary: &Summary,
    step: Option<&str>,
    bytes: Option<&str>,
) -> Result<Vec<u8>, String> {
    let step = match step {
        Some(s) => s,
        None => {
            &summary
                .steps
                .iter()
                .find(|s| !matches!(s.status.as_str(), "pass" | "skipped"))
                .or(summary.steps.last())
                .ok_or("build has run no step yet")?
                .name
        }
    };
    let rel = if let Some(s) = summary
        .steps
        .iter()
        .find(|s| s.name == step && s.log.is_some())
    {
        format!("{}.log", s.name)
    } else if summary
        .steps
        .iter()
        .any(|s| s.artifacts.iter().any(|a| a == step))
    {
        step.to_string()
    } else {
        return Err("file is not named by the build summary".into());
    };
    let path = Path::new(&rel);
    if !path.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err("invalid log path".into());
    }
    let mut f = open_file(dir, path).map_err(|e| e.to_string())?;
    let metadata = f.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("log is not a regular file".into());
    }
    let len = metadata.len();
    let (start, end) = if let Some(r) = bytes {
        let (a, b) = r.split_once('-').ok_or("--bytes START-END")?;
        let a: u64 = a.parse().map_err(|_| "--bytes START-END")?;
        let b: u64 = b.parse().map_err(|_| "--bytes START-END")?;
        (a.min(len), b.min(len).max(a.min(len)))
    } else {
        (len.saturating_sub(MAX), len)
    };
    let end = end.min(start.saturating_add(MAX));
    f.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    f.take(end - start)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    Ok(data)
}

// Resolve each component under a held directory descriptor; never follow a symlink,
// even if a build replaces one while a request is being served.
fn open_file(dir: &Path, path: &Path) -> std::io::Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    let mut current = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)?;
    let mut parts = path.components().peekable();
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else {
            return Err(std::io::Error::other("invalid path"));
        };
        let name =
            CString::new(name.as_bytes()).map_err(|_| std::io::Error::other("invalid path"))?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if parts.peek().is_some() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: the directory descriptor and NUL-terminated name stay live for openat.
        let fd = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: openat returned a new owned descriptor.
        current = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    Ok(current)
}

pub async fn request(
    bus: &Bus,
    sender: Seat,
    label: &str,
    id: &str,
    step: Option<String>,
    bytes: Option<String>,
) -> Result<Vec<u8>, String> {
    let reply = reply_seat(&new_build_id()).map_err(|e| e.to_string())?;
    let req = Request {
        id: id.into(),
        step,
        bytes,
        reply: reply.clone(),
    };
    let dest = address(label).map_err(|e| e.to_string())?;
    let note = Note::new(
        sender,
        dest.clone(),
        Kind::Note,
        serde_json::to_string(&req).map_err(|e| e.to_string())?,
    );
    bus.send(&note).await.map_err(|e| e.to_string())?;
    let response = bus
        .reply_wait(&reply, Duration::from_secs(15))
        .await
        .map_err(|e| e.to_string())?
        .ok_or("builder did not answer the log request")?;
    if response.sender != dest {
        return Err("unexpected log response sender".into());
    }
    let response: Response = serde_json::from_str(&response.body).map_err(|e| e.to_string())?;
    match response.data {
        Some(data) => STANDARD.decode(data).map_err(|e| e.to_string()),
        None => Err(response
            .error
            .unwrap_or_else(|| "invalid log response".into())),
    }
}

pub async fn serve(
    bus: std::sync::Arc<Bus>,
    lane: std::sync::Arc<crate::service::Lane>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    let addr = address(&lane.label).map_err(|e| e.to_string())?;
    let mut inbox = bus.inbox(&addr).await.map_err(|e| e.to_string())?;
    while !*stop.borrow() {
        let deliveries = tokio::select! { r = inbox.fetch(1, Duration::from_secs(2)) => r.map_err(|e| e.to_string())?, _ = stop.changed() => continue };
        for d in deliveries {
            let req: Request = match serde_json::from_str(d.note.body.trim()) {
                Ok(r) => r,
                Err(_) => {
                    let _ = d.term().await;
                    continue;
                }
            };
            if check_build_id(&req.id).is_err()
                || !req.reply.as_str().starts_with("r-")
                || check_build_id(&req.reply.as_str()[2..]).is_err()
            {
                let _ = d.term().await;
                continue;
            }
            let dir = lane.logs.join(&req.id);
            let result = crate::store::read_status(&dir)
                .ok_or_else(|| "build log is unavailable".to_string())
                .and_then(|s| {
                    if s.id == req.id && s.host == lane.label {
                        read(&dir, &s, req.step.as_deref(), req.bytes.as_deref())
                    } else {
                        Err("build log is unavailable".into())
                    }
                });
            let response = match result {
                Ok(data) => Response {
                    data: Some(STANDARD.encode(data)),
                    error: None,
                },
                Err(error) => Response {
                    data: None,
                    error: Some(error),
                },
            };
            let note = Note::new(
                addr.clone(),
                req.reply,
                Kind::Note,
                serde_json::to_string(&response).unwrap(),
            );
            if bus.send(&note).await.is_ok() {
                let _ = d.ack().await;
            } else {
                let _ = d.nak(Duration::from_secs(2)).await;
            }
        }
    }
    Ok(())
}
