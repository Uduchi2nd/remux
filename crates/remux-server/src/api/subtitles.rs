use anyhow::anyhow;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    response::IntoResponse,
};
use axum_anyhow::ApiResult as Result;
use futures::future::{BoxFuture, FutureExt, Shared};
use http::{Response, StatusCode};
use remux_macros::get;
use std::time::Duration;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

mod alignment;
pub(crate) mod gate;

#[derive(Default, serde::Deserialize)]
pub struct SubtitleOptions {
    /// Additive escape hatch; default Jellyfin URLs need no client changes.
    #[serde(default)]
    remux_original: bool,
}

pub(super) const GOOD_TRACK_CACHE_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
const MAX_PERSISTED_GOOD_TRACKS: usize = 512;
pub(super) const MAX_PERSISTED_ALIGNMENT_FILES: usize = 512;
const MAX_PERSISTED_SUBTITLE_CACHE_BYTES: u64 = 200 * 1024 * 1024;
const MAX_GOOD_TRACK_BYTES: usize = 2_000_000;
static PERSISTED_SUBTITLE_CACHE_PRUNE_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

#[derive(Clone, Copy, PartialEq, Eq)]
enum PersistedSubtitleCacheKind {
    GoodTrack,
    Alignment,
}

struct PersistedSubtitleCacheFile {
    modified: std::time::SystemTime,
    path: std::path::PathBuf,
    bytes: u64,
    kind: PersistedSubtitleCacheKind,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedGoodTrack {
    saved_at_unix: u64,
    auto_synced: bool,
    text: String,
}

struct GoodTrack {
    bytes: axum::body::Bytes,
    auto_synced: bool,
}

fn good_track_key(
    scope: &str,
    item_id: Uuid,
    source_id: Option<Uuid>,
    subtitle_id: &str,
    language: Option<&str>,
    format: Option<&str>,
) -> Option<String> {
    let subtitle_id = subtitle_id.trim();
    if subtitle_id.is_empty() {
        return None;
    }
    let mut identity = serde_json::json!({
        "scope": scope,
        "item": item_id,
        "source": source_id,
        "track": subtitle_id,
        "language": language.map(str::trim).map(str::to_ascii_lowercase),
        "format": format.map(str::to_ascii_lowercase),
    });
    identity.sort_all_objects();
    Some(format!(
        "subtitle-good-track:{}",
        Uuid::new_v5(&Uuid::nil(), &serde_json::to_vec(&identity).ok()?)
    ))
}

fn good_track_path(
    data_dir: &std::path::Path,
    key: &str,
) -> Option<std::path::PathBuf> {
    let id = key.strip_prefix("subtitle-good-track:")?;
    if id.len() != 36
        || !id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'-')
    {
        return None;
    }
    Some(
        data_dir
            .join("subtitle-good-tracks")
            .join(format!("{id}.json")),
    )
}

async fn load_good_track(ctx: &crate::AppContext, key: &str) -> Option<GoodTrack> {
    let path = good_track_path(
        &ctx.config
            .data_dir,
        key,
    )?;
    let body = tokio::fs::read(&path)
        .await
        .ok()?;
    let cached: PersistedGoodTrack = match serde_json::from_slice(&body) {
        Ok(cached) => cached,
        Err(_) => {
            let _ = tokio::fs::remove_file(path).await;
            return None;
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let age = now.saturating_sub(cached.saved_at_unix);
    if age >= GOOD_TRACK_CACHE_TTL.as_secs()
        || cached
            .text
            .len()
            > MAX_GOOD_TRACK_BYTES
        || !gate::valid_subtitle(
            cached
                .text
                .as_bytes(),
        )
    {
        let _ = tokio::fs::remove_file(path).await;
        return None;
    }
    Some(GoodTrack {
        bytes: axum::body::Bytes::from(cached.text),
        auto_synced: cached.auto_synced,
    })
}

async fn save_good_track(
    ctx: &crate::AppContext,
    key: &str,
    bytes: &axum::body::Bytes,
    auto_synced: bool,
) -> anyhow::Result<()> {
    if bytes.len() > MAX_GOOD_TRACK_BYTES || !gate::valid_subtitle(bytes) {
        anyhow::bail!("refusing to persist an invalid subtitle track");
    }
    let path = good_track_path(
        &ctx.config
            .data_dir,
        key,
    )
    .ok_or_else(|| anyhow!("invalid good subtitle cache key"))?;
    let dir = path
        .parent()
        .expect("subtitle cache path has parent");
    tokio::fs::create_dir_all(dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let saved_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let record = PersistedGoodTrack {
        saved_at_unix,
        auto_synced,
        text: String::from_utf8(bytes.to_vec())?,
    };
    let staging = dir.join(format!(".{}.{}.tmp", Uuid::new_v4(), saved_at_unix));
    tokio::fs::write(&staging, serde_json::to_vec(&record)?).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o600))
            .await?;
    }
    tokio::fs::rename(&staging, &path).await?;
    prune_persisted_subtitle_caches(
        &ctx.config
            .data_dir,
    )
    .await;
    Ok(())
}

pub(super) async fn prune_persisted_subtitle_caches(data_dir: &std::path::Path) {
    let _guard = PERSISTED_SUBTITLE_CACHE_PRUNE_LOCK
        .lock()
        .await;
    let mut files = Vec::new();
    for (directory, kind) in [
        (
            "subtitle-good-tracks",
            PersistedSubtitleCacheKind::GoodTrack,
        ),
        (
            "subtitle-alignment-results",
            PersistedSubtitleCacheKind::Alignment,
        ),
    ] {
        let Ok(mut entries) = tokio::fs::read_dir(data_dir.join(directory)).await
        else {
            continue;
        };
        while let Ok(Some(entry)) = entries
            .next_entry()
            .await
        {
            let Ok(metadata) = entry
                .metadata()
                .await
            else {
                continue;
            };
            let path = entry.path();
            if !metadata.is_file()
                || path
                    .extension()
                    .is_none_or(|extension| extension != "json")
            {
                continue;
            }
            let modified = metadata
                .modified()
                .unwrap_or(std::time::UNIX_EPOCH);
            if modified
                .elapsed()
                .unwrap_or_default()
                >= GOOD_TRACK_CACHE_TTL
            {
                let _ = tokio::fs::remove_file(path).await;
            } else {
                files.push(PersistedSubtitleCacheFile {
                    modified,
                    path,
                    bytes: metadata.len(),
                    kind,
                });
            }
        }
    }

    let files_to_remove = select_cache_files_to_prune(
        files,
        MAX_PERSISTED_SUBTITLE_CACHE_BYTES,
        MAX_PERSISTED_GOOD_TRACKS,
        MAX_PERSISTED_ALIGNMENT_FILES,
    );
    for file in files_to_remove {
        let _ = tokio::fs::remove_file(file.path).await;
    }
}

fn select_cache_files_to_prune(
    mut files: Vec<PersistedSubtitleCacheFile>,
    max_bytes: u64,
    max_good_tracks: usize,
    max_alignment_files: usize,
) -> Vec<PersistedSubtitleCacheFile> {
    files.sort_by_key(|file| file.modified);
    let mut total_bytes: u64 = files
        .iter()
        .map(|file| file.bytes)
        .sum();
    let mut good_track_count = files
        .iter()
        .filter(|file| file.kind == PersistedSubtitleCacheKind::GoodTrack)
        .count();
    let mut alignment_count = files
        .iter()
        .filter(|file| file.kind == PersistedSubtitleCacheKind::Alignment)
        .count();
    let mut pruned = Vec::new();

    loop {
        let index = if total_bytes > max_bytes {
            (!files.is_empty()).then_some(0)
        } else if good_track_count > max_good_tracks {
            files
                .iter()
                .position(|file| file.kind == PersistedSubtitleCacheKind::GoodTrack)
        } else if alignment_count > max_alignment_files {
            files
                .iter()
                .position(|file| file.kind == PersistedSubtitleCacheKind::Alignment)
        } else {
            None
        };
        let Some(index) = index else {
            break;
        };
        let file = files.remove(index);
        total_bytes = total_bytes.saturating_sub(file.bytes);
        match file.kind {
            PersistedSubtitleCacheKind::GoodTrack => good_track_count -= 1,
            PersistedSubtitleCacheKind::Alignment => alignment_count -= 1,
        }
        pruned.push(file);
    }
    pruned
}

use crate::{
    AppState, IntoApiError, OptionExt, ResultExt, api, common::HideConsole, db,
    db::auth, playback::engine::ffmpeg_http_input_args,
};

fn ffmpeg_bin() -> String {
    std::env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into())
}

/// The cache storage codec for a requested text subtitle format: ASS/SSA requests
/// get a native ASS cache (styled dialogue preserved), everything else the SRT
/// cache that VTT/JSON conversions are built on.
fn subtitle_cache_codec(output_format: &str) -> api::SubtitleCodec {
    if matches!(
        output_format.parse::<api::SubtitleCodec>(),
        Ok(api::SubtitleCodec::Ass)
    ) {
        api::SubtitleCodec::Ass
    } else {
        api::SubtitleCodec::Srt
    }
}

/// ffmpeg `-c:s` for the cache: stream-copy native ASS/SSA when the cache wants
/// ASS, otherwise re-encode to the cache codec.
fn subtitle_cache_ffmpeg_codec(
    cache_codec: &api::SubtitleCodec,
    source_codec: Option<&str>,
) -> String {
    if *cache_codec == api::SubtitleCodec::Ass
        && matches!(
            source_codec.and_then(|c| c
                .parse::<api::SubtitleCodec>()
                .ok()),
            Some(api::SubtitleCodec::Ass)
        )
    {
        "copy".to_string()
    } else {
        cache_codec.to_string()
    }
}

/// Cache key includes `media_source_id`, not just `item_id`: an item can
/// have multiple media sources (different releases/qualities), each
/// potentially numbering its embedded subtitle streams differently. Keying
/// on `item_id` alone let switching sources serve a *different* source's
/// cached subtitle at the same stream index — wrong language, wrong sync,
/// silently.
fn subtitle_cache_path(
    data_dir: &std::path::Path,
    item_id: Uuid,
    media_source_id: Uuid,
    stream_index: i64,
    cache_codec: &api::SubtitleCodec,
) -> std::path::PathBuf {
    data_dir
        .join("subtitle-cache")
        .join(format!(
            "{item_id}_{media_source_id}_{stream_index}.{}",
            cache_codec.to_string()
        ))
}

/// One subtitle output to pull out of a batch extraction — everything
/// `extract_subtitles_to_cache` needs to place it at its own cache path.
/// A single stream can produce more than one of these (see
/// `extractable_subtitles`: a native-ASS stream gets both an ASS and an SRT
/// output), so `stream_index`/`map_spec` are not unique across the list.
#[derive(Debug)]
struct ExtractableSubtitle {
    stream_index: i64,
    map_spec: String,
    cache_codec: api::SubtitleCodec,
    ffmpeg_codec: String,
    /// Whether ffmpeg is known to convert this stream's codec. An unknown
    /// one aborts a whole batch, so it's only ever extracted on its own.
    known_codec: bool,
}

/// How long an extraction may run before it's killed and its partial output
/// discarded. Only local files are ever extracted, so this is generous.
const SUBTITLE_EXTRACTION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(600);

/// A batch extraction's outcome, shared across every caller waiting on the
/// same in-flight run. `anyhow::Error` isn't `Clone` (required for `Shared`,
/// since every waiter gets a copy of the final output), hence `String`.
type ExtractionOutcome = std::result::Result<(), String>;

/// Process-wide single-flight registry, keyed by (item_id, media_source_id).
/// A source is only ever read once concurrently — every subtitle stream on
/// it is extracted together in one ffmpeg pass (see
/// `extract_subtitles_to_cache`), so this only needs to be per-source, not
/// per-stream.
///
/// Holds the in-flight extraction's `Shared` future rather than a plain
/// mutex: the actual work runs in its own `tokio::spawn`'d task (see
/// `ensure_subtitle_cached`), independent of whichever HTTP request
/// triggered it. If that request's connection drops mid-extraction, only
/// its own `.await` on the shared future is dropped — the spawned task
/// keeps polling ffmpeg to completion regardless, so a client that gives up
/// and retries doesn't restart a 30-minute remote extraction from zero
/// every time (previously `cmd.kill_on_drop(true)` killed ffmpeg the moment
/// the triggering request's future was dropped, since everything ran
/// inline in that request's own task).
///
/// Entries remove themselves once the extraction completes (success or
/// failure), so a later request retries fresh rather than replaying a
/// stale error forever.
static SUBTITLE_EXTRACTION_INFLIGHT: std::sync::LazyLock<
    tokio::sync::Mutex<
        std::collections::HashMap<
            (Uuid, Uuid, Option<i64>),
            Shared<BoxFuture<'static, ExtractionOutcome>>,
        >,
    >,
> = std::sync::LazyLock::new(|| {
    tokio::sync::Mutex::new(std::collections::HashMap::new())
});

/// Starts extracting `stream_index` (and every other extractable track on the
/// source, in the same pass) in the background, so the subtitle request that
/// follows PlaybackInfo finds the cache written or joins the running pass.
pub(crate) fn prefetch_embedded_subtitles(
    data_dir: std::path::PathBuf,
    input_url: String,
    probe: api::MediaSourceInfo,
    item_id: Uuid,
    media_source_id: Uuid,
    stream_index: i64,
    delivery_format: &str,
) {
    let cache_codec = subtitle_cache_codec(delivery_format);
    tokio::spawn(async move {
        if let Err(e) = ensure_subtitle_cached(
            &data_dir,
            &input_url,
            &probe,
            item_id,
            media_source_id,
            stream_index,
            cache_codec,
        )
        .await
        {
            debug!(%item_id, stream_index, "subtitle prefetch failed: {e}");
        }
    });
}

/// Builds the list of every extractable (embedded, non-external, text)
/// subtitle stream on `probe`, in container order, with the ffmpeg `-map`
/// spec and cache codec(s) each one needs. Image/bitmap subtitles (PGS,
/// DVD/DVB sub) are never included — ffmpeg can't convert them to a text
/// codec, and one such stream in the same batch as text streams would fail
/// the whole ffmpeg invocation (they're served on-the-fly instead, see the
/// `is_binary` branch in `subtitles_stream_inner`).
///
/// Mirrors Jellyfin's own rule: a native ASS/SSA stream stays ASS
/// (stream-copied) — decided per stream from its *own* codec, not whatever
/// format a client happens to be asking for right now, since this batch also
/// caches every other subtitle track on the source for later requests. A
/// native-ASS stream additionally gets a plain SRT cache alongside its ASS
/// one: VTT/SRT/JSON requests are always served from the SRT cache (see
/// `subtitle_cache_codec`), and without it those requests would 404 against
/// a stream that only ever produced a `.ass` file.
///
/// `requested` adds one extra output for a stream when the plan above doesn't
/// already cover it — an `.ass` request for an SRT track is converted, like
/// Jellyfin does, rather than left with no file to serve.
fn extractable_subtitles(
    probe: &api::MediaSourceInfo,
    requested: Option<(i64, &api::SubtitleCodec)>,
) -> Vec<ExtractableSubtitle> {
    // ffmpeg's `0:s:N` stream specifier counts every embedded subtitle
    // stream in the container, image ones included — so the ordinal has to
    // be computed over *all* of them, in container order, not just the text
    // ones we can actually extract. Skipping image streams from this list
    // entirely would shift every later text stream's ordinal left, mapping
    // it onto the wrong track whenever an image subtitle precedes it.
    let mut embedded: Vec<&api::MediaStream> = probe
        .media_streams
        .iter()
        .filter(|s| {
            matches!(s.type_, Some(api::MediaStreamType::Subtitle)) && !s.is_external
        })
        .collect();
    embedded.sort_by_key(|s| s.index);
    embedded
        .into_iter()
        .enumerate()
        // Image subtitle: not extractable to text (see the module doc
        // comment), but its ordinal slot is still accounted for above.
        .filter(|(_, stream)| stream.is_text_subtitle_stream())
        .flat_map(|(ordinal, stream)| {
            let source_codec = stream
                .codec
                .as_deref();
            let known_codec = is_known_text_codec(stream);
            let output = |cache_codec: api::SubtitleCodec| ExtractableSubtitle {
                stream_index: stream.index,
                map_spec: format!("0:s:{ordinal}"),
                ffmpeg_codec: subtitle_cache_ffmpeg_codec(&cache_codec, source_codec),
                cache_codec,
                known_codec,
            };
            let cache_codec = subtitle_cache_codec(source_codec.unwrap_or(""));
            let mut outputs = vec![output(cache_codec.clone())];
            if cache_codec == api::SubtitleCodec::Ass {
                outputs.push(output(api::SubtitleCodec::Srt));
            }
            if let Some((index, codec)) = requested
                && index == stream.index
                && !outputs
                    .iter()
                    .any(|o| &o.cache_codec == codec)
            {
                outputs.push(output(codec.clone()));
            }
            outputs
        })
        .collect()
}

/// Whether `stream` has a recognized text subtitle codec.
/// `is_text_subtitle_stream` also counts unknown/missing codecs as text,
/// which ffmpeg may not be able to convert.
fn is_known_text_codec(stream: &api::MediaStream) -> bool {
    stream
        .subtitle_codec()
        .is_some_and(|c| c.is_text() && !matches!(c, api::SubtitleCodec::Other(_)))
}

/// A subtitle cache file counts as a hit only when it holds actual text.
fn is_cached(path: &std::path::Path) -> bool {
    std::fs::read(path).is_ok_and(|b| {
        !String::from_utf8_lossy(&b)
            .trim()
            .is_empty()
    })
}

/// The path ffmpeg actually writes to for a given final cache path — never
/// the final path itself. `extract_subtitles_to_cache` only renames a temp
/// file into place after ffmpeg has fully closed it, so a reader can never
/// observe a partially-written file at the path it checks for a cache hit
/// (see `ensure_subtitle_cached`'s pre-lock fast path).
fn subtitle_cache_tmp_path(final_path: &std::path::Path) -> std::path::PathBuf {
    let mut name = final_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    final_path.with_file_name(name)
}

/// Builds the ffmpeg argument list (minus the binary name) for extracting
/// every stream in `streams` from `input_url` in one pass, plus the
/// (tmp_path, final_path) pair each one is written to. ffmpeg writes to
/// `tmp_path`; the caller renames to `final_path` only once extraction has
/// fully succeeded. Pure and side-effect free so it's unit-testable without
/// actually running ffmpeg — mirrors how `build_hls_args` in
/// `playback::engine` is tested.
fn build_subtitle_extraction_args(
    data_dir: &std::path::Path,
    input_url: &str,
    item_id: Uuid,
    media_source_id: Uuid,
    streams: &[ExtractableSubtitle],
) -> anyhow::Result<(Vec<String>, Vec<(std::path::PathBuf, std::path::PathBuf)>)> {
    let mut args = vec![
        "-y".to_string(),
        "-nostdin".to_string(),
        "-copyts".to_string(),
    ];
    args.extend(
        ffmpeg_http_input_args(input_url)
            .iter()
            .map(|s| s.to_string()),
    );
    args.push("-i".to_string());
    args.push(input_url.to_string());
    let mut cache_paths = Vec::with_capacity(streams.len());
    for s in streams {
        let final_path = subtitle_cache_path(
            data_dir,
            item_id,
            media_source_id,
            s.stream_index,
            &s.cache_codec,
        );
        let tmp_path = subtitle_cache_tmp_path(&final_path);
        args.extend([
            "-map".to_string(),
            s.map_spec
                .clone(),
            "-an".to_string(),
            "-vn".to_string(),
            "-avoid_negative_ts".to_string(),
            "disabled".to_string(),
            "-c:s".to_string(),
            s.ffmpeg_codec
                .clone(),
            "-f".to_string(),
            s.cache_codec
                .to_string(),
        ]);
        args.push(
            tmp_path
                .to_str()
                .ok_or_else(|| anyhow!("invalid cache path"))?
                .to_string(),
        );
        cache_paths.push((tmp_path, final_path));
    }
    Ok((args, cache_paths))
}

/// Extracts every subtitle in `streams` from `input_url` in a single ffmpeg
/// pass — one `-i`, one `-map`/`-c:s` output per stream — so a remote
/// source is read once for its subtitles no matter how many tracks it has,
/// not once per requested track. On timeout or failure every output in this
/// batch is deleted, matching Jellyfin's own behavior (confirmed against
/// its `SubtitleEncoder.cs`: it doesn't keep partial output either).
///
/// ffmpeg writes to a `.tmp` path and each output is only renamed into its
/// real cache path after the whole batch succeeds — a concurrent reader
/// checking the real path (outside the extraction lock, see
/// `ensure_subtitle_cached`) can therefore never observe a partially-written
/// file.
async fn extract_subtitles_to_cache(
    data_dir: &std::path::Path,
    input_url: &str,
    item_id: Uuid,
    media_source_id: Uuid,
    streams: &[ExtractableSubtitle],
) -> anyhow::Result<()> {
    if streams.is_empty() {
        return Ok(());
    }
    let cache_dir = data_dir.join("subtitle-cache");
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .map_err(|e| anyhow!("failed to create subtitle cache dir: {e}"))?;

    let (args, cache_paths) = build_subtitle_extraction_args(
        data_dir,
        input_url,
        item_id,
        media_source_id,
        streams,
    )?;
    let mut cmd = tokio::process::Command::new(ffmpeg_bin());
    cmd.hide_console();
    cmd.kill_on_drop(true);
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::piped());

    let delete_tmp_outputs = |paths: Vec<(std::path::PathBuf, std::path::PathBuf)>| {
        tokio::spawn(async move {
            for (tmp, _final) in paths {
                let _ = tokio::fs::remove_file(tmp).await;
            }
        });
    };

    let output =
        match tokio::time::timeout(SUBTITLE_EXTRACTION_TIMEOUT, cmd.output()).await {
            Err(_) => {
                delete_tmp_outputs(cache_paths);
                anyhow::bail!("subtitle extraction timed out");
            }
            Ok(Err(e)) => anyhow::bail!("failed to run ffmpeg: {e}"),
            Ok(Ok(output)) => output,
        };

    if !output
        .status
        .success()
    {
        let stderr = String::from_utf8_lossy(&output.stderr);
        delete_tmp_outputs(cache_paths);
        anyhow::bail!("ffmpeg subtitle extraction failed: {stderr}");
    }

    // A stream ffmpeg couldn't actually produce (e.g. an empty track) leaves
    // an empty or missing tmp file — drop just that one rather than failing
    // the whole batch, so the other tracks it successfully extracted still
    // cache. Everything else is only now moved into its real cache path.
    for (tmp, final_path) in &cache_paths {
        let empty = tokio::fs::read(tmp)
            .await
            .map(|b| {
                b.iter()
                    .all(|b| b.is_ascii_whitespace())
            })
            .unwrap_or(true);
        if empty {
            let _ = tokio::fs::remove_file(tmp).await;
        } else if let Err(e) = tokio::fs::rename(tmp, final_path).await {
            warn!(error = %e, tmp = %tmp.display(), "failed to move extracted subtitle into cache");
            let _ = tokio::fs::remove_file(tmp).await;
        }
    }

    Ok(())
}

/// Extract one embedded subtitle for alignment's reference timeline. Reuse
/// the same atomic-output and empty-file checks as the normal batched cache
/// path, while keeping this isolated extraction in the caller's temporary
/// directory.
pub(super) async fn extract_subtitle_to_cache(
    data_dir: &std::path::Path,
    input_url: &str,
    map_spec: &str,
    item_id: Uuid,
    media_source_id: Uuid,
    stream_index: i64,
    cache_codec: api::SubtitleCodec,
    source_codec: Option<&str>,
) -> anyhow::Result<std::path::PathBuf> {
    let cache_path = subtitle_cache_path(
        data_dir,
        item_id,
        media_source_id,
        stream_index,
        &cache_codec,
    );
    if is_cached(&cache_path) {
        return Ok(cache_path);
    }

    let stream = ExtractableSubtitle {
        stream_index,
        map_spec: map_spec.to_string(),
        ffmpeg_codec: subtitle_cache_ffmpeg_codec(&cache_codec, source_codec),
        cache_codec,
        // This helper extracts only one stream, so an unknown codec cannot
        // poison a batch containing otherwise-convertible tracks.
        known_codec: false,
    };
    extract_subtitles_to_cache(
        data_dir,
        input_url,
        item_id,
        media_source_id,
        &[stream],
    )
    .await?;

    if is_cached(&cache_path) {
        Ok(cache_path)
    } else {
        anyhow::bail!("subtitle extraction produced empty output")
    }
}

/// Ensures `stream_index`'s subtitle is cached, batch-extracting every
/// not-yet-cached extractable stream on the source in one pass if needed
/// (see `extract_subtitles_to_cache`). Single-flight per source: concurrent
/// or player-retried requests for different tracks on the same source await
/// the one running extraction instead of each starting their own full read
/// of a possibly-remote file — and that extraction runs in its own detached
/// task (see `SUBTITLE_EXTRACTION_INFLIGHT`), so a caller giving up and
/// disconnecting doesn't kill it for everyone else waiting on it too.
async fn ensure_subtitle_cached(
    data_dir: &std::path::Path,
    input_url: &str,
    probe: &api::MediaSourceInfo,
    item_id: Uuid,
    media_source_id: Uuid,
    stream_index: i64,
    requested_cache_codec: api::SubtitleCodec,
) -> anyhow::Result<std::path::PathBuf> {
    let requested_path = subtitle_cache_path(
        data_dir,
        item_id,
        media_source_id,
        stream_index,
        &requested_cache_codec,
    );
    if is_cached(&requested_path) {
        debug!(%item_id, stream_index, "subtitle cache hit");
        return Ok(requested_path);
    }
    info!(%item_id, stream_index, "subtitle cache miss — extracting on-demand");
    // Another request's batch may already be running without the output this
    // one needs (e.g. an `.ass` request joining an SRT-only batch). That run
    // is gone by the time we wake, so go again once, now as the starter.
    let mut attempt = 0;
    loop {
        let mut joined = false;

        // A codec ffmpeg may not be able to convert (unknown or missing) aborts
        // the whole batch, losing every good track with it. Batch only known
        // text codecs; an unknown-codec track is extracted on its own, and only
        // when it's the one requested.
        let solo = (!probe
            .media_streams
            .iter()
            .any(|s| {
                s.index == stream_index && !s.is_external && is_known_text_codec(s)
            }))
        .then_some(stream_index);
        let key = (item_id, media_source_id, solo);
        let shared = {
            let mut inflight = SUBTITLE_EXTRACTION_INFLIGHT
                .lock()
                .await;
            if let Some(existing) = inflight.get(&key) {
                joined = true;
                existing.clone()
            } else {
                let missing: Vec<ExtractableSubtitle> = extractable_subtitles(
                    probe,
                    Some((stream_index, &requested_cache_codec)),
                )
                .into_iter()
                .filter(|s| match solo {
                    Some(index) => s.stream_index == index,
                    None => s.known_codec,
                })
                .filter(|s| {
                    !is_cached(&subtitle_cache_path(
                        data_dir,
                        item_id,
                        media_source_id,
                        s.stream_index,
                        &s.cache_codec,
                    ))
                })
                .collect();
                let data_dir = data_dir.to_path_buf();
                let input_url = input_url.to_string();
                let future: BoxFuture<'static, ExtractionOutcome> =
                    Box::pin(async move {
                        let result = extract_subtitles_to_cache(
                            &data_dir,
                            &input_url,
                            item_id,
                            media_source_id,
                            &missing,
                        )
                        .await
                        .map_err(|e| e.to_string());
                        // Remove ourselves once done (success or failure) so a
                        // later request retries fresh instead of replaying a stale
                        // error, or needlessly re-joining a completed run, forever.
                        SUBTITLE_EXTRACTION_INFLIGHT
                            .lock()
                            .await
                            .remove(&key);
                        result
                    });
                let shared = future.shared();
                // Drives `shared` to completion on the runtime directly, so it
                // keeps running even if every caller currently awaiting a clone
                // of it (including the one about to be spawned below) stops
                // polling — e.g. because the HTTP request that triggered it
                // disconnected.
                tokio::spawn(shared.clone());
                inflight.insert(key, shared.clone());
                shared
            }
        };

        shared
            .await
            .map_err(|e| anyhow!("{e}"))?;

        if !is_cached(&requested_path) {
            if joined && attempt == 0 {
                attempt += 1;
                continue;
            }
            anyhow::bail!(
                "subtitle extraction produced no output for stream {stream_index}"
            );
        }
        return Ok(requested_path);
    }
}

/// Subtitle extraction endpoint - extracts a subtitle stream from a media source
/// and optionally converts it to the requested format (vtt, srt, ass).
// Jellyfin clients include a start-position-ticks segment in the path.
#[get(
    "/videos/{item_id}/{media_source_id}/subtitles/{stream_index}/{start_ticks}/stream.{format}"
)]
pub async fn subtitles_stream(
    State(state): State<AppState>,
    session: auth::AuthSession,
    Query(options): Query<SubtitleOptions>,
    Path((item_id, media_source_id, stream_index, _start_ticks, format)): Path<(
        Uuid,
        Uuid,
        i64,
        String,
        String,
    )>,
) -> Result<impl IntoResponse> {
    subtitles_stream_inner(
        state,
        session,
        item_id,
        media_source_id,
        stream_index,
        format,
        options.remux_original,
    )
    .await
}

/// Jellyfin also accepts the tickless subtitle route (defaults the start-position
/// ticks segment to 0) — Moonfin for webOS uses it.
/// https://github.com/jellyfin/jellyfin/blob/master/Jellyfin.Api/Controllers/SubtitleController.cs
#[get("/videos/{item_id}/{media_source_id}/subtitles/{stream_index}/stream.{format}")]
pub async fn subtitles_stream_tickless(
    State(state): State<AppState>,
    session: auth::AuthSession,
    Query(options): Query<SubtitleOptions>,
    Path((item_id, media_source_id, stream_index, format)): Path<(
        Uuid,
        Uuid,
        i64,
        String,
    )>,
) -> Result<impl IntoResponse> {
    subtitles_stream_inner(
        state,
        session,
        item_id,
        media_source_id,
        stream_index,
        format,
        options.remux_original,
    )
    .await
}

/// Fetch the raw bytes of an external subtitle URL through our stream proxy.
/// A refused/corrupt upstream response (flaky subtitle CDN) is treated as
/// "subtitle unavailable", not a server error: the caller logs it and answers
/// 404 to the client instead of aborting the request with a 500.
async fn fetch_external_subtitle_bytes(
    state: &AppState,
    descriptor: &crate::stream::StreamDescriptor,
) -> anyhow::Result<axum::body::Bytes> {
    let cache_key = gate::raw_key(descriptor);
    if let Some(bytes) = state
        .ctx
        .store
        .get::<axum::body::Bytes>(&cache_key)
    {
        return Ok((*bytes).clone());
    }
    if gate::known_invalid(&state.ctx, descriptor) {
        return Err(anyhow!(
            "upstream subtitle was recently rejected as invalid"
        ));
    }
    let resp = match descriptor {
        crate::stream::StreamDescriptor::Opendal { addon_id, .. } => {
            let addon = state
                .ctx
                .addons
                .get(*addon_id)
                .ok_or_else(|| anyhow!("addon not found for subtitle"))?;
            let stream_cap = addon
                .stream
                .as_ref()
                .ok_or_else(|| anyhow!("addon has no stream capability"))?;
            stream_cap
                .serve_stream(descriptor, &axum::http::HeaderMap::new())
                .await
                .map_err(|e| anyhow!("upstream serve failed: {e:?}"))?
        }
        _ => descriptor
            .clone()
            .into_source()
            .serve(state, &axum::http::HeaderMap::new())
            .await
            .map_err(|e| anyhow!("upstream serve failed: {e:?}"))?,
    };
    let status = resp.status();
    if !status.is_success() {
        if matches!(status.as_u16(), 400 | 404 | 410) {
            state
                .ctx
                .store
                .save(
                    gate::invalid_key(descriptor),
                    true,
                    Duration::from_secs(15 * 60),
                );
        }
        return Err(anyhow!("upstream subtitle status {}", status));

    }
    let bytes = axum::body::to_bytes(resp.into_body(), 2_000_000)
        .await
        .map_err(|_| anyhow!("subtitle read failed"))?;
    if !gate::valid_subtitle(&bytes) {
        state
            .ctx
            .store
            .save(
                gate::invalid_key(descriptor),
                true,
                Duration::from_secs(15 * 60),
            );
        return Err(anyhow!("upstream returned no valid subtitle cues"));
    }
    state
        .ctx
        .store
        .delete(gate::invalid_key(descriptor));
    state
        .ctx
        .store
        .save(
            cache_key,
            bytes.clone(),
            std::time::Duration::from_secs(1800),
        );
    Ok(bytes)
}

fn external_subtitle_response(
    bytes: axum::body::Bytes,
    output_format: &str,
) -> Response<Body> {
    let body = String::from_utf8_lossy(&bytes).into_owned();
    let (converted, content_type) = match output_format {
        "vtt" | "webvtt" => (
            crate::conversions::srt_to_vtt(&body),
            "text/vtt; charset=utf-8",
        ),
        "js" | "json" => (
            crate::conversions::srt_to_jellyfin_json(&body),
            "application/json; charset=utf-8",
        ),
        _ => (body, "text/plain; charset=utf-8"),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", content_type)
        .header("Cache-Control", "public, max-age=3600")
        .header("Access-Control-Allow-Origin", "*")
        .body(Body::from(converted))
        .unwrap()
}

fn cached_external_subtitle_response(
    bytes: axum::body::Bytes,
    output_format: &str,
    auto_synced: bool,
) -> Response<Body> {
    let mut response = external_subtitle_response(bytes, output_format);
    response
        .headers_mut()
        .insert(
            "X-Remux-Subtitle-Alignment",
            if auto_synced { "aligned" } else { "unchanged" }
                .parse()
                .unwrap(),
        );
    response
        .headers_mut()
        .insert(
            "Cache-Control",
            "private, no-store"
                .parse()
                .unwrap(),
        );
    response
}

async fn aligned_external_response(
    state: &AppState,
    source: &db::Media,
    item_id: Uuid,
    subtitle_id: Option<&str>,
    source_alias: Option<Uuid>,
    stream_index: Option<i64>,
    descriptor: &crate::stream::StreamDescriptor,
    bytes: axum::body::Bytes,
    language: Option<&str>,
    format: &str,
    bypass: bool,
) -> Response<Body> {
    let original_bytes = bytes.clone();
    // Bounded best-effort wait (Config.subtitle_alignment_wait_seconds, default
    // 2s): a track eligible for alignment never blocks the request past that
    // deadline. The job itself keeps running in the background regardless,
    // and a later request for the same track serves the finished result from
    // cache. Never fail the request over alignment being slow or unavailable
    // — worst case this just serves the original (already cue-validated) text.
    let (bytes, status) =
        alignment::resolve(state, source, bytes, language, format, bypass).await;
    if !bypass {
        let source_info = api::MediaSourceInfo::from(source.clone());
        let mut source_ids = vec![source_info.id, source.id];
        if let Some(alias) = source_alias {
            source_ids.push(alias);
        }
        source_ids.sort_unstable();
        source_ids.dedup();

        if let Some(subtitle_id) = subtitle_id {
            if let Some(key) =
                good_track_key("raw", item_id, None, subtitle_id, language, None)
            {
                let _ = save_good_track(&state.ctx, &key, &original_bytes, false).await;
            }
            if matches!(status, "aligned" | "unchanged") {
                for source_id in &source_ids {
                    if let Some(key) = good_track_key(
                        "ready",
                        item_id,
                        Some(*source_id),
                        subtitle_id,
                        language,
                        Some(format),
                    ) {
                        let _ = save_good_track(
                            &state.ctx,
                            &key,
                            &bytes,
                            status == "aligned",
                        )
                        .await;
                    }
                }
            }
        }
        // Item metadata exposes a redirectable source as its remote HTTP URL,
        // while the stored source still has its internal /remux path.
        let remote_path = source
            .stream_info
            .as_ref()
            .and_then(|info| {
                info.descriptor
                    .as_http_url()
            });
        for path in [
            source_info
                .path
                .as_deref(),
            remote_path,
        ] {
            if let Some(key) =
                subtitle_sync_label_key(path, source_info.run_time_ticks, descriptor)
            {
                state
                    .ctx
                    .store
                    .save(key, status == "aligned", GOOD_TRACK_CACHE_TTL);
            }
        }
        if let Some(subtitle_id) = subtitle_id {
            for source_id in &source_ids {
                if let Some(key) = subtitle_sync_track_label_key(
                    item_id,
                    *source_id,
                    subtitle_id,
                    language,
                ) {
                    state
                        .ctx
                        .store
                        .save(key, status == "aligned", GOOD_TRACK_CACHE_TTL);
                }
            }
        }
        if let Some(stream_index) = stream_index {
            for source_id in source_ids {
                let key = subtitle_sync_track_index_key(
                    item_id,
                    source_id,
                    stream_index,
                    language,
                );
                state
                    .ctx
                    .store
                    .save(key, status == "aligned", GOOD_TRACK_CACHE_TTL);
            }
        }
    }
    let mut response = external_subtitle_response(bytes, format);
    response
        .headers_mut()
        .insert(
            "X-Remux-Subtitle-Alignment",
            status
                .parse()
                .unwrap(),
        );
    // Clients must reload the endpoint to see newly ready corrections. Never
    // retain a cold original in an intermediary cache after alignment finishes.
    response
        .headers_mut()
        .insert(
            "Cache-Control",
            "private, no-store"
                .parse()
                .unwrap(),
        );
    response
}

#[derive(Clone)]
pub(crate) struct SidecarSubtitleRoute {
    pub(crate) index: i64,
    pub(crate) subtitle: crate::addons::SubtitleInfo,
}

pub(crate) fn save_subtitle_source(
    ctx: &crate::AppContext,
    device_id: &str,
    item_id: Uuid,
    alias: Uuid,
    actual: Uuid,
) {
    ctx.store
        .save(
            format!("subtitle-source:{device_id}:{item_id}:{alias}"),
            actual,
            std::time::Duration::from_secs(6 * 60 * 60),
        );
}

fn resolve_subtitle_source(
    ctx: &crate::AppContext,
    device_id: &str,
    item_id: Uuid,
    alias: Uuid,
) -> Uuid {
    ctx.store
        .get::<Uuid>(&format!("subtitle-source:{device_id}:{item_id}:{alias}"))
        .map(|id| *id)
        .unwrap_or(alias)
}

fn external_base_key(item_id: Uuid, source_id: Uuid) -> String {
    format!("subtitle-ext-bases:{item_id}:{source_id}")
}

/// Remember the first add-on subtitle index advertised for a source (the
/// numbering changes once the source is probed; clients keep the old one).
pub(crate) fn remember_external_base(
    ctx: &crate::AppContext,
    item_id: Uuid,
    source_id: Uuid,
    base: i64,
) {
    let key = external_base_key(item_id, source_id);
    let mut bases: Vec<i64> = ctx
        .store
        .get::<Vec<i64>>(&key)
        .map(|v| (*v).clone())
        .unwrap_or_default();
    bases.retain(|b| *b != base);
    bases.push(base);
    if bases.len() > 8 {
        bases.remove(0);
    }
    ctx.store
        .save(key, bases, std::time::Duration::from_secs(6 * 60 * 60));
}

/// Most recently advertised base (other than the current one) under which
/// `stream_index` is an add-on subtitle.
fn advertised_external_base(
    ctx: &crate::AppContext,
    item_id: Uuid,
    source_ids: &[Uuid],
    stream_index: i64,
    current: i64,
) -> Option<i64> {
    source_ids
        .iter()
        .filter_map(|id| {
            ctx.store
                .get::<Vec<i64>>(&external_base_key(item_id, *id))
        })
        .flat_map(|v| {
            v.iter()
                .rev()
                .copied()
                .collect::<Vec<_>>()
        })
        .find(|b| *b != current && *b <= stream_index)
}

fn sidecar_subtitle_routes_key(
    device_id: &str,
    item_id: Uuid,
    media_source_id: Uuid,
) -> String {
    format!("sidecar-subtitle-routes:{device_id}:{item_id}:{media_source_id}")
}

fn next_external_subtitle_index(
    source_indices: impl IntoIterator<Item = i64>,
    sidecar_indices: impl IntoIterator<Item = i64>,
) -> i64 {
    source_indices
        .into_iter()
        .chain(sidecar_indices)
        .max()
        .map_or(0, |index| index + 1)
}

/// Resolve an add-on subtitle against the current index base, then retry a
/// previously advertised base if the current layout has no candidate at that
/// offset. Item details and PlaybackInfo can number the same external track
/// differently when a source is probed between the two requests.
fn external_subtitle_candidate_index(
    stream_index: i64,
    current_base: i64,
    previous_base: Option<i64>,
    candidate_count: usize,
) -> Option<usize> {
    let index_at_base = |base| {
        let index = usize::try_from(stream_index.checked_sub(base)?).ok()?;
        (index < candidate_count).then_some(index)
    };
    index_at_base(current_base).or_else(|| previous_base.and_then(index_at_base))
}

/// Pick the current external-subtitle index range, or a previously advertised
/// range when the requested index now falls below it. Only retained embedded
/// indexes are reserved; raw probe tracks filtered from PlaybackInfo may have
/// occupied the same index as an external subtitle the client was shown.
fn external_subtitle_request_base(
    stream_index: i64,
    current_base: i64,
    embedded_indices: &std::collections::HashSet<i64>,
    previous_base: Option<i64>,
) -> Option<i64> {
    if embedded_indices.contains(&stream_index) {
        return None;
    }
    if stream_index >= current_base {
        return Some(current_base);
    }
    previous_base.filter(|base| *base >= 0 && *base <= stream_index)
}

pub(crate) fn inject_sidecar_subtitles(
    source: &mut api::MediaSourceInfo,
    subtitles: Vec<crate::addons::SubtitleInfo>,
) -> Vec<SidecarSubtitleRoute> {
    let next_idx = source
        .media_streams
        .iter()
        .map(|stream| stream.index)
        .max()
        .map_or(0, |index| index + 1);

    subtitles
        .into_iter()
        .enumerate()
        .map(|(offset, subtitle)| {
            let index = next_idx + offset as i64;
            let mut stream = crate::conversions::subtitle_to_media_stream(&subtitle);
            stream.index = index;
            source
                .media_streams
                .push(stream);
            SidecarSubtitleRoute { index, subtitle }
        })
        .collect()
}

pub(crate) fn save_sidecar_subtitle_routes(
    ctx: &crate::AppContext,
    device_id: &str,
    item_id: Uuid,
    media_source_id: Uuid,
    routes: Vec<SidecarSubtitleRoute>,
) {
    if routes.is_empty() {
        return;
    }
    ctx.store
        .save(
            sidecar_subtitle_routes_key(device_id, item_id, media_source_id),
            routes,
            std::time::Duration::from_secs(6 * 60 * 60),
        );
}

fn load_sidecar_subtitle_routes(
    ctx: &crate::AppContext,
    device_id: &str,
    item_id: Uuid,
    media_source_id: Uuid,
) -> Option<std::sync::Arc<Vec<SidecarSubtitleRoute>>> {
    ctx.store
        .get::<Vec<SidecarSubtitleRoute>>(&sidecar_subtitle_routes_key(
            device_id,
            item_id,
            media_source_id,
        ))
}

async fn sidecar_subtitle_response(
    state: &AppState,
    user_id: Uuid,
    routes: Option<&[SidecarSubtitleRoute]>,
    item_id: Uuid,
    media_source_id: Uuid,
    source_alias: Uuid,
    stream_index: i64,
    format: &str,
    bypass: bool,
) -> Option<Response<Body>> {
    let route = routes?
        .iter()
        .find(|route| route.index == stream_index)?;
    let descriptor = route
        .subtitle
        .url
        .as_ref()?;

    Some(
        match fetch_external_subtitle_bytes(state, descriptor).await {
            Ok(bytes) => {
                if state
                    .ctx
                    .config
                    .subtitle_alignment_url
                    .is_some()
                    && !bypass
                {
                    if let Ok(source) = crate::services::StreamService::lookup(
                        &state.ctx,
                        item_id,
                        Some(media_source_id),
                        None,
                        Some(user_id),
                    )
                    .await
                    {
                        return Some(
                            aligned_external_response(
                                state,
                                &source,
                                item_id,
                                Some(
                                    &route
                                        .subtitle
                                        .id,
                                ),
                                Some(source_alias),
                                Some(stream_index),
                                descriptor,
                                bytes,
                                route
                                    .subtitle
                                    .lang
                                    .as_deref(),
                                &format.to_ascii_lowercase(),
                                bypass,
                            )
                            .await,
                        );
                    }
                }
                external_subtitle_response(bytes, &format.to_ascii_lowercase())
            }
            Err(error) => {
                warn!(%error, %item_id, %media_source_id, stream_index,
                "sidecar subtitle unavailable");
                (StatusCode::NOT_FOUND, "subtitle unavailable").into_response()
            }
        },
    )
}

/// The item's add-on subtitles, as PlaybackInfo fetched them (same cache).
async fn fetch_item_subtitles(
    state: &AppState,
    session: &auth::AuthSession,
    item_media: &mut db::Media,
) -> Vec<crate::addons::SubtitleInfo> {
    state
        .ctx
        .addons
        .fetch_subtitles(
            item_media,
            &state.ctx,
            true,
            Some(
                session
                    .user
                    .id,
            ),
        )
        .await
}

async fn subtitles_stream_inner(
    state: AppState,
    session: auth::AuthSession,
    item_id: Uuid,
    media_source_id: Uuid,
    stream_index: i64,
    format: String,
    bypass: bool,
) -> Result<impl IntoResponse> {
    let advertised_source_id = media_source_id;
    let media_source_id = resolve_subtitle_source(
        &state.ctx,
        &session
            .device
            .id,
        item_id,
        advertised_source_id,
    );
    let sidecar_routes = load_sidecar_subtitle_routes(
        &state.ctx,
        &session
            .device
            .id,
        item_id,
        advertised_source_id,
    );
    if let Some(response) = sidecar_subtitle_response(
        &state,
        session
            .user
            .id,
        sidecar_routes
            .as_ref()
            .map(|routes| routes.as_slice()),
        item_id,
        media_source_id,
        advertised_source_id,
        stream_index,
        &format,
        bypass,
    )
    .await
    {
        return Ok(response);
    }

    // Try to resolve as an external subtitle injected during PlaybackInfo.
    // fetch_subtitles is cached (24h Stremio / SQLite Opendal) so this is cheap.
    if let Some(mut item_media) = db::Media::get_by_id(
        &state
            .ctx
            .db,
        &item_id,
    )
    .await
    .ok()
    .flatten()
    {
        let source_media = crate::services::StreamService::lookup(
            &state.ctx,
            item_id,
            Some(media_source_id),
            None,
            Some(
                session
                    .user
                    .id,
            ),
        )
        .await
        .ok();
        if let Some(ref source) = source_media {
            let device_profile = crate::jellyfin_client::merge_device_profile_subtitles(
                &session.device,
                session
                    .device
                    .parsed_device_profile(),
            );
            let encoding_cfg = db::Settings::get_encoding_config(
                &state
                    .ctx
                    .db,
            )
            .await
            .unwrap_or_default();
            let subtitle_mode = encoding_cfg
                .subtitle_mode
                .unwrap_or_default();
            let allow_subtitle_extraction = source
                .allows_subtitle_extraction(
                    &state
                        .ctx
                        .db,
                )
                .await;
            let server_cfg = db::Settings::get_config_or_default(
                &state
                    .ctx
                    .db,
            )
            .await;
            let sub_langs = server_cfg
                .subtitle_languages
                .clone()
                .unwrap_or_default();
            let dedup = SubtitleDedupSettings::from_config(&server_cfg);
            // With dedup on, PlaybackInfo's embedded set depends on the add-on
            // list, so it's needed up front; otherwise only for an external.
            let mut subs = if dedup.enabled {
                Some(fetch_item_subtitles(&state, &session, &mut item_media).await)
            } else {
                None
            };
            // Only the embedded streams PlaybackInfo kept, dropped by the same
            // two steps in the same order: add-on externals are numbered after
            // those, not after every probed stream.
            let embedded_indices: std::collections::HashSet<i64> = source
                .probe_data
                .clone()
                .map(|mut probe| {
                    probe
                        .media_streams
                        .retain(|s| {
                            crate::device_profile::keeps_embedded_subtitle(
                                s,
                                device_profile.as_ref(),
                                allow_subtitle_extraction,
                                subtitle_mode,
                            )
                        });
                    if let Some(ref subs) = subs {
                        drop_unsupported_embedded_subtitles_with_external_match(
                            &mut probe,
                            subs,
                            device_profile.as_ref(),
                        );
                    }
                    probe
                        .media_streams
                        .iter()
                        .map(|s| s.index)
                        .collect()
                })
                .unwrap_or_default();
            // Sidecars are inserted before add-on subtitles in PlaybackInfo,
            // but are not present in the source's probe data. Include their
            // advertised indexes so the requested add-on matches the selected
            // menu entry.
            let next_idx = next_external_subtitle_index(
                embedded_indices
                    .iter()
                    .copied(),
                sidecar_routes
                    .as_deref()
                    .into_iter()
                    .flatten()
                    .map(|entry| entry.index),
            );
            let previous_base = advertised_external_base(
                &state.ctx,
                item_id,
                &[advertised_source_id, media_source_id],
                stream_index,
                next_idx,
            );
            let selected_base = external_subtitle_request_base(
                stream_index,
                next_idx,
                &embedded_indices,
                previous_base,
            );
            let i = selected_base
                .map(|base| stream_index - base)
                .unwrap_or(-1);
            let request_base = selected_base.unwrap_or(next_idx);
            let external = selected_base.is_some();
            // PATCH (uduchi2nd, 2026-09-29): clients can retain an index from
            // an earlier, pre-probe layout. Resolve with that base when the
            // current filtered PlaybackInfo no longer reserves the index.
            if let Some(base) = selected_base.filter(|base| *base != next_idx) {
                debug!(item_id = %item_id, stream_index, base, "subtitle index read with the advertised numbering");
            }
            // Only attempt external resolution if the index is not an embedded stream.
            if external {
                let subs = match subs.take() {
                    Some(subs) => subs,
                    None => {
                        fetch_item_subtitles(&state, &session, &mut item_media).await
                    }
                };
                // Match append_external_subtitles' filtering exactly, or the
                // index a client requests (from the menu it was shown) won't
                // line up with this reconstructed list. That means resolving
                // delivery_method the same way PlaybackInfo did first: raw
                // probe data leaves text subtitles' delivery_method unset
                // (None), which would bypass has_supported_embedded_subtitle's
                // Embed-only guard and let a stale/best-effort profile
                // reach its codec fallback instead of agreeing with what was
                // actually advertised.
                let resolved_probe = source
                    .probe_data
                    .clone()
                    .map(|mut probe| {
                        for stream in &mut probe.media_streams {
                            if matches!(
                                stream.type_,
                                Some(api::MediaStreamType::Subtitle)
                            ) {
                                stream.is_text_subtitle_stream =
                                    stream.is_text_subtitle_stream();
                            }
                        }
                        crate::playback::decision::apply_subtitle_delivery(
                            &mut probe,
                            item_id,
                            session
                                .device
                                .access_token
                                .expose(),
                            &device_profile,
                            subtitle_mode,
                            allow_subtitle_extraction,
                        );
                        probe
                    })
                    .unwrap_or_default();
                let validated_subs = validate_external_subtitles_for_advertising(
                    &state,
                    item_id,
                    subs,
                    std::slice::from_ref(&resolved_probe),
                    &sub_langs,
                    device_profile.as_ref(),
                    dedup,
                )
                .await;
                let source_filename = source
                    .stream_info
                    .as_ref()
                    .and_then(|info| info.filename.as_deref());
                let scored = filter_external_subtitles_for_source(
                    &resolved_probe,
                    source_filename,
                    &validated_subs,
                    &sub_langs,
                    device_profile.as_ref(),
                    dedup,
                );
                let candidate_index = external_subtitle_candidate_index(
                    stream_index,
                    request_base,
                    (request_base == next_idx)
                        .then_some(previous_base)
                        .flatten(),
                    scored.len(),
                )
                .or_else(|| usize::try_from(i).ok().filter(|index| *index < scored.len()));
                if let Some(sub) = candidate_index.and_then(|index| scored.get(index)) {
                    if let Some(ref descriptor) = sub.url {
                        let output_format = format.to_ascii_lowercase();
                        if !bypass {
                            if let Some(key) = good_track_key(
                                "ready",
                                item_id,
                                Some(source.id),
                                &sub.id,
                                sub.lang
                                    .as_deref(),
                                Some(&output_format),
                            ) {
                                if let Some(cached) =
                                    load_good_track(&state.ctx, &key).await
                                {
                                    return Ok(cached_external_subtitle_response(
                                        cached.bytes,
                                        &output_format,
                                        cached.auto_synced,
                                    ));
                                }
                            }
                        }
                        let raw_key = good_track_key(
                            "raw",
                            item_id,
                            None,
                            &sub.id,
                            sub.lang
                                .as_deref(),
                            None,
                        );
                        let bytes = if !bypass {
                            match raw_key.as_deref() {
                                Some(key) => load_good_track(&state.ctx, key)
                                    .await
                                    .map(|track| track.bytes),
                                None => None,
                            }
                        } else {
                            None
                        };
                        let bytes = match bytes {
                            Some(bytes) => bytes,
                            None => {
                                match fetch_external_subtitle_bytes(&state, descriptor)
                                    .await
                                {
                                    Ok(bytes) => {
                                        if let Some(key) = raw_key.as_deref() {
                                            let _ = save_good_track(
                                                &state.ctx, key, &bytes, false,
                                            )
                                            .await;
                                        }
                                        bytes
                                    }
                                    Err(e) => {
                                        warn!(error = %e, item_id = %item_id, stream_index,
                                        "external subtitle unavailable");
                                        return Ok((
                                            StatusCode::NOT_FOUND,
                                            "subtitle unavailable",
                                        )
                                            .into_response());
                                    }
                                }
                            }
                        };
                        return Ok(aligned_external_response(
                            &state,
                            source,
                            item_id,
                            Some(&sub.id),
                            Some(advertised_source_id),
                            Some(stream_index),
                            descriptor,
                            bytes,
                            sub.lang
                                .as_deref(),
                            &output_format,
                            bypass,
                        )
                        .await);
                    }
                }
            }
        }
    }

    let Ok(media) = crate::services::StreamService::lookup(
        &state.ctx,
        item_id,
        Some(media_source_id),
        None,
        Some(
            session
                .user
                .id,
        ),
    )
    .await
    else {
        return Ok((StatusCode::NOT_FOUND, "stream not found").into_response());
    };

    let url = media
        .stream_info
        .as_ref()
        .map(|si| {
            si.descriptor
                .server_input(
                    media.id,
                    state
                        .ctx
                        .config
                        .port,
                )
        })
        .context_not_found("media source has no URL")?;
    // PATCH (uduchi2nd): a "[+VN dub]" row is an HLS mux without text tracks;
    // its listed subtitle streams are the HQ release's, extracted from the
    // HQ file the mux was built from.
    let url = crate::services::dubmux::hq_url_of(&media).unwrap_or(url);

    // Defense in depth: a client can hit this endpoint directly with any
    // stream_index, regardless of what delivery_method PlaybackInfo actually
    // advertised (`apply_subtitle_delivery`'s upstream feasibility check).
    // Never start an extraction — reading the whole remote file — for a
    // remote source.
    if !media
        .allows_subtitle_extraction(
            &state
                .ctx
                .db,
        )
        .await
    {
        return Ok((
            StatusCode::NOT_FOUND,
            "subtitle extraction is disabled for this source",
        )
            .into_response());
    }

    let output_format = format.to_ascii_lowercase();
    let is_json = matches!(output_format.as_str(), "js" | "json");
    let (ffmpeg_format, content_type) = match output_format.as_str() {
        "vtt" | "webvtt" => ("webvtt", "text/vtt; charset=utf-8"),
        "srt" | "subrip" => ("srt", "text/plain; charset=utf-8"),
        "ass" | "ssa" => ("ass", "text/plain; charset=utf-8"),
        "pgssub" | "sup" => ("sup", "application/octet-stream"),
        "js" | "json" => ("srt", "application/json; charset=utf-8"),
        _ => ("srt", "text/plain; charset=utf-8"),
    };

    let map_spec = media
        .probe_data
        .as_ref()
        .and_then(|probe| {
            let mut sub_indexes: Vec<i64> = probe
                .media_streams
                .iter()
                .filter(|s| matches!(s.type_, Some(api::MediaStreamType::Subtitle)))
                .map(|s| s.index)
                .collect();
            sub_indexes.sort_unstable();
            sub_indexes
                .iter()
                .position(|idx| *idx == stream_index)
                .map(|ordinal| format!("0:s:{}", ordinal))
        })
        .context_not_found("subtitle stream not found")?;

    let is_binary = matches!(output_format.as_str(), "sup" | "pgssub");

    // Binary formats (PGS/SUP): extract on-the-fly as raw bytes.
    if is_binary {
        let mut cmd = tokio::process::Command::new(ffmpeg_bin());
        cmd.hide_console();
        cmd.kill_on_drop(true);
        cmd.args(["-copyts"]);
        cmd.args(ffmpeg_http_input_args(&url));
        cmd.args(["-i", &url]);
        cmd.args([
            "-map",
            &map_spec,
            "-an",
            "-vn",
            "-c:s",
            "copy",
            "-f",
            output_format.as_str(),
            "-",
        ]);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let output = tokio::time::timeout(SUBTITLE_EXTRACTION_TIMEOUT, cmd.output())
            .await
            .map_err(|_| anyhow!("subtitle extraction timed out"))?
            .map_err(|e| anyhow!("failed to run ffmpeg: {e}"))?;
        if !output
            .status
            .success()
        {
            return Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("subtitle extraction failed"))
                .unwrap());
        }
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", content_type)
            .body(Body::from(output.stdout))
            .unwrap());
    }

    // VTT/SRT/JSON requests use the SRT cache populated at PlaybackInfo time.
    // ASS/SSA requests use a separate native ASS cache so styled subtitle data is
    // never replaced by SRT bytes under an .ass URL.
    let cache_codec = subtitle_cache_codec(&output_format);
    let probe = media
        .probe_data
        .clone()
        .unwrap_or_default();
    let cache_path = match ensure_subtitle_cached(
        &state
            .ctx
            .config
            .data_dir,
        &url,
        &probe,
        item_id,
        media_source_id,
        stream_index,
        cache_codec,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            error!(%item_id, stream_index, %map_spec, "subtitle extraction failed: {e}");
            return Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("subtitle extraction failed"))
                .unwrap());
        }
    };

    let cached = String::from_utf8_lossy(
        &tokio::fs::read(&cache_path)
            .await
            .map_err(|e| anyhow!("failed to read cached subtitle: {e}"))?,
    )
    .into_owned();

    let body = if is_json {
        crate::conversions::srt_to_jellyfin_json(&cached)
    } else if ffmpeg_format == "webvtt" {
        crate::conversions::srt_to_vtt(&cached)
    } else {
        cached
    };

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", content_type)
        .header("Cache-Control", "public, max-age=3600")
        .header("Access-Control-Allow-Origin", "*")
        .body(Body::from(body))
        .unwrap())
}

pub(crate) use remux_sdks::remux::lang_to_two_letter;

pub(crate) fn subtitle_path_hint(sub: &crate::addons::SubtitleInfo) -> &str {
    match &sub.url {
        Some(crate::stream::StreamDescriptor::Http { url, .. }) => url.as_str(),
        Some(crate::stream::StreamDescriptor::Local(path)) => path
            .to_str()
            .unwrap_or(""),
        Some(crate::stream::StreamDescriptor::Opendal { path, .. }) => path.as_str(),
        _ => "",
    }
}

pub(crate) fn descriptor_to_subtitle_url(sub: &crate::addons::SubtitleInfo) -> String {
    match &sub.url {
        Some(d) => serde_json::to_string(d).unwrap_or_default(),
        None => String::new(),
    }
}

/// `ServerConfiguration.deduplicate_subtitle_tracks` /
/// `max_external_subtitles_per_language`, resolved once per request so
/// `append_external_subtitles` and the download endpoint's index
/// reconstruction always agree on what was advertised.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SubtitleDedupSettings {
    /// When true: at most one subtitle per language, and a language already
    /// covered by a supported embedded track gets no external entry at all.
    /// When false: up to `max_external_per_language_when_disabled` external
    /// candidates per language, regardless of embedded coverage.
    pub enabled: bool,
    pub max_external_per_language_when_disabled: i64,
}

impl SubtitleDedupSettings {
    pub(crate) fn from_config(cfg: &api::ServerConfiguration) -> Self {
        Self {
            enabled: cfg
                .deduplicate_subtitle_tracks
                .unwrap_or(true),
            max_external_per_language_when_disabled: cfg
                .max_external_subtitles_per_language
                .unwrap_or(1),
        }
    }

    fn max_external_per_language(&self) -> usize {
        if self.enabled {
            1
        } else {
            self.max_external_per_language_when_disabled
                .max(0) as usize
        }
    }
}

/// Selects which of `subs` should actually be offered for `source`: ranked
/// and capped per language (`select_external_subtitles`), then — with dedup
/// on — anything a supported embedded track already covers is dropped
/// entirely rather than offered as a redundant duplicate. With dedup off,
/// every external candidate up to the configured cap is kept regardless of
/// embedded coverage — the user asked to see everything.
///
/// Shared by `append_external_subtitles` (the item-level provider-addon
/// list) and the per-source addon-attached list (Stremio's
/// `Stream.subtitles[]`, folded into the sidecar mechanism in
/// `api/playback.rs`) — both are "a list of candidate external subtitles
/// for this specific source" and need the same language-cap/dedup
/// treatment. Before this was shared, the per-source list bypassed both:
/// an addon attaching dozens of languages put all of them in the menu.
/// The release filename PlaybackInfo's probed source carries from its
/// provider (used to prefer subtitles that match the release).
pub(crate) fn provider_filename(source: &api::MediaSourceInfo) -> Option<&str> {
    source
        .remux
        .as_ref()
        .and_then(|remux| {
            remux
                .provider_info
                .as_ref()
        })
        .and_then(|info| info.get("filename"))
        .and_then(serde_json::Value::as_str)
}

pub(crate) fn filter_external_subtitles_for_source<'a>(
    source: &api::MediaSourceInfo,
    source_filename: Option<&str>,
    subs: &'a [crate::addons::SubtitleInfo],
    sub_langs: &[String],
    device_profile: Option<&api::DeviceProfile>,
    dedup: SubtitleDedupSettings,
) -> Vec<&'a crate::addons::SubtitleInfo> {
    crate::subtitle_selection::select_external_subtitles(
        subs,
        sub_langs,
        source_filename,
        dedup.max_external_per_language(),
    )
    .into_iter()
    .filter(|sub| {
        !dedup.enabled || !has_supported_embedded_subtitle(source, sub, device_profile)
    })
    .collect()
}

// The exact media path and subtitle descriptor make aliases harmless and avoid
// carrying a label across different releases/tracks. URL renewal may conservatively
// lose a label until another successful request; no signed URL appears in labels.
fn subtitle_sync_label_key(
    path: Option<&str>,
    duration: Option<i64>,
    descriptor: &crate::stream::StreamDescriptor,
) -> Option<String> {
    let path = path.filter(|p| !p.is_empty())?;
    let mut identity =
        serde_json::json!({"path":path,"duration":duration,"subtitle":descriptor});
    identity.sort_all_objects();
    Some(format!(
        "subtitle-sync-label:{}",
        Uuid::new_v5(&Uuid::nil(), &serde_json::to_vec(&identity).ok()?)
    ))
}

/// Track IDs survive signed URL refreshes and source-path rewriting in PlaybackInfo.
fn subtitle_sync_track_label_key(
    item_id: Uuid,
    source_id: Uuid,
    subtitle_id: &str,
    language: Option<&str>,
) -> Option<String> {
    if subtitle_id
        .trim()
        .is_empty()
    {
        return None;
    }
    let mut identity = serde_json::json!({
        "item": item_id,
        "source": source_id,
        "subtitle_id": subtitle_id,
        "language": language.map(str::trim).map(str::to_ascii_lowercase),
    });
    identity.sort_all_objects();
    Some(format!(
        "subtitle-sync-track:{}",
        Uuid::new_v5(&Uuid::nil(), &serde_json::to_vec(&identity).ok()?)
    ))
}

fn subtitle_sync_track_index_key(
    item_id: Uuid,
    source_id: Uuid,
    stream_index: i64,
    language: Option<&str>,
) -> String {
    let mut identity = serde_json::json!({
        "item": item_id,
        "source": source_id,
        "stream_index": stream_index,
        "language": language.map(str::trim).map(str::to_ascii_lowercase),
    });
    identity.sort_all_objects();
    format!(
        "subtitle-sync-index:{}",
        Uuid::new_v5(
            &Uuid::nil(),
            &serde_json::to_vec(&identity).unwrap_or_default()
        )
    )
}

pub(crate) fn apply_subtitle_sync_label(
    ctx: &crate::AppContext,
    item_id: Uuid,
    source: &api::MediaSourceInfo,
    descriptor: &crate::stream::StreamDescriptor,
    subtitle_id: &str,
    stream: &mut api::MediaStream,
) {
    if ctx.config.subtitle_alignment_url.is_none()
        || ctx.config.subtitle_alignment_token_file.is_none()
    {
        return;
    }
    let changed = ctx
        .store
        .get::<bool>(&subtitle_sync_track_index_key(
            item_id,
            source.id,
            stream.index,
            stream
                .language
                .as_deref(),
        ))
        .is_some_and(|value| *value)
        || subtitle_sync_track_label_key(
            item_id,
            source.id,
            subtitle_id,
            stream
                .language
                .as_deref(),
        )
        .and_then(|key| {
            ctx.store
                .get::<bool>(&key)
        })
        .is_some_and(|value| *value)
        || subtitle_sync_label_key(
            source
                .path
                .as_deref(),
            source.run_time_ticks,
            descriptor,
        )
        .and_then(|key| {
            ctx.store
                .get::<bool>(&key)
        })
        .is_some_and(|value| *value);
    if changed {
        mark_subtitle_auto_synced(stream);
    }
}

pub(crate) async fn restore_persisted_subtitle_sync_label(
    ctx: &crate::AppContext,
    item_id: Uuid,
    source_id: Uuid,
    subtitle_id: &str,
    language: Option<&str>,
    stream: &mut api::MediaStream,
) {
    if ctx
        .config
        .subtitle_alignment_url
        .is_none()
        || ctx
            .config
            .subtitle_alignment_token_file
            .is_none()
    {
        return;
    }
    let Some(key) = good_track_key(
        "ready",
        item_id,
        Some(source_id),
        subtitle_id,
        language,
        Some("vtt"),
    ) else {
        return;
    };
    if load_good_track(ctx, &key)
        .await
        .is_some_and(|track| track.auto_synced)
    {
        mark_subtitle_auto_synced(stream);
    }
}

fn mark_subtitle_auto_synced(stream: &mut api::MediaStream) {
    // (2026-09-29) was "[Auto-synced]"; the user asked for a plain "Synced"
    // suffix. Older labels are still recognised so nothing is marked twice.
    const MARKER: &str = "[Synced]";
    const OLD_MARKER: &str = "[Auto-synced]";
    let title = stream
        .display_title
        .get_or_insert_with(|| {
            stream
                .language
                .clone()
                .unwrap_or_else(|| "Subtitle".into())
        });
    if !title.contains(MARKER) && !title.contains(OLD_MARKER) {
        title.push_str(" [Synced]");
    }
    let path = stream
        .path
        .get_or_insert_with(|| {
            format!(
                "{}.vtt",
                stream
                    .language
                    .as_deref()
                    .unwrap_or("und")
            )
        });
    if !path.contains(MARKER) && !path.contains(OLD_MARKER) {
        let (stem, extension) = path
            .rsplit_once('.')
            .unwrap_or((path.as_str(), "vtt"));
        // keep a trailing language code (".vie") right before the extension
        let (stem, lang) = match stem.rsplit_once('.') {
            Some((base, code))
                if (2..=3).contains(&code.len())
                    && code
                        .chars()
                        .all(|c| c.is_ascii_alphabetic()) =>
            {
                (base, format!(".{code}"))
            }
            _ => (stem, String::new()),
        };
        *path = format!("{stem} [Synced]{lang}.{extension}");
    }
}

/// Keep the readiness gate away from a validated direct CDN redirect when
/// PlaybackInfo already waited for the first source. Other versions use the
/// gate so they are checked on demand before playback starts.
fn has_verified_external_redirect(source: &api::MediaSourceInfo) -> bool {
    if !source.is_remote || !matches!(source.protocol, api::MediaProtocol::Http) {
        return false;
    }
    let Some(path) = source.path.as_deref() else {
        return false;
    };
    let Some(origin) = source
        .remux
        .as_ref()
        .and_then(|r| r.provider_info.as_ref())
        .and_then(|p| p.get("descriptor"))
        .and_then(|d| d.get("Http"))
        .and_then(|h| h.get("url"))
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    if path == origin {
        return false;
    }
    url::Url::parse(path).ok().is_some_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url
                .host_str()
                .is_some_and(|host| !crate::stream::is_internal_host(host))
    })
}

fn advertise_subtitle_gate(
    ctx: &crate::AppContext,
    source: &mut api::MediaSourceInfo,
    item_id: Uuid,
    api_key: &str,
    index: usize,
    release_ready_first_redirect: bool,
) {
    if should_preserve_ready_redirect(source, index, release_ready_first_redirect) {
        return;
    }
    gate::advertise(ctx, source, item_id, api_key);
}

fn should_preserve_ready_redirect(
    source: &api::MediaSourceInfo,
    index: usize,
    release_ready_first_redirect: bool,
) -> bool {
    release_ready_first_redirect && index == 0 && has_verified_external_redirect(source)
}

/// Add validated addon subtitles to media sources. Tests and callers without
/// context omit alignment labels and readiness routing.
pub(crate) fn append_external_subtitles(
    media_sources: &mut [api::MediaSourceInfo],
    subs: &[crate::addons::SubtitleInfo],
    sub_langs: &[String],
    device_profile: Option<&api::DeviceProfile>,
    item_id: Uuid,
    api_key: &str,
    dedup: SubtitleDedupSettings,
) {
    let _ = append_external_subtitles_inner(
        None,
        media_sources,
        subs,
        sub_langs,
        device_profile,
        item_id,
        api_key,
        dedup,
        false,
    );
}

pub(crate) async fn append_external_subtitles_with_context(
    ctx: &crate::AppContext,
    media_sources: &mut [api::MediaSourceInfo],
    subs: &[crate::addons::SubtitleInfo],
    sub_langs: &[String],
    device_profile: Option<&api::DeviceProfile>,
    item_id: Uuid,
    api_key: &str,
    dedup: SubtitleDedupSettings,
    release_ready_first_redirect: bool,
) {
    let restore_targets = append_external_subtitles_inner(
        Some(ctx),
        media_sources,
        subs,
        sub_langs,
        device_profile,
        item_id,
        api_key,
        dedup,
        release_ready_first_redirect,
    );
    for (source_index, stream_index, subtitle_id, language) in restore_targets {
        if let Some(source) = media_sources.get_mut(source_index) {
            let source_id = source.id;
            if let Some(stream) = source.media_streams.get_mut(stream_index) {
                restore_persisted_subtitle_sync_label(
                    ctx,
                    item_id,
                    source_id,
                    &subtitle_id,
                    language.as_deref(),
                    stream,
                )
                .await;
            }
        }
    }
}

fn append_external_subtitles_inner(
    ctx: Option<&crate::AppContext>,
    media_sources: &mut [api::MediaSourceInfo],
    subs: &[crate::addons::SubtitleInfo],
    sub_langs: &[String],
    device_profile: Option<&api::DeviceProfile>,
    item_id: Uuid,
    api_key: &str,
    dedup: SubtitleDedupSettings,
    release_ready_first_redirect: bool,
) -> Vec<(usize, usize, String, Option<String>)> {
    let mut restore_targets = Vec::new();
    for (index, source) in media_sources.iter_mut().enumerate() {
        if !has_real_probe_data(source) {
            continue;
        }
        let next_idx = source
            .media_streams
            .iter()
            .map(|s| s.index)
            .max()
            .map_or(0, |m| m + 1);
        if let Some(ctx) = ctx {
            remember_external_base(ctx, item_id, source.id, next_idx);
        }
        let scored = filter_external_subtitles_for_source(
            source,
            provider_filename(source),
            subs,
            sub_langs,
            device_profile,
            dedup,
        );
        let labels = external_subtitle_labels(
            scored
                .iter()
                .map(|s| s.lang.as_deref()),
        );
        let wants_default = !sub_langs.is_empty()
            && source.default_subtitle_stream_index.is_none();
        for (i, sub) in scored.into_iter().enumerate() {
            let mut stream = crate::conversions::subtitle_to_media_stream(sub);
            let idx = next_idx + i as i64;
            stream.index = idx;
            stream.delivery_url = Some(format!(
                "/Videos/{item_id}/{source_id}/Subtitles/{idx}/0/Stream.vtt?ApiKey={api_key}",
                source_id = source.id,
            ));
            // VidHub requires Path to discover external subtitles and reads
            // the trailing language extension for its language preference.
            // Keep credentials in DeliveryUrl only.
            let label = labels[i].clone();
            let language = stream
                .language
                .as_deref()
                .unwrap_or("und")
                .to_ascii_lowercase();
            stream.path = Some(format!("{label}.{language}.vtt"));
            stream.display_title = Some(label);
            if let (Some(ctx), Some(descriptor)) = (ctx, sub.url.as_ref()) {
                apply_subtitle_sync_label(ctx, item_id, source, descriptor, &sub.id, &mut stream);
            }
            if wants_default && i == 0 {
                stream.is_default = Some(true);
                source.default_subtitle_stream_index = Some(next_idx);
            }
            let stream_position = source.media_streams.len();
            source.media_streams.push(stream);
            restore_targets.push((
                index,
                stream_position,
                sub.id.clone(),
                sub.lang.clone(),
            ));
        }
        if let Some(ctx) = ctx {
            advertise_subtitle_gate(
                ctx,
                source,
                item_id,
                api_key,
                index,
                release_ready_first_redirect,
            );
        }
    }
    restore_targets
}


/// Validate only candidates that survive the same source-specific filtering as
/// PlaybackInfo. Confirmed invalid tracks are hidden; slow hosts get a bounded
/// metadata probe, and successful raw copies are retained for later alignment.
pub(super) async fn validate_external_subtitles_for_advertising(
    state: &AppState,
    item_id: Uuid,
    subs: Vec<crate::addons::SubtitleInfo>,
    media_sources: &[api::MediaSourceInfo],
    sub_langs: &[String],
    device_profile: Option<&api::DeviceProfile>,
    dedup: SubtitleDedupSettings,
) -> Vec<crate::addons::SubtitleInfo> {
    use futures::StreamExt;

    let mut candidates: std::collections::BTreeMap<
        String,
        (crate::stream::StreamDescriptor, Vec<(String, Option<String>)>),
    > = Default::default();
    for source in media_sources {
        for sub in filter_external_subtitles_for_source(
            source,
            provider_filename(source),
            &subs,
            sub_langs,
            device_profile,
            dedup,
        ) {
            let Some(descriptor) = sub.url.as_ref() else {
                continue;
            };
            let key = gate::raw_key(descriptor);
            let entry = candidates
                .entry(key)
                .or_insert_with(|| (descriptor.clone(), Vec::new()));
            let track = (
                sub.id
                    .clone(),
                sub.lang
                    .clone(),
            );
            if !entry
                .1
                .contains(&track)
            {
                entry
                    .1
                    .push(track);
            }
        }
    }

    let state_for_checks = state.clone();
    let checks = futures::stream::iter(
        candidates
            .into_iter()
            .take(64),
    )
    .map(move |(descriptor_key, (descriptor, tracks))| {
        let state = state_for_checks.clone();
        async move {
            for (subtitle_id, language) in &tracks {
                if let Some(key) = good_track_key(
                    "raw",
                    item_id,
                    None,
                    subtitle_id,
                    language.as_deref(),
                    None,
                ) {
                    if load_good_track(&state.ctx, &key)
                        .await
                        .is_some()
                    {
                        return None;
                    }
                }
            }
            if gate::known_invalid(&state.ctx, &descriptor) {
                return Some(descriptor_key);
            }
            if let Ok(Ok(bytes)) = tokio::time::timeout(
                Duration::from_secs(6),
                fetch_external_subtitle_bytes(&state, &descriptor),
            )
            .await
            {
                for (subtitle_id, language) in &tracks {
                    if let Some(key) = good_track_key(
                        "raw",
                        item_id,
                        None,
                        subtitle_id,
                        language.as_deref(),
                        None,
                    ) {
                        let _ = save_good_track(&state.ctx, &key, &bytes, false).await;
                    }
                }
                return None;
            }
            gate::known_invalid(&state.ctx, &descriptor).then_some(descriptor_key)
        }
    })
        .buffer_unordered(4)
        .collect::<Vec<_>>();
    // Metadata must not wait indefinitely for subtitle hosts. Confirmed invalid
    // results are recorded in the shared negative cache and filtered below.
    let _ = tokio::time::timeout(Duration::from_secs(15), checks).await;

    let mut advertised = Vec::with_capacity(subs.len());
    for sub in subs {
        let Some(descriptor) = sub
            .url
            .as_ref()
        else {
            continue;
        };
        let cached_raw_key = good_track_key(
            "raw",
            item_id,
            None,
            &sub.id,
            sub.lang
                .as_deref(),
            None,
        );
        let has_saved_good_copy = if let Some(key) = cached_raw_key {
            load_good_track(&state.ctx, &key)
                .await
                .is_some()
        } else {
            false
        };
        let has_fresh_valid_copy = state
            .ctx
            .store
            .get::<axum::body::Bytes>(&gate::raw_key(descriptor))
            .is_some();
        if !has_saved_good_copy && !has_fresh_valid_copy {
            continue;
        }
        advertised.push(sub);
    }
    advertised
}

#[derive(Clone, Copy)]
pub(crate) enum EmptySubtitleGatePolicy {
    /// PlaybackInfo advertises readiness only for sources with real probe data.
    ProbedSources,
    /// Item details advertise the gate for every source, even without subtitles.
    AllSources,
}

/// Validate addon tracks and inject the surviving tracks through the same
/// source-specific filtering, labeling, and cache-backed sync restoration path.
pub(crate) async fn inject_prefetched_external_subtitles(
    state: &AppState,
    media_sources: &mut [api::MediaSourceInfo],
    item_id: Uuid,
    subs: Vec<crate::addons::SubtitleInfo>,
    sub_langs: &[String],
    device_profile: Option<&api::DeviceProfile>,
    api_key: &str,
    dedup: SubtitleDedupSettings,
    release_ready_first_redirect: bool,
    empty_gate_policy: EmptySubtitleGatePolicy,
) {
    let subs = validate_external_subtitles_for_advertising(
        state,
        item_id,
        subs,
        media_sources,
        sub_langs,
        device_profile,
        dedup,
    )
    .await;

    if subs.is_empty() && matches!(empty_gate_policy, EmptySubtitleGatePolicy::AllSources) {
        for (index, source) in media_sources.iter_mut().enumerate() {
            advertise_subtitle_gate(
                &state.ctx,
                source,
                item_id,
                api_key,
                index,
                release_ready_first_redirect,
            );
        }
        return;
    }

    append_external_subtitles_with_context(
        &state.ctx,
        media_sources,
        &subs,
        sub_langs,
        device_profile,
        item_id,
        api_key,
        dedup,
        release_ready_first_redirect,
    )
    .await;
}

/// PATCH (uduchi2nd): the "[+VN dub]" muxed rows carry the HQ release's
/// embedded text subtitles at `SUBTITLE_INDEX_OFFSET + n` (they are served by
/// this server, not by the muxer's playlist). Stored probes keep them
/// `is_external = false` so alignment still treats them as an embedded
/// reference, but VidHub/Infuse only list tracks that look external AND have
/// a `Path`, so present them that way in every client-facing document.
/// Labels for addon-provided subtitle tracks, in list order: `<lang> (N)`
/// (was `<lang> (vnphim N)` — misleading, most come from other add-ons)
/// with N counting tracks of the same language (1-based), so two Vietnamese
/// files — and the release's own "(release)" track — can be told apart.
fn external_subtitle_labels<'a>(
    langs: impl Iterator<Item = Option<&'a str>>,
) -> Vec<String> {
    // (2026-10-02) full language names, matching the release's own tracks:
    // "Vietnamese (online)" — numbered only when a language has several.
    let langs: Vec<String> = langs
        .map(|l| {
            l.unwrap_or("und")
                .to_ascii_lowercase()
        })
        .collect();
    let mut totals: std::collections::HashMap<&str, usize> = Default::default();
    for l in &langs {
        *totals
            .entry(l.as_str())
            .or_insert(0) += 1;
    }
    let mut seen: std::collections::HashMap<&str, usize> = Default::default();
    langs
        .iter()
        .map(|lang| {
            let n = seen
                .entry(lang.as_str())
                .or_insert(0);
            *n += 1;
            let name = remux_sdks::remux::language_label(lang);
            if totals[lang.as_str()] > 1 {
                format!("{name} (online {n})")
            } else {
                format!("{name} (online)")
            }
        })
        .collect()
}

pub(crate) fn present_dub_row_embedded_subtitles(
    media_sources: &mut [api::MediaSourceInfo],
    item_id: Uuid,
    api_key: &str,
    sub_langs: &[String],
) {
    for source in media_sources.iter_mut() {
        if !crate::services::dubmux::is_mux_path(
            source
                .path
                .as_deref(),
        ) {
            continue;
        }
        let source_id = source
            .id
            .clone();
        // The release's own text tracks follow the same language filter as
        // add-on subtitles (SubtitleLanguages, e.g. en+vi): a dub row listed
        // every Chinese/Arabic/... track of the release, and VidHub picked
        // the first one (Chinese) — 2026-10-02.
        if !sub_langs.is_empty() {
            source
                .media_streams
                .retain(|s| {
                    !matches!(s.type_, Some(api::MediaStreamType::Subtitle))
                        || s.index < crate::services::dubmux::SUBTITLE_INDEX_OFFSET
                        || s.is_external
                        || s.language
                            .as_deref()
                            .and_then(lang_to_two_letter)
                            .is_some_and(|l| {
                                sub_langs
                                    .iter()
                                    .any(|w| w.eq_ignore_ascii_case(&l))
                            })
                });
        }
        for stream in source
            .media_streams
            .iter_mut()
        {
            if !matches!(stream.type_, Some(api::MediaStreamType::Subtitle))
                || stream.index < crate::services::dubmux::SUBTITLE_INDEX_OFFSET
                || stream.is_external
            {
                continue;
            }
            let idx = stream.index;
            stream.is_external = true;
            stream.supports_external_stream = true;
            stream.is_text_subtitle_stream = true;
            stream.delivery_method = Some(api::SubtitleDeliveryMethod::External);
            stream.delivery_url = Some(format!(
                "/Videos/{item_id}/{source_id}/Subtitles/{idx}/0/Stream.vtt?ApiKey={api_key}"
            ));
            // "(release)": tells the viewer this is the HQ release's own
            // text track, not the addon one (both show as external here).
            let lang = stream
                .language
                .clone()
                .unwrap_or_else(|| "und".into());
            if stream
                .path
                .is_none()
            {
                // the trailing ".<lang>.vtt" is how Jellyfin sidecars carry
                // their language; VidHub's language preference reads it
                // (2026-10-02: it picked the first track, Chinese, without)
                let name = remux_sdks::remux::language_label(&lang);
                stream.path = Some(format!("{name} (release).{lang}.vtt"));
            }
            // "English (release)", like the online tracks' "English (online)"
            let forced = stream.is_forced;
            stream.display_title = Some(format!(
                "{}{} (release)",
                remux_sdks::remux::language_label(&lang),
                if forced { " forced" } else { "" }
            ));
        }
    }
}

/// Players that ignore `DefaultSubtitleStreamIndex` (VidHub) follow the
/// tracks' own default flags: only the server's chosen track keeps one.
pub(crate) fn sync_subtitle_default_flags(media_sources: &mut [api::MediaSourceInfo]) {
    for source in media_sources.iter_mut() {
        let Some(default_idx) = source.default_subtitle_stream_index else {
            continue;
        };
        if default_idx < 0 {
            continue;
        }
        for stream in source
            .media_streams
            .iter_mut()
            .filter(|s| matches!(s.type_, Some(api::MediaStreamType::Subtitle)))
        {
            stream.is_default = Some(stream.index == default_idx);
        }
    }
}

pub(crate) async fn inject_external_subtitles(
    state: &AppState,
    subtitle_media: &mut crate::db::Media,
    media_sources: &mut Vec<api::MediaSourceInfo>,
    item_id: Uuid,
    api_key: &str,
    sub_langs: Vec<String>,
    user_id: Option<Uuid>,
    device_profile: Option<&api::DeviceProfile>,
    dedup: SubtitleDedupSettings,
    release_ready_first_redirect: bool,
) {
    let ctx = &state.ctx;
    let subs = ctx
        .addons
        .fetch_subtitles(subtitle_media, ctx, false, user_id)
        .await;
    inject_prefetched_external_subtitles(
        state,
        media_sources,
        item_id,
        subs,
        &sub_langs,
        device_profile,
        api_key,
        dedup,
        release_ready_first_redirect,
        EmptySubtitleGatePolicy::AllSources,
    )
    .await;
}

fn has_real_probe_data(source: &api::MediaSourceInfo) -> bool {
    matches!(
        source
            .remux
            .as_ref()
            .and_then(|remux| remux.source),
        Some(api::ProbeOrigin::Ffprobe | api::ProbeOrigin::RemuxDb)
    ) && !source
        .media_streams
        .is_empty()
}

fn has_supported_embedded_subtitle(
    source: &api::MediaSourceInfo,
    external: &crate::addons::SubtitleInfo,
    device_profile: Option<&api::DeviceProfile>,
) -> bool {
    let Some(profile) = device_profile else {
        return false;
    };
    let external_language = crate::subtitle_selection::normalized_language(
        external
            .lang
            .as_deref(),
    );
    if external_language == "und" || external_language.is_empty() {
        return false;
    }

    source
        .media_streams
        .iter()
        .any(|stream| {
            if stream.type_ != Some(api::MediaStreamType::Subtitle)
                || stream.is_external
                || stream
                    .delivery_method
                    .as_ref()
                    .is_some_and(|method| method != &api::SubtitleDeliveryMethod::Embed)
                || stream.is_forced != external.is_forced
                || stream.is_hearing_impaired != external.is_hi
                || crate::subtitle_selection::normalized_language(
                    stream
                        .language
                        .as_deref(),
                ) != external_language
            {
                return false;
            }
            let Some(codec) = stream
                .codec
                .as_deref()
            else {
                return false;
            };
            profile
                .subtitle_profiles
                .iter()
                .any(|supported| {
                    supported.method == Some(api::SubtitleDeliveryMethod::Embed)
                        && supported
                            .format
                            .as_deref()
                            .is_some_and(|format| {
                                crate::device_profile::subtitle_codec_matches_profile(
                                    codec, format,
                                )
                            })
                })
        })
}

/// Drops embedded subtitle streams the device can't play embedded when a
/// matching addon external subtitle already covers them (same language,
/// forced flag, and hearing-impaired flag — the same "is this actually the
/// same track" bar `has_supported_embedded_subtitle` already uses in the
/// opposite direction).
///
/// Extracting an unsupported embedded subtitle on demand is a slow HTTP
/// round trip against the source file (the download endpoint has to seek
/// into it with ffmpeg) — there's no reason to offer that when a fast,
/// already-fetched external copy exists.
///
/// Must run before `resolve_default_streams`/`compute_transcode_reasons` in
/// playback.rs's per-source loop — those, not `apply_subtitle_delivery`, are
/// what actually pick the default subtitle and decide whether a burn-in
/// transcode is needed. Removing the stream from `media_streams` here is
/// what keeps them from ever considering it; this function doesn't (and
/// shouldn't need to) touch their logic.
pub(crate) fn drop_unsupported_embedded_subtitles_with_external_match(
    source: &mut api::MediaSourceInfo,
    external_subtitles: &[crate::addons::SubtitleInfo],
    device_profile: Option<&api::DeviceProfile>,
) {
    source
        .media_streams
        .retain(|stream| {
            if stream.type_ != Some(api::MediaStreamType::Subtitle)
                || stream.is_external
            {
                return true;
            }
            let Some(codec) = stream
                .codec
                .as_deref()
                .and_then(|c| {
                    c.parse::<crate::device_profile::SubtitleCodec>()
                        .ok()
                })
            else {
                return true;
            };
            if crate::device_profile::profile_embeds_subtitle_codec(
                device_profile,
                &codec,
            ) {
                // Would be Embed delivery anyway — free, keep it.
                return true;
            }
            let stream_language = crate::subtitle_selection::normalized_language(
                stream
                    .language
                    .as_deref(),
            );
            let has_replacement = external_subtitles
                .iter()
                .any(|ext| {
                    ext.is_forced == stream.is_forced
                        && ext.is_hi == stream.is_hearing_impaired
                        && crate::subtitle_selection::normalized_language(
                            ext.lang
                                .as_deref(),
                        ) == stream_language
                });
            !has_replacement
        });
}

#[cfg(test)]
mod label_tests {
    #[test]
    fn external_labels_number_same_language_tracks() {
        let l = super::external_subtitle_labels(
            [Some("vie"), Some("vie"), Some("eng"), None].into_iter(),
        );
        assert_eq!(
            l,
            [
                "Vietnamese (online 1)",
                "Vietnamese (online 2)",
                "English (online)",
                "und (online)"
            ]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::HeaderValue;

    use crate::integration_test::{auth_header_with_token, authenticated_server};

    fn test_cache_file(
        path: &str,
        kind: PersistedSubtitleCacheKind,
        bytes: u64,
        modified_secs: u64,
    ) -> PersistedSubtitleCacheFile {
        PersistedSubtitleCacheFile {
            modified: std::time::UNIX_EPOCH + Duration::from_secs(modified_secs),
            path: path.into(),
            bytes,
            kind,
        }
    }

    #[test]
    fn persistent_subtitle_cache_prunes_oldest_for_total_bytes_and_per_cache_counts() {
        assert_eq!(GOOD_TRACK_CACHE_TTL.as_secs(), 30 * 24 * 3600);
        assert_eq!(MAX_PERSISTED_SUBTITLE_CACHE_BYTES, 200 * 1024 * 1024);
        let pruned = select_cache_files_to_prune(
            vec![
                test_cache_file(
                    "old-good",
                    PersistedSubtitleCacheKind::GoodTrack,
                    80,
                    1,
                ),
                test_cache_file(
                    "new-alignment",
                    PersistedSubtitleCacheKind::Alignment,
                    50,
                    2,
                ),
                test_cache_file(
                    "new-good",
                    PersistedSubtitleCacheKind::GoodTrack,
                    50,
                    3,
                ),
            ],
            100,
            10,
            10,
        );
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].path, std::path::PathBuf::from("old-good"));

        let pruned = select_cache_files_to_prune(
            vec![
                test_cache_file(
                    "old-good",
                    PersistedSubtitleCacheKind::GoodTrack,
                    1,
                    1,
                ),
                test_cache_file(
                    "new-good",
                    PersistedSubtitleCacheKind::GoodTrack,
                    1,
                    2,
                ),
                test_cache_file(
                    "alignment",
                    PersistedSubtitleCacheKind::Alignment,
                    1,
                    3,
                ),
            ],
            10,
            1,
            1,
        );
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].path, std::path::PathBuf::from("old-good"));
    }

    fn redirected_source(path: &str, origin: &str) -> api::MediaSourceInfo {
        api::MediaSourceInfo {
            path: Some(path.into()),
            protocol: api::MediaProtocol::Http,
            is_remote: true,
            remux: Some(api::MediaSourceRemuxInfo {
                provider_info: Some(serde_json::json!({
                    "descriptor": {"Http": {"url": origin}}
                })),
                source: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn only_releases_a_verified_external_redirect_for_prepared_first_source() {
        let source = redirected_source(
            "https://store.example.net/signed-file",
            "https://torrentio.example/resolve/file",
        );
        assert!(should_preserve_ready_redirect(&source, 0, true));
        assert!(!should_preserve_ready_redirect(&source, 1, true));
        assert!(!should_preserve_ready_redirect(&source, 0, false));

        let unverified = redirected_source(
            "https://torrentio.example/resolve/file",
            "https://torrentio.example/resolve/file",
        );
        assert!(!should_preserve_ready_redirect(&unverified, 0, true));

        let internal = redirected_source(
            "http://127.0.0.1:3000/private",
            "https://torrentio.example/resolve/file",
        );
        assert!(!should_preserve_ready_redirect(&internal, 0, true));
    }

    #[tokio::test]
    async fn subtitle_alias_follows_probed_source_and_is_device_scoped() {
        let (_server, guard, _token) = authenticated_server().await;
        let ctx = &guard.0;
        let item = Uuid::new_v4();
        let actual = Uuid::new_v4();
        save_subtitle_source(ctx, "player-a", item, item, actual);
        assert_eq!(resolve_subtitle_source(ctx, "player-a", item, item), actual);
        assert_eq!(resolve_subtitle_source(ctx, "player-b", item, item), item);
        assert_eq!(
            resolve_subtitle_source(ctx, "player-a", Uuid::new_v4(), item),
            item
        );
        // A subsequent selection must replace the previous fallback mapping.
        save_subtitle_source(ctx, "player-a", item, item, item);
        assert_eq!(resolve_subtitle_source(ctx, "player-a", item, item), item);
    }

    #[test]
    fn synced_marker_goes_before_the_language_code() {
        let mut stream = api::MediaStream {
            language: Some("vie".into()),
            display_title: Some("Vietnamese (online 1)".into()),
            path: Some("Vietnamese (online 1).vie.vtt".into()),
            ..Default::default()
        };
        mark_subtitle_auto_synced(&mut stream);
        mark_subtitle_auto_synced(&mut stream);
        assert_eq!(stream.path.as_deref(), Some("Vietnamese (online 1) [Synced].vie.vtt"));
        assert_eq!(stream.display_title.as_deref(), Some("Vietnamese (online 1) [Synced]"));
    }

    #[test]
    fn auto_synced_label_preserves_language_extension_and_is_idempotent() {
        let mut stream = api::MediaStream {
            language: Some("vie".into()),
            display_title: Some("Vietnamese - External".into()),
            path: Some("vie.vtt".into()),
            ..Default::default()
        };
        mark_subtitle_auto_synced(&mut stream);
        mark_subtitle_auto_synced(&mut stream);
        assert_eq!(
            stream
                .display_title
                .as_deref(),
            Some("Vietnamese - External [Synced]")
        );
        assert_eq!(
            stream
                .path
                .as_deref(),
            Some("vie [Synced].vtt")
        );
        assert_eq!(
            stream
                .language
                .as_deref(),
            Some("vie")
        );
    }
    #[test]
    fn sync_labels_are_isolated_by_media_and_subtitle() {
        let a = crate::stream::StreamDescriptor::http("https://sub.test/a.vtt");
        let b = crate::stream::StreamDescriptor::http("https://sub.test/b.vtt");
        assert_ne!(
            subtitle_sync_label_key(Some("release-a"), Some(10), &a),
            subtitle_sync_label_key(Some("release-b"), Some(10), &a)
        );
        assert_ne!(
            subtitle_sync_label_key(Some("release-a"), Some(10), &a),
            subtitle_sync_label_key(Some("release-a"), Some(10), &b)
        );
        assert_ne!(
            subtitle_sync_label_key(Some("release-a"), Some(10), &a),
            subtitle_sync_label_key(Some("release-a"), Some(20), &a)
        );
        assert!(subtitle_sync_label_key(None, Some(10), &a).is_none());
    }

    const DEDUP_ON: SubtitleDedupSettings = SubtitleDedupSettings {
        enabled: true,
        max_external_per_language_when_disabled: 1,
    };

    /// Jellyfin's tickless subtitle route (`.../Subtitles/{index}/Stream.{format}`,
    /// no start-position-ticks segment) must dispatch to the same handler as the
    /// canonical route. With a non-existent item both produce the identical
    /// handler response — an unregistered route would yield axum's bare 404.
    #[tokio::test]
    async fn tickless_subtitle_route_dispatches_to_handler() {
        let (server, guard, token) = authenticated_server().await;
        let auth = auth_header_with_token(&token);
        let bogus = "00000000-0000-0000-0000-000000000000";
        let _ = &guard;

        let canonical = server
            .get(&format!("/videos/{bogus}/{bogus}/subtitles/2/0/stream.ass"))
            .add_header(
                http::header::AUTHORIZATION,
                HeaderValue::from_str(&auth).unwrap(),
            )
            .expect_failure()
            .await;
        let tickless = server
            .get(&format!("/videos/{bogus}/{bogus}/subtitles/2/stream.ass"))
            .add_header(
                http::header::AUTHORIZATION,
                HeaderValue::from_str(&auth).unwrap(),
            )
            .expect_failure()
            .await;

        assert_eq!(
            canonical.status_code(),
            tickless.status_code(),
            "both subtitle route forms must reach the same handler"
        );
        assert!(
            !tickless
                .text()
                .is_empty(),
            "tickless route must dispatch to the subtitle handler, not a bare route-miss 404"
        );
    }

    #[test]
    fn ass_requests_use_a_native_cache_separate_from_srt() {
        let data_dir = std::path::Path::new("/data");
        let item_id = Uuid::nil();
        let media_source_id = Uuid::nil();

        let srt = subtitle_cache_path(
            data_dir,
            item_id,
            media_source_id,
            2,
            &api::SubtitleCodec::Srt,
        );
        let ass = subtitle_cache_path(
            data_dir,
            item_id,
            media_source_id,
            2,
            &api::SubtitleCodec::Ass,
        );

        assert_eq!(
            srt,
            data_dir
                .join("subtitle-cache")
                .join(format!("{item_id}_{media_source_id}_2.srt"))
        );
        assert_eq!(
            ass,
            data_dir
                .join("subtitle-cache")
                .join(format!("{item_id}_{media_source_id}_2.ass"))
        );
        assert_ne!(srt, ass);
    }

    #[test]
    fn subtitle_cache_path_is_scoped_per_media_source() {
        // Regression test: two sources of the same item (different
        // releases/qualities) must never collide on the same cache file,
        // even if they happen to number a subtitle stream the same way.
        let data_dir = std::path::Path::new("/data");
        let item_id = Uuid::nil();
        let source_a = Uuid::from_u128(1);
        let source_b = Uuid::from_u128(2);

        let a = subtitle_cache_path(
            data_dir,
            item_id,
            source_a,
            2,
            &api::SubtitleCodec::Srt,
        );
        let b = subtitle_cache_path(
            data_dir,
            item_id,
            source_b,
            2,
            &api::SubtitleCodec::Srt,
        );

        assert_ne!(
            a, b,
            "the same item_id/stream_index from two different media sources must not share a cache file"
        );
    }

    #[test]
    fn native_ass_extraction_preserves_the_original_stream() {
        let cache = subtitle_cache_codec("ass");

        assert_eq!(cache, api::SubtitleCodec::Ass);
        assert_eq!(subtitle_cache_ffmpeg_codec(&cache, Some("ass")), "copy");
        assert_eq!(subtitle_cache_ffmpeg_codec(&cache, Some("SSA")), "copy");
        assert_eq!(cache.to_string(), "ass");
    }

    #[test]
    fn non_ass_source_is_converted_when_ass_is_requested() {
        let cache = subtitle_cache_codec("ssa");

        assert_eq!(cache, api::SubtitleCodec::Ass);
        assert_eq!(subtitle_cache_ffmpeg_codec(&cache, Some("subrip")), "ass");
    }

    #[test]
    fn sidecars_follow_existing_stream_indexes() {
        let mut source = api::MediaSourceInfo {
            media_streams: vec![
                api::MediaStream {
                    index: 0,
                    type_: Some(api::MediaStreamType::Video),
                    ..Default::default()
                },
                api::MediaStream {
                    index: 2,
                    type_: Some(api::MediaStreamType::Audio),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let subtitles = vec![crate::addons::SubtitleInfo {
            id: "sidecar-4".to_string(),
            url: Some(crate::stream::StreamDescriptor::http(
                "https://example.com/Movie.en.srt",
            )),
            lang: Some("en".to_string()),
            is_forced: false,
            is_hi: false,
            filename: None,
            from_trusted: None,
            ai_translated: None,
        }];

        let routes = inject_sidecar_subtitles(&mut source, subtitles);

        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].index, 3);
        assert_eq!(source.media_streams[2].index, 3);
        assert_eq!(
            source.media_streams[2]
                .codec
                .as_deref(),
            Some("subrip")
        );
    }

    #[test]
    fn external_subtitles_follow_sidecars() {
        assert_eq!(next_external_subtitle_index([0, 1], [2]), 3);
    }

    #[test]
    fn external_subtitle_index_falls_back_to_previous_advertised_base() {
        // Item details advertised this one subtitle at index 3. After probing,
        // the route's current base is 0, so index 3 is outside the one-track
        // candidate list; it must still resolve to candidate zero.
        assert_eq!(external_subtitle_candidate_index(3, 0, Some(3), 1), Some(0));
    }

    #[test]
    fn external_subtitle_index_prefers_a_valid_current_base() {
        assert_eq!(external_subtitle_candidate_index(1, 0, Some(1), 2), Some(1));
    }

    #[test]
    fn external_subtitle_index_rejects_missing_or_negative_candidates() {
        assert_eq!(external_subtitle_candidate_index(3, 0, None, 1), None);
        assert_eq!(external_subtitle_candidate_index(0, 1, Some(1), 1), None);
    }

    #[test]
    fn external_subtitle_request_uses_prior_layout_for_filtered_probe_index() {
        let retained_embedded = std::collections::HashSet::new();

        // Raw probe data may contain this index, but if PlaybackInfo filtered
        // it out, a prior external index remains valid for the selected track.
        assert_eq!(
            external_subtitle_request_base(3, 4, &retained_embedded, Some(3)),
            Some(3)
        );
    }

    #[test]
    fn external_subtitle_request_never_reuses_a_retained_embedded_index() {
        let retained_embedded = [3].into_iter().collect();

        assert_eq!(
            external_subtitle_request_base(3, 4, &retained_embedded, Some(0)),
            None
        );
    }

    #[test]
    fn external_subtitles_only_follow_real_probe_streams() {
        let item_id = Uuid::new_v4();
        let existing = vec![
            api::MediaStream {
                index: 0,
                type_: Some(api::MediaStreamType::Video),
                ..Default::default()
            },
            api::MediaStream {
                index: 2,
                type_: Some(api::MediaStreamType::Subtitle),
                codec: Some("subrip".into()),
                ..Default::default()
            },
        ];
        let mut sources = vec![
            api::MediaSourceInfo {
                id: Uuid::new_v4(),
                media_streams: existing.clone(),
                remux: Some(api::MediaSourceRemuxInfo {
                    source: Some(api::ProbeOrigin::Ffprobe),
                    provider_info: Some(
                        serde_json::json!({"filename": "Movie.2026.mkv"}),
                    ),
                }),
                ..Default::default()
            },
            api::MediaSourceInfo {
                id: Uuid::new_v4(),
                media_streams: existing.clone(),
                remux: Some(api::MediaSourceRemuxInfo {
                    source: Some(api::ProbeOrigin::FilenameGuess),
                    ..Default::default()
                }),
                ..Default::default()
            },
            api::MediaSourceInfo {
                id: Uuid::new_v4(),
                ..Default::default()
            },
            api::MediaSourceInfo {
                id: Uuid::new_v4(),
                media_streams: existing,
                ..Default::default()
            },
        ];
        let subs = vec![crate::addons::SubtitleInfo {
            id: "external".into(),
            url: Some(crate::stream::StreamDescriptor::http(
                "https://example.com/Movie.2026.srt",
            )),
            lang: Some("eng".into()),
            is_forced: false,
            is_hi: false,
            filename: Some("Movie.2026.srt".into()),
            from_trusted: None,
            ai_translated: None,
        }];

        append_external_subtitles(
            &mut sources,
            &subs,
            &[],
            None,
            item_id,
            "test-key",
            DEDUP_ON,
        );

        assert_eq!(
            sources[0]
                .media_streams
                .len(),
            3
        );
        assert_eq!(sources[0].media_streams[2].index, 3);
        assert_eq!(sources[0].media_streams[2].is_external, true);
        assert_eq!(
            sources[0].media_streams[2].path.as_deref(),
            Some("English (online).eng.vtt")
        );
        assert_eq!(
            sources[1]
                .media_streams
                .len(),
            2
        );
        assert!(
            sources[2]
                .media_streams
                .is_empty()
        );
        assert_eq!(
            sources[3]
                .media_streams
                .len(),
            2
        );
    }

    #[test]
    fn supported_embedded_subtitle_replaces_only_the_matching_external_variant() {
        let item_id = Uuid::new_v4();
        let source = api::MediaSourceInfo {
            id: Uuid::new_v4(),
            media_streams: vec![api::MediaStream {
                index: 2,
                type_: Some(api::MediaStreamType::Subtitle),
                codec: Some("pgssub".into()),
                language: Some("eng".into()),
                ..Default::default()
            }],
            remux: Some(api::MediaSourceRemuxInfo {
                source: Some(api::ProbeOrigin::Ffprobe),
                ..Default::default()
            }),
            ..Default::default()
        };
        let external = crate::addons::SubtitleInfo {
            id: "regular".into(),
            url: None,
            lang: Some("en".into()),
            is_forced: false,
            is_hi: false,
            filename: None,
            from_trusted: None,
            ai_translated: None,
        };
        let embed_profile = api::DeviceProfile {
            subtitle_profiles: vec![api::SubtitleProfile {
                format: Some("pgs".into()),
                method: Some(api::SubtitleDeliveryMethod::Embed),
            }],
            ..Default::default()
        };
        let external_profile = api::DeviceProfile {
            subtitle_profiles: vec![api::SubtitleProfile {
                format: Some("pgs".into()),
                method: Some(api::SubtitleDeliveryMethod::External),
            }],
            ..Default::default()
        };

        let mut sources = vec![source.clone()];
        append_external_subtitles(
            &mut sources,
            &[external.clone()],
            &[],
            Some(&embed_profile),
            item_id,
            "test-key",
            DEDUP_ON,
        );
        assert_eq!(
            sources[0]
                .media_streams
                .len(),
            1
        );

        for profile in [None, Some(&external_profile)] {
            let mut sources = vec![source.clone()];
            append_external_subtitles(
                &mut sources,
                &[external.clone()],
                &[],
                profile,
                item_id,
                "test-key",
                DEDUP_ON,
            );
            assert_eq!(
                sources[0]
                    .media_streams
                    .len(),
                2
            );
        }

        let mut forced = external.clone();
        forced.is_forced = true;
        let mut sources = vec![source.clone()];
        append_external_subtitles(
            &mut sources,
            &[forced],
            &[],
            Some(&embed_profile),
            item_id,
            "test-key",
            DEDUP_ON,
        );
        assert_eq!(
            sources[0]
                .media_streams
                .len(),
            2
        );

        let mut external_source = source;
        external_source.media_streams[0].delivery_method =
            Some(api::SubtitleDeliveryMethod::External);
        let mut sources = vec![external_source];
        append_external_subtitles(
            &mut sources,
            &[external],
            &[],
            Some(&embed_profile),
            item_id,
            "test-key",
            DEDUP_ON,
        );
        assert_eq!(
            sources[0]
                .media_streams
                .len(),
            2
        );
    }

    #[test]
    fn dutch_bibliographic_code_does_not_duplicate_embedded_subtitle() {
        let source = api::MediaSourceInfo {
            id: Uuid::new_v4(),
            media_streams: vec![api::MediaStream {
                index: 2,
                type_: Some(api::MediaStreamType::Subtitle),
                codec: Some("PGSSUB".into()),
                language: Some("nld".into()),
                ..Default::default()
            }],
            remux: Some(api::MediaSourceRemuxInfo {
                source: Some(api::ProbeOrigin::RemuxDb),
                ..Default::default()
            }),
            ..Default::default()
        };
        let external = crate::addons::SubtitleInfo {
            id: "dutch".into(),
            url: None,
            lang: Some("dut".into()),
            is_forced: false,
            is_hi: false,
            filename: None,
            from_trusted: None,
            ai_translated: None,
        };
        let profile = api::DeviceProfile {
            subtitle_profiles: vec![api::SubtitleProfile {
                format: Some("pgs".into()),
                method: Some(api::SubtitleDeliveryMethod::Embed),
            }],
            ..Default::default()
        };

        let mut sources = vec![source];
        append_external_subtitles(
            &mut sources,
            &[external],
            &[],
            Some(&profile),
            Uuid::new_v4(),
            "test-key",
            DEDUP_ON,
        );
        assert_eq!(
            sources[0]
                .media_streams
                .len(),
            1
        );
    }

    #[test]
    fn dedup_disabled_keeps_multiple_external_candidates_up_to_the_cap() {
        let source = api::MediaSourceInfo {
            id: Uuid::new_v4(),
            media_streams: vec![api::MediaStream {
                index: 2,
                type_: Some(api::MediaStreamType::Subtitle),
                codec: Some("pgssub".into()),
                language: Some("eng".into()),
                ..Default::default()
            }],
            remux: Some(api::MediaSourceRemuxInfo {
                source: Some(api::ProbeOrigin::Ffprobe),
                ..Default::default()
            }),
            ..Default::default()
        };
        let embed_profile = api::DeviceProfile {
            subtitle_profiles: vec![api::SubtitleProfile {
                format: Some("pgs".into()),
                method: Some(api::SubtitleDeliveryMethod::Embed),
            }],
            ..Default::default()
        };
        let regular = crate::addons::SubtitleInfo {
            id: "regular".into(),
            url: None,
            lang: Some("en".into()),
            is_forced: false,
            is_hi: false,
            filename: None,
            from_trusted: None,
            ai_translated: None,
        };
        let mut forced = regular.clone();
        forced.id = "forced".into();
        forced.is_forced = true;

        // Dedup on: the embedded English track (Embed-supported) suppresses
        // both external candidates for English.
        let mut sources = vec![source.clone()];
        append_external_subtitles(
            &mut sources,
            &[regular.clone(), forced.clone()],
            &[],
            Some(&embed_profile),
            Uuid::new_v4(),
            "test-key",
            DEDUP_ON,
        );
        assert_eq!(
            sources[0]
                .media_streams
                .len(),
            1,
            "dedup on: embedded already covers English, no external added"
        );

        // Dedup off with a cap of 2: both externals show up alongside the
        // embedded track, even though the embedded one would otherwise
        // suppress a same-flavor external.
        let mut sources = vec![source];
        append_external_subtitles(
            &mut sources,
            &[regular, forced],
            &[],
            Some(&embed_profile),
            Uuid::new_v4(),
            "test-key",
            SubtitleDedupSettings {
                enabled: false,
                max_external_per_language_when_disabled: 2,
            },
        );
        assert_eq!(
            sources[0]
                .media_streams
                .len(),
            3,
            "dedup off: embedded track plus both external candidates, up to the cap"
        );
    }

    fn dutch_pgs_source() -> api::MediaSourceInfo {
        api::MediaSourceInfo {
            id: Uuid::new_v4(),
            media_streams: vec![api::MediaStream {
                index: 2,
                type_: Some(api::MediaStreamType::Subtitle),
                codec: Some("pgssub".into()),
                language: Some("dut".into()),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn no_embed_profile() -> api::DeviceProfile {
        // Supports the codec for some non-Embed method (or not at all) —
        // either way, profile_embeds_subtitle_codec must say no.
        api::DeviceProfile::default()
    }

    #[test]
    fn drops_unsupported_embedded_subtitle_with_a_matching_external() {
        let mut source = dutch_pgs_source();
        let external = crate::addons::SubtitleInfo {
            id: "dutch".into(),
            url: None,
            lang: Some("dut".into()),
            is_forced: false,
            is_hi: false,
            filename: None,
            from_trusted: None,
            ai_translated: None,
        };
        drop_unsupported_embedded_subtitles_with_external_match(
            &mut source,
            &[external],
            Some(&no_embed_profile()),
        );
        assert!(
            source
                .media_streams
                .is_empty(),
            "unsupported embedded subtitle with a matching external must be dropped"
        );
    }

    #[test]
    fn keeps_unsupported_embedded_subtitle_without_a_language_match() {
        let mut source = dutch_pgs_source();
        // Different language — not a replacement for the Dutch track.
        let external = crate::addons::SubtitleInfo {
            id: "english".into(),
            url: None,
            lang: Some("eng".into()),
            is_forced: false,
            is_hi: false,
            filename: None,
            from_trusted: None,
            ai_translated: None,
        };
        drop_unsupported_embedded_subtitles_with_external_match(
            &mut source,
            &[external],
            Some(&no_embed_profile()),
        );
        assert_eq!(
            source
                .media_streams
                .len(),
            1,
            "a different-language external must not hide the embedded track"
        );
    }

    #[test]
    fn keeps_embed_supported_subtitle_even_with_a_matching_external() {
        let mut source = dutch_pgs_source();
        let external = crate::addons::SubtitleInfo {
            id: "dutch".into(),
            url: None,
            lang: Some("dut".into()),
            is_forced: false,
            is_hi: false,
            filename: None,
            from_trusted: None,
            ai_translated: None,
        };
        let profile = api::DeviceProfile {
            subtitle_profiles: vec![api::SubtitleProfile {
                format: Some("pgs".into()),
                method: Some(api::SubtitleDeliveryMethod::Embed),
            }],
            ..Default::default()
        };
        drop_unsupported_embedded_subtitles_with_external_match(
            &mut source,
            &[external],
            Some(&profile),
        );
        assert_eq!(
            source
                .media_streams
                .len(),
            1,
            "an embed-supported subtitle is free to deliver — never drop it in favor of external"
        );
    }

    fn source_with_subtitles(streams: Vec<api::MediaStream>) -> api::MediaSourceInfo {
        api::MediaSourceInfo {
            id: Uuid::new_v4(),
            media_streams: streams,
            ..Default::default()
        }
    }

    #[test]
    fn requested_ass_output_is_planned_for_an_srt_track() {
        let source = source_with_subtitles(vec![api::MediaStream {
            index: 2,
            type_: Some(api::MediaStreamType::Subtitle),
            codec: Some("subrip".into()),
            is_external: false,
            ..Default::default()
        }]);

        let plain = extractable_subtitles(&source, None);
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].cache_codec, api::SubtitleCodec::Srt);

        let planned =
            extractable_subtitles(&source, Some((2, &api::SubtitleCodec::Ass)));
        let ass = planned
            .iter()
            .find(|s| s.cache_codec == api::SubtitleCodec::Ass)
            .expect("the requested ASS output must be planned");
        assert_eq!(ass.stream_index, 2);
        assert_eq!(ass.ffmpeg_codec, "ass");
        assert_eq!(planned.len(), 2);

        let other = extractable_subtitles(&source, Some((9, &api::SubtitleCodec::Ass)));
        assert_eq!(other.len(), 1);
    }

    #[test]
    fn extractable_subtitles_skips_external_and_image_streams_and_orders_by_index() {
        let source = source_with_subtitles(vec![
            api::MediaStream {
                index: 5,
                type_: Some(api::MediaStreamType::Subtitle),
                codec: Some("subrip".into()),
                is_external: false,
                ..Default::default()
            },
            api::MediaStream {
                index: 1,
                type_: Some(api::MediaStreamType::Subtitle),
                codec: Some("mov_text".into()),
                is_external: false,
                ..Default::default()
            },
            api::MediaStream {
                index: 3,
                type_: Some(api::MediaStreamType::Subtitle),
                codec: Some("subrip".into()),
                is_external: true,
                ..Default::default()
            },
            api::MediaStream {
                index: 2,
                type_: Some(api::MediaStreamType::Subtitle),
                codec: Some("hdmv_pgs_subtitle".into()),
                is_external: false,
                ..Default::default()
            },
            api::MediaStream {
                index: 0,
                type_: Some(api::MediaStreamType::Audio),
                codec: Some("aac".into()),
                ..Default::default()
            },
        ]);

        let extractable = extractable_subtitles(&source, None);
        let indexes: Vec<i64> = extractable
            .iter()
            .map(|s| s.stream_index)
            .collect();
        assert_eq!(
            indexes,
            vec![1, 5],
            "external, non-subtitle, and image/bitmap subtitle streams must be \
             excluded (ffmpeg can't convert PGS/VobSub to a text codec — one in \
             the batch would fail the whole ffmpeg invocation), remaining ones \
             sorted by index"
        );
        // map_spec is the ordinal among *all* embedded subtitle streams
        // (image included, per ffmpeg's own 0:s:N numbering) — index 2 (the
        // PGS stream) consumes ordinal 1, so stream 5 lands on "0:s:2", not
        // "0:s:1". Getting this wrong maps the wrong track in the batch
        // extraction ffmpeg command.
        assert_eq!(extractable[0].map_spec, "0:s:0");
        assert_eq!(extractable[1].map_spec, "0:s:2");
        assert_eq!(extractable[0].cache_codec, api::SubtitleCodec::Srt);
        assert_eq!(extractable[1].cache_codec, api::SubtitleCodec::Srt);
    }

    #[test]
    fn is_known_text_codec_rejects_unknown_and_missing_codecs() {
        let sub = |index: i64, codec: Option<&str>| api::MediaStream {
            index,
            type_: Some(api::MediaStreamType::Subtitle),
            codec: codec.map(Into::into),
            ..Default::default()
        };
        let source = source_with_subtitles(vec![
            sub(0, Some("subrip")),
            sub(1, Some("xsub")),
            sub(2, None),
            sub(3, Some("hdmv_pgs_subtitle")),
        ]);
        let known: Vec<bool> = source
            .media_streams
            .iter()
            .map(is_known_text_codec)
            .collect();
        assert_eq!(known, [true, false, false, false]);
    }

    #[test]
    fn extractable_subtitles_gives_native_ass_a_second_srt_output() {
        // A VTT/SRT/JSON request is always served from the SRT cache. Without
        // this second output, a stream whose native codec is ASS would only
        // ever get a .ass file, and such a request would 404 forever.
        let source = source_with_subtitles(vec![api::MediaStream {
            index: 0,
            type_: Some(api::MediaStreamType::Subtitle),
            codec: Some("ass".into()),
            ..Default::default()
        }]);

        let extractable = extractable_subtitles(&source, None);
        assert_eq!(
            extractable.len(),
            2,
            "a native ASS stream must produce both an .ass and an .srt output"
        );
        assert!(
            extractable
                .iter()
                .all(|s| s.stream_index == 0 && s.map_spec == "0:s:0"),
            "both outputs come from the same mapped input stream: {extractable:?}"
        );
        assert!(
            extractable
                .iter()
                .any(|s| s.cache_codec == api::SubtitleCodec::Ass)
                && extractable
                    .iter()
                    .any(|s| s.cache_codec == api::SubtitleCodec::Srt),
            "expected one native-ASS (copy) output and one SRT-converted output: {extractable:?}"
        );
        let ass_output = extractable
            .iter()
            .find(|s| s.cache_codec == api::SubtitleCodec::Ass)
            .unwrap();
        assert_eq!(
            ass_output.ffmpeg_codec, "copy",
            "native ASS should be stream-copied, not re-encoded"
        );
        let srt_output = extractable
            .iter()
            .find(|s| s.cache_codec == api::SubtitleCodec::Srt)
            .unwrap();
        assert_eq!(srt_output.ffmpeg_codec, "srt");
    }

    #[test]
    fn build_subtitle_extraction_args_has_one_input_and_one_map_pair_per_output() {
        let data_dir = std::path::Path::new("/tmp/remux-subtitle-test");
        let item_id = Uuid::new_v4();
        // subrip -> 1 SRT output. ass -> 2 outputs (ass copy + srt convert),
        // both mapped from the same input stream. PGS is excluded entirely.
        let source = source_with_subtitles(vec![
            api::MediaStream {
                index: 0,
                type_: Some(api::MediaStreamType::Subtitle),
                codec: Some("subrip".into()),
                ..Default::default()
            },
            api::MediaStream {
                index: 1,
                type_: Some(api::MediaStreamType::Subtitle),
                codec: Some("ass".into()),
                ..Default::default()
            },
            api::MediaStream {
                index: 2,
                type_: Some(api::MediaStreamType::Subtitle),
                codec: Some("hdmv_pgs_subtitle".into()),
                ..Default::default()
            },
        ]);
        let streams = extractable_subtitles(&source, None);
        assert_eq!(streams.len(), 3, "1 (subrip) + 2 (ass) + 0 (pgs, excluded)");

        let (args, cache_paths) = build_subtitle_extraction_args(
            data_dir,
            "http://example.com/stream.mkv",
            item_id,
            Uuid::new_v4(),
            &streams,
        )
        .expect("arg construction should not fail for a valid path");

        // Exactly one -i (single ffmpeg process reads the file once), and it
        // comes before every -map (single input feeding every output).
        let input_positions: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| a.as_str() == "-i")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            input_positions.len(),
            1,
            "expected exactly one -i: {args:?}"
        );
        let input_pos = input_positions[0];
        assert_eq!(args[input_pos + 1], "http://example.com/stream.mkv");
        let first_map_pos = args
            .iter()
            .position(|a| a == "-map")
            .expect("expected at least one -map");
        assert!(
            input_pos < first_map_pos,
            "-i must precede every -map: {args:?}"
        );

        // http source should get reconnect args.
        assert!(
            args.contains(&"-reconnect".to_string()),
            "http input should carry reconnect args: {args:?}"
        );

        // One -map/-c:s pair per output, and one (tmp, final) path pair per output.
        let map_count = args
            .iter()
            .filter(|a| a.as_str() == "-map")
            .count();
        let codec_count = args
            .iter()
            .filter(|a| a.as_str() == "-c:s")
            .count();
        assert_eq!(map_count, 3, "expected one -map per output: {args:?}");
        assert_eq!(codec_count, 3, "expected one -c:s per output: {args:?}");
        assert_eq!(cache_paths.len(), 3);

        // subrip (ordinal 0) maps once; ass (ordinal 1) maps twice — once for
        // each of its two outputs — from the same input stream.
        let map_specs: Vec<&String> = args
            .windows(2)
            .filter(|w| w[0] == "-map")
            .map(|w| &w[1])
            .collect();
        assert_eq!(map_specs, vec!["0:s:0", "0:s:1", "0:s:1"]);

        // ffmpeg is only ever pointed at .tmp output paths — the final cache
        // path is populated by an atomic rename after success, never written
        // to directly, so a concurrent reader can never see a partial file.
        for (tmp, final_path) in &cache_paths {
            assert!(
                tmp.to_str()
                    .unwrap()
                    .ends_with(".tmp"),
                "ffmpeg output path should be a .tmp path: {tmp:?}"
            );
            assert_ne!(tmp, final_path);
            assert!(
                args.contains(
                    &tmp.to_str()
                        .unwrap()
                        .to_string()
                ),
                "the .tmp path must be what's actually passed to ffmpeg: {args:?}"
            );
            assert!(
                !args.contains(
                    &final_path
                        .to_str()
                        .unwrap()
                        .to_string()
                ),
                "the final cache path must never be passed to ffmpeg directly: {args:?}"
            );
        }
    }

    #[test]
    fn build_subtitle_extraction_args_skips_reconnect_for_local_files() {
        let data_dir = std::path::Path::new("/tmp/remux-subtitle-test");
        let source = source_with_subtitles(vec![api::MediaStream {
            index: 0,
            type_: Some(api::MediaStreamType::Subtitle),
            codec: Some("subrip".into()),
            ..Default::default()
        }]);
        let streams = extractable_subtitles(&source, None);

        let (args, _) = build_subtitle_extraction_args(
            data_dir,
            "/mnt/media/movie.mkv",
            Uuid::new_v4(),
            Uuid::new_v4(),
            &streams,
        )
        .expect("arg construction should not fail for a valid path");

        assert!(
            !args.contains(&"-reconnect".to_string()),
            "a local path should not get http reconnect args: {args:?}"
        );
    }
}
