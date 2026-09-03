//! Pure conversion of bounded text-caption sidecars to browser WebVTT.
//!
//! The HTTP layer confines and caps sidecar reads before calling this module;
//! these helpers only validate and transform the supplied bytes.

use rusty_dlna_protocol::CaptionWebVttConversion;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BrowserCaptionError {
    Encoding,
    Malformed,
}

pub(super) fn caption_to_webvtt(
    conversion: CaptionWebVttConversion,
    body: &[u8],
) -> Result<Vec<u8>, BrowserCaptionError> {
    let text = std::str::from_utf8(body)
        .map_err(|_| BrowserCaptionError::Encoding)?
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let output = match conversion {
        CaptionWebVttConversion::ValidateWebVtt => validate_webvtt(&text)?,
        CaptionWebVttConversion::SubRipToWebVtt => srt_to_webvtt(&text)?,
        CaptionWebVttConversion::SubStationAlphaToWebVtt => ass_to_webvtt(&text)?,
        CaptionWebVttConversion::SamiToWebVtt => smi_to_webvtt(&text)?,
    };
    Ok(output.into_bytes())
}

fn validate_webvtt(text: &str) -> Result<String, BrowserCaptionError> {
    let Some(first) = text.lines().next() else {
        return Err(BrowserCaptionError::Malformed);
    };
    if !first.trim_end().starts_with("WEBVTT") {
        return Err(BrowserCaptionError::Malformed);
    }
    let mut saw_cue = false;
    for line in text.lines().filter(|line| line.contains("-->")) {
        let (start, rest) = line
            .split_once("-->")
            .ok_or(BrowserCaptionError::Malformed)?;
        let end = rest
            .split_whitespace()
            .next()
            .ok_or(BrowserCaptionError::Malformed)?;
        let start_seconds =
            parse_caption_time(start.trim()).ok_or(BrowserCaptionError::Malformed)?;
        let end_seconds = parse_caption_time(end).ok_or(BrowserCaptionError::Malformed)?;
        if end_seconds < start_seconds {
            return Err(BrowserCaptionError::Malformed);
        }
        saw_cue = true;
    }
    if !saw_cue {
        return Err(BrowserCaptionError::Malformed);
    }
    Ok(format!("{}\n", text.trim_end()))
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
        let (start, end_and_settings) = timing
            .split_once("-->")
            .ok_or(BrowserCaptionError::Malformed)?;
        let mut end_parts = end_and_settings.split_whitespace();
        let end = end_parts.next().ok_or(BrowserCaptionError::Malformed)?;
        let start_seconds =
            parse_caption_time(start.trim()).ok_or(BrowserCaptionError::Malformed)?;
        let end_seconds = parse_caption_time(end).ok_or(BrowserCaptionError::Malformed)?;
        if end_seconds < start_seconds {
            return Err(BrowserCaptionError::Malformed);
        }
        let payload = lines.collect::<Vec<_>>();
        if payload.is_empty() {
            return Err(BrowserCaptionError::Malformed);
        }
        output.push_str(&start.trim().replace(',', "."));
        output.push_str(" --> ");
        output.push_str(&end.replace(',', "."));
        for setting in end_parts {
            output.push(' ');
            output.push_str(setting);
        }
        output.push('\n');
        output.push_str(&payload.join("\n"));
        output.push_str("\n\n");
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
            .strip_prefix("Format:")
            .or_else(|| line.strip_prefix("format:"))
        {
            columns = format
                .split(',')
                .map(|field| field.trim().to_ascii_lowercase())
                .collect();
            continue;
        }
        let Some(dialogue) = line
            .strip_prefix("Dialogue:")
            .or_else(|| line.strip_prefix("dialogue:"))
        else {
            continue;
        };
        if columns.is_empty() {
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
        if parse_caption_time(&end).unwrap_or(0.0) < parse_caption_time(&start).unwrap_or(0.0) {
            return Err(BrowserCaptionError::Malformed);
        }
        let cue = strip_ass_overrides(field("text").ok_or(BrowserCaptionError::Malformed)?);
        if cue.trim().is_empty() {
            continue;
        }
        output.push_str(&format!("{start} --> {end}\n{cue}\n\n"));
        cues += 1;
    }
    (cues > 0)
        .then_some(output)
        .ok_or(BrowserCaptionError::Malformed)
}

fn ass_time(value: &str) -> Option<String> {
    let mut fields = value.trim().split(':');
    let hours = fields.next()?.parse::<u32>().ok()?;
    let minutes = fields.next()?.parse::<u32>().ok()?;
    let seconds = fields.next()?.parse::<f64>().ok()?;
    if fields.next().is_some() || minutes > 59 || !(0.0..60.0).contains(&seconds) {
        return None;
    }
    let second_millis = (seconds * 1000.0).round() as u64;
    let millis = u64::from(hours)
        .checked_mul(3_600_000)?
        .checked_add(u64::from(minutes).checked_mul(60_000)?)?
        .checked_add(second_millis)?;
    Some(millis_vtt(millis))
}

fn strip_ass_overrides(value: &str) -> String {
    let mut output = String::new();
    let mut in_override = false;
    for character in value.chars() {
        match character {
            '{' => in_override = true,
            '}' => in_override = false,
            _ if !in_override => output.push(character),
            _ => {}
        }
    }
    output
        .replace("\\N", "\n")
        .replace("\\n", "\n")
        .replace("\\h", " ")
}

fn smi_to_webvtt(text: &str) -> Result<String, BrowserCaptionError> {
    let lower = text.to_ascii_lowercase();
    let mut cues = Vec::<(u64, String)>::new();
    let mut cursor = 0usize;
    while let Some(relative) = lower[cursor..].find("<sync") {
        let tag_start = cursor + relative;
        let tag_end = lower[tag_start..]
            .find('>')
            .map(|index| tag_start + index + 1)
            .ok_or(BrowserCaptionError::Malformed)?;
        let tag = &lower[tag_start..tag_end];
        let start = tag
            .find("start")
            .and_then(|index| {
                tag[index + 5..]
                    .find('=')
                    .map(|offset| index + 5 + offset + 1)
            })
            .and_then(|index| {
                tag[index..]
                    .trim_start_matches([' ', '\'', '"'])
                    .split(|character: char| !character.is_ascii_digit())
                    .next()
            })
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(BrowserCaptionError::Malformed)?;
        let next = lower[tag_end..]
            .find("<sync")
            .map(|index| tag_end + index)
            .unwrap_or(text.len());
        let cue = strip_smi_markup(&text[tag_end..next]);
        if !cue.trim().is_empty() && !cue.trim().eq_ignore_ascii_case("&nbsp;") {
            cues.push((start, cue));
        }
        cursor = next;
        if cursor >= text.len() {
            break;
        }
    }
    if cues.is_empty() {
        return Err(BrowserCaptionError::Malformed);
    }
    let mut output = String::from("WEBVTT\n\n");
    for (index, (start, cue)) in cues.iter().enumerate() {
        let end = cues
            .get(index + 1)
            .map(|next| next.0)
            .unwrap_or_else(|| start.saturating_add(5_000));
        output.push_str(&format!(
            "{} --> {}\n{}\n\n",
            millis_vtt(*start),
            millis_vtt(end.max(start.saturating_add(1))),
            cue
        ));
    }
    Ok(output)
}

fn strip_smi_markup(value: &str) -> String {
    let normalized = value
        .replace("<br>", "\n")
        .replace("<BR>", "\n")
        .replace("<br/>", "\n")
        .replace("<BR/>", "\n");
    let mut output = String::new();
    let mut in_tag = false;
    for character in normalized.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(character),
            _ => {}
        }
    }
    output
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_owned()
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

fn parse_caption_time(value: &str) -> Option<f64> {
    let normalized = value.replace(',', ".");
    let mut parts = normalized.split(':').collect::<Vec<_>>();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let seconds = parts.pop()?.parse::<f64>().ok()?;
    let minutes = parts.pop()?.parse::<u32>().ok()?;
    let hours = parts
        .pop()
        .map(str::parse::<u32>)
        .transpose()
        .ok()?
        .unwrap_or(0);
    (minutes <= 59 && (0.0..60.0).contains(&seconds))
        .then_some(hours as f64 * 3600.0 + minutes as f64 * 60.0 + seconds)
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
        assert!(output.contains("00:00:01.000 --> 00:00:02.500\nHello\nTom & Jerry <3>"));
        assert!(output.contains("00:00:02.500 --> 00:00:07.500\nBye > now"));
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
