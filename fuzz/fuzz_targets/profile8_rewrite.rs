#![no_main]
mod media_file;
use libfuzzer_sys::fuzz_target;

// Independent preservation mask, not a sample-table or NAL parser: only media
// payloads, stsz entries and the DV configuration type may change in staging.
fn mutable_bytes(bytes: &[u8], start: usize, end: usize, mask: &mut [bool], parent: &[u8]) {
    let mut at = start;
    while at < end {
        assert!(end - at >= 8);
        let short = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let (size, header) = if short == 1 {
            assert!(end - at >= 16);
            (
                usize::try_from(u64::from_be_bytes(
                    bytes[at + 8..at + 16].try_into().unwrap(),
                ))
                .unwrap(),
                16,
            )
        } else if short == 0 {
            (end - at, 8)
        } else {
            (short, 8)
        };
        assert!(size >= header && size <= end - at);
        let content = at + header;
        let stop = at + size;
        let kind = &bytes[at + 4..at + 8];
        // Only the selected sample-description path is interpreted. A box with
        // a familiar name in another parent remains opaque and fully protected.
        match (parent, kind) {
            (b"root", b"mdat") => mask[content..stop].fill(true),
            (b"stbl", b"stsz") => mask[content + 12..stop].fill(true),
            (b"hvc1" | b"hev1", b"dvcC" | b"dvvC" | b"dvwC") => mask[at + 4..at + 8].fill(true),
            (b"root", b"moov")
            | (b"moov", b"trak")
            | (b"trak", b"mdia")
            | (b"mdia", b"minf")
            | (b"minf", b"stbl") => mutable_bytes(bytes, content, stop, mask, kind),
            (b"stbl", b"stsd") => mutable_bytes(bytes, content + 8, stop, mask, kind),
            (b"stsd", b"hvc1" | b"hev1") => mutable_bytes(bytes, content + 78, stop, mask, kind),
            _ => {}
        }
        at = stop;
    }
}

fn exercise(input: &[u8], known_output: Option<&[u8]>) {
    if input.len() < 4 || input.len() > 64 * 1024 {
        return;
    }
    let length = u32::from_be_bytes(input[..4].try_into().unwrap()) as usize;
    if length > input.len() - 4 {
        return;
    }
    let original = &input[4..4 + length];
    let converted = &input[4 + length..];
    let staging = media_file::anonymous(original);
    let hevc = media_file::anonymous(converted);
    let result = rusty_dlna_transcode::fuzz_profile8_rewrite(
        &media_file::path(&staging),
        &media_file::path(&hevc),
    );
    let golden = input == include_bytes!("../seeds/profile8_rewrite/two-vfr-samples.bin");
    if known_output.is_some()
        || golden
        || input == include_bytes!("../seeds/profile8_rewrite/opaque-moov-children.bin")
    {
        assert!(result.is_ok(), "{result:?}");
    }
    for malformed in [
        &include_bytes!("../regressions/profile8_rewrite/chunk-outside-mdat.bin")[..],
        &include_bytes!("../regressions/profile8_rewrite/mismatched-base-layer.bin")[..],
    ] {
        if input == malformed {
            assert!(result.is_err());
        }
    }
    if result.is_ok() {
        let output = std::fs::read(media_file::path(&staging)).unwrap();
        assert_eq!(original.len(), output.len());
        if let Some(expected) = known_output {
            assert_eq!(output, expected);
        }
        if golden {
            assert_eq!(output, include_bytes!("../expected/two-vfr-samples.mp4"));
        }
        let mut mutable = vec![false; original.len()];
        mutable_bytes(original, 0, original.len(), &mut mutable, b"root");
        for (at, allowed) in mutable.iter().enumerate() {
            if !allowed {
                assert_eq!(original[at], output[at], "protected byte {at}");
            }
        }
        assert_eq!(std::fs::read(media_file::path(&hevc)).unwrap(), converted);
    }
}

fuzz_target!(|input: &[u8]| {
    if input.len() > 64 * 1024 {
        return;
    }
    exercise(input, None);
    // Structured variants independently name exact source VCL, replacement RPU,
    // sample lengths, VFR timing and output bytes. Every iteration reaches the
    // packet rewrite; arbitrary successful raw mutations also check metadata.
    let mut packed = include_bytes!("../seeds/profile8_rewrite/two-vfr-samples.bin").to_vec();
    let mut expected = include_bytes!("../expected/two-vfr-samples.mp4").to_vec();
    let media_length = 489;
    for (index, source_at, hevc_at, output_at) in [(0, 38, 21, 38), (1, 77, 57, 67)] {
        let byte = input.get(index).copied().unwrap_or(index as u8) % 250 + 1;
        packed[4 + source_at] = byte;
        packed[4 + media_length + hevc_at] = byte;
        expected[output_at] = byte;
    }
    for (index, hevc_at, output_at) in [(2, 35, 52), (3, 71, 81)] {
        let byte = input.get(index).copied().unwrap_or(index as u8) % 250 + 1;
        packed[4 + media_length + hevc_at] = byte;
        expected[output_at] = byte;
    }
    for (index, kind) in [b"stts", b"ctts"].into_iter().enumerate() {
        let at = expected.windows(4).position(|bytes| bytes == kind).unwrap();
        for field in [16, 24] {
            let value = u32::from(input.get(4 + index + field).copied().unwrap_or(7)) * 31 + 1;
            packed[4 + at + field..4 + at + field + 4].copy_from_slice(&value.to_be_bytes());
            expected[at + field..at + field + 4].copy_from_slice(&value.to_be_bytes());
        }
    }
    exercise(&packed, Some(&expected));
});
