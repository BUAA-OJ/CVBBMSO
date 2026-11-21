//! Exercises real recording completion from sealed fixtures or an opt-in test display.

use std::{
    fmt::Write as _,
    fs::{self, File},
    io::{BufReader, Read},
    path::Path,
};

use cvbbmso_protocol::{RecordingSummary, SegmentMeta, Sha256Digest};
use sha2::{Digest, Sha256};
use tauri::Manager;

use super::{
    AppHandle, ClientMessage, CommandName, CommandStatus, Connection, DEADLINE, Error, ExamEvent,
    ExamPhase, FIRST, Fixture, OffsetDateTime, RecordingPolicy, ServerMessage, Uuid, WebviewWindow,
    check_pending_close, connection, io, oneshot, open_enrolled_fixture, timeout, wait_for,
};
use crate::desktop::{Desktop, smoke::FIXTURE_USER_ID};

pub(super) async fn exercise(
    app: &AppHandle,
    ui: &WebviewWindow,
    mut fixture: Fixture,
    capture_display: bool,
) -> Result<oneshot::Receiver<()>, Error> {
    let recording_dir = app
        .state::<Desktop>()
        .data_dir
        .join("exams")
        .join(FIRST.to_string())
        .join(FIXTURE_USER_ID.to_string())
        .join("recording");
    if !capture_display {
        seed_recording(&recording_dir)?;
    }
    open_enrolled_fixture(app, ui).await?;
    ui.set_focus()?;
    ui.eval("document.querySelector('.open-exam').click()")?;
    let mut connection = connection(&mut fixture.incoming, FIRST).await?;
    let now = OffsetDateTime::now_utc();
    connection
        .outgoing
        .send(ServerMessage::Sync {
            server_time: now,
            phase: ExamPhase::Running,
            start_at: now - time::Duration::seconds(1),
            end_at: now + time::Duration::minutes(5),
            recording_policy: RecordingPolicy::new(2, 15)?,
            ack_event_through: 0,
        })
        .await?;
    wait_for(
        ui,
        "window.examEvents.at(-1)?.state.execution === 'ready'",
        "the recording exercise receiving initial sync",
    )
    .await?;

    if capture_display {
        start_capture(ui, &mut connection, now).await?;
    }
    let (first_seq, first) = next_recording_event(&mut connection).await?;
    let ExamEvent::SegmentFinalized(first) = first else {
        return Err(io::Error::other("Expected a sealed segment before ending the exam").into());
    };
    require(
        first.index == 1 && first_seq > 0,
        "Invalid first recording segment",
    )?;

    let end = Uuid::from_u128(0x42);
    send_command(&connection, end, CommandName::EndExam, now).await?;
    let (segments, summary) =
        timeout(DEADLINE, receive_summary(&mut connection, first_seq, first)).await??;
    require(
        capture_display || segments.len() == 2,
        "Expected both restored recording segments",
    )?;
    expect_completed(&mut connection, end).await?;
    wait_for(
        ui,
        "window.examEvents.at(-1)?.state.execution === 'finalizing' && window.examEvents.at(-1)?.state.recording === 'complete'",
        "the successful recording waiting for the end acknowledgement",
    )
    .await?;
    check_pending_close(ui).await?;
    app.exit(0);
    wait_for(ui, "document.querySelector('#exit-confirmation').open && document.querySelector('#confirm-exit').disabled", "application exit being rejected before the final ACK").await?;
    ui.eval("document.querySelector('#exit-confirmation button').click()")?;
    tokio::task::spawn_blocking(move || {
        verify_recording(&recording_dir, &segments, &summary.merged_sha256)
    })
    .await??;

    connection
        .outgoing
        .send(ServerMessage::CommandAck { command_id: end })
        .await?;
    wait_for(
        ui,
        "window.examEvents.at(-1)?.state.execution === 'finished' && window.examEvents.at(-1)?.state.recording === 'complete'",
        "end acknowledgement finishing the exam with complete recording",
    )
    .await?;
    if capture_display {
        ui.eval("clearInterval(window.recordingPaint)")?;
    }
    println!("Desktop recording sealed and hashed real files; the end ACK completed the exam.");
    Ok(connection.closed)
}

async fn start_capture(
    ui: &WebviewWindow,
    connection: &mut Connection,
    now: OffsetDateTime,
) -> Result<(), Error> {
    // Some capture backends emit no new frames while the display is static.
    ui.eval(
        r"
        const sample = document.createElement('p');
        document.querySelector('main').append(sample);
        let sequence = 0;
        window.recordingPaint = setInterval(() => {
            sample.textContent = 'CVBBMSO test pattern ' + (++sequence);
        }, 100);
    ",
    )?;
    let start = Uuid::from_u128(0x41);
    send_command(connection, start, CommandName::StartExam, now).await?;
    expect_completed(connection, start).await?;
    wait_for(
        ui,
        "window.examEvents.at(-1)?.state.recording === 'capturing'",
        "real Recorder setup processing the start command",
    )
    .await?;
    connection
        .outgoing
        .send(ServerMessage::CommandAck { command_id: start })
        .await?;
    Ok(())
}

async fn send_command(
    connection: &Connection,
    command_id: Uuid,
    name: CommandName,
    effective_at: OffsetDateTime,
) -> Result<(), Error> {
    connection
        .outgoing
        .send(ServerMessage::Command {
            command_id,
            name,
            effective_at,
        })
        .await?;
    Ok(())
}

async fn expect_completed(connection: &mut Connection, command_id: Uuid) -> Result<(), Error> {
    let result = timeout(DEADLINE, connection.results.recv()).await?;
    if result
        != Some(ClientMessage::CommandResult {
            command_id,
            status: CommandStatus::Completed,
            error_code: None,
        })
    {
        return Err(io::Error::other(format!(
            "Expected a successful recording command, got {result:?}"
        ))
        .into());
    }
    Ok(())
}

async fn next_recording_event(connection: &mut Connection) -> Result<(i64, ExamEvent), Error> {
    timeout(DEADLINE, connection.recordings.recv())
        .await?
        .ok_or_else(|| {
            io::Error::other("The monitor connection ended before the recording event").into()
        })
}

async fn receive_summary(
    connection: &mut Connection,
    first_seq: i64,
    first: SegmentMeta,
) -> Result<(Vec<SegmentMeta>, RecordingSummary), Error> {
    let mut seq = first_seq;
    let mut segments = vec![first];
    loop {
        let (next_seq, event) = next_recording_event(connection).await?;
        require(next_seq > seq, "Recording events were out of sequence")?;
        seq = next_seq;
        match event {
            ExamEvent::SegmentFinalized(segment) => {
                require(
                    u32::try_from(segments.len() + 1)? == segment.index,
                    "Recording segment indices were not consecutive",
                )?;
                segments.push(segment);
            }
            ExamEvent::RecordingFinalized(summary) => {
                require(
                    u32::try_from(segments.len())? == summary.segment_count.get(),
                    "The summary count did not match the sealed segment events",
                )?;
                return Ok((segments, summary));
            }
            _ => return Err(io::Error::other("Unexpected recording event").into()),
        }
    }
}

fn seed_recording(directory: &Path) -> io::Result<()> {
    let files: [(&str, &[u8]); 4] = [
        (
            "000001.mkv",
            include_bytes!("../../../../tests/fixtures/recording/000001.mkv"),
        ),
        (
            "000001.json",
            include_bytes!("../../../../tests/fixtures/recording/000001.json"),
        ),
        (
            "000002.mkv",
            include_bytes!("../../../../tests/fixtures/recording/000002.mkv"),
        ),
        (
            "000002.json",
            include_bytes!("../../../../tests/fixtures/recording/000002.json"),
        ),
    ];
    let segments = directory.join("segments");
    fs::create_dir_all(&segments)?;
    for (name, bytes) in files {
        fs::write(segments.join(name), bytes)?;
    }
    Ok(())
}

fn verify_recording(
    directory: &Path,
    segments: &[SegmentMeta],
    merged_sha256: &Sha256Digest,
) -> io::Result<()> {
    let segment_dir = directory.join("segments");
    for segment in segments {
        let name = format!("{:06}", segment.index);
        let saved: SegmentMeta =
            serde_json::from_slice(&fs::read(segment_dir.join(format!("{name}.json")))?)?;
        require(
            saved == *segment,
            "Segment metadata differs from the monitor event",
        )?;
        verify_digest(&segment_dir.join(format!("{name}.mkv")), &segment.sha256)?;
    }
    verify_digest(&directory.join("final.mp4"), merged_sha256)
}

fn verify_digest(file: &Path, expected: &Sha256Digest) -> io::Result<()> {
    let file = File::open(file)?;
    require(file.metadata()?.len() > 0, "A recording file is empty")?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut bytes = [0_u8; 8192];
    loop {
        let count = reader.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        hasher.update(&bytes[..count]);
    }
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(hex, "{byte:02x}").map_err(io::Error::other)?;
    }
    require(
        hex == expected.as_str(),
        "Recording file SHA-256 differs from the monitor event",
    )
}

fn require(condition: bool, message: &'static str) -> io::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message))
    }
}
