//! Completed-output validation. Reads only box metadata; media payloads are skipped.
use super::*;
use rusty_dlna_http::RemuxOutputExpectation;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

const MAX_METADATA_BYTES: u64 = 256 * 1024 * 1024;
const MAX_BOXES: u64 = 200_000;
const MAX_SAMPLES: u64 = 32_000_000;
// Offline output admits up to 32 selected audio tracks plus one video track.
const MAX_MEDIA_TRACKS: usize = 33;
// Non-browser MP4 remuxing also preserves one referenced QuickTime chapter track.
const MAX_TRACKS: usize = MAX_MEDIA_TRACKS + 1;

#[derive(Debug, Default)]
pub(in crate::remux) struct ValidationStats {
    pub metadata_bytes: u64,
    pub samples: u64,
    pub boxes: u64,
}

struct MediaTrack {
    track: Track,
    codec: String,
    chapter: bool,
    default_duration: u32,
    default_size: u32,
    default_flags: u32,
    start: Option<f64>,
    end: f64,
    samples: u64,
}

pub(in crate::remux) fn validate_finished(
    file: &File,
    expected: &RemuxOutputExpectation,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<ValidationStats, String> {
    let available = file.metadata().map_err(|error| error.to_string())?.len();
    let mut stats = ValidationStats::default();
    let mut offset = 0_u64;
    let mut initialized = false;
    let mut tracks = HashMap::new();
    let mut pending: Option<Vec<(u64, u64)>> = None;
    let mut sequence = None;
    while offset < available {
        checkpoint(deadline, cancelled)?;
        stats.boxes += 1;
        if stats.boxes > MAX_BOXES {
            return Err("completed MP4 exceeds box budget".into());
        }
        let header = read_box_header(file, offset, available)
            .map_err(|error| error.to_string())?
            .ok_or("completed MP4 ends inside a box header")?;
        let end = offset.checked_add(header.size).ok_or("MP4 box overflow")?;
        if end > available {
            return Err("completed MP4 ends inside a box".into());
        }
        if offset == 0 && (&header.kind != b"ftyp" || header.size < 16) {
            return Err("completed MP4 has no file type initialization".into());
        }
        match &header.kind {
            b"moov" | b"moof" => {
                if header.size > MAX_INDEX_BOX_BYTES {
                    return Err("completed MP4 metadata box is too large".into());
                }
                stats.metadata_bytes += header.size;
                if stats.metadata_bytes > MAX_METADATA_BYTES {
                    return Err("completed MP4 exceeds metadata budget".into());
                }
                let bytes = read_box(file, header).map_err(|error| error.to_string())?;
                if &header.kind == b"moov" {
                    if initialized || pending.is_some() {
                        return Err("duplicate or misplaced MP4 initialization".into());
                    }
                    tracks = initialization(&bytes, expected)?;
                    initialized = true;
                } else {
                    if !initialized || pending.is_some() {
                        return Err("movie fragment omits initialization or media data".into());
                    }
                    let children = boxes(&bytes)?;
                    let mfhd = unique(&children, b"mfhd")?;
                    let next_sequence = be_u32(mfhd, 4)?;
                    if sequence.is_some_and(|previous| next_sequence <= previous) {
                        return Err("movie fragment sequence regressed".into());
                    }
                    sequence = Some(next_sequence);
                    let mut ranges = Vec::new();
                    let mut seen = std::collections::HashSet::new();
                    let mut implicit_base = header.offset;
                    for traf in children.iter().filter(|child| &child.kind == b"traf") {
                        let id = fragment_track(
                            traf.payload,
                            header.offset,
                            &mut implicit_base,
                            &mut tracks,
                            &mut ranges,
                            &mut stats,
                            deadline,
                            cancelled,
                        )?;
                        if !seen.insert(id) {
                            return Err("duplicate track in movie fragment".into());
                        }
                    }
                    if ranges.is_empty() {
                        return Err("movie fragment has no samples".into());
                    }
                    ranges.sort_unstable();
                    if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
                        return Err("fragment samples overlap".into());
                    }
                    pending = Some(ranges);
                }
            }
            b"mdat" => {
                let ranges = pending.take().ok_or("media data has no movie fragment")?;
                let start = offset + header.header_size;
                if ranges.iter().any(|&(from, to)| from < start || to > end) {
                    return Err("fragment sample extent lies outside media data".into());
                }
            }
            b"ftyp" if offset != 0 => return Err("duplicate MP4 file type box".into()),
            _ => {}
        }
        offset = end;
    }
    if !initialized || pending.is_some() || sequence.is_none() {
        return Err("completed MP4 has no complete media fragments".into());
    }
    let expected_duration = expected
        .duration_seconds
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(|seconds| (seconds - expected.seek_seconds).max(0.0));
    for track in tracks.values() {
        let start = track.start.ok_or("required track has no media samples")?;
        // Muxer priming/reordering is bounded by 250 ms. Each track may have
        // its own source ending; the catalog records only the overall duration.
        if !track.chapter && start.abs() > 0.250 {
            return Err("track begins outside timestamp tolerance".into());
        }
        if let Some(duration) = expected_duration {
            let preroll = if expected.video_copy && expected.seek_seconds > 0.0 {
                10.0
            } else {
                0.250
            };
            if track.end > duration + preroll + 1.0 {
                return Err(format!(
                    "{} track covers {:.3}s, expected {:.3}s",
                    track.codec, track.end, duration
                ));
            }
        }
    }
    if let Some(duration) = expected_duration {
        let longest = tracks
            .values()
            .filter(|track| !track.chapter)
            .map(|track| track.end)
            .fold(0.0_f64, f64::max);
        let shortfall = (duration * 0.05).clamp(0.050, 1.0);
        if longest + shortfall < duration {
            return Err(format!(
                "output tracks cover {longest:.3}s, expected {duration:.3}s"
            ));
        }
    }
    checkpoint(deadline, cancelled)?;
    Ok(stats)
}

fn checkpoint(deadline: Instant, cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) {
        return Err("cancelled".into());
    }
    if Instant::now() >= deadline {
        return Err("completed MP4 verification deadline exceeded".into());
    }
    Ok(())
}

fn unique<'a>(children: &[SliceBox<'a>], kind: &[u8; 4]) -> Result<&'a [u8], String> {
    let mut found = children.iter().filter(|child| &child.kind == kind);
    let value = found
        .next()
        .ok_or("required MP4 initialization/fragment box is absent")?;
    if found.next().is_some() {
        return Err("duplicate MP4 initialization/fragment box".into());
    }
    Ok(value.payload)
}

fn initialization_header(
    bytes: &[u8],
    version_zero_len: usize,
    version_one_len: usize,
) -> Result<u8, String> {
    let version = *bytes
        .first()
        .ok_or("missing initialization header version")?;
    let minimum = match version {
        0 => version_zero_len,
        1 => version_one_len,
        _ => return Err("unsupported initialization header version".into()),
    };
    if bytes.len() < minimum {
        return Err("truncated initialization header".into());
    }
    Ok(version)
}

fn initialization(
    bytes: &[u8],
    expected: &RemuxOutputExpectation,
) -> Result<HashMap<u32, MediaTrack>, String> {
    let children = boxes(bytes)?;
    let mvhd = unique(&children, b"mvhd")?;
    let version = initialization_header(mvhd, 100, 112)?;
    if full_box_flags(mvhd)? != 0 || be_u32(mvhd, if version == 1 { 20 } else { 12 })? == 0 {
        return Err("invalid movie header timescale or flags".into());
    }
    let mvex = boxes(unique(&children, b"mvex")?)?;
    let mut defaults = HashMap::new();
    for entry in mvex.iter().filter(|child| &child.kind == b"trex") {
        if be_u32(entry.payload, 8)? != 1
            || defaults
                .insert(be_u32(entry.payload, 4)?, entry.payload)
                .is_some()
        {
            return Err("invalid track defaults".into());
        }
    }
    let mut tracks = HashMap::new();
    let mut video = Vec::new();
    let mut audio = Vec::new();
    let mut chapter_ids = std::collections::HashSet::new();
    for entry in children.iter().filter(|child| &child.kind == b"trak") {
        let trak = boxes(entry.payload)?;
        for reference in trak.iter().filter(|child| &child.kind == b"tref") {
            let references = boxes(reference.payload)?;
            for chapter in references.iter().filter(|child| &child.kind == b"chap") {
                let mdia = boxes(unique(&trak, b"mdia")?)?;
                let handler = unique(&mdia, b"hdlr")?.get(8..12);
                if !matches!(handler, Some(b"vide" | b"soun"))
                    || chapter.payload.len() != 4
                    || be_u32(chapter.payload, 0)? == 0
                {
                    return Err("invalid chapter track reference".into());
                }
                chapter_ids.insert(be_u32(chapter.payload, 0)?);
            }
        }
    }
    if chapter_ids.len() > 1 {
        return Err("completed MP4 has too many chapter tracks".into());
    }
    for entry in children.iter().filter(|child| &child.kind == b"trak") {
        if tracks.len() >= MAX_TRACKS {
            return Err("completed MP4 has too many tracks".into());
        }
        let track = parse_trak(entry.payload)?;
        let boxes_trak = boxes(entry.payload)?;
        initialization_header(unique(&boxes_trak, b"tkhd")?, 84, 96)?;
        let mdia = boxes(unique(&boxes_trak, b"mdia")?)?;
        let mdhd = unique(&mdia, b"mdhd")?;
        initialization_header(mdhd, 24, 36)?;
        if full_box_flags(mdhd)? != 0 {
            return Err("invalid media header flags".into());
        }
        let handler = unique(&mdia, b"hdlr")?
            .get(8..12)
            .ok_or("missing track handler")?;
        let chapter = chapter_ids.contains(&track.id);
        if if chapter {
            handler != b"text"
        } else {
            handler != b"vide" && handler != b"soun"
        } {
            return Err("unexpected non-media track".into());
        }
        let minf = boxes(unique(&mdia, b"minf")?)?;
        let stbl = boxes(unique(&minf, b"stbl")?)?;
        let stsd = unique(&stbl, b"stsd")?;
        if be_u32(stsd, 4)? != 1 {
            return Err("unexpected sample description count".into());
        }
        let descriptions = boxes(stsd.get(8..).ok_or("truncated sample descriptions")?)?;
        if descriptions.len() != 1 {
            return Err("invalid sample description".into());
        }
        let description = descriptions[0];
        if chapter && &description.kind != b"text" {
            return Err("invalid chapter sample description".into());
        }
        let (mut codec, config) = match &description.kind {
            b"avc1" | b"avc3" => ("h264", Some(b"avcC")),
            b"hvc1" | b"hev1" => ("hevc", Some(b"hvcC")),
            b"vp09" => ("vp9", Some(b"vpcC")),
            b"av01" => ("av1", Some(b"av1C")),
            b"mp4v" => ("mpeg4", Some(b"esds")),
            b"mp4a" => ("aac", Some(b"esds")),
            b"ac-3" => ("ac3", Some(b"dac3")),
            b"ec-3" => ("eac3", Some(b"dec3")),
            b"Opus" => ("opus", Some(b"dOps")),
            b"fLaC" => ("flac", Some(b"dfLa")),
            b"alac" => ("alac", Some(b"alac")),
            b".mp3" => ("mp3", None),
            b"text" if chapter => ("chapter", None),
            _ => return Err("unsupported output sample description".into()),
        };
        let prefix = if chapter {
            51
        } else if track.video {
            78
        } else {
            28
        };
        let codec_tail = description
            .payload
            .get(prefix..)
            .ok_or("truncated codec initialization")?;
        let codec_boxes = if chapter {
            Vec::new()
        } else {
            boxes(codec_tail)?
        };
        if let Some(kind) = config {
            let config_bytes = unique(&codec_boxes, kind)?;
            match kind {
                b"avcC" => validate_avcc(config_bytes)?,
                b"hvcC" => validate_hvcc(config_bytes)?,
                b"esds" => {
                    let (object_type, decoder_config) = esds_config(config_bytes)?;
                    codec = match object_type {
                        0x40 if decoder_config.is_some_and(|config| config.len() >= 2) => "aac",
                        0x69 | 0x6b => "mp3",
                        0xa9..=0xac => "dts",
                        0x20 => "mpeg4",
                        0x60..=0x65 => "mpeg2",
                        0x6a => "mpeg1video",
                        _ => return Err("unsupported MPEG codec initialization".into()),
                    };
                }
                b"dac3" if config_bytes.len() >= 3 => {}
                b"dec3" if config_bytes.len() >= 2 => {}
                b"dOps" if config_bytes.len() >= 11 => {}
                b"dfLa" if config_bytes.len() >= 8 => {}
                b"alac" if config_bytes.len() >= 24 => {}
                b"vpcC"
                    if config_bytes.len() >= 12
                        && config_bytes[0] == 1
                        && full_box_flags(config_bytes)? == 0
                        && config_bytes.len()
                            == 12
                                + usize::from(u16::from_be_bytes([
                                    config_bytes[10],
                                    config_bytes[11],
                                ])) => {}
                b"av1C" if config_bytes.len() >= 4 && config_bytes[0] == 0x81 => {}
                _ => return Err("truncated codec configuration".into()),
            }
        }
        if track.video {
            video.push(codec.to_owned());
        } else if !chapter {
            audio.push(codec.to_owned());
        }
        let trex = defaults
            .remove(&track.id)
            .ok_or("track has no fragment defaults")?;
        let id = track.id;
        if id == 0
            || tracks
                .insert(
                    id,
                    MediaTrack {
                        track,
                        codec: codec.into(),
                        chapter,
                        default_duration: be_u32(trex, 12)?,
                        default_size: be_u32(trex, 16)?,
                        default_flags: be_u32(trex, 20)?,
                        start: None,
                        end: 0.0,
                        samples: 0,
                    },
                )
                .is_some()
        {
            return Err("duplicate or zero track identifier".into());
        }
    }
    let expected_video: Vec<_> = expected.video_codec.iter().cloned().collect();
    if video != expected_video
        || audio != expected.audio_codecs
        || tracks.is_empty()
        || video.len() + audio.len() > MAX_MEDIA_TRACKS
        || chapter_ids
            .iter()
            .any(|id| !tracks.get(id).is_some_and(|track| track.chapter))
        || !defaults.is_empty()
    {
        return Err(format!(
            "output tracks/codecs differ from negotiated plan: video={video:?}, audio={audio:?}"
        ));
    }
    Ok(tracks)
}

#[allow(clippy::too_many_arguments)] // One bounded fragment parse shares its aggregate budget and output track state.
fn fragment_track(
    bytes: &[u8],
    moof: u64,
    implicit_base: &mut u64,
    tracks: &mut HashMap<u32, MediaTrack>,
    ranges: &mut Vec<(u64, u64)>,
    stats: &mut ValidationStats,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<u32, String> {
    let children = boxes(bytes)?;
    let tfhd = unique(&children, b"tfhd")?;
    let id = be_u32(tfhd, 4)?;
    let track = tracks
        .get_mut(&id)
        .ok_or("fragment references an unknown track")?;
    let flags = full_box_flags(tfhd)?;
    if tfhd[0] != 0 || flags & !0x02003b != 0 {
        return Err("unsupported track fragment header".into());
    }
    let mut cursor = 8;
    let base = if flags & 1 != 0 {
        let base = be_u64(tfhd, cursor)?;
        cursor += 8;
        base
    } else if flags & 0x020000 != 0 {
        moof
    } else {
        *implicit_base
    };
    if flags & 2 != 0 {
        if be_u32(tfhd, cursor)? != 1 {
            return Err("unknown fragment sample description".into());
        }
        cursor += 4;
    }
    let duration = if flags & 8 != 0 {
        let value = be_u32(tfhd, cursor)?;
        cursor += 4;
        value
    } else {
        track.default_duration
    };
    let size = if flags & 16 != 0 {
        let value = be_u32(tfhd, cursor)?;
        cursor += 4;
        value
    } else {
        track.default_size
    };
    let default_flags = if flags & 32 != 0 {
        let value = be_u32(tfhd, cursor)?;
        cursor += 4;
        value
    } else {
        track.default_flags
    };
    if cursor != tfhd.len() {
        return Err("invalid track fragment header boundary".into());
    }
    let tfdt = unique(&children, b"tfdt")?;
    let mut timestamp = match tfdt.first() {
        Some(0) => u64::from(be_u32(tfdt, 4)?),
        Some(1) => be_u64(tfdt, 4)?,
        _ => return Err("unsupported decode timestamp version".into()),
    };
    if tfdt.len() != if tfdt[0] == 1 { 12 } else { 8 } || full_box_flags(tfdt)? != 0 {
        return Err("invalid decode timestamp boundary".into());
    }
    let start = timestamp as f64 / f64::from(track.track.timescale);
    if track.start.is_some() && (start - track.end).abs() > 0.002 {
        return Err("track decode timeline has a gap or overlap".into());
    }
    if track.start.is_none() {
        track.start = Some(start);
    }
    let mut previous_end = None;
    let initial_samples = track.samples;
    for trun in children.iter().filter(|child| &child.kind == b"trun") {
        checkpoint(deadline, cancelled)?;
        let flags = full_box_flags(trun.payload)?;
        if trun.payload[0] > 1 || flags & !0x000f05 != 0 || flags & 0x404 == 0x404 {
            return Err("unsupported sample run flags or version".into());
        }
        let count = u64::from(be_u32(trun.payload, 4)?);
        stats.samples = stats
            .samples
            .checked_add(count)
            .ok_or("sample budget overflow")?;
        if stats.samples > MAX_SAMPLES {
            return Err("completed MP4 exceeds sample budget".into());
        }
        let mut cursor = 8;
        let start = if flags & 1 != 0 {
            let relative = i64::from(be_u32(trun.payload, cursor)? as i32);
            cursor += 4;
            base.checked_add_signed(relative)
                .ok_or("sample data offset overflow")?
        } else {
            previous_end.unwrap_or(base)
        };
        let first_flags = if flags & 4 != 0 {
            let value = be_u32(trun.payload, cursor)?;
            cursor += 4;
            value
        } else {
            default_flags
        };
        let mut end = start;
        for sample in 0..count {
            if sample % 4096 == 0 {
                checkpoint(deadline, cancelled)?;
            }
            let sample_duration = if flags & 0x100 != 0 {
                let value = be_u32(trun.payload, cursor)?;
                cursor += 4;
                value
            } else {
                duration
            };
            let sample_size = if flags & 0x200 != 0 {
                let value = be_u32(trun.payload, cursor)?;
                cursor += 4;
                value
            } else {
                size
            };
            let sample_flags = if flags & 0x400 != 0 {
                let value = be_u32(trun.payload, cursor)?;
                cursor += 4;
                value
            } else if sample == 0 {
                first_flags
            } else {
                default_flags
            };
            if flags & 0x800 != 0 {
                let composition = be_u32(trun.payload, cursor)?;
                cursor += 4;
                let composition = if trun.payload[0] == 1 {
                    i64::from(composition as i32)
                } else {
                    i64::from(composition)
                };
                if (composition as f64 / f64::from(track.track.timescale)).abs() > 2.0 {
                    return Err("sample composition offset exceeds reorder tolerance".into());
                }
            }
            if sample_duration == 0 || sample_size == 0 {
                return Err("empty or durationless media sample".into());
            }
            if track.samples == 0 && track.track.video && sample_flags & 0x10000 != 0 {
                return Err("video begins without a random access sample".into());
            }
            timestamp = timestamp
                .checked_add(u64::from(sample_duration))
                .ok_or("sample timestamp overflow")?;
            end = end
                .checked_add(u64::from(sample_size))
                .ok_or("sample size overflow")?;
            track.samples += 1;
        }
        if cursor != trun.payload.len() {
            return Err("invalid sample run boundary".into());
        }
        if count > 0 {
            ranges.push((start, end));
        }
        previous_end = Some(end);
    }
    if track.samples == initial_samples {
        return Err("empty track fragment".into());
    }
    *implicit_base = previous_end.ok_or("track has no sample runs")?;
    track.end = timestamp as f64 / f64::from(track.track.timescale);
    Ok(id)
}

fn nal_unit<'a>(bytes: &mut &'a [u8]) -> Result<&'a [u8], String> {
    let length = bytes.get(..2).ok_or("truncated codec NAL size")?;
    let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
    let nal = bytes
        .get(2..2 + length)
        .filter(|nal| !nal.is_empty())
        .ok_or("truncated codec NAL")?;
    *bytes = &bytes[2 + length..];
    Ok(nal)
}

fn validate_avcc(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 7 || bytes[0] != 1 {
        return Err("invalid AVC initialization".into());
    }
    let count = bytes[5] & 31;
    if count == 0 {
        return Err("AVC initialization has no SPS".into());
    }
    let mut remaining = &bytes[6..];
    for _ in 0..count {
        if nal_unit(&mut remaining)?[0] & 31 != 7 {
            return Err("invalid AVC SPS".into());
        }
    }
    let count = *remaining.first().ok_or("AVC initialization has no PPS")?;
    if count == 0 {
        return Err("AVC initialization has no PPS".into());
    }
    remaining = &remaining[1..];
    for _ in 0..count {
        if nal_unit(&mut remaining)?[0] & 31 != 8 {
            return Err("invalid AVC PPS".into());
        }
    }
    // High-profile avcC may carry additional chroma/bit-depth/SPS-extension fields.
    Ok(())
}

fn validate_hvcc(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 23 || bytes[0] != 1 {
        return Err("invalid HEVC initialization".into());
    }
    let mut remaining = &bytes[23..];
    let mut sps = false;
    let mut pps = false;
    for _ in 0..bytes[22] {
        let header = remaining.get(..3).ok_or("truncated HEVC NAL array")?;
        let kind = header[0] & 63;
        let count = u16::from_be_bytes([header[1], header[2]]);
        remaining = &remaining[3..];
        for _ in 0..count {
            let nal = nal_unit(&mut remaining)?;
            if nal.len() < 2 || (nal[0] >> 1) & 63 != kind {
                return Err("invalid HEVC parameter set".into());
            }
            sps |= kind == 33;
            pps |= kind == 34;
        }
    }
    if !sps || !pps || !remaining.is_empty() {
        return Err("incomplete HEVC parameter sets".into());
    }
    Ok(())
}

fn descriptor(bytes: &[u8], tag: u8) -> Result<&[u8], String> {
    if bytes.first() != Some(&tag) {
        return Err("missing MPEG codec descriptor".into());
    }
    let mut length = 0_usize;
    for offset in 1..=4 {
        let byte = *bytes
            .get(offset)
            .ok_or("truncated MPEG descriptor length")?;
        length = (length << 7) | usize::from(byte & 127);
        if byte & 128 == 0 {
            return bytes
                .get(offset + 1..offset + 1 + length)
                .ok_or_else(|| "truncated MPEG descriptor".into());
        }
    }
    Err("invalid MPEG descriptor length".into())
}

fn esds_config(bytes: &[u8]) -> Result<(u8, Option<&[u8]>), String> {
    let es = descriptor(bytes.get(4..).ok_or("truncated ES descriptor")?, 3)?;
    let flags = *es.get(2).ok_or("truncated ES flags")?;
    let mut offset = 3;
    if flags & 128 != 0 {
        offset += 2;
    }
    if flags & 64 != 0 {
        offset += 1 + usize::from(*es.get(offset).ok_or("truncated ES URL")?);
    }
    if flags & 32 != 0 {
        offset += 2;
    }
    let decoder = descriptor(es.get(offset..).ok_or("truncated decoder descriptor")?, 4)?;
    let config = decoder.get(13..).ok_or("truncated decoder configuration")?;
    let specific = if config.first() == Some(&5) {
        Some(descriptor(config, 5)?)
    } else {
        None
    };
    Ok((decoder[0], specific))
}
