//! Compact stream metadata persisted in the media catalog.
//!
//! The representation is a comma-separated sequence of audio descriptors and
//! tagged records. It predates the richer in-memory probe types, so parsing is
//! intentionally tolerant where existing readers were tolerant. In
//! particular, marker presence is distinct from whether a marker's fields are
//! useful.

use std::borrow::Cow;
use std::error::Error;
use std::fmt;

/// Maximum raw byte length accepted from a persisted compact metadata value.
pub const MAX_COMPACT_STREAM_METADATA_BYTES: usize = 1024 * 1024;

/// Maximum number of parsed audio records exposed to a consumer.
pub const MAX_COMPACT_AUDIO_RECORDS: usize = 1024;

/// Maximum number of chapter entries persisted or inspected.
pub const MAX_COMPACT_CHAPTERS: usize = 512;

/// Maximum number of Unicode scalar values persisted for a chapter title.
pub const MAX_COMPACT_CHAPTER_TITLE_CHARS: usize = 256;

/// A compact field could not be percent-decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactFieldDecodeError {
    /// A complete three-byte percent triplet contained a non-hex digit.
    InvalidEscape,
    /// Percent decoding produced bytes that are not UTF-8.
    InvalidUtf8,
}

impl fmt::Display for CompactFieldDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEscape => formatter.write_str("invalid compact field percent escape"),
            Self::InvalidUtf8 => formatter.write_str("compact field is not valid UTF-8"),
        }
    }
}

impl Error for CompactFieldDecodeError {}

/// A persisted compact metadata value exceeded its parsing budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactStreamMetadataParseError {
    /// Raw input length in bytes.
    pub length: usize,
    /// Maximum accepted raw length in bytes.
    pub max_bytes: usize,
}

impl fmt::Display for CompactStreamMetadataParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "compact stream metadata is {} bytes; maximum is {} bytes",
            self.length, self.max_bytes
        )
    }
}

impl Error for CompactStreamMetadataParseError {}

/// Compact metadata could not be serialized within its storage bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactStreamMetadataWriteError {
    /// The encoded value would exceed the persisted byte budget.
    TooLong {
        /// Maximum encoded length in bytes.
        max_bytes: usize,
    },
    /// More audio descriptors were supplied than readers expose.
    TooManyAudioRecords {
        /// Number of supplied audio descriptors.
        count: usize,
        /// Maximum number of audio descriptors.
        max: usize,
    },
}

impl fmt::Display for CompactStreamMetadataWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { max_bytes } => write!(
                formatter,
                "compact stream metadata exceeds the {max_bytes}-byte maximum"
            ),
            Self::TooManyAudioRecords { count, max } => write!(
                formatter,
                "compact stream metadata has {count} audio records; maximum is {max}"
            ),
        }
    }
}

impl Error for CompactStreamMetadataWriteError {}

/// Encode one field using the catalog's legacy percent representation.
///
/// ASCII alphanumerics, `-`, `_`, `.`, `~`, and spaces remain literal. Every
/// other UTF-8 byte is written as an uppercase `%HH` triplet.
pub fn encode_compact_stream_field(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    let mut scratch = [0; 3];
    for byte in value.bytes() {
        encoded.push_str(compact_encoded_byte(byte, &mut scratch));
    }
    encoded
}

/// Decode one field using the catalog's legacy percent representation.
///
/// A trailing `%` or `%X` is retained literally. A complete triplet with a
/// non-hex digit is rejected, as is a decoded byte sequence that is not UTF-8.
pub fn decode_compact_stream_field(value: &str) -> Result<Cow<'_, str>, CompactFieldDecodeError> {
    if !value.as_bytes().contains(&b'%') {
        return Ok(Cow::Borrowed(value));
    }

    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = hex_value(bytes[index + 1]).ok_or(CompactFieldDecodeError::InvalidEscape)?;
            let low = hex_value(bytes[index + 2]).ok_or(CompactFieldDecodeError::InvalidEscape)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .map(Cow::Owned)
        .map_err(|_| CompactFieldDecodeError::InvalidUtf8)
}

/// A bounded borrowed view of persisted compact stream metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactStreamMetadata<'a> {
    raw: &'a str,
}

impl<'a> CompactStreamMetadata<'a> {
    /// Validate the total input budget and construct a borrowed view.
    pub fn parse(raw: &'a str) -> Result<Self, CompactStreamMetadataParseError> {
        if raw.len() > MAX_COMPACT_STREAM_METADATA_BYTES {
            return Err(CompactStreamMetadataParseError {
                length: raw.len(),
                max_bytes: MAX_COMPACT_STREAM_METADATA_BYTES,
            });
        }
        Ok(Self { raw })
    }

    /// Return the original persisted representation.
    pub fn as_str(self) -> &'a str {
        self.raw
    }

    /// Whether any record has the exact, case-sensitive `@v:` marker.
    ///
    /// This intentionally says nothing about the validity of its fields.
    pub fn has_video_capabilities_marker(self) -> bool {
        self.records().any(|record| record.starts_with("@v:"))
    }

    /// Whether any record has the exact, case-sensitive `@t:` marker.
    ///
    /// An empty or unknown timestamp mode still counts as a present marker.
    pub fn has_timestamp_marker(self) -> bool {
        self.records().any(|record| record.starts_with("@t:"))
    }

    /// Parse audio descriptors in record order.
    ///
    /// Records need only the global index, audio ordinal, and codec to be
    /// returned. `channels` remains optional so transcode selection can retain
    /// its legacy three-field behavior while server presentation can require
    /// [`CompactAudioRecord::is_server_usable`].
    pub fn audio_records(self) -> impl Iterator<Item = CompactAudioRecord<'a>> {
        self.records()
            .filter_map(parse_audio_record)
            .take(MAX_COMPACT_AUDIO_RECORDS)
    }

    /// Decode the first `@v:` record using the legacy lenient defaults.
    ///
    /// Missing, malformed, or non-UTF-8 text fields become empty. Missing or
    /// malformed numeric fields become zero. Extra fields are ignored.
    pub fn video_capabilities(self) -> Option<CompactVideoCapabilities<'a>> {
        let record = self.records().find(|record| record.starts_with("@v:"))?;
        let mut fields = record.split(':');
        let _marker = fields.next();
        Some(CompactVideoCapabilities {
            profile: decode_or_empty(fields.next().unwrap_or("")),
            level: parse_u32_or_zero(fields.next()),
            pixel_format: decode_or_empty(fields.next().unwrap_or("")),
            bit_depth: parse_u32_or_zero(fields.next()),
            frame_rate: decode_or_empty(fields.next().unwrap_or("")),
            codec_string: decode_or_empty(fields.next().unwrap_or("")),
            audio_layout: decode_or_empty(fields.next().unwrap_or("")),
        })
    }

    /// Return the complete suffix of the first exact `@t:` record.
    ///
    /// The suffix can be empty and can itself contain colons.
    pub fn timestamp_mode(self) -> Option<&'a str> {
        self.records().find_map(|record| record.strip_prefix("@t:"))
    }

    /// Parse valid chapters from the first exact `@c:` record.
    ///
    /// At most 512 raw entries are inspected. The source index is assigned
    /// before validation, so skipped malformed entries deliberately leave gaps.
    pub fn chapters(self) -> impl Iterator<Item = CompactChapter<'a>> {
        self.records()
            .find_map(|record| record.strip_prefix("@c:"))
            .into_iter()
            .flat_map(|record| {
                record
                    .split('|')
                    .take(MAX_COMPACT_CHAPTERS)
                    .enumerate()
                    .filter_map(parse_chapter)
            })
    }

    fn records(self) -> impl Iterator<Item = &'a str> {
        self.raw.split(',')
    }
}

/// A parsed audio descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactAudioRecord<'a> {
    /// Absolute stream index in the container.
    pub global_index: usize,
    /// Zero-based audio-stream ordinal used by `0:a:N` selectors.
    pub audio_index: usize,
    /// Trimmed codec label. Legacy readers permit an empty label.
    pub codec: &'a str,
    /// Channel count, or `None` when absent or malformed.
    pub channels: Option<u32>,
    /// Encoded language field when it was present, including an empty field.
    pub encoded_language: Option<&'a str>,
    /// Encoded title field when it was present, including an empty field.
    pub encoded_title: Option<&'a str>,
    /// Whether the disposition field is exactly `1`.
    pub default: bool,
}

impl<'a> CompactAudioRecord<'a> {
    /// Whether the record has the valid channel count required by the server.
    pub fn is_server_usable(self) -> bool {
        self.channels.is_some()
    }

    /// Decode a nonempty language, preserving field decode errors.
    pub fn decoded_language(self) -> Result<Option<Cow<'a, str>>, CompactFieldDecodeError> {
        decode_optional_field(self.encoded_language)
    }

    /// Decode a nonempty title, preserving field decode errors.
    pub fn decoded_title(self) -> Result<Option<Cow<'a, str>>, CompactFieldDecodeError> {
        decode_optional_field(self.encoded_title)
    }
}

/// Leniently decoded video capability fields from an `@v:` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactVideoCapabilities<'a> {
    /// Codec profile label.
    pub profile: Cow<'a, str>,
    /// Codec level, or zero when absent or malformed.
    pub level: u32,
    /// Pixel format label.
    pub pixel_format: Cow<'a, str>,
    /// Component bit depth, or zero when absent or malformed.
    pub bit_depth: u32,
    /// Frame-rate representation.
    pub frame_rate: Cow<'a, str>,
    /// RFC 6381 codec string when known.
    pub codec_string: Cow<'a, str>,
    /// Primary audio layout label.
    pub audio_layout: Cow<'a, str>,
}

/// A valid chapter from an `@c:` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactChapter<'a> {
    /// Position in the raw chapter sequence, before malformed entries are removed.
    pub source_index: usize,
    /// Chapter start in milliseconds.
    pub start_millis: u64,
    /// Chapter end in milliseconds.
    pub end_millis: u64,
    /// Decoded nonempty title. Invalid and empty titles become `None`.
    pub title: Option<Cow<'a, str>>,
}

/// One audio descriptor supplied to the compact writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactAudioRecordInput<'a> {
    /// Absolute stream index in the container.
    pub global_index: usize,
    /// Zero-based audio-stream ordinal.
    pub audio_index: usize,
    /// Codec label, written literally for legacy compatibility.
    pub codec: &'a str,
    /// Channel count.
    pub channels: u32,
    /// Optional decoded language.
    pub language: Option<&'a str>,
    /// Optional decoded title.
    pub title: Option<&'a str>,
    /// Default disposition.
    pub default: bool,
}

/// Video capability fields supplied to the compact writer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompactVideoCapabilitiesInput<'a> {
    /// Codec profile label.
    pub profile: &'a str,
    /// Codec level.
    pub level: u32,
    /// Pixel format label.
    pub pixel_format: &'a str,
    /// Component bit depth.
    pub bit_depth: u32,
    /// Frame-rate representation.
    pub frame_rate: &'a str,
    /// RFC 6381 codec string.
    pub codec_string: &'a str,
    /// Primary audio layout label.
    pub audio_layout: &'a str,
}

/// One chapter supplied to the compact writer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactChapterInput<'a> {
    /// Chapter start in seconds.
    pub start_seconds: f64,
    /// Chapter end in seconds.
    pub end_seconds: f64,
    /// Optional decoded chapter title.
    pub title: Option<&'a str>,
}

/// Complete input for the canonical compact writer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactStreamMetadataInput<'a> {
    /// Ordered audio descriptors.
    pub audio_records: &'a [CompactAudioRecordInput<'a>],
    /// Video capability marker, which is always persisted.
    pub video: CompactVideoCapabilitiesInput<'a>,
    /// Timestamp classification, written literally after `@t:`.
    pub timestamp_mode: &'a str,
    /// Ordered chapters. Only the first 512 are persisted.
    pub chapters: &'a [CompactChapterInput<'a>],
}

/// Serialize compact stream metadata in its canonical legacy order.
///
/// Audio descriptors come first, followed by exactly one `@v:` and one `@t:`
/// record, then an optional `@c:` record. The operation fails atomically when
/// the output would exceed [`MAX_COMPACT_STREAM_METADATA_BYTES`].
pub fn encode_compact_stream_metadata(
    input: CompactStreamMetadataInput<'_>,
) -> Result<String, CompactStreamMetadataWriteError> {
    if input.audio_records.len() > MAX_COMPACT_AUDIO_RECORDS {
        return Err(CompactStreamMetadataWriteError::TooManyAudioRecords {
            count: input.audio_records.len(),
            max: MAX_COMPACT_AUDIO_RECORDS,
        });
    }

    let mut writer = BoundedWriter::default();
    for audio in input.audio_records {
        writer.start_record()?;
        writer.push_usize(audio.global_index)?;
        writer.push_char(':')?;
        writer.push_usize(audio.audio_index)?;
        writer.push_char(':')?;
        writer.push_str(audio.codec)?;
        writer.push_char(':')?;
        writer.push_u32(audio.channels)?;
        if audio.language.is_some() || audio.title.is_some() || audio.default {
            writer.push_char(':')?;
            writer.push_encoded(audio.language.unwrap_or(""))?;
            writer.push_char(':')?;
            writer.push_encoded(audio.title.unwrap_or(""))?;
            writer.push_char(':')?;
            writer.push_char(if audio.default { '1' } else { '0' })?;
        }
    }

    writer.start_record()?;
    writer.push_str("@v:")?;
    writer.push_encoded(input.video.profile)?;
    writer.push_char(':')?;
    writer.push_u32(input.video.level)?;
    writer.push_char(':')?;
    writer.push_encoded(input.video.pixel_format)?;
    writer.push_char(':')?;
    writer.push_u32(input.video.bit_depth)?;
    writer.push_char(':')?;
    writer.push_encoded(input.video.frame_rate)?;
    writer.push_char(':')?;
    writer.push_encoded(input.video.codec_string)?;
    writer.push_char(':')?;
    writer.push_encoded(input.video.audio_layout)?;

    writer.start_record()?;
    writer.push_str("@t:")?;
    writer.push_str(input.timestamp_mode)?;

    if !input.chapters.is_empty() {
        writer.start_record()?;
        writer.push_str("@c:")?;
        for (index, chapter) in input.chapters.iter().take(MAX_COMPACT_CHAPTERS).enumerate() {
            if index > 0 {
                writer.push_char('|')?;
            }
            writer.push_u64(seconds_to_millis(chapter.start_seconds))?;
            writer.push_char(':')?;
            writer.push_u64(seconds_to_millis(chapter.end_seconds))?;
            writer.push_char(':')?;
            let title = chapter
                .title
                .unwrap_or("")
                .chars()
                .take(MAX_COMPACT_CHAPTER_TITLE_CHARS)
                .collect::<String>();
            writer.push_encoded(&title)?;
        }
    }

    Ok(writer.output)
}

fn parse_audio_record(record: &str) -> Option<CompactAudioRecord<'_>> {
    let mut fields = record.split(':');
    let global_index = fields.next()?.parse::<usize>().ok()?;
    let audio_index = fields.next()?.parse::<usize>().ok()?;
    let codec = fields.next()?.trim();
    let channels = fields.next().and_then(|value| value.parse::<u32>().ok());
    let encoded_language = fields.next();
    let encoded_title = fields.next();
    let default = fields.next() == Some("1");
    Some(CompactAudioRecord {
        global_index,
        audio_index,
        codec,
        channels,
        encoded_language,
        encoded_title,
        default,
    })
}

fn parse_chapter((source_index, chapter): (usize, &str)) -> Option<CompactChapter<'_>> {
    let mut fields = chapter.splitn(3, ':');
    let start_millis = fields.next()?.parse::<u64>().ok()?;
    let end_millis = fields.next()?.parse::<u64>().ok()?;
    if end_millis < start_millis {
        return None;
    }
    let title = fields
        .next()
        .and_then(|value| decode_optional_field(Some(value)).ok().flatten());
    Some(CompactChapter {
        source_index,
        start_millis,
        end_millis,
        title,
    })
}

fn decode_optional_field(
    encoded: Option<&str>,
) -> Result<Option<Cow<'_, str>>, CompactFieldDecodeError> {
    let Some(encoded) = encoded else {
        return Ok(None);
    };
    let decoded = decode_compact_stream_field(encoded)?;
    Ok((!decoded.is_empty()).then_some(decoded))
}

fn decode_or_empty(value: &str) -> Cow<'_, str> {
    decode_compact_stream_field(value).unwrap_or(Cow::Borrowed(""))
}

fn parse_u32_or_zero(value: Option<&str>) -> u32 {
    value.and_then(|value| value.parse().ok()).unwrap_or(0)
}

fn seconds_to_millis(seconds: f64) -> u64 {
    (seconds * 1000.0).round().max(0.0) as u64
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn compact_encoded_byte(byte: u8, scratch: &mut [u8; 3]) -> &str {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let length = if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b' ')
    {
        scratch[0] = byte;
        1
    } else {
        scratch[0] = b'%';
        scratch[1] = HEX[usize::from(byte >> 4)];
        scratch[2] = HEX[usize::from(byte & 0x0f)];
        3
    };
    std::str::from_utf8(&scratch[..length]).expect("compact field encoding is ASCII")
}

#[derive(Default)]
struct BoundedWriter {
    output: String,
}

impl BoundedWriter {
    fn start_record(&mut self) -> Result<(), CompactStreamMetadataWriteError> {
        if !self.output.is_empty() {
            self.push_char(',')?;
        }
        Ok(())
    }

    fn push_encoded(&mut self, value: &str) -> Result<(), CompactStreamMetadataWriteError> {
        let mut scratch = [0; 3];
        for byte in value.bytes() {
            self.push_str(compact_encoded_byte(byte, &mut scratch))?;
        }
        Ok(())
    }

    fn push_char(&mut self, value: char) -> Result<(), CompactStreamMetadataWriteError> {
        let mut bytes = [0; 4];
        self.push_str(value.encode_utf8(&mut bytes))
    }

    fn push_usize(&mut self, value: usize) -> Result<(), CompactStreamMetadataWriteError> {
        self.push_str(&value.to_string())
    }

    fn push_u32(&mut self, value: u32) -> Result<(), CompactStreamMetadataWriteError> {
        self.push_str(&value.to_string())
    }

    fn push_u64(&mut self, value: u64) -> Result<(), CompactStreamMetadataWriteError> {
        self.push_str(&value.to_string())
    }

    fn push_str(&mut self, value: &str) -> Result<(), CompactStreamMetadataWriteError> {
        let Some(length) = self.output.len().checked_add(value.len()) else {
            return Err(CompactStreamMetadataWriteError::TooLong {
                max_bytes: MAX_COMPACT_STREAM_METADATA_BYTES,
            });
        };
        if length > MAX_COMPACT_STREAM_METADATA_BYTES {
            return Err(CompactStreamMetadataWriteError::TooLong {
                max_bytes: MAX_COMPACT_STREAM_METADATA_BYTES,
            });
        }
        self.output.push_str(value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_input<'a>(
        audio_records: &'a [CompactAudioRecordInput<'a>],
        chapters: &'a [CompactChapterInput<'a>],
    ) -> CompactStreamMetadataInput<'a> {
        CompactStreamMetadataInput {
            audio_records,
            video: CompactVideoCapabilitiesInput::default(),
            timestamp_mode: "",
            chapters,
        }
    }

    #[test]
    fn compact_fields_preserve_the_legacy_escape_contract() {
        let source = "AZaz09-_.~ space,:|% Кино";
        let encoded = encode_compact_stream_field(source);
        assert_eq!(
            encoded,
            "AZaz09-_.~ space%2C%3A%7C%25 %D0%9A%D0%B8%D0%BD%D0%BE"
        );
        assert_eq!(decode_compact_stream_field(&encoded).unwrap(), source);

        assert!(matches!(
            decode_compact_stream_field("plain").unwrap(),
            Cow::Borrowed("plain")
        ));
        assert_eq!(decode_compact_stream_field("%2c%3a").unwrap(), ",:");
        assert_eq!(decode_compact_stream_field("tail%").unwrap(), "tail%");
        assert_eq!(decode_compact_stream_field("tail%A").unwrap(), "tail%A");
        assert_eq!(decode_compact_stream_field("%00").unwrap(), "\0");
        assert_eq!(
            decode_compact_stream_field("bad%XX"),
            Err(CompactFieldDecodeError::InvalidEscape)
        );
        assert_eq!(
            decode_compact_stream_field("%FF"),
            Err(CompactFieldDecodeError::InvalidUtf8)
        );
    }

    #[test]
    fn writer_matches_the_complete_legacy_layout() {
        let audio = [
            CompactAudioRecordInput {
                global_index: 1,
                audio_index: 0,
                codec: "truehd",
                channels: 6,
                language: None,
                title: None,
                default: false,
            },
            CompactAudioRecordInput {
                global_index: 2,
                audio_index: 1,
                codec: "aac",
                channels: 2,
                language: Some("en,US"),
                title: Some("Dub: Кино|100%"),
                default: true,
            },
        ];
        let chapters = [CompactChapterInput {
            start_seconds: 1.25,
            end_seconds: 4.0,
            title: Some("Intro: Кино"),
        }];
        let encoded = encode_compact_stream_metadata(CompactStreamMetadataInput {
            audio_records: &audio,
            video: CompactVideoCapabilitiesInput {
                profile: "Main 10",
                level: 153,
                pixel_format: "yuv420p10le",
                bit_depth: 10,
                frame_rate: "24000/1001",
                codec_string: "hvc1.2.4.L153.B0,mp4a.40.2",
                audio_layout: "5.1",
            },
            timestamp_mode: "broken-reordered",
            chapters: &chapters,
        })
        .unwrap();
        assert_eq!(
            encoded,
            concat!(
                "1:0:truehd:6,",
                "2:1:aac:2:en%2CUS:Dub%3A %D0%9A%D0%B8%D0%BD%D0%BE%7C100%25:1,",
                "@v:Main 10:153:yuv420p10le:10:24000%2F1001:",
                "hvc1.2.4.L153.B0%2Cmp4a.40.2:5.1,",
                "@t:broken-reordered,",
                "@c:1250:4000:Intro%3A %D0%9A%D0%B8%D0%BD%D0%BE"
            )
        );
    }

    #[test]
    fn writer_keeps_four_fields_unless_optional_audio_metadata_is_needed() {
        let audio = [
            CompactAudioRecordInput {
                global_index: 0,
                audio_index: 0,
                codec: "aac",
                channels: 2,
                language: None,
                title: None,
                default: false,
            },
            CompactAudioRecordInput {
                global_index: 1,
                audio_index: 1,
                codec: "ac3",
                channels: 6,
                language: None,
                title: None,
                default: true,
            },
        ];
        let encoded = encode_compact_stream_metadata(empty_input(&audio, &[])).unwrap();
        assert_eq!(encoded, "0:0:aac:2,1:1:ac3:6:::1,@v::0::0:::,@t:");
    }

    #[test]
    fn marker_presence_is_independent_of_useful_fields() {
        let metadata = CompactStreamMetadata::parse("@x:ignored,@v:bad%XX:nope,@t:").unwrap();
        assert!(metadata.has_video_capabilities_marker());
        assert!(metadata.has_timestamp_marker());
        let video = metadata.video_capabilities().unwrap();
        assert!(video.profile.is_empty());
        assert_eq!(video.level, 0);
        assert!(video.pixel_format.is_empty());
        assert_eq!(metadata.timestamp_mode(), Some(""));

        let wrong_case = CompactStreamMetadata::parse("@V:x,@T:valid").unwrap();
        assert!(!wrong_case.has_video_capabilities_marker());
        assert!(!wrong_case.has_timestamp_marker());
    }

    #[test]
    fn first_marker_wins_and_unknown_or_extra_fields_are_ignored() {
        let metadata = CompactStreamMetadata::parse(concat!(
            "@x:unknown,",
            "@v:Main%2010:153:yuv420p10le:10:24000/1001:hvc1:5.1:legacy-extra,",
            "@v:Second:1:p:8:r:c:l,",
            "@t:first:with:colons,@t:second,",
            "@c:100:200:First,@c:300:400:Second"
        ))
        .unwrap();
        let video = metadata.video_capabilities().unwrap();
        assert_eq!(video.profile, "Main 10");
        assert_eq!(video.level, 153);
        assert_eq!(video.audio_layout, "5.1");
        assert_eq!(metadata.timestamp_mode(), Some("first:with:colons"));
        let chapters = metadata.chapters().collect::<Vec<_>>();
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].title.as_deref(), Some("First"));
    }

    #[test]
    fn audio_records_preserve_server_and_transcode_validity_differences() {
        let metadata = CompactStreamMetadata::parse(concat!(
            "1:0:truehd,",
            "2:1: ac3 :6:eng:bad%XX:1:ignored,",
            "3:2::nope::tail%:01,",
            " 4:3:aac:2,",
            "bad:4:aac:2,",
            "@v:"
        ))
        .unwrap();
        let audio = metadata.audio_records().collect::<Vec<_>>();
        assert_eq!(audio.len(), 3);

        assert_eq!(audio[0].global_index, 1);
        assert_eq!(audio[0].audio_index, 0);
        assert_eq!(audio[0].codec, "truehd");
        assert_eq!(audio[0].channels, None);
        assert!(!audio[0].is_server_usable());

        assert_eq!(audio[1].codec, "ac3");
        assert_eq!(audio[1].channels, Some(6));
        assert!(audio[1].is_server_usable());
        assert_eq!(audio[1].decoded_language().unwrap().as_deref(), Some("eng"));
        assert_eq!(
            audio[1].decoded_title(),
            Err(CompactFieldDecodeError::InvalidEscape)
        );
        assert!(audio[1].default);

        assert_eq!(audio[2].codec, "");
        assert_eq!(audio[2].channels, None);
        assert_eq!(audio[2].decoded_language().unwrap(), None);
        assert_eq!(audio[2].decoded_title().unwrap().as_deref(), Some("tail%"));
        assert!(!audio[2].default, "only the exact field `1` is default");
    }

    #[test]
    fn audio_record_exposure_is_bounded_without_reordering_or_deduplication() {
        let raw = (0..=MAX_COMPACT_AUDIO_RECORDS)
            .map(|index| format!("{index}:0:aac:2"))
            .collect::<Vec<_>>()
            .join(",");
        let metadata = CompactStreamMetadata::parse(&raw).unwrap();
        let audio = metadata.audio_records().collect::<Vec<_>>();
        assert_eq!(audio.len(), MAX_COMPACT_AUDIO_RECORDS);
        assert_eq!(audio.first().unwrap().global_index, 0);
        assert_eq!(
            audio.last().unwrap().global_index,
            MAX_COMPACT_AUDIO_RECORDS - 1
        );

        let duplicate = CompactStreamMetadata::parse("2:1:aac:2,2:1:aac:2").unwrap();
        assert_eq!(duplicate.audio_records().count(), 2);
    }

    #[test]
    fn chapters_keep_source_gaps_and_field_failure_fallbacks() {
        let metadata = CompactStreamMetadata::parse(concat!(
            "@c:",
            "malformed|",
            "200:100:backwards|",
            "100:200:Good|",
            "200:300:bad%XX|",
            "300:400:tail%|",
            "400:500:colon:raw|",
            "18446744073709551616:500:overflow"
        ))
        .unwrap();
        let chapters = metadata.chapters().collect::<Vec<_>>();
        assert_eq!(
            chapters
                .iter()
                .map(|chapter| chapter.source_index)
                .collect::<Vec<_>>(),
            vec![2, 3, 4, 5]
        );
        assert_eq!(chapters[0].title.as_deref(), Some("Good"));
        assert_eq!(chapters[1].title, None);
        assert_eq!(chapters[2].title.as_deref(), Some("tail%"));
        assert_eq!(chapters[3].title.as_deref(), Some("colon:raw"));
    }

    #[test]
    fn chapter_writer_and_parser_share_limits_and_rounding() {
        let long_title = "К".repeat(MAX_COMPACT_CHAPTER_TITLE_CHARS + 1);
        let chapters = (0..=MAX_COMPACT_CHAPTERS)
            .map(|index| CompactChapterInput {
                start_seconds: index as f64 + 0.0005,
                end_seconds: index as f64 + 0.9995,
                title: Some(&long_title),
            })
            .collect::<Vec<_>>();
        let encoded = encode_compact_stream_metadata(empty_input(&[], &chapters)).unwrap();
        let metadata = CompactStreamMetadata::parse(&encoded).unwrap();
        let decoded = metadata.chapters().collect::<Vec<_>>();
        assert_eq!(decoded.len(), MAX_COMPACT_CHAPTERS);
        assert_eq!(decoded[0].start_millis, 1);
        assert_eq!(decoded[0].end_millis, 1000);
        assert_eq!(
            decoded[0].title.as_deref().unwrap().chars().count(),
            MAX_COMPACT_CHAPTER_TITLE_CHARS
        );
        assert_eq!(
            decoded.last().unwrap().source_index,
            MAX_COMPACT_CHAPTERS - 1
        );
    }

    #[test]
    fn parser_and_writer_enforce_the_total_byte_budget() {
        let exact = "x".repeat(MAX_COMPACT_STREAM_METADATA_BYTES);
        assert!(CompactStreamMetadata::parse(&exact).is_ok());
        let oversized = format!("{exact}x");
        assert_eq!(
            CompactStreamMetadata::parse(&oversized),
            Err(CompactStreamMetadataParseError {
                length: MAX_COMPACT_STREAM_METADATA_BYTES + 1,
                max_bytes: MAX_COMPACT_STREAM_METADATA_BYTES,
            })
        );

        let baseline = encode_compact_stream_metadata(empty_input(&[], &[])).unwrap();
        let timestamp = "x".repeat(MAX_COMPACT_STREAM_METADATA_BYTES - baseline.len());
        let exact_output = encode_compact_stream_metadata(CompactStreamMetadataInput {
            timestamp_mode: &timestamp,
            ..empty_input(&[], &[])
        })
        .unwrap();
        assert_eq!(exact_output.len(), MAX_COMPACT_STREAM_METADATA_BYTES);

        let timestamp = format!("{timestamp}x");
        assert_eq!(
            encode_compact_stream_metadata(CompactStreamMetadataInput {
                timestamp_mode: &timestamp,
                ..empty_input(&[], &[])
            }),
            Err(CompactStreamMetadataWriteError::TooLong {
                max_bytes: MAX_COMPACT_STREAM_METADATA_BYTES,
            })
        );
    }

    #[test]
    fn writer_rejects_audio_above_the_shared_record_limit() {
        let audio = vec![
            CompactAudioRecordInput {
                global_index: 0,
                audio_index: 0,
                codec: "aac",
                channels: 2,
                language: None,
                title: None,
                default: false,
            };
            MAX_COMPACT_AUDIO_RECORDS + 1
        ];
        assert_eq!(
            encode_compact_stream_metadata(empty_input(&audio, &[])),
            Err(CompactStreamMetadataWriteError::TooManyAudioRecords {
                count: MAX_COMPACT_AUDIO_RECORDS + 1,
                max: MAX_COMPACT_AUDIO_RECORDS,
            })
        );
    }
}
