use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

use crate::session::{PTY_EXIT_SENTINEL, SessionHandle};

/// Asciicast v2 header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsciicastHeader {
    pub version: u8,
    pub width: u16,
    pub height: u16,
    pub timestamp: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<std::collections::HashMap<String, String>>,
}

impl AsciicastHeader {
    pub fn new(width: u16, height: u16, title: Option<String>) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .ok();

        Self {
            version: 2,
            width,
            height,
            timestamp,
            title,
            env: None,
        }
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("failed to serialize asciicast header")
    }
}

/// A single asciicast v2 event: `[time, type, data]`.
#[derive(Debug, Clone)]
pub struct AsciicastEvent {
    /// Seconds since recording start.
    pub time: f64,
    /// "o" for output, "i" for input.
    pub event_type: EventType,
    /// The data (terminal bytes as a string).
    pub data: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    Output,
    Input,
}

impl EventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventType::Output => "o",
            EventType::Input => "i",
        }
    }
}

impl AsciicastEvent {
    pub fn to_json(&self) -> Result<String> {
        let value = serde_json::json!([self.time, self.event_type.as_str(), self.data]);
        serde_json::to_string(&value).context("failed to serialize asciicast event")
    }
}

/// Stop signal for the recorder.
enum RecorderCommand {
    Stop,
}

/// Handle to a running recording. Call `stop()` to finish.
pub struct RecordingHandle {
    stop_tx: mpsc::Sender<RecorderCommand>,
    pub path: PathBuf,
}

impl RecordingHandle {
    /// Stop the recording, flushing and closing the file.
    pub async fn stop(self) -> Result<()> {
        let _ = self.stop_tx.send(RecorderCommand::Stop).await;
        // Give the recorder task a moment to flush.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        Ok(())
    }
}

/// Start recording a session to an asciicast v2 file.
///
/// Returns a `RecordingHandle` that can be used to stop the recording.
pub async fn start_recording(
    session: &SessionHandle,
    path: impl AsRef<Path>,
    width: u16,
    height: u16,
    title: Option<String>,
) -> Result<RecordingHandle> {
    let path = path.as_ref().to_path_buf();

    // Ensure parent directory exists.
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("failed to create recording directory")?;
    }

    let header = AsciicastHeader::new(width, height, title);
    let header_line = header.to_json()?;

    let mut file = tokio::fs::File::create(&path)
        .await
        .context("failed to create recording file")?;

    // Write header.
    file.write_all(header_line.as_bytes()).await?;
    file.write_all(b"\n").await?;

    let mut output_rx = session.subscribe_output();
    let (stop_tx, mut stop_rx) = mpsc::channel::<RecorderCommand>(1);

    let recording_path = path.clone();
    let session_id = session.id.clone();

    tokio::spawn(async move {
        let start = Instant::now();

        loop {
            tokio::select! {
                biased;
                cmd = stop_rx.recv() => {
                    match cmd {
                        Some(RecorderCommand::Stop) | None => {
                            info!(session = %session_id, path = %recording_path.display(), "recording stopped");
                            break;
                        }
                    }
                }
                result = output_rx.recv() => {
                    match result {
                        Ok(data) if data == PTY_EXIT_SENTINEL => {
                            info!(session = %session_id, "PTY exited, stopping recording");
                            break;
                        }
                        Ok(data) => {
                            let elapsed = start.elapsed().as_secs_f64();
                            let text = String::from_utf8_lossy(&data);
                            let event = AsciicastEvent {
                                time: elapsed,
                                event_type: EventType::Output,
                                data: text.into_owned(),
                            };
                            match event.to_json() {
                                Ok(line) => {
                                    if let Err(e) = file.write_all(line.as_bytes()).await {
                                        error!(error = %e, "failed to write recording event");
                                        break;
                                    }
                                    if let Err(e) = file.write_all(b"\n").await {
                                        error!(error = %e, "failed to write recording newline");
                                        break;
                                    }
                                }
                                Err(e) => {
                                    warn!(error = %e, "failed to serialize recording event");
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "recording lagged, dropped frames");
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!(session = %session_id, "broadcast closed, stopping recording");
                            break;
                        }
                    }
                }
            }
        }

        // Flush.
        let _ = file.flush().await;
    });

    info!(session = %session.id, path = %path.display(), "recording started");

    Ok(RecordingHandle {
        stop_tx,
        path,
    })
}

/// List recording files in a directory.
pub async fn list_recordings(dir: &Path) -> Result<Vec<RecordingInfo>> {
    let mut entries = Vec::new();

    if !dir.exists() {
        return Ok(entries);
    }

    let mut read_dir = tokio::fs::read_dir(dir)
        .await
        .context("failed to read recordings directory")?;

    while let Some(entry) = read_dir.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("cast") {
            if let Ok(metadata) = entry.metadata().await {
                let filename = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let size = metadata.len();
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());

                entries.push(RecordingInfo {
                    filename,
                    size,
                    modified,
                });
            }
        }
    }

    entries.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(entries)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingInfo {
    pub filename: String,
    pub size: u64,
    pub modified: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_serialization() {
        let header = AsciicastHeader {
            version: 2,
            width: 80,
            height: 24,
            timestamp: Some(1700000000),
            title: Some("test session".to_string()),
            env: None,
        };
        let json = header.to_json().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["version"], 2);
        assert_eq!(parsed["width"], 80);
        assert_eq!(parsed["height"], 24);
        assert_eq!(parsed["timestamp"], 1700000000u64);
        assert_eq!(parsed["title"], "test session");
        // env should be absent (skip_serializing_if)
        assert!(parsed.get("env").is_none());
    }

    #[test]
    fn header_without_optional_fields() {
        let header = AsciicastHeader {
            version: 2,
            width: 120,
            height: 40,
            timestamp: Some(1700000000),
            title: None,
            env: None,
        };
        let json = header.to_json().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["version"], 2);
        assert!(parsed.get("title").is_none());
        assert!(parsed.get("env").is_none());
    }

    #[test]
    fn event_output_serialization() {
        let event = AsciicastEvent {
            time: 1.234567,
            event_type: EventType::Output,
            data: "hello world\r\n".to_string(),
        };
        let json = event.to_json().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_array());
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert!((arr[0].as_f64().unwrap() - 1.234567).abs() < 0.0001);
        assert_eq!(arr[1].as_str().unwrap(), "o");
        assert_eq!(arr[2].as_str().unwrap(), "hello world\r\n");
    }

    #[test]
    fn event_input_serialization() {
        let event = AsciicastEvent {
            time: 0.5,
            event_type: EventType::Input,
            data: "ls -la\n".to_string(),
        };
        let json = event.to_json().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr[1].as_str().unwrap(), "i");
        assert_eq!(arr[2].as_str().unwrap(), "ls -la\n");
    }

    #[test]
    fn event_zero_time() {
        let event = AsciicastEvent {
            time: 0.0,
            event_type: EventType::Output,
            data: "$ ".to_string(),
        };
        let json = event.to_json().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr[0].as_f64().unwrap(), 0.0);
    }

    #[test]
    fn event_with_special_chars() {
        let event = AsciicastEvent {
            time: 0.1,
            event_type: EventType::Output,
            data: "line1\r\nline2\t\"quoted\"\x1b[31mred\x1b[0m".to_string(),
        };
        let json = event.to_json().unwrap();
        // Should roundtrip through JSON without issue.
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = parsed.as_array().unwrap();
        assert!(arr[2].as_str().unwrap().contains("\\\"quoted\\\"") == false);
        // The JSON parser handles escaping; the string should contain the original chars.
        assert!(arr[2].as_str().unwrap().contains("quoted"));
    }

    #[test]
    fn event_with_unicode() {
        let event = AsciicastEvent {
            time: 2.0,
            event_type: EventType::Output,
            data: "hello \u{1F600} world".to_string(),
        };
        let json = event.to_json().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = parsed.as_array().unwrap();
        assert!(arr[2].as_str().unwrap().contains('\u{1F600}'));
    }

    #[test]
    fn full_asciicast_file_format() {
        // Simulate what a complete .cast file looks like.
        let header = AsciicastHeader {
            version: 2,
            width: 80,
            height: 24,
            timestamp: Some(1700000000),
            title: Some("demo".to_string()),
            env: None,
        };

        let events = vec![
            AsciicastEvent {
                time: 0.0,
                event_type: EventType::Output,
                data: "$ ".to_string(),
            },
            AsciicastEvent {
                time: 0.5,
                event_type: EventType::Input,
                data: "echo hello\r\n".to_string(),
            },
            AsciicastEvent {
                time: 0.6,
                event_type: EventType::Output,
                data: "echo hello\r\nhello\r\n$ ".to_string(),
            },
        ];

        let mut lines = vec![header.to_json().unwrap()];
        for event in &events {
            lines.push(event.to_json().unwrap());
        }
        let content = lines.join("\n") + "\n";

        // Verify: first line is valid header, rest are valid events.
        let mut file_lines = content.lines();

        let header_line = file_lines.next().unwrap();
        let parsed_header: AsciicastHeader =
            serde_json::from_str(header_line).unwrap();
        assert_eq!(parsed_header.version, 2);
        assert_eq!(parsed_header.width, 80);

        for line in file_lines {
            if line.is_empty() {
                continue;
            }
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(parsed.is_array());
            let arr = parsed.as_array().unwrap();
            assert_eq!(arr.len(), 3);
            assert!(arr[0].is_f64() || arr[0].is_u64());
            let etype = arr[1].as_str().unwrap();
            assert!(etype == "o" || etype == "i");
            assert!(arr[2].is_string());
        }
    }
}
