//! Optional subtitle-only worker integration. The video delivery path is untouched.
use std::time::Duration;

use anyhow::anyhow;
use axum::body::Bytes;
use serde::Deserialize;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{AppState, api, db};

const MAX_SUBTITLE_BYTES: usize = 2_000_000;
const CACHE_TTL: Duration = super::GOOD_TRACK_CACHE_TTL;
static ALIGNMENT_CACHE_LAST_PRUNE: std::sync::LazyLock<
    std::sync::Mutex<Option<std::time::Instant>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));
static JOB_SLOT: Semaphore = Semaphore::const_new(1);

#[derive(Clone)]
enum Outcome {
    Pending,
    Ready(String, bool),
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
    #[serde(default)]
    timing_changed: bool,
}

#[derive(serde::Serialize, Deserialize)]
struct PersistedAlignment {
    created_at_unix: u64,
    timing_changed: bool,
    subtitle: String,
}

fn persisted_result_path(
    data_dir: &std::path::Path,
    key: &str,
) -> Option<std::path::PathBuf> {
    let id = key.strip_prefix("subtitle-alignment-v4:")?;
    if id.len() != 36
        || !id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'-')
    {
        return None;
    }
    Some(
        data_dir
            .join("subtitle-alignment-results")
            .join(format!("{id}.json")),
    )
}

async fn load_persisted_alignment(
    data_dir: &std::path::Path,
    key: &str,
) -> Option<(Outcome, Duration)> {
    let path = persisted_result_path(data_dir, key)?;
    let should_prune = if let Ok(mut last_prune) = ALIGNMENT_CACHE_LAST_PRUNE.lock() {
        let now = std::time::Instant::now();
        let due = last_prune.is_none_or(|last| {
            now.duration_since(last) >= Duration::from_secs(6 * 3600)
        });
        if due {
            *last_prune = Some(now);
        }
        due
    } else {
        false
    };
    if should_prune {
        super::prune_persisted_subtitle_caches(data_dir).await;
    }
    let bytes = tokio::fs::read(&path)
        .await
        .ok()?;
    let cached: PersistedAlignment = serde_json::from_slice(&bytes).ok()?;
    if !super::gate::valid_subtitle(
        cached
            .subtitle
            .as_bytes(),
    ) || cached
        .subtitle
        .len()
        > MAX_SUBTITLE_BYTES
    {
        let _ = tokio::fs::remove_file(path).await;
        return None;
    }
    let age = std::time::SystemTime::now()
        .duration_since(
            std::time::UNIX_EPOCH + Duration::from_secs(cached.created_at_unix),
        )
        .unwrap_or_default();
    if age >= CACHE_TTL {
        let _ = tokio::fs::remove_file(path).await;
        return None;
    }
    Some((
        Outcome::Ready(cached.subtitle, cached.timing_changed),
        CACHE_TTL - age,
    ))
}

async fn persist_alignment(
    data_dir: &std::path::Path,
    key: &str,
    subtitle: &str,
    timing_changed: bool,
) -> anyhow::Result<()> {
    if !super::gate::valid_subtitle(subtitle.as_bytes())
        || subtitle.len() > MAX_SUBTITLE_BYTES
    {
        anyhow::bail!("refusing to persist empty or oversized aligned subtitle");
    }
    let path = persisted_result_path(data_dir, key)
        .ok_or_else(|| anyhow!("invalid subtitle alignment cache key"))?;
    let dir = path
        .parent()
        .expect("alignment result path has parent");
    tokio::fs::create_dir_all(dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let body = serde_json::to_vec(&PersistedAlignment {
        created_at_unix: now,
        timing_changed,
        subtitle: subtitle.to_owned(),
    })?;
    let staging = dir.join(format!(".{}.{}.tmp", Uuid::new_v4(), now));
    tokio::fs::write(&staging, body).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o600))
            .await?;
    }
    tokio::fs::rename(&staging, &path).await?;
    super::prune_persisted_subtitle_caches(data_dir).await;
    Ok(())
}

pub(super) fn text_codec(codec: Option<&str>) -> bool {
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

pub(super) fn alignment_skip_reason(language: Option<&str>) -> Option<&'static str> {
    let language = language.and_then(super::lang_to_two_letter);
    if !matches!(language.as_deref(), Some("en" | "vi")) {
        return Some("language-skipped");
    }
    None
}

pub(crate) fn source_identity(
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
        Outcome::Ready(text, changed) => (
            Bytes::copy_from_slice(text.as_bytes()),
            if *changed { "aligned" } else { "unchanged" },
        ),
        Outcome::Pending => (original, "pending"),
        Outcome::Rejected => (original, "rejected"),
        Outcome::Unavailable => (original, "unavailable"),
    }
}

// Join an in-flight job without returning the uncorrected track immediately.
// Polling avoids a lost wakeup between a cache lookup and waiter registration.
async fn await_outcome(
    mut read: impl FnMut() -> Option<Outcome>,
    deadline: tokio::time::Instant,
) -> Option<Outcome> {
    tokio::time::timeout_at(deadline, async {
        loop {
            if let Some(outcome) = read() {
                if !matches!(outcome, Outcome::Pending) {
                    return Some(outcome);
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .ok()
    .flatten()
}

async fn awaited_result(
    state: &AppState,
    key: &str,
    original: Bytes,
    deadline: tokio::time::Instant,
) -> (Bytes, &'static str) {
    match await_outcome(
        || {
            state
                .ctx
                .store
                .get::<Outcome>(key)
                .map(|outcome| (*outcome).clone())
        },
        deadline,
    )
    .await
    {
        Some(outcome) => result(&outcome, original),
        None => (original, "wait-timeout"),
    }
}

async fn load_reference_text(
    data_dir: &std::path::Path,
    source: Uuid,
    index: i64,
) -> Option<String> {
    let path = data_dir
        .join("subtitle-alignment-reference")
        .join(format!("{source}_{index}.srt"));
    let metadata = tokio::fs::metadata(&path)
        .await
        .ok()?;
    if metadata.len() > MAX_SUBTITLE_BYTES as u64
        || metadata
            .modified()
            .ok()?
            .elapsed()
            .unwrap_or_default()
            > Duration::from_secs(7 * 86400)
    {
        return None;
    }
    let text = tokio::fs::read_to_string(&path)
        .await
        .ok()?;
    super::gate::valid_subtitle(text.as_bytes()).then_some(text)
}

/// Reuse text already extracted by background alignment. This never starts an
/// extraction or reads video, so it also works when remote extraction is off.
pub(super) async fn cached_embedded_subtitle(
    state: &AppState,
    source: &db::Media,
    item: Uuid,
    index: i64,
) -> Option<Bytes> {
    cached_embedded_subtitle_before(state, source, item, index, None).await
}

pub(super) async fn cached_embedded_subtitle_before(
    state: &AppState,
    source: &db::Media,
    item: Uuid,
    index: i64,
    network_deadline: Option<tokio::time::Instant>,
) -> Option<Bytes> {
    let hq = if let Some(id) = crate::services::dubmux::hq_id_of(source) {
        db::Media::get_by_id(
            &state
                .ctx
                .db,
            &id,
        )
        .await
        .ok()
        .flatten()
    } else {
        None
    };
    let release = hq
        .as_ref()
        .unwrap_or(source);
    let reference_index = if crate::services::dubmux::hq_url_of(source).is_some() {
        index - crate::services::dubmux::SUBTITLE_INDEX_OFFSET
    } else {
        index
    };
    let probe = release
        .probe_data
        .as_ref()?;
    let stream = probe
        .media_streams
        .iter()
        .find(|s| {
            s.index == if hq.is_some() { reference_index } else { index }
                && s.type_ == Some(api::MediaStreamType::Subtitle)
                && !s.is_external
                && text_codec(
                    s.codec
                        .as_deref(),
                )
        })?;
    let identity = source_identity(
        release.id,
        release
            .stream_info
            .as_ref()?,
        probe.run_time_ticks,
    )?;
    let requirements = super::quality::Requirements {
        duration_ticks: probe.run_time_ticks,
        forced: stream.is_forced,
    };
    let key = super::good_track_key(
        "release",
        item,
        Some(release.id),
        &format!("{identity}:{reference_index}"),
        stream
            .language
            .as_deref(),
        Some("srt"),
    )?;
    if let Some(track) = super::load_usable_track(&state.ctx, &key, requirements).await
    {
        return Some(track.bytes);
    }
    let text = load_reference_text(
        &state
            .ctx
            .config
            .data_dir,
        identity,
        reference_index,
    )
    .await;
    let legacy_text = if text.is_none() && hq.is_some() {
        // Earlier background jobs keyed references by the synthetic dub row.
        // Reuse those only through that row's own guarded identity.
        let legacy_identity = source_identity(
            source.id,
            source
                .stream_info
                .as_ref()?,
            source
                .probe_data
                .as_ref()?
                .run_time_ticks,
        )?;
        load_reference_text(
            &state
                .ctx
                .config
                .data_dir,
            legacy_identity,
            reference_index,
        )
        .await
    } else {
        None
    };
    let text = match text.or(legacy_text) {
        Some(text) => text,
        None => {
            mux_cached_release_text(state, identity, reference_index, network_deadline)
                .await?
        }
    };
    super::quality::check(text.as_bytes(), requirements).ok()?;
    let bytes = Bytes::from(text);
    let _ = super::save_good_track(&state.ctx, &key, &bytes, false).await;
    Some(bytes)
}

/// Only GET the muxer's completed text cache. This endpoint never starts a
/// preparation, resolves a video link or runs ffmpeg. Misses are short-lived.
async fn mux_cached_release_text(
    state: &AppState,
    identity: Uuid,
    index: i64,
    network_deadline: Option<tokio::time::Instant>,
) -> Option<String> {
    if network_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
    {
        return None;
    }
    let base = state
        .ctx
        .config
        .dubmux_subtitle_cache_url
        .as_deref()?;
    let miss_key = format!("release-text-miss:{identity}:{index}");
    if state
        .ctx
        .store
        .get::<bool>(&miss_key)
        .is_some()
    {
        return None;
    }
    let fetch = async {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(500))
            .timeout(Duration::from_secs(2))
            .build()
            .ok()?;
        let mut response = client
            .get(format!(
                "{}/subtitles/{identity}/{index}.srt",
                base.trim_end_matches('/')
            ))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        if response.status() != reqwest::StatusCode::OK
            || response
                .content_length()
                .is_some_and(|n| n > MAX_SUBTITLE_BYTES as u64)
        {
            return None;
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .ok()?
        {
            if body.len() + chunk.len() > MAX_SUBTITLE_BYTES {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        String::from_utf8(body).ok()
    };
    let fetched = if let Some(deadline) = network_deadline {
        tokio::time::timeout_at(deadline, fetch)
            .await
            .ok()
            .flatten()
    } else {
        fetch.await
    };
    if fetched.is_none() {
        state
            .ctx
            .store
            .save(miss_key, true, Duration::from_secs(60));
    }
    fetched
}

async fn reference_text(
    state: &AppState,
    input: &str,
    source: Uuid,
    index: i64,
    codec: Option<&str>,
    allow_extraction: bool,
    read_budget: Option<crate::services::media_budget::ReadBudget>,
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
    if let Some(text) = load_reference_text(
        &state
            .ctx
            .config
            .data_dir,
        source,
        index,
    )
    .await
    {
        return Some(text);
    }
    // Alignment must honor the subtitle route's extraction opt-in. Scanning
    // an uncached remote movie can consume debrid fair-use quota.
    if !allow_extraction {
        return None;
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
        source,
        index,
        api::SubtitleCodec::Srt,
        codec,
        read_budget,
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
    if !super::gate::valid_subtitle(reference.as_bytes()) {
        return None;
    }
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

/// Waits within the configured request budget for completed alignment.
/// Cache identity includes the guarded provider release
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
    // Prepare every advertised English/Vietnamese external track. A matching
    // embedded track can be a useful reference, but it does not guarantee the
    // player can receive it or that the external track is timed to this source.
    if let Some(reason) = alignment_skip_reason(language) {
        return (original, reason);
    }
    let Some(source_key) = source_identity(source.id, info, probe.run_time_ticks)
    else {
        return (original, "unavailable");
    };
    let key = cache_key(source_key, &original);
    if let Some((cached, ttl)) = load_persisted_alignment(&config.data_dir, &key).await
    {
        if let Outcome::Ready(text, _) = &cached
            && (!super::quality::preserves_dialogue(&original, text.as_bytes())
                || super::quality::check(
                    text.as_bytes(),
                    super::quality::Requirements {
                        duration_ticks: probe.run_time_ticks,
                        forced: true,
                    },
                )
                .is_err())
        {
            if let Some(path) = persisted_result_path(&config.data_dir, &key) {
                let _ = tokio::fs::remove_file(path).await;
            }
        } else {
            let weight = match &cached {
                Outcome::Ready(text, _) => text.len() as u32 + 256,
                _ => 256,
            };
            state
                .ctx
                .store
                .save_with_weight(key.clone(), cached.clone(), weight, ttl);
            return result(&cached, original);
        }
    }
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(
            config
                .subtitle_alignment_wait_seconds
                .min(120),
        );
    if let Some(cached) = state
        .ctx
        .store
        .get::<Outcome>(&key)
    {
        if matches!(*cached, Outcome::Pending) {
            return awaited_result(state, &key, original, deadline).await;
        }
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
    let Ok(Ok(permit)) = tokio::time::timeout_at(deadline, JOB_SLOT.acquire()).await
    else {
        return (original, "wait-timeout");
    };
    // Re-check after taking the global slot to avoid duplicate jobs.
    if let Some(cached) = state
        .ctx
        .store
        .get::<Outcome>(&key)
    {
        if matches!(*cached, Outcome::Pending) {
            return awaited_result(state, &key, original, deadline).await;
        }
        return result(&cached, original);
    }
    // PATCH (uduchi2nd): a "[+VN dub]" row lists the HQ release's embedded
    // text tracks (at an index offset) but its own URL is a mux without them;
    // the reference is extracted from the HQ file with the original index.
    let dub_row_hq = crate::services::dubmux::hq_url_of(source);
    let input = dub_row_hq
        .clone()
        .unwrap_or_else(|| {
            info.descriptor
                .server_input(source.id, config.port)
        });
    let references: Vec<_> = references
        .into_iter()
        .map(|(index, codec)| {
            if dub_row_hq.is_some() {
                (
                    index - crate::services::dubmux::SUBTITLE_INDEX_OFFSET,
                    codec,
                )
            } else {
                (index, codec)
            }
        })
        .collect();
    let external = external.to_owned();
    let duration_ticks = probe.run_time_ticks;
    let allow_reference_extraction = source
        .allows_subtitle_extraction(
            &state
                .ctx
                .db,
        )
        .await;
    state
        .ctx
        .store
        .save(key.clone(), Outcome::Pending, Duration::from_secs(900));
    let job_state = state.clone();
    let job_key = key.clone();
    let read_budget = crate::services::media_budget::ReadBudget::for_media(
        &state.ctx, source, "full",
    )
    .await;
    tokio::spawn(async move {
        let state = job_state;
        let key = job_key;
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
                let Some(reference) = reference_text(
                    &state,
                    &input,
                    source_key,
                    index,
                    codec.as_deref(),
                    allow_reference_extraction,
                    Some(read_budget.clone()),
                )
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
                            s.len() <= MAX_SUBTITLE_BYTES
                                && super::quality::preserves_dialogue(
                                    external.as_bytes(),
                                    s.as_bytes(),
                                )
                                && super::quality::check(
                                    s.as_bytes(),
                                    super::quality::Requirements {
                                        duration_ticks,
                                        forced: true,
                                    },
                                )
                                .is_ok()
                        })
                    {
                        return Some(Outcome::Ready(
                            text,
                            reply
                                .report
                                .timing_changed,
                        ));
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
        let ttl = match &outcome {
            Outcome::Ready(..) => CACHE_TTL,
            Outcome::Rejected => Duration::from_secs(300),
            Outcome::Unavailable => Duration::from_secs(600),
            Outcome::Pending => Duration::from_secs(900),
        };
        tracing::info!(cache_key = %key, aligned = matches!(outcome, Outcome::Ready(..)),
            "subtitle alignment job finished");
        let weight = match &outcome {
            Outcome::Ready(text, _) => text.len() as u32 + 256,
            _ => 256,
        };
        if let Outcome::Ready(text, timing_changed) = &outcome {
            if let Err(error) = persist_alignment(
                &state
                    .ctx
                    .config
                    .data_dir,
                &key,
                text,
                *timing_changed,
            )
            .await
            {
                tracing::warn!(error = %error, "failed to persist aligned subtitle result");
            }
        }
        state
            .ctx
            .store
            .save_with_weight(key, outcome, weight, ttl);
    });
    awaited_result(state, &key, original, deadline).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mux_text_cache_is_read_only_guarded_and_shared_with_the_original() {
        use std::sync::{Arc, Mutex};
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let address = listener
            .local_addr()
            .unwrap();
        let body = "1\n00:00:01,000 --> 00:00:02,000\nExact release dialogue\n";
        let router = axum::Router::new().fallback(axum::routing::get(
            move |uri: axum::http::Uri| {
                let slow = uri
                    .path()
                    .ends_with("/99.srt");
                recorded
                    .lock()
                    .unwrap()
                    .push(
                        uri.path()
                            .to_string(),
                    );
                async move {
                    if slow {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                    body
                }
            },
        ));
        let worker = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .unwrap()
        });
        let data = tempfile::tempdir().unwrap();
        let (_server, guard) =
            crate::integration_test::new_test_server_with_config(crate::Config {
                database_url: Some("sqlite::memory:".into()),
                data_dir: data
                    .path()
                    .to_path_buf(),
                torrent_http_port: None,
                torrent_peer_port: None,
                disable_dht: true,
                dubmux_subtitle_cache_url: Some(format!("http://{address}")),
                ..Default::default()
            })
            .await
            .unwrap();
        let ctx = guard
            .0
            .clone();
        assert!(
            ctx.config
                .dubmux_url
                .is_none()
        );
        let state = crate::AppState {
            tasks: crate::tasks::TaskService::new(ctx.clone())
                .await
                .unwrap(),
            ctx,
        };
        let item = Uuid::new_v4();
        let source = db::Media {
            id: Uuid::new_v4(),
            kind: db::MediaKind::Stream,
            stream_info: Some(crate::stream::StreamInfo {
                descriptor: crate::stream::StreamDescriptor::http(
                    "https://never-read.test/movie.mkv",
                ),
                filename: Some("release.mkv".into()),
                size: Some(12345),
                ..Default::default()
            }),
            probe_data: Some(api::MediaSourceInfo {
                run_time_ticks: Some(60 * 10_000_000),
                media_streams: vec![api::MediaStream {
                    index: 23,
                    type_: Some(api::MediaStreamType::Subtitle),
                    codec: Some("ass".into()),
                    language: Some("vie".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        db::Media::upsert(
            &state
                .ctx
                .db,
            &[source.clone()],
        )
        .await
        .unwrap();
        let mut dub = source.clone();
        dub.id = Uuid::new_v4();
        dub.stream_info
            .as_mut()
            .unwrap()
            .descriptor = crate::stream::StreamDescriptor::http(format!(
            "https://mux.test/mux/dub/{}/file.ts?video=https%3A%2F%2Fnever-read.test%2Fmovie.mkv",
            source
                .id
                .simple()
        ));
        dub.probe_data
            .as_mut()
            .unwrap()
            .media_streams[0]
            .index = 123;
        assert!(
            !source
                .allows_subtitle_extraction(
                    &state
                        .ctx
                        .db
                )
                .await
        );
        assert_eq!(
            cached_embedded_subtitle(&state, &dub, item, 123)
                .await
                .unwrap()
                .as_ref(),
            body.as_bytes()
        );
        let identity = source_identity(
            source.id,
            source
                .stream_info
                .as_ref()
                .unwrap(),
            Some(60 * 10_000_000),
        )
        .unwrap();
        assert_eq!(
            *requests
                .lock()
                .unwrap(),
            vec![format!("/subtitles/{identity}/23.srt")]
        );
        let expired = tokio::time::Instant::now() - Duration::from_secs(1);
        assert_eq!(
            cached_embedded_subtitle_before(&state, &source, item, 23, Some(expired))
                .await
                .unwrap()
                .as_ref(),
            body.as_bytes()
        );
        let start = tokio::time::Instant::now();
        assert!(
            mux_cached_release_text(
                &state,
                identity,
                99,
                Some(start + Duration::from_millis(50))
            )
            .await
            .is_none()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        worker.abort();
        // Once fetched, original and dub share the same monthly text cache.
        assert_eq!(
            cached_embedded_subtitle(&state, &source, item, 23)
                .await
                .unwrap()
                .as_ref(),
            body.as_bytes()
        );
        let mut changed = source.clone();
        changed
            .stream_info
            .as_mut()
            .unwrap()
            .size = Some(23456);
        assert!(
            cached_embedded_subtitle(&state, &changed, item, 23)
                .await
                .is_none()
        );
        let missing_identity = source_identity(
            changed.id,
            changed
                .stream_info
                .as_ref()
                .unwrap(),
            Some(60 * 10_000_000),
        )
        .unwrap();
        assert!(
            state
                .ctx
                .store
                .get::<bool>(&format!("release-text-miss:{missing_identity}:23"))
                .is_some()
        );
        assert!(
            cached_embedded_subtitle(&state, &changed, item, 23)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn cached_release_text_survives_reference_eviction_but_not_a_changed_file() {
        let (_server, guard, _token) =
            crate::integration_test::authenticated_server().await;
        let ctx = guard
            .0
            .clone();
        let state = crate::AppState {
            tasks: crate::tasks::TaskService::new(ctx.clone())
                .await
                .unwrap(),
            ctx,
        };
        let item = Uuid::new_v4();
        let mut source = db::Media {
            id: Uuid::new_v4(),
            kind: db::MediaKind::Stream,
            stream_info: Some(crate::stream::StreamInfo {
                descriptor: crate::stream::StreamDescriptor::http(
                    "https://unreachable.test/video.mkv",
                ),
                filename: Some("release.mkv".into()),
                size: Some(12345),
                ..Default::default()
            }),
            probe_data: Some(api::MediaSourceInfo {
                run_time_ticks: Some(60 * 10_000_000),
                media_streams: vec![api::MediaStream {
                    index: 23,
                    type_: Some(api::MediaStreamType::Subtitle),
                    codec: Some("ass".into()),
                    language: Some("vie".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let identity = source_identity(
            source.id,
            source
                .stream_info
                .as_ref()
                .unwrap(),
            Some(60 * 10_000_000),
        )
        .unwrap();
        let dir = state
            .ctx
            .config
            .data_dir
            .join("subtitle-alignment-reference");
        tokio::fs::create_dir_all(&dir)
            .await
            .unwrap();
        let path = dir.join(format!("{identity}_23.srt"));
        let text = b"1\n00:00:01,000 --> 00:00:02,000\nActual release text\n";
        tokio::fs::write(&path, text)
            .await
            .unwrap();
        assert!(
            !source
                .allows_subtitle_extraction(
                    &state
                        .ctx
                        .db
                )
                .await
        );
        assert_eq!(
            reference_text(
                &state,
                "https://unreachable.test/video.mkv",
                identity,
                23,
                Some("srt"),
                false,
                None
            )
            .await
            .unwrap()
            .as_bytes(),
            text
        );
        assert!(
            reference_text(
                &state,
                "https://unreachable.test/video.mkv",
                identity,
                22,
                Some("srt"),
                false,
                None
            )
            .await
            .is_none()
        );
        assert_eq!(
            cached_embedded_subtitle(&state, &source, item, 23)
                .await
                .unwrap()
                .as_ref(),
            text
        );
        tokio::fs::remove_file(&path)
            .await
            .unwrap();
        assert!(
            cached_embedded_subtitle(&state, &source, item, 23)
                .await
                .is_some()
        );
        assert!(
            cached_embedded_subtitle(&state, &source, item, 22)
                .await
                .is_none()
        );
        db::Media::upsert(
            &state
                .ctx
                .db,
            &[source.clone()],
        )
        .await
        .unwrap();
        let mut dub = source.clone();
        dub.id = Uuid::new_v4();
        dub.stream_info
            .as_mut()
            .unwrap()
            .descriptor = crate::stream::StreamDescriptor::http(format!(
            "https://mux.test/mux/dub/{}/file.ts?video=https%3A%2F%2Funreachable.test%2Fvideo.mkv",
            source
                .id
                .simple()
        ));
        dub.probe_data
            .as_mut()
            .unwrap()
            .media_streams[0]
            .index += crate::services::dubmux::SUBTITLE_INDEX_OFFSET;
        assert_eq!(
            cached_embedded_subtitle(&state, &dub, item, 123)
                .await
                .unwrap()
                .as_ref(),
            text
        );
        let mut unavailable = dub
            .probe_data
            .as_ref()
            .unwrap()
            .media_streams[0]
            .clone();
        unavailable.index = 124;
        unavailable.language = Some("eng".into());
        dub.probe_data
            .as_mut()
            .unwrap()
            .media_streams
            .push(unavailable);
        db::Media::upsert(
            &state
                .ctx
                .db,
            &[dub.clone()],
        )
        .await
        .unwrap();
        let mut view = api::MediaSourceInfo::from(dub.clone());
        view.id = dub.id;
        view.path = dub
            .stream_info
            .as_ref()
            .unwrap()
            .descriptor
            .as_http_url()
            .map(str::to_owned);
        view.media_streams
            .clear();
        view.default_subtitle_stream_index = Some(124);
        super::super::retain_servable_dub_subtitles(
            &state,
            std::slice::from_mut(&mut view),
            item,
            "test-player",
            &["vi".into(), "en".into()],
        )
        .await;
        assert!(
            view.media_streams
                .iter()
                .any(|s| s.index == 123)
        );
        assert!(
            !view
                .media_streams
                .iter()
                .any(|s| s.index == 124)
        );
        assert_eq!(view.default_subtitle_stream_index, None);
        super::super::present_dub_row_embedded_subtitles(
            std::slice::from_mut(&mut view),
            item,
            "test-token",
            &["vi".into()],
        );
        let presented = view
            .media_streams
            .iter()
            .find(|s| s.index == 123)
            .unwrap();
        assert_eq!(
            presented
                .codec
                .as_deref(),
            Some("vtt")
        );
        assert!(presented.is_external);
        assert_eq!(
            dub.probe_data
                .as_ref()
                .unwrap()
                .media_streams[0]
                .codec
                .as_deref(),
            Some("ass")
        );
        source
            .stream_info
            .as_mut()
            .unwrap()
            .size = Some(23456);
        db::Media::upsert(
            &state
                .ctx
                .db,
            &[source.clone()],
        )
        .await
        .unwrap();
        assert!(
            cached_embedded_subtitle(&state, &source, item, 23)
                .await
                .is_none()
        );
        assert!(
            cached_embedded_subtitle(&state, &dub, item, 123)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn discards_preexisting_alignment_with_no_readable_cues() {
        let dir = tempfile::tempdir().unwrap();
        let key = cache_key(Uuid::new_v4(), b"original");
        let path = persisted_result_path(dir.path(), &key).unwrap();
        tokio::fs::create_dir_all(
            path.parent()
                .unwrap(),
        )
        .await
        .unwrap();
        let created_at_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let record = PersistedAlignment {
            created_at_unix,
            timing_changed: true,
            subtitle: "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n<i> </i>\n".into(),
        };
        tokio::fs::write(&path, serde_json::to_vec(&record).unwrap())
            .await
            .unwrap();
        assert!(
            load_persisted_alignment(dir.path(), &key)
                .await
                .is_none()
        );
        assert!(!path.exists());
    }
    #[tokio::test]
    async fn first_request_waits_for_ready_instead_of_pending() {
        let start = tokio::time::Instant::now();
        let outcome = await_outcome(
            || {
                Some(if start.elapsed() < Duration::from_millis(50) {
                    Outcome::Pending
                } else {
                    Outcome::Ready("corrected".into(), true)
                })
            },
            start + Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(
            result(&outcome, Bytes::from_static(b"original")),
            (Bytes::from_static(b"corrected"), "aligned")
        );
    }
    #[tokio::test]
    async fn pending_wait_has_a_deadline() {
        let outcome = await_outcome(
            || Some(Outcome::Pending),
            tokio::time::Instant::now() + Duration::from_millis(30),
        )
        .await;
        assert!(outcome.is_none());
    }
    #[tokio::test]
    async fn failed_job_finishes_wait_without_changing_original() {
        let outcome = await_outcome(
            || Some(Outcome::Unavailable),
            tokio::time::Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(
            result(&outcome, Bytes::from_static(b"original")),
            (Bytes::from_static(b"original"), "unavailable")
        );
    }
    #[test]
    fn only_english_or_vietnamese_is_aligned() {
        assert_eq!(alignment_skip_reason(Some("en")), None);
        assert_eq!(alignment_skip_reason(Some("vie")), None);
        assert_eq!(alignment_skip_reason(Some("vi")), None);
        for lang in [None, Some("es"), Some("zho"), Some("und")] {
            assert_eq!(alignment_skip_reason(lang), Some("language-skipped"));
        }
        assert_eq!(alignment_skip_reason(Some("eng")), None);
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
