//! Streaming ASC/BLF bridge to the pinned canlog library.
use super::{
    message::CanFrame,
    trace_buffer::{CapturedFrame, MAX_TRACE_FRAMES},
    trace_export::TraceBus,
};
use canlog::{
    core::{Direction, Frame, IssueKind, Limits, Metadata, ReadItem},
    formats::{self, Format},
};
use std::{collections::HashMap, fs::File, path::Path};

pub struct LoadedLog {
    pub frames: Vec<CanFrame>,
    pub metadata: Metadata,
    pub issues: usize,
    pub issue_examples: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        filter::{FilterLogic, FilterRule, FilterSet},
        trace_export::{self, ExportFormat},
        trace_player::TracePlayer,
    };
    fn fixture() -> &'static Path {
        Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/fixtures/integration.asc"
        ))
    }
    fn assert_same(a: &CanFrame, b: &CanFrame) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.is_extended, b.is_extended);
        assert_eq!(a.is_remote, b.is_remote);
        assert_eq!(a.raw_dlc, b.raw_dlc);
        assert_eq!(a.timestamp_ns, b.timestamp_ns);
        assert_eq!(a.bus, b.bus);
        assert_eq!(a.direction, b.direction);
        assert_eq!(a.data, b.data);
        assert_eq!(a.is_fd, b.is_fd);
        assert_eq!(a.brs, b.brs);
        assert_eq!(a.esi, b.esi);
    }
    #[test]
    fn asc_blf_asc_preserves_frame_semantics_and_nanoseconds() {
        let dir = tempfile::tempdir().unwrap();
        let mapping = HashMap::from([(1, "net-a".into()), (2, "net-b".into())]);
        let original = load(fixture(), &mapping, None).unwrap();
        assert_eq!(original.frames.len(), 7);
        assert!(original.frames[5].is_fd && original.frames[5].brs && original.frames[5].esi);
        assert_eq!(original.frames[5].data.len(), 64);
        assert_eq!(original.frames[6].raw_dlc, Some(9));
        let (_, mut captured) = trace_export::frames_from_player(&original.frames);
        let blf = dir.path().join("roundtrip.blf");
        trace_export::export_trace_with_metadata(
            &blf,
            ExportFormat::Blf,
            0,
            &mut captured,
            &[],
            &HashMap::new(),
            Some(&original.metadata),
        )
        .unwrap();
        let restored = load(&blf, &mapping, None).unwrap();
        for (a, b) in original.frames.iter().zip(&restored.frames) {
            assert_same(a, b);
        }
        assert_eq!(restored.frames.len(), 7);
        let (_, mut captured) = trace_export::frames_from_player(&restored.frames);
        let asc = dir.path().join("roundtrip.asc");
        trace_export::export_trace_with_metadata(
            &asc,
            ExportFormat::Asc,
            0,
            &mut captured,
            &[],
            &HashMap::new(),
            Some(&restored.metadata),
        )
        .unwrap();
        let restored = load(&asc, &mapping, None).unwrap();
        assert_eq!(restored.frames.len(), 7);
        for (a, b) in original.frames.iter().zip(&restored.frames) {
            assert_same(a, b);
        }
    }
    #[tokio::test]
    async fn playback_filter_export_and_clear_keep_full_source() {
        let dir = tempfile::tempdir().unwrap();
        let mut player = TracePlayer::new();
        assert_eq!(
            player
                .load_file(fixture().into(), None, None)
                .await
                .unwrap(),
            7
        );
        let filter = FilterSet::new(
            vec![
                FilterRule::Channel(2),
                FilterRule::IdExact(256),
                FilterRule::ExtendedId(true),
                FilterRule::TimeRange {
                    start: Some(0.2),
                    end: Some(0.3),
                },
            ],
            FilterLogic::And,
        )
        .prepare(HashMap::new())
        .unwrap();
        let all = player.get_all_frames();
        let selected: Vec<_> = all.iter().filter(|f| filter.matches(f)).cloned().collect();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].direction, "tx");
        let (_, mut captured) = trace_export::frames_from_player(&selected);
        let dest = dir.path().join("filtered.blf");
        trace_export::export_trace_with_metadata(
            &dest,
            ExportFormat::Blf,
            0,
            &mut captured,
            &[],
            &HashMap::new(),
            player.source_metadata().as_ref(),
        )
        .unwrap();
        let imported = load(&dest, &HashMap::new(), None).unwrap();
        assert_eq!(imported.frames.len(), 1);
        assert_same(&selected[0], &imported.frames[0]);
        assert_eq!(
            player
                .get_all_frames()
                .iter()
                .filter(|f| FilterSet::default().matches(f))
                .count(),
            7
        );
        player.start().unwrap();
        assert!(player.get_next_frame().is_some());
        let bad = dir.path().join("corrupted.asc");
        std::fs::write(&bad, "base hex timestamps absolute\n0.1 1 100 Rx d 2 AA\n").unwrap();
        assert!(player.load_file(bad, None, None).await.is_err());
        assert_eq!(player.get_all_frames().len(), 7);
    }
    #[test]
    fn unknown_direction_export_is_atomic_and_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("protected.asc");
        std::fs::write(&target, "original").unwrap();
        let mut frame = load(fixture(), &HashMap::new(), None)
            .unwrap()
            .frames
            .remove(0);
        frame.direction = "unknown".into();
        let (_, mut captured) = trace_export::frames_from_player(&[frame]);
        assert!(trace_export::export_trace(
            &target,
            ExportFormat::Asc,
            0,
            &mut captured,
            &[],
            &HashMap::new()
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "original");
    }
}

pub fn load(
    path: &Path,
    buses: &HashMap<u16, String>,
    progress: Option<&(dyn Fn(usize) + Send + Sync)>,
) -> Result<LoadedLog, String> {
    let mut reader = formats::open_reader(&path.to_string_lossy(), None, Limits::default())
        .map_err(|e| format!("{e:#}"))?;
    let mut frames = Vec::new();
    let mut issues = 0;
    let mut examples = Vec::new();
    let mut processed = 0;
    while let Some(item) = reader.next_item().map_err(|e| format!("{e:#}"))? {
        processed += 1;
        match item {
            ReadItem::Frame(record) => {
                if frames.len() >= MAX_TRACE_FRAMES {
                    return Err(format!("Trace exceeds {MAX_TRACE_FRAMES} frame limit"));
                }
                let f = record.frame;
                frames.push(CanFrame {
                    id: f.id(),
                    is_extended: f.extended(),
                    is_remote: f.remote(),
                    dlc: if f.remote() {
                        f.raw_dlc().min(8)
                    } else {
                        f.data().len() as u8
                    },
                    data: f.data().to_vec(),
                    timestamp: f.timestamp_ns() as f64 / 1e9,
                    channel: buses
                        .get(&f.channel())
                        .cloned()
                        .unwrap_or_else(|| format!("channel_{}", f.channel())),
                    direction: match f.direction() {
                        Direction::Rx => "rx",
                        Direction::Tx => "tx",
                        Direction::Unknown => "unknown",
                    }
                    .into(),
                    is_fd: f.fd(),
                    brs: f.brs(),
                    esi: f.esi(),
                    raw_dlc: Some(f.raw_dlc()),
                    timestamp_ns: Some(f.timestamp_ns()),
                    bus: Some(f.channel()),
                });
            }
            ReadItem::Issue(issue) => {
                if issue.kind == IssueKind::CorruptedRegion {
                    return Err(format!(
                        "Corrupted trace at {:?}: {}",
                        issue.location, issue.message
                    ));
                }
                issues += 1;
                if examples.len() < 20 {
                    examples.push(format!("{:?}: {}", issue.location, issue.message));
                }
            }
        }
        if processed % 10_000 == 0 {
            if let Some(callback) = progress {
                callback(processed);
            }
        }
    }
    if frames.is_empty() {
        return Err("Trace contains no supported CAN frames".into());
    }
    if let Some(callback) = progress {
        callback(processed);
    }
    Ok(LoadedLog {
        frames,
        metadata: reader.metadata().clone(),
        issues,
        issue_examples: examples,
    })
}

pub fn as_log_frame(frame: &CanFrame, bus: u16) -> Result<Frame, String> {
    let fd = frame.is_fd || frame.dlc > 8;
    let dlc = match frame.raw_dlc {
        Some(raw) => raw,
        None if fd => canlog::core::FD_LENGTHS
            .iter()
            .position(|&n| n == frame.data.len())
            .ok_or("Invalid CAN FD payload length")? as u8,
        None => frame.dlc,
    };
    if !frame.timestamp.is_finite() || frame.timestamp < 0.0 {
        return Err("Invalid frame timestamp".into());
    }
    let direction = match frame.direction.as_str() {
        "rx" => Direction::Rx,
        "tx" => Direction::Tx,
        _ => Direction::Unknown,
    };
    Frame::new(
        frame
            .timestamp_ns
            .unwrap_or_else(|| (frame.timestamp * 1e9).round() as i64),
        frame.bus.unwrap_or(bus),
        frame.id,
        frame.is_extended,
        direction,
        frame.is_remote,
        fd,
        dlc,
        frame.data.clone(),
        frame.brs,
        frame.esi,
    )
    .map_err(|e| format!("{e:#}"))
}

pub fn write(
    path: &Path,
    format: Format,
    frames: &[CapturedFrame],
    buses: &[TraceBus],
    metadata: &Metadata,
) -> Result<u64, String> {
    let mut channels: HashMap<String, u16> = buses
        .iter()
        .map(|b| (b.channel_id.clone(), b.bus))
        .collect();
    for frame in frames {
        if !channels.contains_key(&frame.channel) {
            let bus = frame
                .bus
                .or_else(|| {
                    frame
                        .channel
                        .strip_prefix("channel_")
                        .and_then(|s| s.parse().ok())
                })
                .unwrap_or_else(|| {
                    (1..=u16::MAX)
                        .find(|b| !channels.values().any(|v| v == b))
                        .unwrap_or(0)
                });
            channels.insert(frame.channel.clone(), bus);
        }
    }
    let mut writer = formats::make_writer(
        File::create(path).map_err(|e| e.to_string())?,
        format,
        metadata,
    )
    .map_err(|e| format!("{e:#}"))?;
    for frame in frames {
        let converted = as_log_frame(&frame.to_can_frame(), channels[&frame.channel])?;
        // Unknown directions require a user-visible conversion decision, never silently Rx.
        if converted.direction() == Direction::Unknown {
            return Err("ASC/BLF export cannot preserve an unknown direction".into());
        }
        writer
            .write_frame(&converted)
            .map_err(|e| format!("{e:#}"))?;
    }
    writer.finish().map_err(|e| format!("{e:#}"))?;
    Ok(frames.len() as u64)
}

pub fn metadata_from_epoch(epoch_ns: u64) -> Metadata {
    use chrono::{Datelike, Timelike};
    let mut metadata = Metadata::default();
    if let Some(date) = chrono::DateTime::<chrono::Utc>::from_timestamp(
        (epoch_ns / 1_000_000_000) as i64,
        (epoch_ns % 1_000_000_000) as u32,
    ) {
        metadata.date_text = Some(date.format("%a %b %d %H:%M:%S%.3f %Y").to_string());
        metadata.blf_start = Some([
            date.year() as u16,
            date.month() as u16,
            date.weekday().num_days_from_sunday() as u16,
            date.day() as u16,
            date.hour() as u16,
            date.minute() as u16,
            date.second() as u16,
            date.timestamp_subsec_millis() as u16,
        ]);
    }
    metadata
}
