//! Optional subtitle-only worker integration. The video delivery path is untouched.
use std::time::Duration;

use axum::body::Bytes;
use serde::Deserialize;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{AppState, api, db};

const MAX_SUBTITLE_BYTES: usize = 2_000_000;
const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);
static JOB_SLOT: Semaphore = Semaphore::const_new(1);

#[derive(Clone)]
enum Outcome {
    Pending,
    Ready(String),
    Rejected,
    Unavailable,
}

#[derive(Deserialize)]
struct WorkerReply {
    report: WorkerReport,
    subtitle: Option<String>,
}

#[derive(Deserialize)]
struct WorkerReport {
    accepted: bool,
}

fn text_codec(codec: Option<&str>) -> bool {
    matches!(
        codec
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "subrip"
            | "srt"
            | "webvtt"
            | "vtt"
            | "ass"
            | "ssa"
            | "mov_text"
            | "text"
            | "ttml"
    )
}

fn cache_key(source: Uuid, external: &[u8]) -> String {
    format!("subtitle-alignment-v4:{}", Uuid::new_v5(&source, external))
}

fn alignment_skip_reason(
    language: Option<&str>,
    streams: &[api::MediaStream],
) -> Option<&'static str> {
    let language = language.and_then(super::lang_to_two_letter);
    if !matches!(language.as_deref(), Some("en" | "vi")) {
        return Some("language-skipped");
    }
    if streams
        .iter()
        .any(|s| {
            matches!(s.type_, Some(api::MediaStreamType::Subtitle))
                && !s.is_external
                && !s.is_forced
                && s.language
                    .as_deref()
                    .and_then(super::lang_to_two_letter)
                    == language
        })
    {
        return Some("embedded-language");
    }
    None
}

fn source_identity(
    source_id: Uuid,
    info: &crate::stream::StreamInfo,
    duration: Option<i64>,
) -> Option<Uuid> {
    // AddonService derives source_id from the provider release's stable dedup
    // key. Signed/encrypted playback URLs renew even for the exact same file.
    // Use that identity only with concrete file metadata; guard against edits
    // by including filename, size, and the probed timeline duration.
    let mut identity = if matches!(
        info.descriptor,
        crate::stream::StreamDescriptor::Http { .. }
    ) && info
        .filename
        .as_deref()
        .is_some_and(|name| !name.is_empty())
        && info
            .size
            .is_some_and(|size| size > 0)
    {
        serde_json::json!({"release": source_id, "filename": info.filename,
            "size": info.size, "duration": duration,
            "torrent": info.torrent_info_hash, "file_index": info.torrent_file_idx,
            "usenet_guid": info.usenet_guid})
    } else {
        // Unknown sources retain the full descriptor, including query params.
        // Never strip arbitrary URL components that may identify different media.
        serde_json::to_value(&info.descriptor).ok()?
    };
    identity.sort_all_objects();
    Some(Uuid::new_v5(
        &source_id,
        &serde_json::to_vec(&identity).ok()?,
    ))
}

fn result(outcome: &Outcome, original: Bytes) -> (Bytes, &'static str) {
    match outcome {
        Outcome::Ready(text) => (Bytes::copy_from_slice(text.as_bytes()), "aligned"),
        Outcome::Pending => (original, "pending"),
        Outcome::Rejected => (original, "rejected"),
        Outcome::Unavailable => (original, "unavailable"),
    }
}

async fn reference_text(
    state: &AppState,
    input: &str,
    source: Uuid,
    index: i64,
    codec: Option<&str>,
) -> Option<String> {
    let dir = state
        .ctx
        .config
        .data_dir
        .join("subtitle-alignment-reference");
    tokio::fs::create_dir_all(&dir)
        .await
        .ok()?;
    let path = dir.join(format!("{source}_{index}.srt"));
    // Bounded persistent text cache: 32 x 2 MB maximum, seven-day expiry.
    // Especially important because demuxing embedded text may read the video.
    let mut entries = tokio::fs::read_dir(&dir)
        .await
        .ok()?;
    let mut retained = Vec::new();
    while let Ok(Some(entry)) = entries
        .next_entry()
        .await
    {
        let Ok(meta) = entry
            .metadata()
            .await
        else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if meta.len() > MAX_SUBTITLE_BYTES as u64
            || modified
                .elapsed()
                .unwrap_or_default()
                > Duration::from_secs(7 * 86400)
        {
            let _ = tokio::fs::remove_file(entry.path()).await;
        } else {
            retained.push((modified, entry.path()));
        }
    }
    if let Ok(text) = tokio::fs::read_to_string(&path).await {
        if !text
            .trim()
            .is_empty()
        {
            return Some(text);
        }
    }
    retained.sort_by_key(|(modified, _)| *modified);
    let count = retained
        .len()
        .saturating_sub(31);
    for (_, old) in retained
        .into_iter()
        .take(count)
    {
        let _ = tokio::fs::remove_file(old).await;
    }
    let temp = tempfile::tempdir().ok()?;
    let extracted = super::extract_subtitle_to_cache(
        temp.path(),
        input,
        &format!("0:{index}"),
        source,
        index,
        api::SubtitleCodec::Srt,
        codec,
    )
    .await
    .ok()?;
    if tokio::fs::metadata(&extracted)
        .await
        .ok()?
        .len()
        > MAX_SUBTITLE_BYTES as u64
    {
        return None;
    }
    let reference = tokio::fs::read_to_string(extracted)
        .await
        .ok()?;
    let stage = tempfile::NamedTempFile::new_in(&dir)
        .ok()?
        .into_temp_path();
    tokio::fs::write(&stage, &reference)
        .await
        .ok()?;
    tokio::fs::rename(&stage, &path)
        .await
        .ok()?;
    Some(reference)
}

/// Never blocks playback on extraction/inference. A subsequent subtitle load uses
/// the completed result. Cache identity includes the guarded provider release
/// identity and external content; unknown sources retain the full descriptor.
pub(super) async fn resolve(
    state: &AppState,
    source: &db::Media,
    original: Bytes,
    language: Option<&str>,
    format: &str,
    bypass: bool,
) -> (Bytes, &'static str) {
    let config = &state
        .ctx
        .config;
    if bypass {
        return (original, "original");
    }
    let (Some(endpoint), Some(token_file)) = (
        config
            .subtitle_alignment_url
            .clone(),
        config
            .subtitle_alignment_token_file
            .clone(),
    ) else {
        return (original, "disabled");
    };
    if !matches!(format, "srt" | "subrip" | "vtt" | "webvtt" | "js" | "json")
        || original.len() > MAX_SUBTITLE_BYTES
    {
        return (original, "unsupported");
    }
    let Ok(external) = std::str::from_utf8(&original) else {
        return (original, "unsupported");
    };
    let (Some(info), Some(probe)) = (&source.stream_info, &source.probe_data) else {
        return (original, "no-reference");
    };
    // Only fill a missing English/Vietnamese language. A full embedded track
    // already in that language needs no external timing work, even if bitmap.
    // Forced/signs-only tracks do not count as full dialogue coverage.
    if let Some(reason) = alignment_skip_reason(language, &probe.media_streams) {
        return (original, reason);
    }
    let Some(source_key) = source_identity(source.id, info, probe.run_time_ticks)
    else {
        return (original, "unavailable");
    };
    let key = cache_key(source_key, &original);
    if let Some(cached) = state
        .ctx
        .store
        .get::<Outcome>(&key)
    {
        return result(&cached, original);
    }
    // Use the actual input stream index. Forced/signs-only and bitmap tracks are
    // deliberately excluded because they cannot establish whole-episode timing.
    let mut references: Vec<_> = probe
        .media_streams
        .iter()
        .filter(|s| {
            matches!(s.type_, Some(api::MediaStreamType::Subtitle))
                && !s.is_external
                && !s.is_forced
                && text_codec(
                    s.codec
                        .as_deref(),
                )
        })
        .collect();
    references.sort_by_key(|s| {
        let same_language = language
            .and_then(super::lang_to_two_letter)
            .zip(
                s.language
                    .as_deref()
                    .and_then(super::lang_to_two_letter),
            )
            .is_some_and(|(a, b)| a == b);
        (!same_language, s.index)
    });
    let references: Vec<_> = references
        .into_iter()
        .take(2)
        .map(|s| {
            (
                s.index,
                s.codec
                    .clone(),
            )
        })
        .collect();
    if references.is_empty() {
        return (original, "no-reference");
    }
    let Ok(permit) = JOB_SLOT.try_acquire() else {
        return (original, "busy");
    };
    // Re-check after taking the global slot to avoid duplicate jobs.
    if let Some(cached) = state
        .ctx
        .store
        .get::<Outcome>(&key)
    {
        return result(&cached, original);
    }
    let input = info
        .descriptor
        .server_input(source.id, config.port);
    let external = external.to_owned();
    let state = state.clone();
    state
        .ctx
        .store
        .save(key.clone(), Outcome::Pending, Duration::from_secs(900));
    tokio::spawn(async move {
        let _permit = permit;
        let outcome = tokio::time::timeout(Duration::from_secs(720), async {
            let token = tokio::fs::read_to_string(token_file)
                .await
                .ok()?;
            if token
                .trim()
                .is_empty()
            {
                return None;
            }
            let client = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(300))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok()?;
            let mut rejected = false;
            for (index, codec) in references {
                let Some(reference) =
                    reference_text(&state, &input, source_key, index, codec.as_deref())
                        .await
                else {
                    continue;
                };
                let mut response = match client
                    .post(&endpoint)
                    .bearer_auth(token.trim())
                    .json(
                        &serde_json::json!({"external":external,"reference":reference}),
                    )
                    .send()
                    .await
                {
                    Ok(r)
                        if r.status()
                            .is_success() =>
                    {
                        r
                    }
                    _ => continue,
                };
                let mut body = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .ok()?
                {
                    if body.len() + chunk.len() > MAX_SUBTITLE_BYTES + 16384 {
                        return None;
                    }
                    body.extend_from_slice(&chunk);
                }
                let reply: WorkerReply = serde_json::from_slice(&body).ok()?;
                if reply
                    .report
                    .accepted
                {
                    if let Some(text) = reply
                        .subtitle
                        .filter(|s| {
                            !s.trim()
                                .is_empty()
                                && s.len() <= MAX_SUBTITLE_BYTES
                        })
                    {
                        return Some(Outcome::Ready(text));
                    }
                } else {
                    rejected = true;
                }
            }
            Some(if rejected {
                Outcome::Rejected
            } else {
                Outcome::Unavailable
            })
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(Outcome::Unavailable);
        let ttl = if matches!(outcome, Outcome::Unavailable) {
            Duration::from_secs(600)
        } else {
            CACHE_TTL
        };
        tracing::info!(cache_key = %key, aligned = matches!(outcome, Outcome::Ready(_)),
            "subtitle alignment job finished");
        let weight = match &outcome {
            Outcome::Ready(text) => text.len() as u32 + 256,
            _ => 256,
        };
        state
            .ctx
            .store
            .save_with_weight(key, outcome, weight, ttl);
    });
    (original, "pending")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_missing_english_or_vietnamese_is_aligned() {
        let mut track = api::MediaStream {
            type_: Some(api::MediaStreamType::Subtitle),
            language: Some("eng".into()),
            codec: Some("hdmv_pgs_subtitle".into()),
            ..Default::default()
        };
        assert_eq!(
            alignment_skip_reason(Some("en"), &[track.clone()]),
            Some("embedded-language")
        );
        assert_eq!(alignment_skip_reason(Some("vie"), &[track.clone()]), None);
        track.language = Some("vie".into());
        assert_eq!(
            alignment_skip_reason(Some("vi"), &[track.clone()]),
            Some("embedded-language")
        );
        track.is_forced = true;
        assert_eq!(alignment_skip_reason(Some("vi"), &[track.clone()]), None);
        track.is_forced = false;
        track.is_external = true;
        assert_eq!(alignment_skip_reason(Some("vi"), &[track]), None);
        for lang in [None, Some("es"), Some("zho"), Some("und")] {
            assert_eq!(alignment_skip_reason(lang, &[]), Some("language-skipped"));
        }
        assert_eq!(alignment_skip_reason(Some("eng"), &[]), None);
    }
    #[test]
    fn renewed_urls_reuse_only_the_same_guarded_release() {
        let mut info = crate::stream::StreamInfo {
            descriptor: crate::stream::StreamDescriptor::http(
                "https://cdn.test/old-token/movie.mkv",
            ),
            filename: Some("movie.release-a.mkv".into()),
            size: Some(123456),
            ..Default::default()
        };
        let id = Uuid::new_v5(&Uuid::nil(), b"release-a");
        let original = source_identity(id, &info, Some(1000));
        info.descriptor = crate::stream::StreamDescriptor::http(
            "https://cdn.test/new-token/movie.mkv",
        );
        assert_eq!(original, source_identity(id, &info, Some(1000)));
        assert_ne!(original, source_identity(id, &info, Some(2000)));
        assert_ne!(original, source_identity(Uuid::nil(), &info, Some(1000)));
        info.size = Some(123457);
        assert_ne!(original, source_identity(id, &info, Some(1000)));
        info.size = Some(123456);
        info.filename = Some("movie.release-b.mkv".into());
        assert_ne!(original, source_identity(id, &info, Some(1000)));
    }
    #[test]
    fn unknown_releases_keep_distinct_full_urls() {
        let mut info = crate::stream::StreamInfo::default();
        info.descriptor =
            crate::stream::StreamDescriptor::http("https://cdn.test/play?id=1");
        let a = source_identity(Uuid::nil(), &info, None);
        info.descriptor =
            crate::stream::StreamDescriptor::http("https://cdn.test/play?id=2");
        assert_ne!(a, source_identity(Uuid::nil(), &info, None));
    }
    #[test]
    fn header_map_order_does_not_change_identity() {
        let mut keys = std::collections::HashSet::new();
        for _ in 0..20 {
            let info = crate::stream::StreamInfo {
                descriptor: crate::stream::StreamDescriptor::Http {
                    url: "https://cdn.test/play".into(),
                    request_headers: [
                        ("Referer".into(), "https://site.test".into()),
                        ("User-Agent".into(), "test".into()),
                    ]
                    .into_iter()
                    .collect(),
                    response_headers: Default::default(),
                },
                ..Default::default()
            };
            keys.insert(source_identity(Uuid::nil(), &info, None));
        }
        assert_eq!(keys.len(), 1);
    }
    #[test]
    fn corrections_are_isolated_by_source_and_content() {
        let a = Uuid::new_v5(&Uuid::nil(), b"release-a");
        let b = Uuid::new_v5(&Uuid::nil(), b"release-b");
        assert_ne!(cache_key(a, b"subtitle"), cache_key(b, b"subtitle"));
        assert_ne!(cache_key(a, b"subtitle"), cache_key(a, b"new subtitle"));
    }
    #[test]
    fn failures_preserve_original_bytes() {
        for status in [Outcome::Pending, Outcome::Rejected, Outcome::Unavailable] {
            let original = Bytes::from_static(b"WEBVTT\noriginal\r\n");
            assert_eq!(result(&status, original.clone()).0, original);
        }
    }
    #[test]
    fn bitmap_subtitles_are_not_timing_references() {
        assert!(!text_codec(Some("hdmv_pgs_subtitle")));
        assert!(!text_codec(Some("dvd_subtitle")));
        assert!(text_codec(Some("ASS")));
        assert!(text_codec(Some("subrip")));
    }
}
