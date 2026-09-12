//! Preserve the source container's sample timeline while replacing Profile-7
//! RPU data. Samples compact only inside their original chunks; all timing and
//! chunk-offset tables remain byte-identical. The private staging file must
//! never be exposed if any validation, I/O operation or control check fails.
//! The explicit limits bound the combined metadata, mapping, original-sample,
//! replacement and Annex-B buffers below 256 MiB (excluding allocator overhead).

use super::{profile8_progress::SignalingFile, RemuxP8Control, RemuxP8Error};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const MAX_MOOV: u64 = 32 * 1024 * 1024;
const MAX_SAMPLE: usize = 32 * 1024 * 1024;
const MAX_SAMPLES: usize = 2_000_000;
const MAX_BOXES: usize = 4096;
const READ_CHUNK: usize = 64 * 1024;

fn check(control: &mut RemuxP8Control<'_>) -> Result<(), RemuxP8Error> {
    control
        .check("Profile-8 packet rewrite")
        .map_err(Into::into)
}

fn invalid(message: &str) -> RemuxP8Error {
    RemuxP8Error::Pipeline(format!("Profile-8 packet rewrite: {message}"))
}

fn io_error(error: std::io::Error) -> RemuxP8Error {
    super::profile8_progress::staging_io_error("Profile-8 packet rewrite staging I/O", error)
}

fn read_controlled(
    file: &mut impl Read,
    bytes: &mut [u8],
    control: &mut RemuxP8Control<'_>,
) -> Result<(), RemuxP8Error> {
    for chunk in bytes.chunks_mut(READ_CHUNK) {
        check(control)?;
        file.read_exact(chunk).map_err(io_error)?;
    }
    Ok(())
}

fn write_controlled(
    file: &mut impl Write,
    bytes: &[u8],
    control: &mut RemuxP8Control<'_>,
) -> Result<(), RemuxP8Error> {
    for chunk in bytes.chunks(READ_CHUNK) {
        check(control)?;
        file.write_all(chunk).map_err(io_error)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct BoxRange {
    start: usize,
    end: usize,
    content: usize,
    kind: [u8; 4],
}

fn be32(bytes: &[u8], offset: usize) -> Result<u32, RemuxP8Error> {
    let bytes = bytes
        .get(
            offset
                ..offset
                    .checked_add(4)
                    .ok_or_else(|| invalid("offset overflow"))?,
        )
        .ok_or_else(|| invalid("truncated integer"))?;
    Ok(u32::from_be_bytes(
        bytes.try_into().map_err(|_| invalid("truncated integer"))?,
    ))
}

fn be64(bytes: &[u8], offset: usize) -> Result<u64, RemuxP8Error> {
    let bytes = bytes
        .get(
            offset
                ..offset
                    .checked_add(8)
                    .ok_or_else(|| invalid("offset overflow"))?,
        )
        .ok_or_else(|| invalid("truncated integer"))?;
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| invalid("truncated integer"))?,
    ))
}

fn children(bytes: &[u8], start: usize, end: usize) -> Result<Vec<BoxRange>, RemuxP8Error> {
    if end > bytes.len() || start > end {
        return Err(invalid("invalid container bounds"));
    }
    let mut boxes = Vec::new();
    let mut offset = start;
    while offset < end {
        if boxes.len() == MAX_BOXES || end - offset < 8 {
            return Err(invalid("too many or truncated metadata boxes"));
        }
        let size32 = be32(bytes, offset)?;
        let (size, header) = match size32 {
            0 => return Err(invalid("open-ended metadata box")),
            1 => (
                usize::try_from(be64(bytes, offset + 8)?)
                    .map_err(|_| invalid("large box overflow"))?,
                16,
            ),
            value => (value as usize, 8),
        };
        if size < header || size > end - offset {
            return Err(invalid("invalid metadata box size"));
        }
        boxes.push(BoxRange {
            start: offset,
            end: offset + size,
            content: offset + header,
            kind: bytes[offset + 4..offset + 8]
                .try_into()
                .map_err(|_| invalid("box kind"))?,
        });
        offset += size;
    }
    Ok(boxes)
}

fn only(boxes: &[BoxRange], kind: &[u8; 4]) -> Result<BoxRange, RemuxP8Error> {
    let mut found = boxes.iter().filter(|item| &item.kind == kind);
    let item = *found
        .next()
        .ok_or_else(|| invalid("required sample table missing"))?;
    if found.next().is_some() {
        return Err(invalid("duplicate sample table or multiple tracks"));
    }
    Ok(item)
}

struct Tables {
    sizes: Vec<u32>,
    sizes_offset: usize,
    chunks: Vec<(u64, usize)>,
    nal_length_bytes: usize,
    dv_boxes: Vec<usize>,
}

fn table_entries(bytes: &[u8], table: BoxRange, width: usize) -> Result<usize, RemuxP8Error> {
    if table.end - table.content < 8 || be32(bytes, table.content)? != 0 {
        return Err(invalid("unsupported sample table version"));
    }
    let count = be32(bytes, table.content + 4)? as usize;
    let expected = count
        .checked_mul(width)
        .and_then(|size| size.checked_add(8))
        .ok_or_else(|| invalid("sample table overflow"))?;
    if count == 0 || count > MAX_SAMPLES || table.end - table.content != expected {
        return Err(invalid("invalid sample table count or size"));
    }
    Ok(count)
}

fn tables(
    bytes: &[u8],
    mdat: (u64, u64),
    control: &mut RemuxP8Control<'_>,
) -> Result<Tables, RemuxP8Error> {
    let mut container = only(&children(bytes, 0, bytes.len())?, b"moov")?;
    for kind in [b"trak", b"mdia", b"minf", b"stbl"] {
        container = only(&children(bytes, container.content, container.end)?, kind)?;
    }
    let entries = children(bytes, container.content, container.end)?;
    if entries
        .iter()
        .any(|item| matches!(&item.kind, b"stz2" | b"senc" | b"saio" | b"saiz"))
    {
        return Err(invalid("unsupported compact or encrypted sample table"));
    }
    let stsz = only(&entries, b"stsz")?;
    if stsz.end - stsz.content < 12
        || be32(bytes, stsz.content)? != 0
        || be32(bytes, stsz.content + 4)? != 0
    {
        return Err(invalid("variable-size stsz table required"));
    }
    let count = be32(bytes, stsz.content + 8)? as usize;
    if count == 0 || count > MAX_SAMPLES || stsz.end - stsz.content != 12 + count * 4 {
        return Err(invalid("invalid sample count"));
    }
    let sizes_offset = stsz.content + 12;
    let mut sizes = Vec::with_capacity(count);
    for index in 0..count {
        if index % 4096 == 0 {
            check(control)?;
        }
        let size = be32(bytes, sizes_offset + index * 4)?;
        if size == 0 || size as usize > MAX_SAMPLE {
            return Err(invalid("sample exceeds the rewrite buffer bound"));
        }
        sizes.push(size);
    }
    let stsc = only(&entries, b"stsc")?;
    let runs = table_entries(bytes, stsc, 12)?;
    let chunk_tables = entries
        .iter()
        .filter(|item| matches!(&item.kind, b"stco" | b"co64"))
        .copied()
        .collect::<Vec<_>>();
    if chunk_tables.len() != 1 {
        return Err(invalid("exactly one chunk offset table required"));
    }
    let chunk_table = chunk_tables[0];
    let wide = chunk_table.kind == *b"co64";
    let chunk_count = table_entries(bytes, chunk_table, if wide { 8 } else { 4 })?;
    let mut mappings = Vec::with_capacity(runs);
    for index in 0..runs {
        if index % 4096 == 0 {
            check(control)?;
        }
        let offset = stsc.content + 8 + index * 12;
        let first = be32(bytes, offset)? as usize;
        let samples = be32(bytes, offset + 4)? as usize;
        if first == 0
            || first > chunk_count
            || samples == 0
            || samples > count
            || be32(bytes, offset + 8)? != 1
            || mappings
                .last()
                .is_some_and(|(previous, _)| *previous >= first)
        {
            return Err(invalid("invalid sample-to-chunk mapping"));
        }
        mappings.push((first, samples));
    }
    if mappings[0].0 != 1 {
        return Err(invalid("sample-to-chunk mapping does not start at one"));
    }
    let mut chunks = Vec::with_capacity(chunk_count);
    let mut run = 0;
    let mut sample = 0usize;
    let mut previous_end = mdat.0;
    for index in 0..chunk_count {
        if index % 4096 == 0 {
            check(control)?;
        }
        while run + 1 < mappings.len() && mappings[run + 1].0 == index + 1 {
            run += 1;
        }
        let offset = chunk_table.content + 8 + index * if wide { 8 } else { 4 };
        let start = if wide {
            be64(bytes, offset)?
        } else {
            u64::from(be32(bytes, offset)?)
        };
        let chunk_samples = mappings[run].1;
        let end_sample = sample
            .checked_add(chunk_samples)
            .ok_or_else(|| invalid("sample count overflow"))?;
        let selected = sizes
            .get(sample..end_sample)
            .ok_or_else(|| invalid("chunk references missing samples"))?;
        let mut length = 0u64;
        for (index, size) in selected.iter().enumerate() {
            if index % 4096 == 0 {
                check(control)?;
            }
            length = length
                .checked_add(u64::from(*size))
                .ok_or_else(|| invalid("chunk size overflow"))?;
        }
        let end = start
            .checked_add(length)
            .ok_or_else(|| invalid("chunk end overflow"))?;
        if start < previous_end || end > mdat.1 {
            return Err(invalid("overlapping or out-of-bounds media chunks"));
        }
        chunks.push((start, chunk_samples));
        previous_end = end;
        sample = end_sample;
    }
    if sample != count {
        return Err(invalid("sample count does not match chunk mapping"));
    }
    let stsd = only(&entries, b"stsd")?;
    if stsd.end - stsd.content < 8
        || be32(bytes, stsd.content)? != 0
        || be32(bytes, stsd.content + 4)? != 1
    {
        return Err(invalid(
            "exactly one unencrypted HEVC sample entry required",
        ));
    }
    let descriptions = children(bytes, stsd.content + 8, stsd.end)?;
    if descriptions.len() != 1 || !matches!(&descriptions[0].kind, b"hvc1" | b"hev1") {
        return Err(invalid("unsupported video sample description"));
    }
    let description = descriptions[0];
    let child_start = description
        .content
        .checked_add(78)
        .ok_or_else(|| invalid("entry overflow"))?;
    let extensions = children(bytes, child_start, description.end)?;
    let hvcc = only(&extensions, b"hvcC")?;
    if hvcc.end - hvcc.content < 23 || bytes[hvcc.content] != 1 {
        return Err(invalid("invalid HEVC configuration"));
    }
    let nal_length_bytes = usize::from(bytes[hvcc.content + 21] & 3) + 1;
    let dv_boxes = extensions
        .iter()
        .filter(|item| matches!(&item.kind, b"dvcC" | b"dvvC" | b"dvwC"))
        .map(|item| item.start + 4)
        .collect::<Vec<_>>();
    if dv_boxes.len() != 1 {
        return Err(invalid(
            "exactly one source Dolby Vision configuration required",
        ));
    }
    Ok(Tables {
        sizes,
        sizes_offset,
        chunks,
        nal_length_bytes,
        dv_boxes,
    })
}

fn kind(nal: &[u8]) -> Result<u8, RemuxP8Error> {
    if nal.len() < 2 || nal[0] & 0x80 != 0 || nal[1] & 7 == 0 {
        return Err(invalid("invalid or truncated HEVC NAL header"));
    }
    let kind = (nal[0] >> 1) & 63;
    if (kind <= 31 || kind == 62) && nal.len() < 3 {
        return Err(invalid("truncated VCL or RPU payload"));
    }
    if kind > 40 && kind != 62 && kind != 63 {
        return Err(invalid("unsupported HEVC NAL type"));
    }
    if kind <= 31 && (nal[0] & 1 != 0 || nal[1] & 0xf8 != 0) {
        return Err(invalid("non-base-layer VCL sample"));
    }
    Ok(kind)
}

struct AnnexReader<R> {
    reader: R,
    bytes: Vec<u8>,
    scan: usize,
    started: bool,
    eof: bool,
}

impl<R: Read> AnnexReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            bytes: Vec::with_capacity(READ_CHUNK),
            scan: 0,
            started: false,
            eof: false,
        }
    }

    fn next(&mut self, control: &mut RemuxP8Control<'_>) -> Result<Option<Vec<u8>>, RemuxP8Error> {
        loop {
            check(control)?;
            while self.scan + 3 <= self.bytes.len() {
                if self.scan.is_multiple_of(READ_CHUNK) {
                    check(control)?;
                }
                if self.bytes[self.scan..self.scan + 3] == [0, 0, 1] {
                    let prefix = if self.scan > 0 && self.bytes[self.scan - 1] == 0 {
                        self.scan - 1
                    } else {
                        self.scan
                    };
                    let end = self.scan + 3;
                    if !self.started {
                        if self.bytes[..prefix].iter().any(|byte| *byte != 0) {
                            return Err(invalid("converted stream lacks Annex-B prefix"));
                        }
                        self.bytes.drain(..end);
                        self.scan = 0;
                        self.started = true;
                        continue;
                    }
                    if prefix == 0 || prefix > MAX_SAMPLE {
                        return Err(invalid("empty Annex-B NAL"));
                    }
                    let nal = self.bytes[..prefix].to_vec();
                    self.bytes.drain(..end);
                    self.scan = 0;
                    return Ok(Some(nal));
                }
                self.scan += 1;
            }
            if self.eof {
                if !self.started {
                    return Err(invalid("empty or truncated converted stream"));
                }
                if self.bytes.is_empty() {
                    return Ok(None);
                }
                self.scan = 0;
                return Ok(Some(std::mem::take(&mut self.bytes)));
            }
            if self.bytes.len() > MAX_SAMPLE {
                return Err(invalid("converted NAL exceeds buffer bound"));
            }
            let mut chunk = [0; READ_CHUNK];
            let count = self.reader.read(&mut chunk).map_err(io_error)?;
            if count == 0 {
                self.eof = true;
            } else {
                self.bytes.extend_from_slice(&chunk[..count]);
            }
        }
    }
}

fn split_sample(sample: &[u8], length_bytes: usize) -> Result<Vec<&[u8]>, RemuxP8Error> {
    let mut offset = 0;
    let mut nals = Vec::new();
    while offset < sample.len() {
        if nals.len() == MAX_BOXES || sample.len() - offset < length_bytes {
            return Err(invalid("too many or truncated sample NALs"));
        }
        let length = sample[offset..offset + length_bytes]
            .iter()
            .fold(0usize, |value, byte| value * 256 + usize::from(*byte));
        offset += length_bytes;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| invalid("NAL size overflow"))?;
        let nal = sample
            .get(offset..end)
            .ok_or_else(|| invalid("truncated sample NAL"))?;
        kind(nal)?;
        nals.push(nal);
        offset = end;
    }
    Ok(nals)
}

fn transform_sample<R: Read>(
    sample: &[u8],
    length_bytes: usize,
    converted: &mut AnnexReader<R>,
    control: &mut RemuxP8Control<'_>,
) -> Result<Vec<u8>, RemuxP8Error> {
    let nals = split_sample(sample, length_bytes)?;
    let mut vcl = Vec::new();
    let mut pictures = 0;
    let mut rpus = 0;
    for nal in &nals {
        let typ = kind(nal)?;
        if typ <= 31 {
            pictures += usize::from(nal[2] & 0x80 != 0);
            vcl.push(*nal);
        }
        if typ == 62 {
            rpus += 1;
        }
    }
    if pictures != 1 || rpus != 1 || vcl.is_empty() {
        return Err(invalid("each sample must contain one picture and one RPU"));
    }
    let mut matched = 0;
    let mut examined = 0usize;
    let rpu = loop {
        let nal = converted
            .next(control)?
            .ok_or_else(|| invalid("converted RPU stream is truncated"))?;
        examined = examined
            .checked_add(nal.len())
            .ok_or_else(|| invalid("access unit overflow"))?;
        if examined > MAX_SAMPLE {
            return Err(invalid("converted access unit exceeds bound"));
        }
        let typ = kind(&nal)?;
        if typ == 63 {
            return Err(invalid("converted enhancement layer was not discarded"));
        }
        if typ <= 31 {
            if vcl.get(matched).copied() != Some(nal.as_slice()) {
                return Err(invalid(
                    "converted base-layer VCL does not match original sample",
                ));
            }
            matched += 1;
        }
        if typ == 62 {
            if matched != vcl.len() {
                return Err(invalid("converted RPU has no matching complete picture"));
            }
            break nal;
        }
    };
    let mut output = Vec::with_capacity(sample.len());
    for nal in nals {
        let typ = kind(nal)?;
        if typ == 63 {
            continue;
        }
        let bytes = if typ == 62 { rpu.as_slice() } else { nal };
        let length = u32::try_from(bytes.len())
            .map_err(|_| invalid("NAL length overflow"))?
            .to_be_bytes();
        if length[..4 - length_bytes].iter().any(|value| *value != 0) {
            return Err(invalid("replacement RPU does not fit sample length field"));
        }
        if output
            .len()
            .checked_add(length_bytes + bytes.len())
            .is_none_or(|len| len > sample.len())
        {
            return Err(invalid("converted sample grows beyond its original extent"));
        }
        output.extend_from_slice(&length[4 - length_bytes..]);
        output.extend_from_slice(bytes);
    }
    Ok(output)
}

/// Rewrite only a private staging MP4. Layout and NAL mismatches are pipeline
/// failures; the caller must remove the staging file and never expose it.
pub(super) fn rewrite(
    mp4: &Path,
    converted_hevc: &Path,
    control: &mut RemuxP8Control<'_>,
) -> Result<(), RemuxP8Error> {
    check(control)?;
    let mut source_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(mp4)
        .map_err(io_error)?;
    let mut converted_file = std::fs::File::open(converted_hevc).map_err(io_error)?;
    let file_len = source_file.metadata().map_err(io_error)?.len();
    let mut source = SignalingFile {
        file: &mut source_file,
        io: control.io,
    };
    let mut converted = AnnexReader::new(SignalingFile {
        file: &mut converted_file,
        io: control.io,
    });
    let mut offset = 0u64;
    let mut mdat = None;
    let mut moov = None;
    let mut boxes = 0;
    while offset < file_len {
        check(control)?;
        boxes += 1;
        if boxes > MAX_BOXES {
            return Err(invalid("too many top-level boxes"));
        }
        let span =
            super::read_mp4_box(&mut source, offset, file_len).map_err(RemuxP8Error::Pipeline)?;
        match &span.kind {
            b"mdat" if mdat.is_none() => {
                mdat = Some((span.content_start(), span.end()));
            }
            b"moov" if moov.is_none() && span.size <= MAX_MOOV => {
                moov = Some((span.offset, span.size));
            }
            b"ftyp" | b"free" | b"wide" => {}
            _ => return Err(invalid("unsupported top-level MP4 layout")),
        }
        offset = span.end();
    }
    let mdat = mdat.ok_or_else(|| invalid("missing media data"))?;
    let (moov_offset, moov_length) =
        moov.ok_or_else(|| invalid("missing or oversized movie metadata"))?;
    if moov_offset < mdat.1 {
        return Err(invalid("movie metadata must follow media data"));
    }
    let mut metadata =
        vec![0; usize::try_from(moov_length).map_err(|_| invalid("metadata size overflow"))?];
    source
        .seek(SeekFrom::Start(moov_offset))
        .map_err(io_error)?;
    read_controlled(&mut source, &mut metadata, control)?;
    let tables = tables(&metadata, mdat, control)?;
    let mut sample_index = 0;
    for (chunk, sample_count) in tables.chunks {
        let mut read_at = chunk;
        let mut write_at = chunk;
        for _ in 0..sample_count {
            check(control)?;
            let size = tables.sizes[sample_index] as usize;
            let mut sample = vec![0; size];
            source.seek(SeekFrom::Start(read_at)).map_err(io_error)?;
            read_controlled(&mut source, &mut sample, control)?;
            let output =
                transform_sample(&sample, tables.nal_length_bytes, &mut converted, control)?;
            check(control)?;
            source.seek(SeekFrom::Start(write_at)).map_err(io_error)?;
            write_controlled(&mut source, &output, control)?;
            let size_offset = tables.sizes_offset + sample_index * 4;
            metadata[size_offset..size_offset + 4]
                .copy_from_slice(&(output.len() as u32).to_be_bytes());
            read_at = read_at
                .checked_add(size as u64)
                .ok_or_else(|| invalid("sample read offset overflow"))?;
            write_at = write_at
                .checked_add(output.len() as u64)
                .ok_or_else(|| invalid("sample write offset overflow"))?;
            sample_index += 1;
        }
    }
    // Raw HEVC demuxers can expose an extra EOS/AUD packet or suffix SEI after
    // the last RPU. As between pictures, converted SEI is not imported: original
    // sample metadata is retained. Extra pictures, RPUs or EL always fail.
    let mut trailing = 0usize;
    while let Some(nal) = converted.next(control)? {
        trailing = trailing
            .checked_add(nal.len())
            .ok_or_else(|| invalid("tail overflow"))?;
        if trailing > READ_CHUNK || !matches!(kind(&nal)?, 35..=40) {
            return Err(invalid(
                "extra converted picture, RPU or ambiguous trailing data",
            ));
        }
    }
    for offset in tables.dv_boxes {
        metadata[offset..offset + 4].copy_from_slice(b"free");
    }
    check(control)?;
    source
        .seek(SeekFrom::Start(moov_offset))
        .map_err(io_error)?;
    write_controlled(&mut source, &metadata, control)?;
    check(control)
}

#[cfg(test)]
pub(super) fn test_fixture() -> (Vec<u8>, Vec<u8>) {
    test_fixture_with_suffix(None)
}

#[cfg(test)]
fn test_fixture_with_suffix(suffix: Option<&[u8]>) -> (Vec<u8>, Vec<u8>) {
    fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        bytes.extend(kind);
        bytes.extend(payload);
        bytes
    }
    fn sample(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for nal in nals {
            bytes.extend((nal.len() as u32).to_be_bytes());
            bytes.extend(nal);
        }
        bytes
    }
    let mut video = Vec::new();
    let mut converted = Vec::new();
    let mut sizes = Vec::new();
    for ordinal in [1, 2] {
        let aud = vec![70, 1, 16];
        let vcl = vec![2, 1, 128, ordinal];
        let rpu = vec![124, 1, 64, ordinal, 99, 99];
        let el = vec![126, 1, 42, ordinal];
        // EOS_NUT is a valid two-byte HEVC NAL and does not create a picture.
        let eos = vec![72, 1];
        let mut nals = vec![aud.clone(), vcl.clone(), el, eos.clone(), rpu];
        if ordinal == 2 {
            if let Some(suffix) = suffix {
                nals.push(suffix.to_vec());
            }
        }
        let bytes = sample(&nals);
        sizes.push(bytes.len() as u32);
        video.extend(bytes);
        for nal in [aud, vec![64, 1, 42], vcl, eos, vec![124, 1, 64, ordinal]] {
            converted.extend([0, 0, 0, 1]);
            converted.extend(nal);
        }
        if ordinal == 2 {
            if let Some(suffix) = suffix {
                converted.extend([0, 0, 0, 1]);
                converted.extend(suffix);
            }
        }
    }
    let mut mp4 = boxed(b"ftyp", b"fixture ");
    let chunk_start = mp4.len() as u32 + 8;
    mp4.extend(boxed(b"mdat", &video));
    let mut hvcc = vec![0; 23];
    hvcc[0] = 1;
    hvcc[21] = 3;
    let mut entry = vec![0; 78];
    entry.extend(boxed(b"hvcC", &hvcc));
    entry.extend(boxed(b"dvcC", &[0; 24]));
    let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
    stsd.extend(boxed(b"hvc1", &entry));
    let mut stsz = vec![0; 8];
    stsz.extend(2u32.to_be_bytes());
    for size in sizes {
        stsz.extend(size.to_be_bytes());
    }
    let mut stco = vec![0; 4];
    stco.extend(1u32.to_be_bytes());
    stco.extend(chunk_start.to_be_bytes());
    let mut stsc = vec![0; 4];
    for value in [1u32, 1, 2, 1] {
        stsc.extend(value.to_be_bytes());
    }
    let mut stts = vec![0; 4];
    for value in [2u32, 1, 1000, 1, 3000] {
        stts.extend(value.to_be_bytes());
    }
    let mut ctts = vec![1, 0, 0, 0];
    for value in [2i32, 1, -100, 1, 300] {
        ctts.extend(value.to_be_bytes());
    }
    let mut stbl = Vec::new();
    for (kind, payload) in [
        (b"stsd", stsd),
        (b"stsz", stsz),
        (b"stco", stco),
        (b"stsc", stsc),
        (b"stts", stts),
        (b"ctts", ctts),
    ] {
        stbl.extend(boxed(kind, &payload));
    }
    let minf = boxed(b"minf", &boxed(b"stbl", &stbl));
    let mdia = boxed(b"mdia", &minf);
    let mut track = boxed(b"edts", &boxed(b"elst", b"unchanged source edit list"));
    track.extend(mdia);
    let moov = boxed(b"moov", &boxed(b"trak", &track));
    mp4.extend(moov);
    (mp4, converted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RemuxP8IoBasis, RemuxP8StageIo};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn temp(label: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "rdlna-p8-rewrite-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn compacts_samples_preserving_vfr_composition_edits_and_chunk_offsets() {
        let directory = temp("timing");
        let (original, converted) = test_fixture();
        let mp4 = directory.join("source.mp4");
        let hevc = directory.join("converted.hevc");
        std::fs::write(&mp4, &original).unwrap();
        std::fs::write(&hevc, &converted).unwrap();
        let cancelled = AtomicBool::new(false);
        let io = std::cell::Cell::new(Some(RemuxP8StageIo {
            read_bytes: 0,
            written_bytes: 0,
            storage_read_bytes: None,
            storage_written_bytes: None,
            basis: RemuxP8IoBasis::ApplicationCounters,
        }));
        let mut observer = || Ok(());
        let mut control = RemuxP8Control {
            deadline: Instant::now() + Duration::from_secs(3),
            cancelled: &cancelled,
            observer: &mut observer,
            io: Some(&io),
        };
        rewrite(&mp4, &hevc, &mut control).unwrap();
        let result = std::fs::read(&mp4).unwrap();
        assert_eq!(result.len(), original.len());
        for kind in [b"stts", b"ctts", b"elst", b"stco", b"stsc"] {
            let at = original.windows(4).position(|value| value == kind).unwrap() - 4;
            let len = be32(&original, at).unwrap() as usize;
            assert_eq!(&result[at..at + len], &original[at..at + len], "{kind:?}");
        }
        let dv = original
            .windows(4)
            .position(|value| value == b"dvcC")
            .unwrap();
        assert_eq!(&result[dv..dv + 4], b"free");
        let moov = result
            .windows(4)
            .position(|value| value == b"moov")
            .unwrap()
            - 4;
        let stsz = result
            .windows(4)
            .position(|value| value == b"stsz")
            .unwrap()
            - 4;
        let original_size = be32(&original, stsz + 20).unwrap();
        let rewritten_size = be32(&result, stsz + 20).unwrap();
        assert!(rewritten_size < original_size);
        let stco = result
            .windows(4)
            .position(|value| value == b"stco")
            .unwrap()
            - 4;
        let start = be32(&result, stco + 16).unwrap() as usize;
        for index in 0..2 {
            let sample = &result[start + index * rewritten_size as usize
                ..start + (index + 1) * rewritten_size as usize];
            let nals = split_sample(sample, 4).unwrap();
            assert!(nals.iter().all(|nal| kind(nal).unwrap() != 63));
            assert_eq!(
                nals.iter().filter(|nal| kind(nal).unwrap() == 62).count(),
                1
            );
            let rpu = nals.iter().find(|nal| kind(nal).unwrap() == 62).unwrap();
            assert_eq!(*rpu, &[124, 1, 64, (index + 1) as u8]);
        }
        let counters = io.get().unwrap();
        assert!(counters.read_bytes >= original.len() as u64 + converted.len() as u64 - 24);
        assert_eq!(
            counters.written_bytes,
            (result.len() - moov) as u64 + 2 * u64::from(rewritten_size)
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn malformed_mapping_truncation_extra_picture_and_sample_growth_fail() {
        let (original, converted) = test_fixture();
        for mutation in 0..5 {
            let directory = temp("rejected");
            let mp4 = directory.join("source.mp4");
            let hevc = directory.join("converted.hevc");
            let mut bad = converted.clone();
            match mutation {
                0 => {
                    let at = bad
                        .windows(4)
                        .position(|bytes| bytes == [2, 1, 128, 1])
                        .unwrap();
                    bad[at + 3] = 88;
                }
                1 => {
                    bad.truncate(bad.len() - 4);
                }
                2 => {
                    bad.extend_from_slice(&converted);
                }
                3 => {
                    let at = bad
                        .windows(4)
                        .position(|bytes| bytes == [124, 1, 64, 1])
                        .unwrap();
                    bad.splice(at + 4..at + 4, [99; 32]);
                }
                _ => {
                    bad = b"not annex b".to_vec();
                }
            }
            std::fs::write(&mp4, &original).unwrap();
            std::fs::write(&hevc, bad).unwrap();
            let cancelled = AtomicBool::new(false);
            let mut observer = || Ok(());
            let mut control = RemuxP8Control {
                deadline: Instant::now() + Duration::from_secs(3),
                cancelled: &cancelled,
                observer: &mut observer,
                io: None,
            };
            assert!(
                rewrite(&mp4, &hevc, &mut control).is_err(),
                "mutation {mutation}"
            );
            let result = std::fs::read(&mp4).unwrap();
            let moov = original
                .windows(4)
                .position(|bytes| bytes == b"moov")
                .unwrap()
                - 4;
            assert_eq!(
                &result[moov..],
                &original[moov..],
                "failed stage cannot publish updated sample tables"
            );
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn final_suffix_sei_preserves_original_sample_metadata_after_converted_rpu() {
        let directory = temp("suffix-sei");
        // HEVC suffix SEI, user_data_unregistered, sixteen-byte UUID, rbsp stop bit.
        let mut suffix = vec![80, 1, 5, 16];
        suffix.extend([42; 16]);
        suffix.push(128);
        let (original, converted) = test_fixture_with_suffix(Some(&suffix));
        let mp4 = directory.join("source.mp4");
        let hevc = directory.join("converted.hevc");
        std::fs::write(&mp4, &original).unwrap();
        std::fs::write(&hevc, &converted).unwrap();
        let cancelled = AtomicBool::new(false);
        let mut observer = || Ok(());
        let mut control = RemuxP8Control {
            deadline: Instant::now() + Duration::from_secs(3),
            cancelled: &cancelled,
            observer: &mut observer,
            io: None,
        };
        rewrite(&mp4, &hevc, &mut control).unwrap();
        let result = std::fs::read(&mp4).unwrap();
        let at = |kind: &[u8; 4]| result.windows(4).position(|value| value == kind).unwrap() - 4;
        let first = be32(&result, at(b"stco") + 16).unwrap() as usize;
        let first_size = be32(&result, at(b"stsz") + 20).unwrap() as usize;
        let last_size = be32(&result, at(b"stsz") + 24).unwrap() as usize;
        let last = split_sample(
            &result[first + first_size..first + first_size + last_size],
            4,
        )
        .unwrap();
        assert_eq!(last.last().copied(), Some(suffix.as_slice()));
        assert_eq!(
            last.iter().filter(|nal| kind(nal).unwrap() == 62).count(),
            1
        );
        assert!(last.iter().all(|nal| kind(nal).unwrap() != 63));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cancellation_and_pressure_interrupt_before_staging_mutation() {
        for pressure in [false, true] {
            let directory = temp("cancel");
            let mp4 = directory.join("source.mp4");
            let hevc = directory.join("converted.hevc");
            let (original, converted) = test_fixture();
            std::fs::write(&mp4, &original).unwrap();
            std::fs::write(&hevc, converted).unwrap();
            let cancelled = AtomicBool::new(false);
            let mut checks = 0;
            let mut observer = || {
                checks += 1;
                if checks == 3 {
                    if pressure {
                        return Err("cache pressure".into());
                    }
                    cancelled.store(true, Ordering::Release);
                }
                Ok(())
            };
            let mut control = RemuxP8Control {
                deadline: Instant::now() + Duration::from_secs(3),
                cancelled: &cancelled,
                observer: &mut observer,
                io: None,
            };
            let error = rewrite(&mp4, &hevc, &mut control).unwrap_err();
            assert!(matches!(
                error,
                RemuxP8Error::Cancelled(_) | RemuxP8Error::Observer(_)
            ));
            assert_eq!(std::fs::read(&mp4).unwrap(), original);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn overlapping_chunks_constant_stsz_and_malformed_moov_fail_before_mutation() {
        let (original, converted) = test_fixture();
        for mutation in 0..3 {
            let directory = temp("layout");
            let mp4 = directory.join("source.mp4");
            let hevc = directory.join("converted.hevc");
            let mut damaged = original.clone();
            let at = |bytes: &[u8], kind: &[u8; 4]| {
                bytes.windows(4).position(|value| value == kind).unwrap() - 4
            };
            match mutation {
                0 => {
                    let stco = at(&damaged, b"stco");
                    let first = damaged[stco + 16..stco + 20].to_vec();
                    damaged.splice(stco + 20..stco + 20, first);
                    for kind in [b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stco"] {
                        let offset = at(&damaged, kind);
                        let size = be32(&damaged, offset).unwrap() + 4;
                        damaged[offset..offset + 4].copy_from_slice(&size.to_be_bytes());
                    }
                    damaged[stco + 12..stco + 16].copy_from_slice(&2u32.to_be_bytes());
                    let stsc = at(&damaged, b"stsc");
                    damaged[stsc + 20..stsc + 24].copy_from_slice(&1u32.to_be_bytes());
                }
                1 => {
                    let stsz = at(&damaged, b"stsz");
                    damaged[stsz + 12..stsz + 16].copy_from_slice(&1u32.to_be_bytes());
                }
                _ => {
                    let moov = at(&damaged, b"moov");
                    damaged[moov..moov + 4].copy_from_slice(&u32::MAX.to_be_bytes());
                }
            }
            std::fs::write(&mp4, &damaged).unwrap();
            std::fs::write(&hevc, &converted).unwrap();
            let cancelled = AtomicBool::new(false);
            let mut observer = || Ok(());
            let mut control = RemuxP8Control {
                deadline: Instant::now() + Duration::from_secs(3),
                cancelled: &cancelled,
                observer: &mut observer,
                io: None,
            };
            let error = rewrite(&mp4, &hevc, &mut control).unwrap_err();
            if mutation == 0 {
                assert!(error.to_string().contains("overlapping"), "{error}");
            }
            assert_eq!(std::fs::read(&mp4).unwrap(), damaged);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn cancellation_after_first_sample_write_leaves_tables_unpublished() {
        let directory = temp("mid-write");
        let mp4 = directory.join("source.mp4");
        let hevc = directory.join("converted.hevc");
        let (original, converted) = test_fixture();
        std::fs::write(&mp4, &original).unwrap();
        std::fs::write(&hevc, converted).unwrap();
        let cancelled = AtomicBool::new(false);
        let io = std::cell::Cell::new(Some(RemuxP8StageIo {
            read_bytes: 0,
            written_bytes: 0,
            storage_read_bytes: None,
            storage_written_bytes: None,
            basis: RemuxP8IoBasis::ApplicationCounters,
        }));
        let mut observer = || {
            if io.get().unwrap().written_bytes > 0 {
                cancelled.store(true, Ordering::Release);
            }
            Ok(())
        };
        let mut control = RemuxP8Control {
            deadline: Instant::now() + Duration::from_secs(3),
            cancelled: &cancelled,
            observer: &mut observer,
            io: Some(&io),
        };
        assert!(matches!(
            rewrite(&mp4, &hevc, &mut control),
            Err(RemuxP8Error::Cancelled(_))
        ));
        let result = std::fs::read(&mp4).unwrap();
        assert_ne!(
            result, original,
            "cancellation must occur after a real staging write"
        );
        let moov = original
            .windows(4)
            .position(|bytes| bytes == b"moov")
            .unwrap()
            - 4;
        assert_eq!(&result[moov..], &original[moov..]);
        // Removing an interrupted private staging file remains the pipeline owner's job.
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_disk_full_write_reports_pressure_without_counting_failed_bytes() {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        let cancelled = AtomicBool::new(false);
        let io = std::cell::Cell::new(Some(RemuxP8StageIo {
            read_bytes: 0,
            written_bytes: 0,
            storage_read_bytes: None,
            storage_written_bytes: None,
            basis: RemuxP8IoBasis::ApplicationCounters,
        }));
        let mut observer = || Ok(());
        let mut control = RemuxP8Control {
            deadline: Instant::now() + Duration::from_secs(3),
            cancelled: &cancelled,
            observer: &mut observer,
            io: Some(&io),
        };
        let mut destination = SignalingFile {
            file: &mut file,
            io: Some(&io),
        };
        let error =
            write_controlled(&mut destination, b"staging output", &mut control).unwrap_err();
        assert!(matches!(error, RemuxP8Error::Observer(_)));
        assert_eq!(io.get().unwrap().written_bytes, 0);
    }

    #[test]
    fn rejects_oversized_samples_before_writing() {
        let (original, converted) = test_fixture();
        for sample_size in [0u32, MAX_SAMPLE as u32 + 1, u32::MAX] {
            let directory = temp("bounds");
            let mp4 = directory.join("source.mp4");
            let hevc = directory.join("converted.hevc");
            let mut damaged = original.clone();
            let stsz = damaged
                .windows(4)
                .position(|bytes| bytes == b"stsz")
                .unwrap()
                - 4;
            damaged[stsz + 20..stsz + 24].copy_from_slice(&sample_size.to_be_bytes());
            std::fs::write(&mp4, &damaged).unwrap();
            std::fs::write(&hevc, &converted).unwrap();
            let cancelled = AtomicBool::new(false);
            let mut observer = || Ok(());
            let mut control = RemuxP8Control {
                deadline: Instant::now() + Duration::from_secs(3),
                cancelled: &cancelled,
                observer: &mut observer,
                io: None,
            };
            assert!(rewrite(&mp4, &hevc, &mut control).is_err());
            assert_eq!(std::fs::read(&mp4).unwrap(), damaged);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
}
