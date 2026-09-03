use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const MAX_INDEX_BOX_BYTES: u64 = 4 * 1024 * 1024;
const MIN_HLS_TARGET_DURATION_SECONDS: u64 = 1;
const HLS_STARTUP_BUFFER_SECONDS: f64 = 1.0;
const MAX_MSE_PLAYLIST_FRAGMENTS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Segment {
    pub(super) offset: u64,
    pub(super) length: u64,
    pub(super) duration: f64,
}

#[derive(Debug, Default)]
pub(super) struct Index {
    scan_offset: u64,
    init_end: Option<u64>,
    selected_track: Option<Track>,
    defaults: HashMap<u32, TrackDefaults>,
    pending_fragment: Option<Fragment>,
    pending_segment: Option<Segment>,
    fragments: Vec<Segment>,
    dependent_fragments: usize,
    segments: Vec<Segment>,
    finalized: bool,
}

#[derive(Clone, Copy, Debug)]
struct Track {
    id: u32,
    timescale: u32,
    video: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct TrackDefaults {
    duration: Option<u32>,
    flags: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
struct Fragment {
    offset: u64,
    duration: f64,
    random_access: bool,
}

#[derive(Clone, Copy, Debug)]
struct BoxHeader {
    offset: u64,
    size: u64,
    header_size: u64,
    kind: [u8; 4],
}

#[derive(Clone, Copy, Debug)]
struct SliceBox<'a> {
    kind: [u8; 4],
    payload: &'a [u8],
}

impl Index {
    pub(super) fn update(&mut self, path: &Path, complete: bool) -> Result<(), String> {
        let mut file = File::open(path).map_err(|error| format!("open HLS media: {error}"))?;
        let available = file
            .metadata()
            .map_err(|error| format!("stat HLS media: {error}"))?
            .len();
        if available < self.scan_offset {
            *self = Self::default();
        }
        while let Some(header) = read_box_header(&mut file, self.scan_offset, available)
            .map_err(|error| format!("read fragmented MP4: {error}"))?
        {
            let end = header
                .offset
                .checked_add(header.size)
                .ok_or_else(|| "fragmented MP4 box offset overflow".to_owned())?;
            if end > available {
                break;
            }
            match &header.kind {
                b"ftyp" if header.offset == 0 => {}
                b"moov" => {
                    if header.size > MAX_INDEX_BOX_BYTES {
                        return Err("fragmented MP4 initialization is too large".into());
                    }
                    let bytes = read_box(&mut file, header)
                        .map_err(|error| format!("read fragmented MP4 initialization: {error}"))?;
                    let (track, defaults) = parse_moov(&bytes)?;
                    self.selected_track = Some(track);
                    self.defaults = defaults;
                    self.init_end = Some(end);
                }
                b"moof" => {
                    if self.pending_fragment.is_some() {
                        return Err("fragmented MP4 movie fragment is missing media data".into());
                    }
                    let track = self.selected_track.ok_or_else(|| {
                        "fragmented MP4 media precedes its initialization".to_owned()
                    })?;
                    if header.size > MAX_INDEX_BOX_BYTES {
                        return Err("fragmented MP4 movie fragment is too large".into());
                    }
                    let bytes = read_box(&mut file, header)
                        .map_err(|error| format!("read fragmented MP4 movie fragment: {error}"))?;
                    let timing = parse_moof(&bytes, track, self.defaults.get(&track.id).copied())?;
                    self.pending_fragment = Some(Fragment {
                        offset: header.offset,
                        duration: timing.duration,
                        random_access: !track.video || timing.random_access,
                    });
                }
                b"mdat" => {
                    if let Some(fragment) = self.pending_fragment.take() {
                        self.push_fragment(fragment, end)?;
                    }
                }
                _ => {}
            }
            self.scan_offset = end;
        }
        if complete && !self.finalized {
            if self.pending_fragment.is_some() {
                return Err("completed fragmented MP4 ends inside a media fragment".into());
            }
            if self.scan_offset != available {
                return Err("completed fragmented MP4 ends inside a box".into());
            }
            if let Some(segment) = self.pending_segment.take() {
                self.segments.push(segment);
            }
            self.finalized = true;
        }
        Ok(())
    }

    pub(super) fn has_playable_segment(&self) -> bool {
        self.init_end.is_some() && !self.segments.is_empty()
    }

    pub(super) fn has_startup_buffer(&self, complete: bool) -> bool {
        self.has_playable_segment()
            && (complete
                || self
                    .segments
                    .iter()
                    .map(|segment| segment.duration)
                    .sum::<f64>()
                    >= HLS_STARTUP_BUFFER_SECONDS)
    }

    pub(super) fn has_mse_startup_buffer(&self, complete: bool) -> bool {
        self.init_end.is_some()
            && !self.fragments.is_empty()
            && (complete
                || self
                    .fragments
                    .iter()
                    .map(|fragment| fragment.duration)
                    .sum::<f64>()
                    >= HLS_STARTUP_BUFFER_SECONDS)
    }

    /// Encoded fragmented producers force every movie fragment to begin with an IDR.
    /// Such a complete fragment is already a fixed, independently decodable
    /// segment and does not need the ordinary one-fragment look-ahead used to
    /// coalesce copied streams with non-random-access fragments.
    pub(super) fn has_independent_startup_buffer(&self, complete: bool) -> bool {
        self.init_end.is_some()
            && !self.fragments.is_empty()
            && self.dependent_fragments == 0
            && (complete
                || self
                    .fragments
                    .iter()
                    .map(|fragment| fragment.duration)
                    .sum::<f64>()
                    >= HLS_STARTUP_BUFFER_SECONDS)
    }

    pub(super) fn has_mse_fragments_after(&self, after: usize, complete: bool) -> bool {
        if after == 0 {
            return self.has_mse_startup_buffer(complete);
        }
        self.init_end.is_some() && (self.fragments.len() > after || complete)
    }

    pub(super) fn playlist(&self, init_uri: &str, segment_uri: &str) -> Result<String, String> {
        if self.segments.is_empty() {
            return Err("fragmented MP4 has no complete media segments".into());
        }
        self.render_playlist(
            init_uri,
            segment_uri,
            &self.segments,
            true,
            0,
            self.finalized,
        )
    }

    pub(super) fn independent_fragment_playlist(
        &self,
        init_uri: &str,
        segment_uri: &str,
    ) -> Result<String, String> {
        if self.fragments.is_empty() {
            return Err("fragmented MP4 has no complete independent fragments".into());
        }
        if self.dependent_fragments != 0 {
            return Err("fragmented MP4 contains a dependent movie fragment".into());
        }
        self.render_playlist(
            init_uri,
            segment_uri,
            &self.fragments,
            true,
            0,
            self.finalized,
        )
    }

    pub(super) fn mse_playlist_after(
        &self,
        init_uri: &str,
        segment_uri: &str,
        after: usize,
    ) -> Result<String, String> {
        if after > self.fragments.len() {
            return Err("Media Source fragment cursor is past the available output".into());
        }
        let end = after
            .saturating_add(MAX_MSE_PLAYLIST_FRAGMENTS)
            .min(self.fragments.len());
        let ended = self.finalized && end == self.fragments.len();
        let fragments = &self.fragments[after..end];
        if fragments.is_empty() && !ended {
            return Err("fragmented MP4 has no new Media Source fragments".into());
        }
        self.render_playlist(init_uri, segment_uri, fragments, false, after, ended)
    }

    fn render_playlist(
        &self,
        init_uri: &str,
        segment_uri: &str,
        segments: &[Segment],
        independent: bool,
        media_sequence: usize,
        ended: bool,
    ) -> Result<String, String> {
        let init_end = self
            .init_end
            .ok_or_else(|| "fragmented MP4 initialization is not complete".to_owned())?;
        let target_duration = segments
            .iter()
            // RFC 8216 constrains TARGETDURATION against EXTINF rounded to
            // the nearest integer, not its ceiling.
            .map(|segment| segment.duration.round().max(1.0) as u64)
            .max()
            .unwrap_or(MIN_HLS_TARGET_DURATION_SECONDS)
            .max(MIN_HLS_TARGET_DURATION_SECONDS);
        let mut output = format!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target_duration}\n#EXT-X-MEDIA-SEQUENCE:{media_sequence}\n#EXT-X-PLAYLIST-TYPE:EVENT\n"
        );
        if independent {
            output.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
        }
        output.push_str(&format!(
            "#EXT-X-START:TIME-OFFSET=0,PRECISE=YES\n#EXT-X-MAP:URI=\"{init_uri}&hls_offset=0&hls_length={init_end}\"\n"
        ));
        for segment in segments {
            output.push_str(&format!(
                "#EXTINF:{:.6},\n{}&hls_offset={}&hls_length={}\n",
                segment.duration, segment_uri, segment.offset, segment.length
            ));
        }
        if ended {
            output.push_str("#EXT-X-ENDLIST\n");
        }
        Ok(output)
    }

    fn push_fragment(&mut self, fragment: Fragment, end: u64) -> Result<(), String> {
        if !fragment.duration.is_finite() || fragment.duration <= 0.0 {
            return Err("fragmented MP4 has an invalid fragment duration".into());
        }
        if !fragment.random_access && self.pending_segment.is_none() {
            return Err("fragmented MP4 begins without a random-access point".into());
        }
        self.fragments.push(Segment {
            offset: fragment.offset,
            length: end.saturating_sub(fragment.offset),
            duration: fragment.duration,
        });
        if !fragment.random_access {
            self.dependent_fragments = self.dependent_fragments.saturating_add(1);
        }
        if fragment.random_access {
            if let Some(segment) = self.pending_segment.replace(Segment {
                offset: fragment.offset,
                length: end.saturating_sub(fragment.offset),
                duration: fragment.duration,
            }) {
                self.segments.push(segment);
            }
        } else if let Some(segment) = self.pending_segment.as_mut() {
            segment.length = end.saturating_sub(segment.offset);
            segment.duration += fragment.duration;
        }
        Ok(())
    }
}

fn read_box_header(file: &mut File, offset: u64, available: u64) -> io::Result<Option<BoxHeader>> {
    if available.saturating_sub(offset) < 8 {
        return Ok(None);
    }
    let mut bytes = [0_u8; 16];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes[..8])?;
    let size32 = u32::from_be_bytes(bytes[..4].try_into().expect("four bytes"));
    let kind = bytes[4..8].try_into().expect("four bytes");
    let (size, header_size) = match size32 {
        0 => (available.saturating_sub(offset), 8),
        1 => {
            if available.saturating_sub(offset) < 16 {
                return Ok(None);
            }
            file.read_exact(&mut bytes[8..16])?;
            (
                u64::from_be_bytes(bytes[8..16].try_into().expect("eight bytes")),
                16,
            )
        }
        size => (u64::from(size), 8),
    };
    if size < header_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid MP4 box size",
        ));
    }
    Ok(Some(BoxHeader {
        offset,
        size,
        header_size,
        kind,
    }))
}

fn read_box(file: &mut File, header: BoxHeader) -> io::Result<Vec<u8>> {
    let payload_size = usize::try_from(header.size.saturating_sub(header.header_size))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "MP4 box is too large"))?;
    let mut bytes = vec![0_u8; payload_size];
    file.seek(SeekFrom::Start(header.offset + header.header_size))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn boxes(mut bytes: &[u8]) -> Result<Vec<SliceBox<'_>>, String> {
    let mut parsed = Vec::new();
    while !bytes.is_empty() {
        if bytes.len() < 8 {
            return Err("truncated nested MP4 box".into());
        }
        let size32 = be_u32(bytes, 0)?;
        let kind = bytes[4..8].try_into().expect("four bytes");
        let (size, header_size) = match size32 {
            0 => (bytes.len(), 8),
            1 => {
                let size = usize::try_from(be_u64(bytes, 8)?)
                    .map_err(|_| "nested MP4 box is too large".to_owned())?;
                (size, 16)
            }
            size => (usize::try_from(size).expect("u32 fits usize"), 8),
        };
        if size < header_size || size > bytes.len() {
            return Err("invalid nested MP4 box size".into());
        }
        parsed.push(SliceBox {
            kind,
            payload: &bytes[header_size..size],
        });
        bytes = &bytes[size..];
    }
    Ok(parsed)
}

fn parse_moov(bytes: &[u8]) -> Result<(Track, HashMap<u32, TrackDefaults>), String> {
    let mut tracks = Vec::new();
    let mut defaults = HashMap::new();
    for child in boxes(bytes)? {
        match &child.kind {
            b"trak" => tracks.push(parse_trak(child.payload)?),
            b"mvex" => {
                for entry in boxes(child.payload)? {
                    if &entry.kind != b"trex" {
                        continue;
                    }
                    let track_id = be_u32(entry.payload, 4)?;
                    defaults.insert(
                        track_id,
                        TrackDefaults {
                            duration: nonzero(be_u32(entry.payload, 12)?),
                            flags: Some(be_u32(entry.payload, 20)?),
                        },
                    );
                }
            }
            _ => {}
        }
    }
    let track = tracks
        .iter()
        .copied()
        .find(|track| track.video)
        .or_else(|| tracks.first().copied())
        .filter(|track| track.timescale > 0)
        .ok_or_else(|| "fragmented MP4 has no usable media track".to_owned())?;
    Ok((track, defaults))
}

fn parse_trak(bytes: &[u8]) -> Result<Track, String> {
    let mut id = None;
    let mut timescale = None;
    let mut video = false;
    for child in boxes(bytes)? {
        match &child.kind {
            b"tkhd" => {
                let version = *child
                    .payload
                    .first()
                    .ok_or_else(|| "truncated track header".to_owned())?;
                id = Some(be_u32(child.payload, if version == 1 { 20 } else { 12 })?);
            }
            b"mdia" => {
                for media in boxes(child.payload)? {
                    match &media.kind {
                        b"mdhd" => {
                            let version = *media
                                .payload
                                .first()
                                .ok_or_else(|| "truncated media header".to_owned())?;
                            timescale =
                                Some(be_u32(media.payload, if version == 1 { 20 } else { 12 })?);
                        }
                        b"hdlr" => video = media.payload.get(8..12) == Some(b"vide"),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(Track {
        id: id.ok_or_else(|| "fragmented MP4 track has no ID".to_owned())?,
        timescale: timescale
            .filter(|timescale| *timescale > 0)
            .ok_or_else(|| "fragmented MP4 track has no timescale".to_owned())?,
        video,
    })
}

#[derive(Clone, Copy, Debug)]
struct FragmentTiming {
    duration: f64,
    random_access: bool,
}

fn parse_moof(
    bytes: &[u8],
    selected: Track,
    trex: Option<TrackDefaults>,
) -> Result<FragmentTiming, String> {
    for child in boxes(bytes)? {
        if &child.kind != b"traf" {
            continue;
        }
        if let Some(timing) = parse_traf(child.payload, selected, trex)? {
            return Ok(timing);
        }
    }
    Err("movie fragment omits the selected track".into())
}

fn parse_traf(
    bytes: &[u8],
    selected: Track,
    trex: Option<TrackDefaults>,
) -> Result<Option<FragmentTiming>, String> {
    let children = boxes(bytes)?;
    let Some(tfhd) = children.iter().find(|child| &child.kind == b"tfhd") else {
        return Err("track fragment has no header".into());
    };
    let track_id = be_u32(tfhd.payload, 4)?;
    if track_id != selected.id {
        return Ok(None);
    }
    let tfhd_flags = full_box_flags(tfhd.payload)?;
    let mut cursor = 8;
    if tfhd_flags & 0x000001 != 0 {
        cursor += 8;
    }
    if tfhd_flags & 0x000002 != 0 {
        cursor += 4;
    }
    let default_duration = if tfhd_flags & 0x000008 != 0 {
        let value = be_u32(tfhd.payload, cursor)?;
        cursor += 4;
        nonzero(value)
    } else {
        trex.and_then(|defaults| defaults.duration)
    };
    if tfhd_flags & 0x000010 != 0 {
        cursor += 4;
    }
    let default_flags = if tfhd_flags & 0x000020 != 0 {
        Some(be_u32(tfhd.payload, cursor)?)
    } else {
        trex.and_then(|defaults| defaults.flags)
    };
    let mut duration = 0_u64;
    let mut first_flags = None;
    let mut samples_seen = 0_u64;
    for trun in children.iter().filter(|child| &child.kind == b"trun") {
        let flags = full_box_flags(trun.payload)?;
        let sample_count = u64::from(be_u32(trun.payload, 4)?);
        if sample_count > 1_000_000 {
            return Err("track fragment contains too many samples".into());
        }
        let mut offset = 8;
        if flags & 0x000001 != 0 {
            offset += 4;
        }
        let trun_first_flags = if flags & 0x000004 != 0 {
            let value = be_u32(trun.payload, offset)?;
            offset += 4;
            Some(value)
        } else {
            None
        };
        for sample in 0..sample_count {
            let sample_duration = if flags & 0x000100 != 0 {
                let value = be_u32(trun.payload, offset)?;
                offset += 4;
                Some(value)
            } else {
                default_duration
            }
            .ok_or_else(|| "track fragment omits sample durations".to_owned())?;
            duration = duration
                .checked_add(u64::from(sample_duration))
                .ok_or_else(|| "track fragment duration overflow".to_owned())?;
            if flags & 0x000200 != 0 {
                offset += 4;
            }
            let sample_flags = if flags & 0x000400 != 0 {
                let value = be_u32(trun.payload, offset)?;
                offset += 4;
                Some(value)
            } else if sample == 0 {
                trun_first_flags.or(default_flags)
            } else {
                default_flags
            };
            if first_flags.is_none() {
                first_flags = sample_flags;
            }
            if flags & 0x000800 != 0 {
                offset += 4;
            }
            if offset > trun.payload.len() {
                return Err("truncated track fragment run".into());
            }
            samples_seen = samples_seen.saturating_add(1);
        }
    }
    if samples_seen == 0 {
        return Err("track fragment contains no samples".into());
    }
    Ok(Some(FragmentTiming {
        duration: duration as f64 / f64::from(selected.timescale),
        random_access: first_flags.is_none_or(|flags| flags & 0x0001_0000 == 0),
    }))
}

fn full_box_flags(bytes: &[u8]) -> Result<u32, String> {
    let flags = bytes
        .get(1..4)
        .ok_or_else(|| "truncated full MP4 box".to_owned())?;
    Ok(u32::from_be_bytes([0, flags[0], flags[1], flags[2]]))
}

fn be_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let value = bytes
        .get(offset..offset.saturating_add(4))
        .ok_or_else(|| "truncated MP4 field".to_owned())?;
    Ok(u32::from_be_bytes(value.try_into().expect("four bytes")))
}

fn be_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let value = bytes
        .get(offset..offset.saturating_add(8))
        .ok_or_else(|| "truncated MP4 field".to_owned())?;
    Ok(u64::from_be_bytes(value.try_into().expect("eight bytes")))
}

fn nonzero(value: u32) -> Option<u32> {
    (value != 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "rusty-dlna-hls-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = u32::try_from(payload.len() + 8).unwrap();
        [size.to_be_bytes().as_slice(), kind, payload].concat()
    }

    fn full_atom(kind: &[u8; 4], flags: u32, payload: &[u8]) -> Vec<u8> {
        let full = [
            &[
                0,
                ((flags >> 16) & 0xff) as u8,
                ((flags >> 8) & 0xff) as u8,
                (flags & 0xff) as u8,
            ][..],
            payload,
        ]
        .concat();
        atom(kind, &full)
    }

    fn fixture() -> Vec<u8> {
        let mut tkhd = vec![0_u8; 16];
        tkhd[12..16].copy_from_slice(&1_u32.to_be_bytes());
        let mut mdhd = vec![0_u8; 16];
        mdhd[12..16].copy_from_slice(&1_000_u32.to_be_bytes());
        let mut hdlr = vec![0_u8; 12];
        hdlr[8..12].copy_from_slice(b"vide");
        let mdia = atom(
            b"mdia",
            &[atom(b"mdhd", &mdhd), atom(b"hdlr", &hdlr)].concat(),
        );
        let trak = atom(b"trak", &[atom(b"tkhd", &tkhd), mdia].concat());
        let mut trex = vec![0_u8; 24];
        trex[4..8].copy_from_slice(&1_u32.to_be_bytes());
        trex[12..16].copy_from_slice(&1_000_u32.to_be_bytes());
        let mvex = atom(b"mvex", &atom(b"trex", &trex));
        let mut bytes = atom(b"ftyp", b"iso6");
        bytes.extend(atom(b"moov", &[trak, mvex].concat()));
        for non_sync in [false, true, false] {
            let mut tfhd = [0_u8; 8];
            tfhd[4..8].copy_from_slice(&1_u32.to_be_bytes());
            let mut trun = [0_u8; 12];
            trun[4..8].copy_from_slice(&1_u32.to_be_bytes());
            trun[8..12]
                .copy_from_slice(&(if non_sync { 0x0001_0000_u32 } else { 0_u32 }).to_be_bytes());
            let traf = atom(
                b"traf",
                &[
                    full_atom(b"tfhd", 0, &tfhd[4..]),
                    full_atom(b"trun", 0x000004, &trun[4..]),
                ]
                .concat(),
            );
            bytes.extend(atom(b"moof", &traf));
            bytes.extend(atom(b"mdat", &[1, 2, 3, 4]));
        }
        bytes
    }

    #[test]
    fn indexes_complete_random_access_segments() {
        let dir = TempDir::new("complete");
        let path = dir.path().join("stream.mp4");
        std::fs::write(&path, fixture()).unwrap();
        let mut index = Index::default();
        index.update(&path, true).unwrap();
        assert_eq!(index.segments.len(), 2);
        assert_eq!(index.fragments.len(), 3);
        assert_eq!(index.segments[0].duration, 2.0);
        assert_eq!(index.segments[1].duration, 1.0);
        let playlist = index
            .playlist(
                "/media.mp4?delivery=hls_init",
                "/media.m4s?delivery=hls_segment",
            )
            .unwrap();
        assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
        assert!(playlist.contains("#EXT-X-TARGETDURATION:2"));
        assert!(playlist
            .contains("#EXT-X-MAP:URI=\"/media.mp4?delivery=hls_init&hls_offset=0&hls_length="));
        assert!(!playlist.contains("#EXT-X-BYTERANGE"));
        assert!(playlist.contains("#EXTINF:2.000000,"));
        assert!(playlist.contains("/media.m4s?delivery=hls_segment&hls_offset="));
        assert!(playlist.ends_with("#EXT-X-ENDLIST\n"));
        let mse_playlist = index
            .mse_playlist_after(
                "/media.mp4?delivery=mse_init",
                "/media.m4s?delivery=mse_segment",
                0,
            )
            .unwrap();
        assert!(!mse_playlist.contains("#EXT-X-INDEPENDENT-SEGMENTS"));
        assert_eq!(mse_playlist.matches("#EXTINF:1.000000,").count(), 3);
        assert!(mse_playlist.contains("delivery=mse_segment"));

        let delta = index
            .mse_playlist_after(
                "/media.mp4?delivery=mse_init",
                "/media.m4s?delivery=mse_segment",
                1,
            )
            .unwrap();
        assert!(delta.contains("#EXT-X-MEDIA-SEQUENCE:1"));
        assert_eq!(delta.matches("#EXTINF:1.000000,").count(), 2);
        assert!(delta.ends_with("#EXT-X-ENDLIST\n"));

        let exhausted = index
            .mse_playlist_after(
                "/media.mp4?delivery=mse_init",
                "/media.m4s?delivery=mse_segment",
                3,
            )
            .unwrap();
        assert!(exhausted.contains("#EXT-X-MEDIA-SEQUENCE:3"));
        assert!(!exhausted.contains("#EXTINF:"));
        assert!(exhausted.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn media_source_delta_playlists_are_bounded() {
        let mut index = Index {
            init_end: Some(64),
            finalized: true,
            ..Index::default()
        };
        index.fragments = (0..300)
            .map(|fragment| Segment {
                offset: 64 + fragment * 16,
                length: 16,
                duration: 1.0,
            })
            .collect();

        let first = index.mse_playlist_after("init", "segment", 0).unwrap();
        assert_eq!(first.matches("#EXTINF:").count(), 256);
        assert!(!first.contains("#EXT-X-ENDLIST"));
        let second = index.mse_playlist_after("init", "segment", 256).unwrap();
        assert_eq!(second.matches("#EXTINF:").count(), 44);
        assert!(second.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn growing_index_waits_for_a_following_random_access_point() {
        let dir = TempDir::new("growing");
        let path = dir.path().join("stream.mp4");
        let fixture = fixture();
        let final_moof = fixture
            .windows(4)
            .rposition(|bytes| bytes == b"moof")
            .unwrap()
            - 4;
        let mut file = File::create(&path).unwrap();
        file.write_all(&fixture[..final_moof]).unwrap();
        file.flush().unwrap();
        let mut index = Index::default();
        index.update(&path, false).unwrap();
        assert!(!index.has_playable_segment());
        assert!(!index.has_startup_buffer(false));
        assert!(index.has_mse_startup_buffer(false));
        file.write_all(&fixture[final_moof..]).unwrap();
        file.flush().unwrap();
        index.update(&path, false).unwrap();
        assert!(index.has_playable_segment());
        assert!(index.has_startup_buffer(false));
        assert!(!index
            .playlist("init", "segment")
            .unwrap()
            .contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn forced_independent_hls_publishes_the_first_complete_fragment() {
        let dir = TempDir::new("independent-growing");
        let path = dir.path().join("stream.mp4");
        let fixture = fixture();
        let second_moof = fixture
            .windows(4)
            .enumerate()
            .filter(|(_, bytes)| *bytes == b"moof")
            .nth(1)
            .map(|(offset, _)| offset - 4)
            .unwrap();
        std::fs::write(&path, &fixture[..second_moof]).unwrap();

        let mut index = Index::default();
        index.update(&path, false).unwrap();
        assert!(!index.has_startup_buffer(false));
        assert!(index.has_independent_startup_buffer(false));
        let playlist = index
            .independent_fragment_playlist("init", "segment")
            .unwrap();
        assert_eq!(playlist.matches("#EXTINF:1.000000,").count(), 1);
        assert!(playlist.contains("#EXT-X-INDEPENDENT-SEGMENTS"));
        assert!(!playlist.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn forced_independent_hls_rejects_a_dependent_fragment() {
        let dir = TempDir::new("dependent-growing");
        let path = dir.path().join("stream.mp4");
        let fixture = fixture();
        let final_moof = fixture
            .windows(4)
            .rposition(|bytes| bytes == b"moof")
            .unwrap()
            - 4;
        std::fs::write(&path, &fixture[..final_moof]).unwrap();

        let mut index = Index::default();
        index.update(&path, false).unwrap();
        assert!(!index.has_independent_startup_buffer(false));
        assert!(index
            .independent_fragment_playlist("init", "segment")
            .is_err());
    }

    #[test]
    fn rejects_truncated_complete_output() {
        let dir = TempDir::new("truncated");
        let path = dir.path().join("stream.mp4");
        let mut bytes = fixture();
        bytes.pop();
        std::fs::write(&path, bytes).unwrap();
        let error = Index::default().update(&path, true).unwrap_err();
        assert!(error.contains("ends inside"));
    }
}
