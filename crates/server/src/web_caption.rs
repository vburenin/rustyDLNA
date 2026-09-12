//! Pure conversion of bounded text-caption sidecars to browser WebVTT.
//!
//! The HTTP layer confines and caps sidecar reads before calling this module;
//! these helpers only validate and transform the supplied bytes.

use rusty_dlna_protocol::CaptionWebVttConversion;

// Match the confined HTTP sidecar read ceiling, and bound escaping/timing expansion.
const MAX_INPUT_BYTES: usize = rusty_dlna_scan::MAX_SIDECAR_BYTES as usize;
const MAX_OUTPUT_BYTES: usize = 6 * MAX_INPUT_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BrowserCaptionError {
    Encoding,
    Malformed,
}

pub(super) fn caption_to_webvtt(
    conversion: CaptionWebVttConversion,
    body: &[u8],
) -> Result<Vec<u8>, BrowserCaptionError> {
    if body.len() > MAX_INPUT_BYTES {
        return Err(BrowserCaptionError::Malformed);
    }
    let text = std::str::from_utf8(body).map_err(|_| BrowserCaptionError::Encoding)?;
    let text = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    if text.contains('\0') {
        return Err(BrowserCaptionError::Malformed);
    }
    let output = match conversion {
        CaptionWebVttConversion::ValidateWebVtt => validate_webvtt(&text)?,
        CaptionWebVttConversion::SubRipToWebVtt => srt_to_webvtt(&text)?,
        CaptionWebVttConversion::SubStationAlphaToWebVtt => ass_to_webvtt(&text)?,
        CaptionWebVttConversion::SamiToWebVtt => smi_to_webvtt(&text)?,
    };
    Ok(output.into_bytes())
}

fn validate_webvtt(text: &str) -> Result<String, BrowserCaptionError> {
    let mut blocks = text.split("\n\n");
    let Some(header) = blocks.next() else {
        return Err(BrowserCaptionError::Malformed);
    };
    let first = header.lines().next().unwrap_or_default();
    if !(first == "WEBVTT" || first.starts_with("WEBVTT ") || first.starts_with("WEBVTT\t"))
        || header.lines().skip(1).any(|line| line.contains("-->"))
    {
        return Err(BrowserCaptionError::Malformed);
    }
    // Header metadata is preserved for browser consumers. Body blocks follow
    // the WebVTT structure, rather than treating every arrow in the file as a cue.
    let mut saw_cue = false;
    for block in blocks
        .map(|block| block.trim_matches('\n'))
        .filter(|block| !block.is_empty())
    {
        let mut lines = block.lines();
        let first = lines.next().ok_or(BrowserCaptionError::Malformed)?;
        let note = first == "NOTE" || first.starts_with("NOTE ") || first.starts_with("NOTE\t");
        let definition = matches!(first.trim_end_matches([' ', '\t']), "STYLE" | "REGION");
        if (note || definition)
            && !lines
                .clone()
                .next()
                .is_some_and(|line| line.contains("-->"))
        {
            if block.contains("-->") || (definition && saw_cue) {
                return Err(BrowserCaptionError::Malformed);
            }
            continue;
        }
        let timing = if first.contains("-->") {
            first
        } else {
            lines.next().ok_or(BrowserCaptionError::Malformed)?
        };
        let (start, end, _) = caption_timing(timing, false)?;
        if end <= start || lines.any(|line| line.contains("-->")) {
            return Err(BrowserCaptionError::Malformed);
        }
        saw_cue = true;
    }
    Ok(format!("{}\n", text.trim_end_matches('\n')))
}

fn caption_timing(line: &str, subrip: bool) -> Result<(u64, u64, &str), BrowserCaptionError> {
    let (start, rest) = line
        .split_once("-->")
        .ok_or(BrowserCaptionError::Malformed)?;
    if !start.ends_with([' ', '\t']) || !rest.starts_with([' ', '\t']) || rest.contains("-->") {
        return Err(BrowserCaptionError::Malformed);
    }
    let rest = rest.trim_start_matches([' ', '\t']);
    let end_len = rest.find([' ', '\t']).unwrap_or(rest.len());
    let start = parse_caption_time(start.trim_matches([' ', '\t']), subrip)
        .ok_or(BrowserCaptionError::Malformed)?;
    let end = parse_caption_time(&rest[..end_len], subrip).ok_or(BrowserCaptionError::Malformed)?;
    Ok((start, end, &rest[end_len..]))
}

fn srt_to_webvtt(text: &str) -> Result<String, BrowserCaptionError> {
    let mut output = String::from("WEBVTT\n\n");
    let mut cues = 0usize;
    for block in text.split("\n\n").filter(|block| !block.trim().is_empty()) {
        let mut lines = block.lines();
        let first = lines.next().ok_or(BrowserCaptionError::Malformed)?;
        let timing = if first.contains("-->") {
            first
        } else {
            if !first.trim().bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(BrowserCaptionError::Malformed);
            }
            lines.next().ok_or(BrowserCaptionError::Malformed)?
        };
        let (start, end, settings) = caption_timing(timing, true)?;
        if end <= start {
            return Err(BrowserCaptionError::Malformed);
        }
        let payload = lines.collect::<Vec<_>>();
        if payload.is_empty() {
            return Err(BrowserCaptionError::Malformed);
        }
        output.push_str(&millis_vtt(start));
        output.push_str(" --> ");
        output.push_str(&millis_vtt(end));
        output.push_str(settings);
        output.push('\n');
        // WebVTT treats a literal arrow in cue text as a new timing line.
        // Escape only the arrow, preserving SubRip's supported cue markup.
        output.push_str(&payload.join("\n").replace("-->", "--&gt;"));
        output.push_str("\n\n");
        check_output_growth(output.len(), 0)?;
        cues += 1;
    }
    (cues > 0)
        .then_some(output)
        .ok_or(BrowserCaptionError::Malformed)
}

fn ass_to_webvtt(text: &str) -> Result<String, BrowserCaptionError> {
    let mut in_events = false;
    let mut columns = Vec::<String>::new();
    let mut output = String::from("WEBVTT\n\n");
    let mut cues = 0usize;
    for raw in text.lines() {
        let line = raw.trim();
        if line.eq_ignore_ascii_case("[events]") {
            in_events = true;
            continue;
        }
        if line.starts_with('[') && !line.eq_ignore_ascii_case("[events]") {
            in_events = false;
        }
        if !in_events {
            continue;
        }
        if let Some(format) = line
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("format"))
            .map(|(_, body)| body)
        {
            columns = format
                .split(',')
                .map(|field| field.trim().to_ascii_lowercase())
                .collect();
            continue;
        }
        let Some(dialogue) = line
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("dialogue"))
            .map(|(_, body)| body)
        else {
            continue;
        };
        if columns.last().is_none_or(|column| column != "text") {
            return Err(BrowserCaptionError::Malformed);
        }
        let fields = dialogue
            .splitn(columns.len(), ',')
            .map(str::trim)
            .collect::<Vec<_>>();
        if fields.len() != columns.len() {
            return Err(BrowserCaptionError::Malformed);
        }
        let field = |name: &str| {
            columns
                .iter()
                .position(|column| column == name)
                .and_then(|index| fields.get(index).copied())
        };
        let start = ass_time(field("start").ok_or(BrowserCaptionError::Malformed)?)
            .ok_or(BrowserCaptionError::Malformed)?;
        let end = ass_time(field("end").ok_or(BrowserCaptionError::Malformed)?)
            .ok_or(BrowserCaptionError::Malformed)?;
        if end <= start {
            return Err(BrowserCaptionError::Malformed);
        }
        let cue = strip_ass_overrides(field("text").ok_or(BrowserCaptionError::Malformed)?)?;
        if cue.trim().is_empty() {
            continue;
        }
        append_cue(&mut output, start, end, &cue)?;
        cues += 1;
    }
    (cues > 0)
        .then_some(output)
        .ok_or(BrowserCaptionError::Malformed)
}

fn ass_time(value: &str) -> Option<u64> {
    let mut fields = value.trim().split(':');
    let hours = ascii_number(fields.next()?)?;
    let minutes = two_digits(fields.next()?)?;
    let (seconds, fraction) = fields.next()?.split_once('.')?;
    let seconds = two_digits(seconds)?;
    if fields.next().is_some()
        || minutes > 59
        || seconds > 59
        || fraction.is_empty()
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    // ASS normally uses centiseconds. Preserve existing finer precision support
    // and round with integers, including carry across second/minute boundaries.
    let mut fraction_digits = fraction.bytes();
    let mut millis = 0;
    for _ in 0..3 {
        millis = millis * 10 + u64::from(fraction_digits.next().unwrap_or(b'0') - b'0');
    }
    millis += u64::from(fraction_digits.next().is_some_and(|digit| digit >= b'5'));
    hours
        .checked_mul(3_600_000)?
        .checked_add(minutes.checked_mul(60_000)?)?
        .checked_add(seconds.checked_mul(1000)?)?
        .checked_add(millis)
}

fn strip_ass_overrides(value: &str) -> Result<String, BrowserCaptionError> {
    let mut output = String::new();
    let mut cursor = 0;
    while cursor < value.len() {
        let rest = &value[cursor..];
        if rest.starts_with("{\\") {
            cursor += rest.find('}').ok_or(BrowserCaptionError::Malformed)? + 1;
        } else if rest.starts_with("\\N") || rest.starts_with("\\n") {
            output.push('\n');
            cursor += 2;
        } else if rest.starts_with("\\h") {
            output.push('\u{a0}');
            cursor += 2;
        } else {
            let character = rest.chars().next().ok_or(BrowserCaptionError::Malformed)?;
            output.push(character);
            cursor += character.len_utf8();
        }
    }
    Ok(if output.trim().is_empty() {
        String::new()
    } else {
        escape_cue_text(&output)
    })
}

fn smi_to_webvtt(text: &str) -> Result<String, BrowserCaptionError> {
    let mut output = String::from("WEBVTT\n\n");
    let mut previous = None;
    let mut payload_start = 0;
    let mut cursor = 0;
    while let Some(tag) = next_smi_tag(text, cursor)? {
        cursor = tag.end;
        if tag.closing || !tag.name.eq_ignore_ascii_case("sync") {
            continue;
        }
        let start = smi_start(tag.attributes).ok_or(BrowserCaptionError::Malformed)?;
        if let Some(previous) = previous {
            if start < previous {
                return Err(BrowserCaptionError::Malformed);
            }
            let cue = strip_smi_markup(&text[payload_start..tag.start])?;
            if start > previous && !cue.is_empty() {
                append_cue(&mut output, previous, start, &cue)?;
            }
        }
        // An empty synchronization point still ends the previous cue. Keeping
        // this boundary leaves real gaps and lets a terminal clear end the text.
        previous = Some(start);
        payload_start = tag.end;
    }
    let start = previous.ok_or(BrowserCaptionError::Malformed)?;
    let cue = strip_smi_markup(&text[payload_start..])?;
    if !cue.is_empty() {
        let end = start
            .checked_add(5_000)
            .ok_or(BrowserCaptionError::Malformed)?;
        append_cue(&mut output, start, end, &cue)?;
    }
    Ok(output)
}

struct SmiTag<'a> {
    start: usize,
    end: usize,
    name: &'a str,
    attributes: &'a str,
    closing: bool,
}

// This deliberately small tokenizer flattens SAMI formatting. It distinguishes
// literal "< 3", quoted attributes, and comments from tags, and scans linearly.
fn next_smi_tag(text: &str, mut cursor: usize) -> Result<Option<SmiTag<'_>>, BrowserCaptionError> {
    while let Some(relative) = text[cursor..].find('<') {
        let start = cursor + relative;
        let rest = &text[start + 1..];
        if let Some(comment) = rest.strip_prefix("!--") {
            let length = comment.find("-->").ok_or(BrowserCaptionError::Malformed)?;
            return Ok(Some(SmiTag {
                start,
                end: start + 4 + length + 3,
                name: "!--",
                attributes: "",
                closing: false,
            }));
        }
        let closing = rest.starts_with('/');
        let name_start = usize::from(closing);
        let name_len = rest[name_start..]
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric())
            .count();
        if name_len == 0
            || !rest.as_bytes()[name_start].is_ascii_alphabetic()
            || !rest[name_start + name_len..].starts_with(|character: char| {
                character.is_ascii_whitespace() || matches!(character, '/' | '>')
            })
        {
            cursor = start + 1;
            continue;
        }
        let mut quote = None;
        for (index, character) in rest[name_start + name_len..].char_indices() {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                if matches!(character, '\'' | '"') {
                    quote = Some(character);
                } else if character == '>' {
                    let end = name_start + name_len + index;
                    return Ok(Some(SmiTag {
                        start,
                        end: start + 1 + end + 1,
                        name: &rest[name_start..name_start + name_len],
                        attributes: rest[name_start + name_len..end].trim_end_matches('/'),
                        closing,
                    }));
                }
            }
        }
        return Err(BrowserCaptionError::Malformed);
    }
    Ok(None)
}

fn smi_start(mut attributes: &str) -> Option<u64> {
    let mut start = None;
    loop {
        attributes =
            attributes.trim_start_matches(|character: char| character.is_ascii_whitespace());
        if attributes.is_empty() {
            return start;
        }
        let name_len = attributes
            .find(|character: char| character.is_ascii_whitespace() || character == '=')
            .unwrap_or(attributes.len());
        if name_len == 0 {
            return None;
        }
        let name = &attributes[..name_len];
        attributes = attributes[name_len..]
            .trim_start_matches(|character: char| character.is_ascii_whitespace());
        let Some(rest) = attributes.strip_prefix('=') else {
            if name.eq_ignore_ascii_case("start") {
                return None;
            }
            continue;
        };
        attributes = rest.trim_start_matches(|character: char| character.is_ascii_whitespace());
        let value;
        if let Some(quote) = attributes
            .chars()
            .next()
            .filter(|character| matches!(character, '\'' | '"'))
        {
            let end = attributes[1..].find(quote)? + 1;
            value = &attributes[1..end];
            attributes = &attributes[end + 1..];
            if !attributes.is_empty()
                && !attributes.starts_with(|character: char| character.is_ascii_whitespace())
            {
                return None;
            }
        } else {
            let end = attributes
                .find(|character: char| character.is_ascii_whitespace())
                .unwrap_or(attributes.len());
            value = &attributes[..end];
            attributes = &attributes[end..];
        }
        if name.eq_ignore_ascii_case("start") {
            if start.is_some() {
                return None;
            }
            start = Some(ascii_number(value)?);
        }
    }
}

fn strip_smi_markup(value: &str) -> Result<String, BrowserCaptionError> {
    let mut output = String::new();
    let mut cursor = 0;
    while let Some(tag) = next_smi_tag(value, cursor)? {
        output.push_str(&value[cursor..tag.start]);
        if tag.name.eq_ignore_ascii_case("br")
            || (tag.name.eq_ignore_ascii_case("p") && !output.is_empty() && !output.ends_with('\n'))
        {
            output.push('\n');
        }
        cursor = tag.end;
    }
    output.push_str(&value[cursor..]);
    let decoded = decode_smi_entities(&output);
    let decoded = decoded.trim();
    Ok(if decoded.is_empty() {
        String::new()
    } else {
        escape_cue_text(decoded)
    })
}

fn decode_smi_entities(value: &str) -> String {
    let mut output = String::new();
    let mut cursor = 0;
    while cursor < value.len() {
        let rest = &value[cursor..];
        // A bounded lookahead prevents repeated unterminated ampersands from
        // making parsing quadratic. Decode once, before WebVTT escaping.
        if rest.starts_with('&') {
            if let Some(end) = rest.bytes().take(32).position(|byte| byte == b';') {
                let entity = &rest[1..end];
                let decoded = match entity {
                    "amp" | "AMP" => Some('&'),
                    "lt" | "LT" => Some('<'),
                    "gt" | "GT" => Some('>'),
                    "nbsp" => Some('\u{a0}'),
                    "quot" | "QUOT" => Some('"'),
                    "apos" => Some('\''),
                    _ => entity
                        .strip_prefix("#x")
                        .or_else(|| entity.strip_prefix("#X"))
                        .filter(|digits| {
                            !digits.is_empty()
                                && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
                        })
                        .and_then(|digits| u32::from_str_radix(digits, 16).ok())
                        .or_else(|| {
                            entity
                                .strip_prefix('#')
                                .and_then(ascii_number)
                                .and_then(|number| u32::try_from(number).ok())
                        })
                        .and_then(char::from_u32)
                        .filter(|character| *character != '\0'),
                };
                if let Some(character) = decoded {
                    output.push(character);
                    cursor += end + 1;
                    continue;
                }
            }
        }
        // cursor always follows a complete Unicode character or an ASCII entity.
        let Some(character) = rest.chars().next() else {
            break;
        };
        output.push(character);
        cursor += character.len_utf8();
    }
    output
}

fn escape_cue_text(value: &str) -> String {
    // Numeric SAMI entities may introduce CR after the input was normalized.
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    let mut output = String::new();
    for (index, line) in normalized.split('\n').enumerate() {
        if index > 0 {
            output.push('\n');
        }
        // An empty WebVTT line would terminate the cue. Keep deliberate blank
        // subtitle lines as whitespace within the same cue instead.
        if line.is_empty() {
            output.push_str("&nbsp;");
        }
        for character in line.chars() {
            match character {
                '&' => output.push_str("&amp;"),
                '<' => output.push_str("&lt;"),
                '>' => output.push_str("&gt;"),
                _ => output.push(character),
            }
        }
    }
    output
}

fn check_output_growth(current: usize, incoming: usize) -> Result<(), BrowserCaptionError> {
    if current
        .checked_add(incoming)
        .is_none_or(|length| length > MAX_OUTPUT_BYTES)
    {
        Err(BrowserCaptionError::Malformed)
    } else {
        Ok(())
    }
}

fn append_cue(
    output: &mut String,
    start: u64,
    end: u64,
    text: &str,
) -> Result<(), BrowserCaptionError> {
    let timing = format!("{} --> {}\n", millis_vtt(start), millis_vtt(end));
    check_output_growth(
        output.len(),
        timing.len().saturating_add(text.len()).saturating_add(2),
    )?;
    output.push_str(&timing);
    output.push_str(text);
    output.push_str("\n\n");
    Ok(())
}

fn millis_vtt(value: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        value / 3_600_000,
        (value / 60_000) % 60,
        (value / 1000) % 60,
        value % 1000
    )
}

fn ascii_number(value: &str) -> Option<u64> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn two_digits(value: &str) -> Option<u64> {
    (value.len() == 2).then(|| ascii_number(value)).flatten()
}

fn parse_caption_time(value: &str, subrip: bool) -> Option<u64> {
    let mut parts = value.rsplit(':');
    let seconds = parts.next()?;
    let minutes = two_digits(parts.next()?)?;
    let hours = match parts.next() {
        Some(hours) if hours.len() >= 2 => ascii_number(hours)?,
        Some(_) => return None,
        None => 0,
    };
    let (seconds, millis) = seconds
        .split_once('.')
        .or_else(|| subrip.then(|| seconds.split_once(',')).flatten())?;
    let seconds = two_digits(seconds)?;
    if parts.next().is_some() || seconds > 59 || minutes > 59 || millis.len() != 3 {
        return None;
    }
    hours
        .checked_mul(3_600_000)?
        .checked_add(minutes.checked_mul(60_000)?)?
        .checked_add(seconds.checked_mul(1000)?)?
        .checked_add(ascii_number(millis)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_valid_input(conversion: CaptionWebVttConversion) -> &'static [u8] {
        match conversion {
            CaptionWebVttConversion::ValidateWebVtt => {
                b"WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nText\n"
            }
            CaptionWebVttConversion::SubRipToWebVtt => b"1\n00:00:00,000 --> 00:00:01,000\nText\n",
            CaptionWebVttConversion::SubStationAlphaToWebVtt => {
                b"[Events]\nFormat: Start, End, Text\nDialogue: 0:00:00.00,0:00:01.00,Text\n"
            }
            CaptionWebVttConversion::SamiToWebVtt => b"<SAMI><SYNC Start=0><P>Text</P></SAMI>",
        }
    }

    #[test]
    fn every_protocol_conversion_has_executable_dispatch() {
        for format in rusty_dlna_protocol::CAPTION_FORMATS {
            let Some(conversion) = format.webvtt_conversion else {
                continue;
            };
            let converted = caption_to_webvtt(conversion, minimal_valid_input(conversion));
            assert!(converted.is_ok(), "{}: {converted:?}", format.extension);
        }
    }

    fn converted(conversion: CaptionWebVttConversion, input: &[u8]) -> String {
        String::from_utf8(caption_to_webvtt(conversion, input).unwrap()).unwrap()
    }

    #[test]
    fn srt_conversion_preserves_multiline_unicode_and_overlap() {
        let input = "1\r\n00:00:01,250 --> 00:00:04,000\r\nHello\r\n世界\r\n\r\n2\r\n00:00:03,500 --> 00:00:05,000\r\nOverlap\r\n";
        let output = converted(CaptionWebVttConversion::SubRipToWebVtt, input.as_bytes());
        assert!(output.starts_with("WEBVTT\n\n"));
        assert!(output.contains("00:00:01.250 --> 00:00:04.000\nHello\n世界"));
        assert!(output.contains("00:00:03.500 --> 00:00:05.000\nOverlap"));
    }

    #[test]
    fn srt_literal_arrows_remain_text_and_preserve_cue_markup() {
        let output = converted(
            CaptionWebVttConversion::SubRipToWebVtt,
            b"1\n00:00:01,250 --> 00:00:02,500\nGo --> <b>next</b>\n00:00 --> still text\n",
        );
        assert!(output.contains("Go --&gt; <b>next</b>\n00:00 --&gt; still text"));
        assert!(
            caption_to_webvtt(CaptionWebVttConversion::ValidateWebVtt, output.as_bytes()).is_ok()
        );
    }

    #[test]
    fn ass_conversion_uses_declared_columns_and_removes_override_commands() {
        let input = "[Events]\nFormat: Layer, Start, End, Style, Text\nDialogue: 0,0:00:01.20,0:00:03.40,Default,{\\i1}Hello\\Nworld";
        let output = converted(
            CaptionWebVttConversion::SubStationAlphaToWebVtt,
            input.as_bytes(),
        );
        assert!(output.contains("00:00:01.200 --> 00:00:03.400"));
        assert!(output.contains("Hello\nworld"));
        assert!(!output.contains("\\i1"));
    }

    #[test]
    fn ass_conversion_carries_rounded_milliseconds_into_the_next_minute() {
        let input =
            "[Events]\nFormat: Start, End, Text\nDialogue: 0:00:59.9996,0:59:59.9996,Boundary";
        let output = converted(
            CaptionWebVttConversion::SubStationAlphaToWebVtt,
            input.as_bytes(),
        );
        assert!(output.contains("00:01:00.000 --> 01:00:00.000"));
        assert!(!output.contains(":60.000"));
    }

    #[test]
    fn smi_conversion_strips_markup_and_decodes_text_entities() {
        let input = "<SAMI><BODY><SYNC Start=1000><P Class=ENCC>Hello<br>Tom &amp; Jerry &lt;3&gt;</P><SYNC Start='2500'><P>Bye &gt; now</P></BODY></SAMI>";
        let output = converted(CaptionWebVttConversion::SamiToWebVtt, input.as_bytes());
        assert_eq!(output, "WEBVTT\n\n00:00:01.000 --> 00:00:02.500\nHello\nTom &amp; Jerry &lt;3&gt;\n\n00:00:02.500 --> 00:00:07.500\nBye &gt; now\n\n");
        assert!(!output.contains("<P"));
    }

    #[test]
    fn webvtt_normalizes_bom_and_crlf_but_preserves_cue_settings() {
        let input = b"\xef\xbb\xbfWEBVTT\r\n\r\nintro\r\n00:00:01.000 --> 00:00:02.500 line:90% position:20%\r\nHello\r\n";
        assert_eq!(
            converted(CaptionWebVttConversion::ValidateWebVtt, input),
            "WEBVTT\n\nintro\n00:00:01.000 --> 00:00:02.500 line:90% position:20%\nHello\n"
        );
    }

    #[test]
    fn webvtt_rejects_malformed_or_reversed_timings() {
        for input in [
            "WEBVTTjunk\n\n00:00:01.000 --> 00:00:02.000\nBad\n",
            "WEBVTT\u{a0}junk\n\n00:00:01.000 --> 00:00:02.000\nBad\n",
            "WEBVTT\n00:00:01.000 --> 00:00:02.000\nMissing blank line\n",
            "WEBVTT\n\n00:00:01,000 --> 00:00:02,000\nComma\n",
            "WEBVTT\n\n0:00:01.000 --> 00:00:02.000\nShort hours\n",
            "WEBVTT\n\n00:0:01.000 --> 00:00:02.000\nShort minutes\n",
            "WEBVTT\n\n00:00:01 --> 00:00:02\nMissing milliseconds\n",
            "WEBVTT\n\n00:00:01.00 --> 00:00:02.000\nShort milliseconds\n",
            "WEBVTT\n\n00:00:01.0000 --> 00:00:02.000\nLong milliseconds\n",
            "WEBVTT\n\n00:00:1e1 --> 00:00:20.000\nExponent\n",
            "WEBVTT\n\n00:00:01.000-->00:00:02.000\nMissing spaces\n",
            "WEBVTT\n\n00:00:01.000 --> 00:00:01.000\nZero length\n",
            "WEBVTT\n\n00:00:60.000 --> 00:01:01.000\nBad\n",
            "WEBVTT\n\n00:00:03.000 --> 00:00:02.000\nBackwards\n",
            "WEBVTT\n\n00:00:01.000 --> nope\nBad\n",
        ] {
            assert_eq!(
                caption_to_webvtt(CaptionWebVttConversion::ValidateWebVtt, input.as_bytes()),
                Err(BrowserCaptionError::Malformed),
                "{input:?}"
            );
        }
    }

    #[test]
    fn webvtt_preserves_supported_headers_blocks_and_payloads() {
        let input = "WEBVTT\tCaption --> annotation\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:0\n\nNOTE commentary\nThis is not cue text.\n\nSTYLE\n::cue { color: lime; }\n\nREGION\nid:bottom\nwidth:80%\n\nopening\n00:01.250\t-->\t00:02.750 line:90% position:20% region:bottom\n<v Alice><b>Hello</b> &amp; <c.color>world</c></v>\n\nNOTE\nEnd of scene\n\n00:02.500 --> 00:03.000\nOverlapping <00:02.750>text\n";
        for newline in ["\n", "\r", "\r\n"] {
            let source = format!("\u{feff}{}", input.replace('\n', newline));
            assert_eq!(
                converted(CaptionWebVttConversion::ValidateWebVtt, source.as_bytes()),
                input
            );
        }
        for empty in ["WEBVTT", "WEBVTT\n\n", "WEBVTT\n\nNOTE Empty track\n"] {
            assert!(
                caption_to_webvtt(CaptionWebVttConversion::ValidateWebVtt, empty.as_bytes())
                    .is_ok()
            );
        }
    }

    #[test]
    fn sami_clear_markers_preserve_gaps_and_terminal_boundaries() {
        for clear in ["", "<P>&nbsp;</P>", "<P>&#160; &#xA0;</P>", "<P>\n \t</P>"] {
            let input = format!("<SAMI><SYNC Start=1000><P>First</P><SYNC Start=2000>{clear}<SYNC Start=5000><P>Next</P><SYNC Start=6500><P>&nbsp;</P></SAMI>");
            assert_eq!(converted(CaptionWebVttConversion::SamiToWebVtt, input.as_bytes()),
                "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nFirst\n\n00:00:05.000 --> 00:00:06.500\nNext\n\n");
        }
        assert_eq!(
            converted(
                CaptionWebVttConversion::SamiToWebVtt,
                b"<SYNC Start=1000><P>&nbsp;</P>"
            ),
            "WEBVTT\n\n"
        );
    }

    #[test]
    fn sami_decodes_once_and_keeps_literal_angles_and_line_breaks() {
        let input = "<SAMI><!-- <SYNC Start=0>ignored --><SyNc title='x > y' START = \"1250\"><P><b>Bold</b> &lt;b&gt;literal&lt;/b&gt; &amp;lt;i&amp;gt;<bR />Tom &amp; Jerry &#60;3&#x3E; &quot;yes&quot; &apos;ok&apos; &unknown;<P>2 < 3 > 1<SYNC Start=2750><P>&nbsp;</P></SAMI>";
        assert_eq!(converted(CaptionWebVttConversion::SamiToWebVtt, input.as_bytes()),
            "WEBVTT\n\n00:00:01.250 --> 00:00:02.750\nBold &lt;b&gt;literal&lt;/b&gt; &amp;lt;i&amp;gt;\nTom &amp; Jerry &lt;3&gt; \"yes\" 'ok' &amp;unknown;\n2 &lt; 3 &gt; 1\n\n");
    }

    #[test]
    fn ass_keeps_literal_markup_entities_and_plain_braces() {
        let input = "[Events]\nFoRmAt: Start, End, Text\nDiAlOgUe: 0:00:01.25,0:00:02.75,{\\i1}Italic{\\i0} <b>literal</b> &lt;3&gt; {braces}\\Nnext\\nline\\hword, comma";
        assert_eq!(converted(CaptionWebVttConversion::SubStationAlphaToWebVtt, input.as_bytes()),
            "WEBVTT\n\n00:00:01.250 --> 00:00:02.750\nItalic &lt;b&gt;literal&lt;/b&gt; &amp;lt;3&amp;gt; {braces}\nnext\nline\u{a0}word, comma\n\n");
        let blank =
            "[Events]\nFormat: Start, End, Text\nDialogue: 0:00:01.00,0:00:02.00,First\\N\\NLast";
        assert_eq!(
            converted(
                CaptionWebVttConversion::SubStationAlphaToWebVtt,
                blank.as_bytes()
            ),
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nFirst\n&nbsp;\nLast\n\n"
        );
    }

    #[test]
    fn sami_rejects_ambiguous_decreasing_and_overflow_timestamps() {
        for input in [
            "<SYNC Restart=1000>Wrong attribute",
            "<SYNC Start=1000junk>Bad number",
            "<SYNC Start=+1000>Signed",
            "<SYNC Start='1000' Start=2000>Duplicate",
            "<SYNC Start='1000>Unclosed quote",
            "<SYNC Start=2000>Backwards<SYNC Start=1000>Text",
            "<SYNC Start=18446744073709551615>Final cue overflows",
            "<SYNC Start=18446744073709551616>Timestamp overflows",
        ] {
            assert_eq!(
                caption_to_webvtt(CaptionWebVttConversion::SamiToWebVtt, input.as_bytes()),
                Err(BrowserCaptionError::Malformed),
                "{input}"
            );
        }
    }

    #[test]
    fn timestamp_arithmetic_and_conversion_inputs_are_bounded() {
        assert_eq!(check_output_growth(MAX_OUTPUT_BYTES - 1, 1), Ok(()));
        assert_eq!(
            check_output_growth(MAX_OUTPUT_BYTES, 1),
            Err(BrowserCaptionError::Malformed)
        );
        assert_eq!(
            check_output_growth(usize::MAX, 1),
            Err(BrowserCaptionError::Malformed)
        );
        assert_eq!(parse_caption_time("5124095576031:00:00.000", false), None);
        assert_eq!(ass_time("5124095576031:00:00.000"), None);
        assert_eq!(ass_time("0:00:5e1"), None);
        assert_eq!(ass_time("0:00:NaN"), None);
        assert_eq!(
            parse_caption_time("100:00:00.001", false),
            Some(360_000_001)
        );
        let oversized = vec![b'x'; MAX_INPUT_BYTES + 1];
        assert_eq!(
            caption_to_webvtt(CaptionWebVttConversion::ValidateWebVtt, &oversized),
            Err(BrowserCaptionError::Malformed)
        );
        assert_eq!(
            caption_to_webvtt(CaptionWebVttConversion::ValidateWebVtt, b"WEBVTT\0\n\n"),
            Err(BrowserCaptionError::Malformed)
        );
    }

    #[test]
    fn sami_numeric_line_endings_cannot_terminate_a_cue() {
        let input = b"<SYNC Start=1000><P>First&#13;&#13;Last<SYNC Start=2000><P>&nbsp;";
        assert_eq!(
            converted(CaptionWebVttConversion::SamiToWebVtt, input),
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nFirst\n&nbsp;\nLast\n\n"
        );
    }

    #[test]
    fn sami_entity_decoding_preserves_unsupported_or_malformed_references() {
        assert_eq!(
            decode_smi_entities("&AMP;&LT;&GT;&QUOT; &#+65; &#x+41; &#xD800; &#0; &unknown;"),
            "&<>\" &#+65; &#x+41; &#xD800; &#0; &unknown;"
        );
    }

    #[test]
    fn caption_conversion_rejects_invalid_text_input() {
        assert_eq!(
            caption_to_webvtt(CaptionWebVttConversion::SubRipToWebVtt, b"not a cue"),
            Err(BrowserCaptionError::Malformed)
        );
        assert_eq!(
            caption_to_webvtt(CaptionWebVttConversion::ValidateWebVtt, &[0xff, 0xfe]),
            Err(BrowserCaptionError::Encoding)
        );
    }
}
