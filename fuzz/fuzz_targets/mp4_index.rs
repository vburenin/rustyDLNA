#![no_main]
mod media_file;
use libfuzzer_sys::fuzz_target;
use rusty_dlna::FuzzIndex;
use std::os::unix::fs::FileExt;

fuzz_target!(|input: &[u8]| {
    if input.len() > 64 * 1024 {
        return;
    }
    let full = media_file::anonymous(input);
    // Raw inputs have no completed-output validation stamp. Anonymous inodes
    // can be recycled within the filesystem timestamp resolution, so reset
    // process-local metadata reuse between inputs. Keep reuse within this case,
    // where the owned descriptors pin the identities, as a separate property.
    let mut index = FuzzIndex::begin_case();
    let result = index
        .update(&full, false)
        .and_then(|()| index.update(&full, true));
    let golden = input == include_bytes!("../seeds/mp4_index/three-fragments.mp4");
    if golden {
        assert!(result.is_ok());
    }
    for malformed in [
        &include_bytes!("../regressions/mp4_index/truncated-fragment.mp4")[..],
        &include_bytes!("../regressions/mp4_index/box-size-underflow.mp4")[..],
    ] {
        if input == malformed {
            assert!(result.is_err());
        }
    }
    if result.is_err() {
        return;
    }
    let expected = index.snapshot();
    if golden {
        assert_eq!(
            expected.fragments,
            [(144, 80, 1.0), (224, 80, 1.0), (304, 80, 1.0)]
        );
        assert_eq!(expected.segments, [(144, 160, 2.0), (304, 80, 1.0)]);
        assert_eq!(expected.duration, Some(3.0));
        assert!(expected.native.as_ref().unwrap().contains("#EXT-X-ENDLIST"));
    }
    // Structural invariants do not compute expected results through another parser.
    for entries in [&expected.fragments, &expected.segments] {
        let mut end = 0;
        for &(offset, length, duration) in entries {
            assert!(length > 0 && offset >= end);
            end = offset.checked_add(length).unwrap();
            assert!(end <= input.len() as u64);
            assert!(duration.is_finite() && duration > 0.0);
        }
    }
    if let Some(duration) = expected.duration {
        assert!(duration.is_finite() && duration > 0.0);
        let sum: f64 = expected.fragments.iter().map(|s| s.2).sum();
        assert!((sum - duration).abs() <= 0.000001 * duration.max(1.0));
    }
    // Completed-index reuse is idempotent and cannot duplicate fragments.
    index.update(&full, true).unwrap();
    assert_eq!(expected, index.snapshot());
    // Reuse is now checked against bytes actually parsed in this iteration,
    // while the original descriptor still pins their identity.
    if !expected.fragments.is_empty() {
        let mut reused = FuzzIndex::default();
        reused.update(&full, true).unwrap();
        assert_eq!(expected, reused.snapshot());
    }
    // Size-zero top-level boxes consume all currently available bytes and are
    // intentionally excluded from the growing-file equivalence property.
    let mut at = 0;
    while at < input.len() {
        let size = u32::from_be_bytes(input[at..at + 4].try_into().unwrap());
        if size == 0 {
            return;
        }
        let size = if size == 1 {
            usize::try_from(u64::from_be_bytes(
                input[at + 8..at + 16].try_into().unwrap(),
            ))
            .unwrap()
        } else {
            size as usize
        };
        at += size;
    }
    // The same bytes published at header/payload boundaries give the same history.
    let growing = media_file::anonymous(&[]);
    let mut incremental = FuzzIndex::default();
    let mut starts = vec![0, 1, 7, 8, input.len() / 2, input.len()];
    starts.retain(|n| *n <= input.len());
    starts.sort_unstable();
    starts.dedup();
    for end in starts {
        growing.write_all_at(&input[..end], 0).unwrap();
        incremental.update(&growing, false).unwrap();
    }
    incremental.update(&growing, true).unwrap();
    assert_eq!(expected, incremental.snapshot());
});
