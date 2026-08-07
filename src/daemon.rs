//! A small resident Agent daemon with a reconnectable JSONL socket protocol.

use crate::{Agent, AgentEvent};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, broadcast, watch},
    time::sleep,
};
use uuid::Uuid;

const PROTOCOL_VERSION: u32 = 1;
const REPLAY_CAPACITY: usize = 512;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonCursor {
    pub generation: Uuid,
    pub sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonSnapshot {
    pub generation: Uuid,
    pub sequence: u64,
    pub summary: crate::session::SessionSummary,
    pub transcript: crate::session::Transcript,
    pub tree: crate::session::SessionTree,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonRequest {
    Hello {
        client_id: String,
        cursor: Option<DaemonCursor>,
    },
    Prompt {
        request_id: String,
        input: String,
    },
    Status,
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
    Snapshot {
        snapshot: DaemonSnapshot,
    },
    Event {
        cursor: DaemonCursor,
        event: AgentEvent,
    },
    Response {
        request_id: String,
        ok: bool,
        message: String,
    },
    Error {
        message: String,
    },
}

struct DaemonCore {
    agent: Agent,
    generation: Uuid,
    sequence: u64,
    history: VecDeque<(u64, AgentEvent)>,
    events: broadcast::Sender<DaemonFrame>,
}

impl DaemonCore {
    fn snapshot(&self) -> DaemonSnapshot {
        DaemonSnapshot {
            generation: self.generation,
            sequence: self.sequence,
            summary: self.agent.session_summary(),
            transcript: self.agent.transcript(),
            tree: self.agent.session_tree(),
        }
    }

    fn publish(&mut self, event: AgentEvent) {
        self.sequence = self.sequence.saturating_add(1);
        self.history.push_back((self.sequence, event.clone()));
        while self.history.len() > REPLAY_CAPACITY {
            self.history.pop_front();
        }
        let _ = self.events.send(DaemonFrame::Event {
            cursor: DaemonCursor {
                generation: self.generation,
                sequence: self.sequence,
            },
            event,
        });
    }

    async fn prompt(&mut self, input: String) -> Result<()> {
        ensure!(!input.trim().is_empty(), "daemon prompt cannot be empty");
        let mut events = Vec::new();
        let result = self
            .agent
            .turn(input, |event| events.push(event))
            .await
            .context("run daemon Agent turn");
        for event in events {
            self.publish(event);
        }
        result
    }

    fn replay_after(&self, cursor: Option<&DaemonCursor>) -> Vec<DaemonFrame> {
        let Some(cursor) = cursor else {
            return Vec::new();
        };
        if cursor.generation != self.generation {
            return Vec::new();
        }
        self.history
            .iter()
            .filter(|(sequence, _)| *sequence > cursor.sequence)
            .map(|(sequence, event)| DaemonFrame::Event {
                cursor: DaemonCursor {
                    generation: self.generation,
                    sequence: *sequence,
                },
                event: event.clone(),
            })
            .collect()
    }
}

type SharedCore = Arc<Mutex<DaemonCore>>;

pub async fn serve(agent: Agent) -> Result<()> {
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
    fs::write(&paths.pid, std::process::id().to_string())
        .with_context(|| format!("write daemon pid {}", paths.pid.display()))?;

    let (events, _) = broadcast::channel(REPLAY_CAPACITY);
    let core = Arc::new(Mutex::new(DaemonCore {
        agent,
        generation: Uuid::new_v4(),
        sequence: 0,
        history: VecDeque::with_capacity(REPLAY_CAPACITY),
        events,
    }));
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accept daemon client")?;
                let client_core = Arc::clone(&core);
                let client_shutdown = shutdown_tx.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_client(stream, client_core, client_shutdown).await {
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
    let _ = fs::remove_file(&paths.socket);
    let _ = fs::remove_file(&paths.pid);
    Ok(())
}

async fn handle_client(
    stream: UnixStream,
    core: SharedCore,
    shutdown: watch::Sender<bool>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let hello = read_request(&mut lines).await?;
    let DaemonRequest::Hello { client_id, cursor } = hello else {
        bail!("first daemon request must be hello");
    };
    ensure!(!client_id.trim().is_empty(), "daemon client ID cannot be empty");

    let (snapshot, replay, mut event_rx) = {
        let state = core.lock().await;
        (
            state.snapshot(),
            state.replay_after(cursor.as_ref()),
            state.events.subscribe(),
        )
    };
    write_frame(
        &mut writer,
        &DaemonFrame::Hello {
            protocol: PROTOCOL_VERSION,
            generation: snapshot.generation,
            client_id,
        },
    )
    .await?;
    for frame in replay {
        write_frame(&mut writer, &frame).await?;
    }
    // The snapshot is emitted after replay so the client's cursor is always
    // monotonic. It also acts as the durable fallback when the cursor is from
    // another generation or older than the bounded replay window.
    write_frame(&mut writer, &DaemonFrame::Snapshot { snapshot }).await?;

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line.context("read daemon request")? else { break; };
                let request: DaemonRequest = serde_json::from_str(&line).context("decode daemon request")?;
                match request {
                    DaemonRequest::Hello { .. } => {
                        write_frame(&mut writer, &DaemonFrame::Error { message: "hello already completed".to_string() }).await?;
                    }
                    DaemonRequest::Prompt { request_id, input } => {
                        let result = {
                            let mut state = core.lock().await;
                            state.prompt(input).await
                        };
                        let frame = match result {
                            Ok(()) => DaemonFrame::Response { request_id, ok: true, message: "prompt completed".to_string() },
                            Err(error) => DaemonFrame::Response { request_id, ok: false, message: error.to_string() },
                        };
                        write_frame(&mut writer, &frame).await?;
                        let state = core.lock().await;
                        write_frame(&mut writer, &DaemonFrame::Snapshot { snapshot: state.snapshot() }).await?;
                    }
                    DaemonRequest::Status => {
                        let state = core.lock().await;
                        write_frame(&mut writer, &DaemonFrame::Snapshot { snapshot: state.snapshot() }).await?;
                    }
                    DaemonRequest::Detach => {
                        write_frame(&mut writer, &DaemonFrame::Response { request_id: "detach".to_string(), ok: true, message: "detached".to_string() }).await?;
                        break;
                    }
                    DaemonRequest::Stop => {
                        write_frame(&mut writer, &DaemonFrame::Response { request_id: "stop".to_string(), ok: true, message: "stopping".to_string() }).await?;
                        let _ = shutdown.send(true);
                        break;
                    }
                }
            }
            event = event_rx.recv() => {
                match event {
                    Ok(frame) => write_frame(&mut writer, &frame).await?,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let state = core.lock().await;
                        write_frame(&mut writer, &DaemonFrame::Snapshot { snapshot: state.snapshot() }).await?;
                        event_rx = state.events.subscribe();
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    Ok(())
}

async fn read_request(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
) -> Result<DaemonRequest> {
    let line = lines.next_line().await?.context("daemon client closed before hello")?;
    serde_json::from_str(&line).context("decode daemon hello")
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

pub async fn status() -> Result<DaemonSnapshot> {
    let paths = DaemonPaths::discover()?;
    let stream = UnixStream::connect(&paths.socket)
        .await
        .with_context(|| format!("connect daemon at {}", paths.socket.display()))?;
    let (reader, mut writer) = stream.into_split();
    send_request(
        &mut writer,
        &DaemonRequest::Hello {
            client_id: format!("status-{}", Uuid::new_v4()),
            cursor: None,
        },
    )
    .await?;
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if let DaemonFrame::Snapshot { snapshot } = serde_json::from_str(&line)? {
            return Ok(snapshot);
        }
    }
    bail!("daemon closed without a snapshot")
}

pub async fn stop() -> Result<()> {
    let paths = DaemonPaths::discover()?;
    let stream = UnixStream::connect(&paths.socket)
        .await
        .with_context(|| format!("connect daemon at {}", paths.socket.display()))?;
    let (reader, mut writer) = stream.into_split();
    send_request(
        &mut writer,
        &DaemonRequest::Hello {
            client_id: format!("stop-{}", Uuid::new_v4()),
            cursor: None,
        },
    )
    .await?;
    send_request(&mut writer, &DaemonRequest::Stop).await?;
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if matches!(serde_json::from_str::<DaemonFrame>(&line)?, DaemonFrame::Response { ok: true, .. }) {
            return Ok(());
        }
    }
    bail!("daemon closed without acknowledging stop")
}

pub async fn attach(client_id: String, reconnect: bool) -> Result<()> {
    let mut cursor = None;
    loop {
        match attach_once(&client_id, cursor.clone()).await {
            Ok((next_cursor, should_reconnect)) => {
                cursor = next_cursor;
                if !reconnect || !should_reconnect {
                    return Ok(());
                }
            }
            Err(error) if reconnect => {
                eprintln!("daemon disconnected: {error}; retrying…");
            }
            Err(error) => return Err(error),
        }
        sleep(Duration::from_millis(500)).await;
    }
}

async fn attach_once(
    client_id: &str,
    cursor: Option<DaemonCursor>,
) -> Result<(Option<DaemonCursor>, bool)> {
    let paths = DaemonPaths::discover()?;
    let stream = UnixStream::connect(&paths.socket)
        .await
        .with_context(|| format!("connect daemon at {}", paths.socket.display()))?;
    let (reader, mut writer) = stream.into_split();
    send_request(
        &mut writer,
        &DaemonRequest::Hello {
            client_id: client_id.to_string(),
            cursor: cursor.clone(),
        },
    )
    .await?;
    let stdin = tokio::io::stdin();
    let mut stdin_lines = BufReader::new(stdin).lines();
    let mut socket_lines = BufReader::new(reader).lines();
    let mut next_request = 0_u64;
    let mut latest = cursor;
    println!("Attached to noya daemon. Type /detach to disconnect.");
    loop {
        tokio::select! {
            line = socket_lines.next_line() => {
                let Some(line) = line.context("read daemon frame")? else {
                    return Ok((latest, true));
                };
                let frame: DaemonFrame = serde_json::from_str(&line)?;
                match &frame {
                    DaemonFrame::Event { cursor, event } => {
                        latest = Some(cursor.clone());
                        println!("event {}: {}", cursor.sequence, describe_event(event));
                    }
                    DaemonFrame::Snapshot { snapshot } => {
                        latest = Some(DaemonCursor { generation: snapshot.generation, sequence: snapshot.sequence });
                        println!("session {} · {:?} · head {}", snapshot.summary.session_id, snapshot.summary.status, snapshot.sequence);
                    }
                    DaemonFrame::Response { message, .. } => println!("{message}"),
                    DaemonFrame::Hello { protocol, generation, .. } => println!("daemon protocol {protocol}, generation {generation}"),
                    DaemonFrame::Error { message } => eprintln!("daemon error: {message}"),
                }
            }
            line = stdin_lines.next_line() => {
                let Some(line) = line? else { return Ok((latest, false)); };
                if line.trim() == "/detach" || line.trim() == "/quit" {
                    send_request(&mut writer, &DaemonRequest::Detach).await?;
                    return Ok((latest, false));
                }
                if line.trim() == "/status" {
                    send_request(&mut writer, &DaemonRequest::Status).await?;
                    continue;
                }
                if line.trim().is_empty() { continue; }
                next_request = next_request.saturating_add(1);
                send_request(&mut writer, &DaemonRequest::Prompt { request_id: format!("{client_id}-{next_request}"), input: line }).await?;
            }
        }
    }
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

fn describe_event(event: &AgentEvent) -> String {
    match event {
        AgentEvent::TurnStarted { .. } => "turn started".to_string(),
        AgentEvent::TextDelta { chunk, .. } => format!("text: {}", chunk.trim()),
        AgentEvent::ToolStarted { name, .. } => format!("tool started: {name}"),
        AgentEvent::ToolFinished { name, success, .. } => format!("tool finished: {name} ({success})"),
        AgentEvent::DiagnosticsUpdated { diagnostics, .. } => format!("diagnostics: {} tokens", diagnostics.usage.total_tokens),
        AgentEvent::ApprovalRequired { tool_name, .. } => format!("approval required: {tool_name}"),
        AgentEvent::TurnCompleted { .. } => "turn completed".to_string(),
        AgentEvent::Error { message, .. } => format!("error: {message}"),
    }
}

pub fn socket_exists() -> bool {
    DaemonPaths::discover()
        .ok()
        .is_some_and(|paths| paths.socket.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_replay_is_generation_aware_and_bounded() {
        assert_ne!(Uuid::new_v4(), Uuid::nil());
        let cursor = DaemonCursor {
            generation: Uuid::new_v4(),
            sequence: 4,
        };
        let json = serde_json::to_string(&cursor).unwrap();
        assert!(json.contains("generation"));
        assert!(json.contains("sequence"));
    }

    #[test]
    fn request_and_frame_protocol_round_trip_as_jsonl_objects() {
        let request = DaemonRequest::Hello {
            client_id: "test-client".to_string(),
            cursor: None,
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<DaemonRequest>(&encoded).unwrap(), request);

        let frame = DaemonFrame::Error {
            message: "test".to_string(),
        };
        let encoded = serde_json::to_string(&frame).unwrap();
        assert!(matches!(
            serde_json::from_str::<DaemonFrame>(&encoded).unwrap(),
            DaemonFrame::Error { message } if message == "test"
        ));
    }
}
