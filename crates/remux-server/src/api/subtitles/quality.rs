//! Content checks shared by discovery, delivery and durable subtitle caches.
//! A readable forced track may be short; an ordinary full-length track must
//! contain dialogue across a plausible part of the video's timeline.
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Problem {
    InvalidText,
    NoReadableCues,
    MalformedCues,
    SparseDialogue,
    IncompleteTimeline,
    OutsideTimeline,
}

#[derive(Clone, Copy, Default)]
pub(super) struct Requirements {
    pub duration_ticks: Option<i64>,
    pub forced: bool,
}

struct Cue {
    start: i64,
    end: i64,
    text: String,
    visible: String,
}

fn timestamp(value: &str) -> Option<i64> {
    let value = value.trim();
    let parts: Vec<_> = value
        .split(':')
        .collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [hours, minutes, seconds] => (
            hours
                .parse::<i64>()
                .ok()?,
            *minutes,
            *seconds,
        ),
        [minutes, seconds] => (0, *minutes, *seconds),
        _ => return None,
    };
    let minutes = minutes
        .parse::<i64>()
        .ok()?;
    let (seconds, fraction) = seconds.split_once(['.', ','])?;
    let seconds = seconds
        .parse::<i64>()
        .ok()?;
    if hours < 0
        || !(0..60).contains(&minutes)
        || !(0..60).contains(&seconds)
        || fraction.is_empty()
        || fraction.len() > 3
        || !fraction
            .bytes()
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let millis = fraction
        .parse::<i64>()
        .ok()?
        * 10_i64.pow(3 - fraction.len() as u32);
    hours
        .checked_mul(3_600_000)?
        .checked_add(minutes * 60_000 + seconds * 1000 + millis)
}

fn visible_text(text: &str) -> String {
    let mut visible = String::new();
    let mut tag_end = None;
    for ch in text.chars() {
        if let Some(end) = tag_end {
            if ch == end {
                tag_end = None;
            }
        } else {
            match ch {
                '<' => tag_end = Some('>'),
                '{' => tag_end = Some('}'),
                _ => visible.push(ch),
            }
        }
    }
    for empty in [
        "&nbsp;",
        "&#160;",
        "&#xA0;",
        "&lrm;",
        "&rlm;",
        "&ZeroWidthSpace;",
    ] {
        visible = visible.replace(empty, " ");
    }
    // Numeric entities for zero-width/space characters must not turn their
    // digit strings into apparent dialogue. Decode real letter entities too.
    let mut decoded = String::new();
    let mut remaining = visible.as_str();
    while let Some(start) = remaining.find('&') {
        decoded.push_str(&remaining[..start]);
        let rest = &remaining[start + 1..];
        let Some(end) = rest
            .find(';')
            .filter(|end| *end < 16)
        else {
            decoded.push('&');
            remaining = rest;
            continue;
        };
        let entity = &rest[..end];
        let number = entity
            .strip_prefix("#x")
            .or_else(|| entity.strip_prefix("#X"))
            .and_then(|n| u32::from_str_radix(n, 16).ok())
            .or_else(|| {
                entity
                    .strip_prefix('#')
                    .and_then(|n| {
                        n.parse()
                            .ok()
                    })
            });
        if let Some(ch) = number.and_then(char::from_u32) {
            decoded.push(ch);
        } else if let Some(ch) = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => None,
        } {
            decoded.push(ch);
        }
        remaining = &rest[end + 1..];
    }
    decoded.push_str(remaining);
    decoded
        .replace("\\N", " ")
        .replace("\\n", " ")
        .replace("\\h", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn cues(bytes: &[u8]) -> Result<Vec<Cue>, Problem> {
    let text = std::str::from_utf8(bytes).map_err(|_| Problem::InvalidText)?;
    let text = text
        .trim_start_matches('\u{feff}')
        .trim_start();
    if text.starts_with(['{', '<']) {
        return Err(Problem::InvalidText);
    }
    let normalized = text
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let lines: Vec<_> = normalized
        .lines()
        .collect();
    let mut parsed = Vec::new();
    let mut attempted = 0;
    let mut malformed = 0;
    let mut ass_fields: Vec<String> = Vec::new();
    let mut in_events = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        i += 1;
        let candidate = if let Some((start, rest)) = line.split_once("-->") {
            attempted += 1;
            let end = rest
                .split_whitespace()
                .next()
                .unwrap_or("");
            let mut content = Vec::new();
            while i < lines.len()
                && !lines[i]
                    .trim()
                    .is_empty()
            {
                content.push(lines[i]);
                i += 1;
            }
            Some((timestamp(start), timestamp(end), content.join("\n")))
        } else if line.starts_with('[') {
            in_events = line.eq_ignore_ascii_case("[Events]");
            None
        } else if in_events && line.starts_with("Format:") {
            ass_fields = line[7..]
                .split(',')
                .map(|f| {
                    f.trim()
                        .to_ascii_lowercase()
                })
                .collect();
            None
        } else if in_events && line.starts_with("Dialogue:") {
            attempted += 1;
            let fields: Vec<_> = line[9..]
                .trim_start()
                .splitn(
                    ass_fields
                        .len()
                        .max(1),
                    ',',
                )
                .collect();
            let field = |name| {
                ass_fields
                    .iter()
                    .position(|f| f == name)
                    .and_then(|n| {
                        fields
                            .get(n)
                            .copied()
                    })
            };
            if ass_fields
                .last()
                .map(String::as_str)
                != Some("text")
            {
                malformed += 1;
                continue;
            }
            Some((
                field("start").and_then(timestamp),
                field("end").and_then(timestamp),
                field("text")
                    .unwrap_or("")
                    .replace("\\N", "\n")
                    .replace("\\n", "\n"),
            ))
        } else {
            None
        };
        if let Some((start, end, text)) = candidate {
            let (Some(start), Some(end)) = (start, end) else {
                malformed += 1;
                continue;
            };
            if end <= start || end - start > 180_000 {
                malformed += 1;
                continue;
            }
            let visible = visible_text(&text);
            if visible
                .chars()
                .any(char::is_alphanumeric)
            {
                parsed.push(Cue {
                    start,
                    end,
                    text,
                    visible,
                });
            }
        }
    }
    if parsed.is_empty() {
        return Err(Problem::NoReadableCues);
    }
    if malformed * 5 > attempted || parsed.len() * 5 < attempted {
        return Err(Problem::MalformedCues);
    }
    parsed.sort_by_key(|c| (c.start, c.end));
    Ok(parsed)
}

pub(super) fn check(bytes: &[u8], requirements: Requirements) -> Result<(), Problem> {
    let cues = cues(bytes)?;
    let Some(duration) = requirements
        .duration_ticks
        .filter(|t| *t > 0)
        .map(|t| t / 10_000)
    else {
        return Ok(());
    };
    // Ignore a small tail of credits/rounding, but reject a shifted or wrong
    // timebase even for forced tracks. A single outlier must not establish
    // apparent full-length coverage.
    if cues
        .iter()
        .filter(|c| c.start > duration + 60_000)
        .count()
        * 10
        > cues.len()
    {
        return Err(Problem::OutsideTimeline);
    }
    if requirements.forced || duration < 600_000 {
        return Ok(());
    }
    let minimum = (duration / 120_000).clamp(8, 30) as usize;
    let mut distinct = HashMap::<&str, usize>::new();
    for cue in &cues {
        *distinct
            .entry(
                cue.visible
                    .as_str(),
            )
            .or_default() += 1;
    }
    let characters = cues
        .iter()
        .flat_map(|c| {
            c.visible
                .chars()
        })
        .filter(|c| c.is_alphanumeric())
        .count();
    let mut covered = 0;
    let mut end = 0;
    for cue in &cues {
        covered += (cue.end
            - cue
                .start
                .max(end))
        .max(0);
        end = end.max(cue.end);
    }
    if cues.len() < minimum
        || distinct.len() < 4
        || characters < minimum * 6
        || distinct
            .values()
            .copied()
            .max()
            .unwrap_or(0)
            * 10
            > cues.len() * 9
        || covered < duration / 200
    {
        return Err(Problem::SparseDialogue);
    }
    let first = cues[cues.len() / 10].start;
    let last = cues[cues.len() * 9 / 10].end;
    if first > duration * 4 / 10
        || last < duration * 6 / 10
        || last - first < duration * 4 / 10
    {
        return Err(Problem::IncompleteTimeline);
    }
    Ok(())
}

/// Canonical text delivery supports SRT, WebVTT and ASS input. Sorting also
/// repairs providers that put late signs before the actual dialogue track.
pub(super) fn to_srt(bytes: &[u8]) -> Option<String> {
    fn time(ms: i64) -> String {
        format!(
            "{:02}:{:02}:{:02},{:03}",
            ms / 3_600_000,
            ms / 60_000 % 60,
            ms / 1000 % 60,
            ms % 1000
        )
    }
    let mut output = String::new();
    for (i, cue) in cues(bytes)
        .ok()?
        .into_iter()
        .enumerate()
    {
        output.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            time(cue.start),
            time(cue.end),
            cue.text
        ));
    }
    Some(output)
}

pub(super) fn preserves_dialogue(original: &[u8], aligned: &[u8]) -> bool {
    let (Ok(original), Ok(aligned)) = (cues(original), cues(aligned)) else {
        return false;
    };
    let texts = |cues: Vec<Cue>| {
        let mut texts: Vec<_> = cues
            .into_iter()
            .map(|c| c.visible)
            .collect();
        texts.sort_unstable();
        texts
    };
    texts(original) == texts(aligned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(count: usize, span: i64) -> Vec<u8> {
        (0..count).map(|n| {
            let second = n as i64 * span / count as i64;
            format!("{}\n{:02}:{:02}:{:02},000 --> {:02}:{:02}:{:02},900\nDialogue number {n}\n\n", n+1,
                second / 3600, second / 60 % 60, second % 60,
                second / 3600, second / 60 % 60, second % 60)
        }).collect::<String>().into_bytes()
    }

    #[test]
    fn refuses_markup_punctuation_and_empty_ass_dialogue() {
        for payload in [
            "1\n00:00:01,000 --> 00:00:02,000\n<i> </i>&nbsp;\n",
            "1\n00:00:01,000 --> 00:00:02,000\n...\n",
            "1\n00:00:01,000 --> 00:00:02,000\n&#8203;&#x200B;\n",
            "[Events]\nFormat: Layer, Start, End, Style, Text\nDialogue: 0,0:00:01.00,0:00:02.00,Default,{\\an8}\n",
        ] {
            assert!(check(payload.as_bytes(), Requirements::default()).is_err());
        }
    }

    #[test]
    fn full_movie_refuses_stub_and_wrong_shorter_film() {
        let movie = Requirements {
            duration_ticks: Some(9300 * 10_000_000),
            forced: false,
        };
        assert_eq!(check(&track(1, 10), movie), Err(Problem::SparseDialogue));
        assert_eq!(
            check(&track(1321, 5201), movie),
            Err(Problem::IncompleteTimeline)
        );
        assert!(check(&track(2500, 9200), movie).is_ok());
        let mut outlier = track(1321, 5201);
        outlier.extend_from_slice(
            b"9999\n02:34:50,000 --> 02:34:52,000\nOnly a late credit\n\n",
        );
        assert!(check(&outlier, movie).is_err());
    }

    #[test]
    fn retains_real_forced_and_short_clip_subtitles() {
        let stub = track(1, 10);
        assert!(
            check(
                &stub,
                Requirements {
                    duration_ticks: Some(9300 * 10_000_000),
                    forced: true
                }
            )
            .is_ok()
        );
        assert!(
            check(
                &stub,
                Requirements {
                    duration_ticks: Some(60 * 10_000_000),
                    forced: false
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn parses_short_vtt_timestamps_settings_and_sorts_cues() {
        let input = b"\xef\xbb\xbfWEBVTT\r\n\r\nlate\r\n01:20.000 --> 01:21.500 align:start\r\nLater\r\n\r\nearly\r\n00:15.000 --> 00:16.000\r\nEarlier\r\n";
        let output = to_srt(input).unwrap();
        assert!(
            output
                .find("Earlier")
                .unwrap()
                < output
                    .find("Later")
                    .unwrap()
        );
        assert!(output.contains("00:00:15,000 --> 00:00:16,000"));
    }

    #[test]
    fn ass_requires_real_timings_and_visible_dialogue() {
        let ass = b"[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\i1}Hello, world\\NSecond line\n";
        assert!(check(ass, Requirements::default()).is_ok());
        let output = to_srt(ass).unwrap();
        assert!(output.contains("00:00:01,000 --> 00:00:02,000"));
        assert!(output.contains("Hello, world\nSecond line"));
        assert!(!preserves_dialogue(
            ass,
            b"1\n00:00:01,000 --> 00:00:02,000\nDifferent\n"
        ));
        assert!(preserves_dialogue(ass, output.as_bytes()));
    }

    #[test]
    fn rejects_invalid_timestamps_and_wrong_timebase() {
        for timestamp in ["-1:00:01.000", "00:70:01.000", "00:00:NaN", "00:00:01.1234"]
        {
            assert!(super::timestamp(timestamp).is_none());
        }
        let late = b"1\n10:00:01,000 --> 10:00:02,000\nHello\n";
        assert_eq!(
            check(
                late,
                Requirements {
                    duration_ticks: Some(60 * 10_000_000),
                    forced: true
                }
            ),
            Err(Problem::OutsideTimeline)
        );
    }
}
