use super::event::{EventEnvelope, SCHEMA_VERSION};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

pub(super) fn load_and_repair(path: &Path) -> Result<Vec<EventEnvelope>> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    ensure!(
        !bytes.is_empty(),
        "session log is empty: {}",
        path.display()
    );
    if bytes.ends_with(b"\n") {
        return parse_complete(&bytes, path);
    }

    let last_newline = bytes.iter().rposition(|byte| *byte == b'\n');
    let tail_start = last_newline.map_or(0, |position| position + 1);
    let tail = &bytes[tail_start..];
    if serde_json::from_slice::<EventEnvelope>(tail).is_ok() {
        let mut file = OpenOptions::new().append(true).open(path)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        let mut repaired = bytes;
        repaired.push(b'\n');
        return parse_complete(&repaired, path);
    }

    let backup = path.with_extension("jsonl.repair");
    fs::copy(path, &backup)
        .with_context(|| format!("back up torn session log to {}", backup.display()))?;
    let valid_len = last_newline.map_or(0, |position| position + 1);
    ensure!(valid_len > 0, "session log contains no complete event");
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(valid_len as u64)?;
    file.sync_all()?;
    parse_complete(&bytes[..valid_len], path)
}

pub(super) fn load_read_only(path: &Path) -> Result<Vec<EventEnvelope>> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    ensure!(
        !bytes.is_empty(),
        "session log is empty: {}",
        path.display()
    );
    ensure!(
        bytes.ends_with(b"\n"),
        "session log has an incomplete final line: {}",
        path.display()
    );
    parse_complete(&bytes, path)
}

fn parse_complete(bytes: &[u8], path: &Path) -> Result<Vec<EventEnvelope>> {
    let text = std::str::from_utf8(bytes).context("session log is not UTF-8")?;
    let mut events = Vec::new();
    for (index, line) in text.split_terminator('\n').enumerate() {
        if line.is_empty() {
            bail!(
                "session log contains an empty line at {}:{}",
                path.display(),
                index + 1
            );
        }
        let event: EventEnvelope = serde_json::from_str(line)
            .with_context(|| format!("decode session event at {}:{}", path.display(), index + 1))?;
        ensure!(
            event.schema_version == SCHEMA_VERSION,
            "unsupported session schema version {}",
            event.schema_version
        );
        events.push(event);
    }
    ensure!(!events.is_empty(), "session log contains no events");
    Ok(events)
}
