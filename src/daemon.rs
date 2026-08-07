//! Resident daemon and JSONL client protocol for durable background Jobs.

use crate::{
    ApprovalDecision,
    job::{JobDaemonStatus, JobManager, JobSnapshot, JobSummary},
    session::SessionId,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::watch,
    time::sleep,
};
use uuid::Uuid;

const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone)]
pub struct DaemonPaths {
    pub root: PathBuf,
    pub socket: PathBuf,
    pub pid: PathBuf,
}

impl DaemonPaths {
    pub fn discover() -> Result<Self> {
        let root = match std::env::var_os("NOYA_DATA_DIR") {
            Some(path) => PathBuf::from(path),
            None => dirs::home_dir()
                .context("cannot determine the user home directory")?
                .join(".noya"),
        };
        Ok(Self {
            socket: root.join("daemon.sock"),
            pid: root.join("daemon.pid"),
            root,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonRequest {
    Hello { client_id: String },
    Prompt { request_id: String, input: String },
    Submit {
        request_id: String,
        source_session_id: Option<SessionId>,
        input: String,
    },
    ListJobs { request_id: String },
    JobStatus { request_id: String, job_id: Uuid },
    JobAttach {
        request_id: String,
        job_id: Uuid,
        cursor: Option<u64>,
    },
    JobCancel { request_id: String, job_id: Uuid },
    JobApprove { request_id: String, job_id: Uuid },
    JobReject { request_id: String, job_id: Uuid },
    JobRetry { request_id: String, job_id: Uuid },
    Detach,
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonFrame {
    Hello {
        protocol: u32,
        generation: Uuid,
        client_id: String,
    },
    Status { status: DaemonStatus },
    JobSnapshot { snapshot: JobSnapshot },
    JobEvent {
        job_id: Uuid,
        event: crate::job::JobEventRecord,
    },
    Response {
        request_id: String,
        ok: bool,
        message: String,
        job_id: Option<Uuid>,
    },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub generation: Uuid,
    pub capacity: usize,
    pub running: usize,
    pub queued: usize,
    pub jobs: Vec<JobSummary>,
}

pub async fn serve(manager: JobManager) -> Result<()> {
    let paths = DaemonPaths::discover()?;
    fs::create_dir_all(&paths.root)
        .with_context(|| format!("create daemon directory {}", paths.root.display()))?;
    if paths.socket.exists() {
        if UnixStream::connect(&paths.socket).await.is_ok() {
            bail!("daemon is already running at {}", paths.socket.display());
        }
        fs::remove_file(&paths.socket)
            .with_context(|| format!("remove stale daemon socket {}", paths.socket.display()))?;
    }
    let listener = UnixListener::bind(&paths.socket)
        .with_context(|| format!("bind daemon socket {}", paths.socket.display()))?;
    fs::write(&paths.pid, std::process::id().to_string())?;
    manager.start().await;
    let generation = Uuid::new_v4();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accept daemon client")?;
                let client_manager = manager.clone();
                let client_shutdown = shutdown_tx.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_client(stream, client_manager, generation, client_shutdown).await {
                        tracing::debug!(%error, "daemon client disconnected");
                    }
                });
            }
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }
    manager.stop().await;
    let _ = fs::remove_file(&paths.socket);
    let _ = fs::remove_file(&paths.pid);
    Ok(())
}

async fn handle_client(
    stream: UnixStream,
    manager: JobManager,
    generation: Uuid,
    shutdown: watch::Sender<bool>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let hello = read_request(&mut lines).await?;
    let DaemonRequest::Hello { client_id } = hello else {
        bail!("first daemon request must be hello");
    };
    ensure!(!client_id.trim().is_empty(), "daemon client ID cannot be empty");
    write_frame(
        &mut writer,
        &DaemonFrame::Hello {
            protocol: PROTOCOL_VERSION,
            generation,
            client_id,
        },
    )
    .await?;
    let mut events = manager.subscribe();
    let mut attached = HashMap::new();

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line.context("read daemon request")? else { break; };
                let request: DaemonRequest = serde_json::from_str(&line).context("decode daemon request")?;
                match request {
                    DaemonRequest::Hello { .. } => send_error(&mut writer, "hello already completed").await?,
                    DaemonRequest::Prompt { request_id, input } => {
                        let summary = manager.submit(None, input, None).await?;
                        send_response(&mut writer, request_id, true, format!("Job submitted: {}", summary.job_id), Some(summary.job_id)).await?;
                    }
                    DaemonRequest::Submit { request_id, source_session_id, input } => {
                        let summary = manager.submit(source_session_id, input, None).await?;
                        send_response(&mut writer, request_id, true, format!("Job submitted: {}", summary.job_id), Some(summary.job_id)).await?;
                    }
                    DaemonRequest::ListJobs { request_id } => {
                        let status = status_for(&manager, generation).await;
                        write_frame(&mut writer, &DaemonFrame::Status { status }).await?;
                        send_response(&mut writer, request_id, true, "Job list sent".to_string(), None).await?;
                    }
                    DaemonRequest::JobStatus { request_id, job_id } => {
                        let snapshot = manager.snapshot(job_id, 0).await?;
                        write_frame(&mut writer, &DaemonFrame::JobSnapshot { snapshot }).await?;
                        send_response(&mut writer, request_id, true, "Job status sent".to_string(), Some(job_id)).await?;
                    }
                    DaemonRequest::JobAttach { request_id, job_id, cursor } => {
                        let cursor = cursor.unwrap_or(0);
                        let snapshot = manager.snapshot(job_id, cursor).await?;
                        attached.insert(job_id, snapshot.summary.last_sequence);
                        write_frame(&mut writer, &DaemonFrame::JobSnapshot { snapshot }).await?;
                        send_response(&mut writer, request_id, true, "Job attached".to_string(), Some(job_id)).await?;
                    }
                    DaemonRequest::JobCancel { request_id, job_id } => {
                        let summary = manager.cancel(job_id).await?;
                        send_response(&mut writer, request_id, true, format!("Job is {:?}", summary.status), Some(job_id)).await?;
                    }
                    DaemonRequest::JobApprove { request_id, job_id } => {
                        let summary = manager.approve(job_id, ApprovalDecision::Approve).await?;
                        send_response(&mut writer, request_id, true, format!("Job is {:?}", summary.status), Some(job_id)).await?;
                    }
                    DaemonRequest::JobReject { request_id, job_id } => {
                        let summary = manager.approve(job_id, ApprovalDecision::Reject).await?;
                        send_response(&mut writer, request_id, true, format!("Job is {:?}", summary.status), Some(job_id)).await?;
                    }
                    DaemonRequest::JobRetry { request_id, job_id } => {
                        let summary = manager.retry(job_id).await?;
                        send_response(&mut writer, request_id, true, format!("Job retried as {}", summary.job_id), Some(summary.job_id)).await?;
                    }
                    DaemonRequest::Detach => {
                        send_response(&mut writer, "detach".to_string(), true, "detached".to_string(), None).await?;
                        break;
                    }
                    DaemonRequest::Stop => {
                        send_response(&mut writer, "stop".to_string(), true, "stopping".to_string(), None).await?;
                        let _ = shutdown.send(true);
                        break;
                    }
                }
            }
            event = events.recv() => {
                if let Ok(notification) = event
                    && let Some(cursor) = attached.get_mut(&notification.job_id)
                    && notification.record.sequence > *cursor
                {
                    *cursor = notification.record.sequence;
                    write_frame(&mut writer, &DaemonFrame::JobEvent {
                        job_id: notification.job_id,
                        event: notification.record,
                    }).await?;
                }
            }
        }
    }
    Ok(())
}

async fn status_for(manager: &JobManager, generation: Uuid) -> DaemonStatus {
    let status: JobDaemonStatus = manager.daemon_status().await;
    DaemonStatus {
        generation,
        capacity: status.capacity,
        running: status.running,
        queued: status.queued,
        jobs: status.jobs,
    }
}

async fn read_request(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
) -> Result<DaemonRequest> {
    let line = lines
        .next_line()
        .await?
        .context("daemon client closed before hello")?;
    Ok(serde_json::from_str(&line)?)
}

async fn write_frame(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    frame: &DaemonFrame,
) -> Result<()> {
    let payload = serde_json::to_string(frame)?;
    writer.write_all(payload.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn send_request(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    request: &DaemonRequest,
) -> Result<()> {
    let payload = serde_json::to_string(request)?;
    writer.write_all(payload.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn send_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    request_id: String,
    ok: bool,
    message: String,
    job_id: Option<Uuid>,
) -> Result<()> {
    write_frame(
        writer,
        &DaemonFrame::Response {
            request_id,
            ok,
            message,
            job_id,
        },
    )
    .await
}

async fn send_error(writer: &mut tokio::net::unix::OwnedWriteHalf, message: &str) -> Result<()> {
    write_frame(
        writer,
        &DaemonFrame::Error {
            message: message.to_string(),
        },
    )
    .await
}

async fn connect(client_id: &str) -> Result<(
    tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    tokio::net::unix::OwnedWriteHalf,
)> {
    let paths = DaemonPaths::discover()?;
    let stream = UnixStream::connect(&paths.socket)
        .await
        .with_context(|| format!("connect daemon at {}", paths.socket.display()))?;
    let (reader, mut writer) = stream.into_split();
    send_request(
        &mut writer,
        &DaemonRequest::Hello {
            client_id: client_id.to_string(),
        },
    )
    .await?;
    let mut lines = BufReader::new(reader).lines();
    let hello = lines.next_line().await?.context("daemon closed during hello")?;
    ensure!(
        matches!(serde_json::from_str::<DaemonFrame>(&hello)?, DaemonFrame::Hello { .. }),
        "daemon did not return hello"
    );
    Ok((lines, writer))
}

pub async fn status() -> Result<DaemonStatus> {
    let (mut lines, mut writer) = connect(&format!("status-{}", Uuid::new_v4())).await?;
    send_request(
        &mut writer,
        &DaemonRequest::ListJobs {
            request_id: Uuid::new_v4().to_string(),
        },
    )
    .await?;
    while let Some(line) = lines.next_line().await? {
        if let DaemonFrame::Status { status } = serde_json::from_str(&line)? {
            return Ok(status);
        }
    }
    bail!("daemon closed without status")
}

pub async fn submit(input: String, source_session_id: Option<SessionId>) -> Result<JobSummary> {
    let (mut lines, mut writer) = connect(&format!("submit-{}", Uuid::new_v4())).await?;
    let request_id = Uuid::new_v4().to_string();
    send_request(
        &mut writer,
        &DaemonRequest::Submit {
            request_id: request_id.clone(),
            source_session_id,
            input,
        },
    )
    .await?;
    while let Some(line) = lines.next_line().await? {
        if let DaemonFrame::Response { request_id: id, ok, message, job_id } = serde_json::from_str(&line)?
            && id == request_id
        {
            ensure!(ok, "{message}");
            let job_id = job_id.context("daemon response omitted Job ID")?;
            return Ok(snapshot(job_id, 0).await?.summary);
        }
    }
    bail!("daemon closed without acknowledging submit")
}

pub async fn snapshot(job_id: Uuid, cursor: u64) -> Result<JobSnapshot> {
    let (mut lines, mut writer) = connect(&format!("job-status-{}", Uuid::new_v4())).await?;
    let request_id = Uuid::new_v4().to_string();
    send_request(
        &mut writer,
        &DaemonRequest::JobAttach {
            request_id,
            job_id,
            cursor: Some(cursor),
        },
    )
    .await?;
    while let Some(line) = lines.next_line().await? {
        if let DaemonFrame::JobSnapshot { snapshot } = serde_json::from_str(&line)? {
            return Ok(snapshot);
        }
    }
    bail!("daemon closed without Job snapshot")
}

pub async fn cancel(job_id: Uuid) -> Result<()> {
    request_job_action(DaemonRequest::JobCancel {
        request_id: Uuid::new_v4().to_string(),
        job_id,
    })
    .await
}

pub async fn approve(job_id: Uuid, approve: bool) -> Result<()> {
    let request_id = Uuid::new_v4().to_string();
    let request = if approve {
        DaemonRequest::JobApprove { request_id: request_id.clone(), job_id }
    } else {
        DaemonRequest::JobReject { request_id: request_id.clone(), job_id }
    };
    request_job_action(request).await
}

pub async fn retry(job_id: Uuid) -> Result<JobSummary> {
    let (mut lines, mut writer) = connect(&format!("job-retry-{}", Uuid::new_v4())).await?;
    let request_id = Uuid::new_v4().to_string();
    send_request(&mut writer, &DaemonRequest::JobRetry { request_id: request_id.clone(), job_id }).await?;
    while let Some(line) = lines.next_line().await? {
        if let DaemonFrame::Response { request_id: id, ok, message, job_id } = serde_json::from_str(&line)?
            && id == request_id
        {
            ensure!(ok, "{message}");
            return snapshot(job_id.context("retry response omitted Job ID")?, 0).await.map(|snapshot| snapshot.summary);
        }
    }
    bail!("daemon closed without acknowledging retry")
}

async fn request_job_action(request: DaemonRequest) -> Result<()> {
    let (mut lines, mut writer) = connect(&format!("job-action-{}", Uuid::new_v4())).await?;
    let request_id = match &request {
        DaemonRequest::JobCancel { request_id, .. }
        | DaemonRequest::JobApprove { request_id, .. }
        | DaemonRequest::JobReject { request_id, .. } => request_id.clone(),
        _ => bail!("not a Job action"),
    };
    send_request(&mut writer, &request).await?;
    while let Some(line) = lines.next_line().await? {
        if let DaemonFrame::Response { request_id: id, ok, message, .. } = serde_json::from_str(&line)?
            && id == request_id
        {
            ensure!(ok, "{message}");
            return Ok(());
        }
    }
    bail!("daemon closed without acknowledging Job action")
}

pub async fn stop() -> Result<()> {
    let (mut lines, mut writer) = connect(&format!("stop-{}", Uuid::new_v4())).await?;
    send_request(&mut writer, &DaemonRequest::Stop).await?;
    while let Some(line) = lines.next_line().await? {
        if matches!(serde_json::from_str::<DaemonFrame>(&line)?, DaemonFrame::Response { ok: true, .. }) {
            return Ok(());
        }
    }
    bail!("daemon closed without acknowledging stop")
}

pub async fn attach(client_id: String, reconnect: bool) -> Result<()> {
    loop {
        let result = attach_once(&client_id).await;
        if result.is_ok() || !reconnect {
            return result;
        }
        eprintln!("daemon disconnected; retrying…");
        sleep(Duration::from_millis(500)).await;
    }
}

async fn attach_once(client_id: &str) -> Result<()> {
    let (mut lines, mut writer) = connect(client_id).await?;
    let stdin = tokio::io::stdin();
    let mut stdin_lines = BufReader::new(stdin).lines();
    println!("Connected to noya Job daemon. Type /detach to disconnect.");
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { return Ok(()); };
                print_frame(&line);
            }
            line = stdin_lines.next_line() => {
                let Some(line) = line? else { return Ok(()); };
                let trimmed = line.trim();
                if matches!(trimmed, "/detach" | "/quit") {
                    send_request(&mut writer, &DaemonRequest::Detach).await?;
                    return Ok(());
                }
                if trimmed == "/status" {
                    send_request(&mut writer, &DaemonRequest::ListJobs { request_id: Uuid::new_v4().to_string() }).await?;
                } else if !trimmed.is_empty() {
                    send_request(&mut writer, &DaemonRequest::Prompt { request_id: Uuid::new_v4().to_string(), input: line }).await?;
                }
            }
        }
    }
}

pub async fn attach_job(job_id: Uuid, reconnect: bool) -> Result<()> {
    let mut cursor = 0;
    loop {
        match attach_job_once(job_id, cursor).await {
            Ok(next) => {
                cursor = next;
                if !reconnect {
                    return Ok(());
                }
            }
            Err(error) if reconnect => {
                eprintln!("Job disconnected: {error}; retrying…");
            }
            Err(error) => return Err(error),
        }
        sleep(Duration::from_millis(500)).await;
    }
}

async fn attach_job_once(job_id: Uuid, cursor: u64) -> Result<u64> {
    let (mut lines, mut writer) = connect(&format!("job-attach-{}", Uuid::new_v4())).await?;
    send_request(&mut writer, &DaemonRequest::JobAttach { request_id: Uuid::new_v4().to_string(), job_id, cursor: Some(cursor) }).await?;
    let stdin = tokio::io::stdin();
    let mut stdin_lines = BufReader::new(stdin).lines();
    let mut latest = cursor;
    println!("Attached to Job {job_id}. Type /detach to disconnect.");
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { return Ok(latest); };
                if let Ok(frame) = serde_json::from_str::<DaemonFrame>(&line) {
                    match frame {
                        DaemonFrame::JobSnapshot { snapshot } => {
                            latest = snapshot.summary.last_sequence;
                            println!("Job {} · {:?} · seq {}", snapshot.summary.job_id, snapshot.summary.status, latest);
                            for event in snapshot.events { print_job_event(&event); }
                        }
                        DaemonFrame::JobEvent { event, .. } => { latest = event.sequence; print_job_event(&event); }
                        _ => print_frame(&line),
                    }
                }
            }
            line = stdin_lines.next_line() => {
                let Some(line) = line? else { return Ok(latest); };
                if matches!(line.trim(), "/detach" | "/quit") {
                    send_request(&mut writer, &DaemonRequest::Detach).await?;
                    return Ok(latest);
                }
            }
        }
    }
}

fn print_frame(line: &str) {
    if let Ok(frame) = serde_json::from_str::<DaemonFrame>(line) {
        match frame {
            DaemonFrame::Status { status } => println!("{}", serde_json::to_string_pretty(&status).unwrap_or_default()),
            DaemonFrame::Response { message, .. } => println!("{message}"),
            DaemonFrame::Error { message } => eprintln!("daemon error: {message}"),
            DaemonFrame::Hello { protocol, generation, .. } => println!("daemon protocol {protocol}, generation {generation}"),
            DaemonFrame::JobSnapshot { snapshot } => println!("Job {} · {:?}", snapshot.summary.job_id, snapshot.summary.status),
            DaemonFrame::JobEvent { event, .. } => print_job_event(&event),
        }
    }
}

fn print_job_event(event: &crate::job::JobEventRecord) {
    println!("event {}: {}", event.sequence, describe_job_event(&event.event));
}

fn describe_job_event(event: &crate::job::JobEvent) -> String {
    match event {
        crate::job::JobEvent::StateChanged { to, .. } => format!("state -> {to:?}"),
        crate::job::JobEvent::Agent(event) => format!("agent: {event:?}"),
        crate::job::JobEvent::ApprovalRequired(approval) => format!("approval required: {}", approval.tool_name),
        crate::job::JobEvent::Completed { .. } => "completed".to_string(),
        crate::job::JobEvent::Failed { message } => format!("failed: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_round_trips_job_submission() {
        let request = DaemonRequest::Submit {
            request_id: "request".to_string(),
            source_session_id: None,
            input: "run the task".to_string(),
        };
        let encoded = serde_json::to_string(&request).unwrap();
        let decoded: DaemonRequest = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(decoded, DaemonRequest::Submit { input, .. } if input == "run the task"));
    }
}
