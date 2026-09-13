//! Bounded conversion of a video-only fragmented MP4. Each sample keeps its
//! original decode/composition timing and base-layer bytes. Only RPU, EL and
//! sample-size/data-offset fields change; no whole-movie staging is needed.

use super::profile8_rewrite::{be32, children, kind, only, split_sample, BoxRange};
use super::RemuxP8Error;
use dolby_vision::rpu::{dovi_rpu::DoviRpu, ConversionMode};
use std::io::{Read, Write};

const MAX_METADATA: usize = 1024 * 1024;
const MAX_FRAGMENT: usize = 64 * 1024 * 1024;
const MAX_RPU: usize = 64 * 1024;
const MAX_SAMPLES: usize = 65_536;

fn invalid(message: &str) -> RemuxP8Error {
    RemuxP8Error::Pipeline(format!("Profile-8 stream: {message}"))
}

fn boxed(kind: &[u8; 4], payload: &[u8]) -> Result<Vec<u8>, RemuxP8Error> {
    let size =
        u32::try_from(payload.len().saturating_add(8)).map_err(|_| invalid("box size overflow"))?;
    let mut bytes = Vec::with_capacity(size as usize);
    bytes.extend(size.to_be_bytes());
    bytes.extend(kind);
    bytes.extend(payload);
    Ok(bytes)
}

fn replace(bytes: &mut Vec<u8>, path: &[BoxRange], replacement: &[u8]) -> Result<(), RemuxP8Error> {
    let item = path.last().ok_or_else(|| invalid("empty box path"))?;
    let old_size = item.end - item.start;
    for ancestor in &path[..path.len() - 1] {
        if ancestor.content - ancestor.start != 8 {
            return Err(invalid("unsupported large metadata box"));
        }
        let size = (ancestor.end - ancestor.start)
            .checked_sub(old_size)
            .and_then(|size| size.checked_add(replacement.len()))
            .and_then(|size| u32::try_from(size).ok())
            .ok_or_else(|| invalid("metadata growth overflow"))?;
        bytes[ancestor.start..ancestor.start + 4].copy_from_slice(&size.to_be_bytes());
    }
    bytes.splice(item.start..item.end, replacement.iter().copied());
    Ok(())
}

struct Video {
    track: u32,
    length_bytes: usize,
}

fn initialization(bytes: &mut Vec<u8>) -> Result<Video, RemuxP8Error> {
    let moov = only(&children(bytes, 0, bytes.len())?, b"moov")?;
    let trak = only(&children(bytes, moov.content, moov.end)?, b"trak")?;
    let track_children = children(bytes, trak.content, trak.end)?;
    let tkhd = only(&track_children, b"tkhd")?;
    let track = match bytes.get(tkhd.content) {
        Some(0) if tkhd.end - tkhd.content >= 20 => be32(bytes, tkhd.content + 12)?,
        Some(1) if tkhd.end - tkhd.content >= 32 => be32(bytes, tkhd.content + 20)?,
        _ => return Err(invalid("unsupported track header")),
    };
    let mut path = vec![moov, trak];
    for name in [b"mdia", b"minf", b"stbl", b"stsd"] {
        let parent = path.last().ok_or_else(|| invalid("missing parent"))?;
        path.push(only(&children(bytes, parent.content, parent.end)?, name)?);
    }
    let stsd = *path
        .last()
        .ok_or_else(|| invalid("missing sample description"))?;
    if stsd.end - stsd.content < 8 || be32(bytes, stsd.content + 4)? != 1 {
        return Err(invalid("one video sample description required"));
    }
    let entry = only(&children(bytes, stsd.content + 8, stsd.end)?, b"hvc1")?;
    if entry.end - entry.content < 78 {
        return Err(invalid("truncated HEVC sample entry"));
    }
    let entries = children(bytes, entry.content + 78, entry.end)?;
    let hvcc = only(&entries, b"hvcC")?;
    if hvcc.end - hvcc.content < 23 {
        return Err(invalid("truncated HEVC configuration"));
    }
    let length_bytes = usize::from((bytes[hvcc.content + 21] & 3) + 1);
    let configurations: Vec<_> = entries
        .iter()
        .filter(|entry| matches!(&entry.kind, b"dvcC" | b"dvvC"))
        .collect();
    if configurations.len() != 1 {
        return Err(invalid("one Dolby Vision configuration required"));
    }
    let config = configurations[0];
    if config.end - config.content < 5 || bytes[config.content] != 1 {
        return Err(invalid("truncated Dolby Vision configuration"));
    }
    let flags = u16::from_be_bytes([bytes[config.content + 2], bytes[config.content + 3]]);
    if flags >> 9 != 7 || flags & 7 != 7 {
        return Err(invalid(
            "Profile 7 with base layer, enhancement layer and RPU required",
        ));
    }
    let level = ((flags >> 3) & 63) as u8;
    let mut payload = bytes[entry.content..entry.content + 78].to_vec();
    for item in entries {
        if !matches!(&item.kind, b"dvcC" | b"dvvC") {
            payload.extend_from_slice(&bytes[item.start..item.end]);
        }
    }
    payload.extend(super::profile8_decoder_configuration(level));
    path.push(entry);
    replace(bytes, &path, &boxed(b"hvc1", &payload)?)?;
    Ok(Video {
        track,
        length_bytes,
    })
}

fn sample(bytes: &[u8], length_bytes: usize) -> Result<Vec<u8>, RemuxP8Error> {
    let nals = split_sample(bytes, length_bytes)?;
    let mut output = Vec::with_capacity(bytes.len());
    let mut pictures = 0;
    let mut rpus = 0;
    for nal in nals {
        let typ = kind(nal)?;
        if typ == 63 {
            continue;
        }
        if typ <= 31 && nal[2] & 0x80 != 0 {
            pictures += 1;
        }
        let converted;
        let nal = if typ == 62 {
            rpus += 1;
            if nal.len() > MAX_RPU {
                return Err(invalid("RPU exceeds buffer bound"));
            }
            let mut rpu = DoviRpu::parse_unspec62_nalu(nal)
                .map_err(|_| invalid("invalid Dolby Vision RPU"))?;
            if rpu.dovi_profile != 7 {
                return Err(invalid("RPU is not Profile 7"));
            }
            rpu.convert_with_mode(ConversionMode::To81)
                .map_err(|_| invalid("RPU conversion failed"))?;
            converted = rpu
                .write_hevc_unspec62_nalu()
                .map_err(|_| invalid("RPU serialization failed"))?;
            converted.as_slice()
        } else {
            nal
        };
        let length = u32::try_from(nal.len())
            .map_err(|_| invalid("NAL size overflow"))?
            .to_be_bytes();
        if length[..4 - length_bytes].iter().any(|byte| *byte != 0)
            || output.len().saturating_add(length_bytes + nal.len()) > MAX_FRAGMENT
        {
            return Err(invalid("converted sample exceeds buffer bound"));
        }
        output.extend_from_slice(&length[4 - length_bytes..]);
        output.extend_from_slice(nal);
    }
    if pictures != 1 || rpus != 1 {
        return Err(invalid("one picture and RPU per sample required"));
    }
    Ok(output)
}

fn fragment(
    moof: &mut Vec<u8>,
    mdat: &[u8],
    video: &Video,
    check: &mut impl FnMut() -> Result<(), RemuxP8Error>,
) -> Result<Vec<u8>, RemuxP8Error> {
    let root = only(&children(moof, 0, moof.len())?, b"moof")?;
    let traf = only(&children(moof, root.content, root.end)?, b"traf")?;
    let entries = children(moof, traf.content, traf.end)?;
    let tfhd = only(&entries, b"tfhd")?;
    if tfhd.end - tfhd.content < 8 || be32(moof, tfhd.content + 4)? != video.track {
        return Err(invalid("fragment track mismatch"));
    }
    let flags = be32(moof, tfhd.content)?;
    if flags & !0x020038 != 0 || flags & 0x020000 == 0 {
        return Err(invalid(
            "fragment must use relative offsets and one description",
        ));
    }
    let mut pos = tfhd.content + 8;
    if flags & 8 != 0 {
        pos += 4;
    }
    let default_size = if flags & 16 != 0 { be32(moof, pos)? } else { 0 };
    if flags & 16 != 0 {
        pos += 4;
    }
    if flags & 32 != 0 {
        pos += 4;
    }
    if pos != tfhd.end {
        return Err(invalid("truncated fragment defaults"));
    }
    let trun = only(&entries, b"trun")?;
    if trun.end - trun.content < 12 {
        return Err(invalid("truncated sample run"));
    }
    let flags = be32(moof, trun.content)?;
    if flags & !0x01000f05 != 0 || flags & 1 == 0 {
        return Err(invalid("unsupported sample run flags"));
    }
    let count = be32(moof, trun.content + 4)? as usize;
    if count == 0 || count > MAX_SAMPLES {
        return Err(invalid("sample count exceeds bound"));
    }
    let data_offset = be32(moof, trun.content + 8)? as usize;
    if data_offset != moof.len() + 8 {
        return Err(invalid("noncontiguous fragment media"));
    }
    let mut run = moof[trun.content..trun.content + 12].to_vec();
    run[..4].copy_from_slice(&(flags | 0x200).to_be_bytes());
    let mut pos = trun.content + 12;
    if flags & 4 != 0 {
        run.extend(be32(moof, pos)?.to_be_bytes());
        pos += 4;
    }
    let mut at = 0usize;
    let mut output = Vec::with_capacity(mdat.len());
    for _ in 0..count {
        check()?;
        if flags & 0x100 != 0 {
            run.extend(be32(moof, pos)?.to_be_bytes());
            pos += 4;
        }
        let size = if flags & 0x200 != 0 {
            let size = be32(moof, pos)?;
            pos += 4;
            size
        } else {
            default_size
        } as usize;
        let end = at
            .checked_add(size)
            .filter(|end| *end <= mdat.len())
            .ok_or_else(|| invalid("sample exceeds media bounds"))?;
        let converted = sample(&mdat[at..end], video.length_bytes)?;
        at = end;
        if output.len().saturating_add(converted.len()) > MAX_FRAGMENT {
            return Err(invalid("converted fragment exceeds buffer bound"));
        }
        run.extend((converted.len() as u32).to_be_bytes());
        output.extend(converted);
        for flag in [0x400, 0x800] {
            if flags & flag != 0 {
                run.extend(be32(moof, pos)?.to_be_bytes());
                pos += 4;
            }
        }
    }
    if at != mdat.len() || pos != trun.end {
        return Err(invalid("unreferenced fragment bytes"));
    }
    let new_offset = moof.len() - (trun.end - trun.start) + run.len() + 16;
    run[8..12].copy_from_slice(&(new_offset as u32).to_be_bytes());
    replace(moof, &[root, traf, trun], &boxed(b"trun", &run)?)?;
    boxed(b"mdat", &output)
}

fn read_exact(
    input: &mut impl Read,
    bytes: &mut [u8],
    check: &mut impl FnMut() -> Result<(), RemuxP8Error>,
) -> Result<(), RemuxP8Error> {
    let mut at = 0;
    while at < bytes.len() {
        check()?;
        match input.read(&mut bytes[at..]) {
            Ok(0) => return Err(invalid("truncated stream")),
            Ok(read) => at += read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(invalid(&format!("read stream: {error}"))),
        }
    }
    Ok(())
}

fn read_box(
    input: &mut impl Read,
    check: &mut impl FnMut() -> Result<(), RemuxP8Error>,
) -> Result<Option<Vec<u8>>, RemuxP8Error> {
    let mut header = [0; 8];
    loop {
        check()?;
        match input.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(invalid(&format!("read box: {error}"))),
        }
    }
    read_exact(input, &mut header[1..], check)?;
    let size =
        u32::from_be_bytes(header[..4].try_into().map_err(|_| invalid("box header"))?) as usize;
    let limit = if &header[4..] == b"mdat" {
        MAX_FRAGMENT + 8
    } else {
        MAX_METADATA
    };
    if size < 8 || size > limit {
        return Err(invalid("box exceeds buffer bound"));
    }
    let mut bytes = vec![0; size];
    bytes[..8].copy_from_slice(&header);
    read_exact(input, &mut bytes[8..], check)?;
    Ok(Some(bytes))
}

fn write_all(
    output: &mut impl Write,
    bytes: &[u8],
    check: &mut impl FnMut() -> Result<(), RemuxP8Error>,
) -> Result<(), RemuxP8Error> {
    let mut at = 0;
    while at < bytes.len() {
        check()?;
        match output.write(&bytes[at..bytes.len().min(at + 64 * 1024)]) {
            Ok(0) => return Err(invalid("output closed")),
            Ok(written) => at += written,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(invalid(&format!("write stream: {error}"))),
        }
    }
    Ok(())
}

pub(super) fn rewrite(
    input: &mut impl Read,
    output: &mut impl Write,
    check: &mut impl FnMut() -> Result<(), RemuxP8Error>,
) -> Result<(), RemuxP8Error> {
    let mut video = None;
    let mut fragments = 0usize;
    while let Some(mut bytes) = read_box(input, check)? {
        match &bytes[4..8] {
            b"ftyp" if video.is_none() => write_all(output, &bytes, check)?,
            b"moov" if video.is_none() => {
                video = Some(initialization(&mut bytes)?);
                write_all(output, &bytes, check)?;
            }
            b"moof" => {
                let video = video
                    .as_ref()
                    .ok_or_else(|| invalid("missing initialization"))?;
                let mdat =
                    read_box(input, check)?.ok_or_else(|| invalid("missing fragment data"))?;
                if &mdat[4..8] != b"mdat" {
                    return Err(invalid("fragment data must follow sample run"));
                }
                let converted = fragment(&mut bytes, &mdat[8..], video, check)?;
                write_all(output, &bytes, check)?;
                write_all(output, &converted, check)?;
                fragments = fragments.saturating_add(1);
            }
            // Absolute file offsets in the source trailer are invalid after
            // conversion; the final mux constructs its own trailer.
            b"mfra" if fragments != 0 => {}
            b"free" => {}
            _ => return Err(invalid("unsupported fragmented MP4 layout")),
        }
    }
    if fragments == 0 {
        return Err(invalid("no converted video fragments"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_boxes_and_truncated_bodies_before_output() {
        for size in [0, 1, 7, (MAX_FRAGMENT + 9) as u32, u32::MAX] {
            let mut bytes = size.to_be_bytes().to_vec();
            bytes.extend(b"mdat");
            let mut output = Vec::new();
            assert!(rewrite(&mut bytes.as_slice(), &mut output, &mut || Ok(())).is_err());
            assert!(output.is_empty());
        }
        let bytes = boxed(b"ftyp", b"isom").unwrap();
        for length in 1..bytes.len() {
            let mut output = Vec::new();
            assert!(rewrite(&mut &bytes[..length], &mut output, &mut || Ok(())).is_err());
            assert!(output.is_empty());
        }
    }

    #[test]
    fn malformed_nal_lengths_and_missing_pictures_are_rejected() {
        for bytes in [
            vec![],
            vec![0, 0, 0, 0],
            vec![255; 8],
            vec![0, 0, 0, 3, 124, 1, 0],
        ] {
            assert!(sample(&bytes, 4).is_err());
        }
    }
}
