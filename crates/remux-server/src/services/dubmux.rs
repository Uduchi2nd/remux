//! Vietnamese-dub augmentation: pair vnphim dub streams with high-quality
//! debrid/usenet sources through the seedbox `dubmux` service and expose the
//! result as extra, first-sorted stream rows ("[+VN dub · hotphim] …").
//!
//! The muxer extracts the dub audio once, cross-correlates it against the HQ
//! video (duration ±1.5 s, lag agreement across three windows) and serves a
//! stream-copy HLS mux: HQ video + dub (default) + every original audio
//! track. Nothing here touches nimo's uplink or the Cloudflare tunnel — the
//! client plays the muxer's public URL directly.
//!
//! Rows are synthetic `db::Media` streams cloned from the HQ source, so the
//! remote-HLS delivery, IsRemote/Http metadata, subtitle aliasing and probe
//! machinery all apply unchanged. Embedded subtitle tracks are dropped from
//! the variant (MPEG-TS can't carry text subs); addon/vnphim external
//! subtitles keep working.
use crate::{AppContext, api, db, stream::StreamDescriptor};
use remux_sdks::remux::{MediaStream, MediaStreamType, VideoContainer};
use std::time::Duration;
use tracing::{debug, info, warn};
use uuid::Uuid;

// Every listed HQ release is tried (bounded): different cuts of the same
// episode coexist (Đầu Xuân Tươi Sáng E02: 2160p at 2902.9 s, 1080p at
// 2915.0 s) and only the matching cut passes the duration gate. Results are
// cached by the muxer, so extra pairs cost one small request each.
const MAX_HQ: usize = 6;
const MAX_DUBS: usize = 3;
const DUB_TOKENS: [&str; 2] = ["Vietnamese.ThuyetMinh", "Vietnamese.LongTieng"];

pub(crate) struct DubmuxConfig<'a> {
    pub api: &'a str,
    pub public: &'a str,
    pub wait_secs: u64,
}

impl<'a> DubmuxConfig<'a> {
    pub fn from(config: &'a crate::Config) -> Option<Self> {
        Some(Self {
            api: config
                .dubmux_url
                .as_deref()?
                .trim_end_matches('/'),
            public: config
                .dubmux_public_url
                .as_deref()?
                .trim_end_matches('/'),
            wait_secs: config.dubmux_prepare_wait_secs,
        })
    }
}

fn http_url(stream: &db::Media) -> Option<&str> {
    match &stream
        .stream_info
        .as_ref()?
        .descriptor
    {
        StreamDescriptor::Http { url, .. } => Some(url.as_str()),
        _ => None,
    }
}

fn filename(stream: &db::Media) -> Option<&str> {
    stream
        .stream_info
        .as_ref()?
        .filename
        .as_deref()
}

/// vnphim dub streams: release names carry the language/audio tokens and the
/// provider ("…Vietnamese.ThuyetMinh.hotphim.mp4").
fn dub_provider(name: &str) -> Option<String> {
    let token = DUB_TOKENS
        .iter()
        .find(|t| name.contains(*t))?;
    let rest = &name[name.find(token)? + token.len()..];
    let provider = rest
        .trim_start_matches('.')
        .split('.')
        .next()
        .filter(|s| !s.is_empty())?;
    Some(provider.to_ascii_lowercase())
}

pub(crate) fn is_dubmux_row(stream: &db::Media) -> bool {
    filename(stream).is_some_and(|f| f.contains(".VNDub-"))
}

fn is_hq_candidate(stream: &db::Media) -> bool {
    let Some(url) = http_url(stream) else {
        return false;
    };
    let Some(name) = filename(stream) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    !is_dubmux_row(stream)
        && !url.contains("vnphim")
        && dub_provider(name).is_none()
        && !lower.contains(".vietsub.")
        && (lower.ends_with(".mkv") || lower.ends_with(".mp4"))
        && stream
            .stream_info
            .as_ref()
            .is_some_and(|si| !si.is_p2p())
}

fn dub_id(media: &db::Media, name: &str) -> String {
    Uuid::new_v5(&media.id, format!("dub:{name}").as_bytes())
        .simple()
        .to_string()
}

fn row_id(media: &db::Media, dub: &str, hq: &Uuid) -> Uuid {
    Uuid::new_v5(
        &media.id,
        format!("dubmux:{dub}:{}", hq.simple()).as_bytes(),
    )
}

#[derive(serde::Deserialize)]
struct PrepareReply {
    status: String,
    #[serde(default)]
    stage: Option<String>,
}

async fn prepare(
    client: &reqwest::Client,
    cfg: &DubmuxConfig<'_>,
    dub_id: &str,
    dub_url: &str,
    hq_id: &str,
    hq_url: &str,
    wait: u64,
    priority: u32,
) -> anyhow::Result<PrepareReply> {
    let body = serde_json::json!({
        "dub": {"id": dub_id, "url": dub_url},
        "video": {"id": hq_id, "url": hq_url},
        "wait": wait,
        "priority": priority,
    });
    let reply = client
        .post(format!("{}/prepare", cfg.api))
        .timeout(Duration::from_secs(wait + 8))
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json::<PrepareReply>()
        .await?;
    Ok(reply)
}

/// Rebuild the HQ probe as the mux's track layout: video, then the dub as the
/// default audio track, then every original audio track; embedded subtitles
/// are not in the TS. Returns None when the HQ was never probed (the mux
/// then just gets probed like any other remote HLS source).
fn mux_probe(hq: &api::MediaSourceInfo, provider: &str) -> api::MediaSourceInfo {
    let mut probe = hq.clone();
    probe.container = Some(VideoContainer::Other("hls".into()));
    let mut streams: Vec<MediaStream> = Vec::new();
    for s in &hq.media_streams {
        if matches!(s.type_, Some(MediaStreamType::Video)) {
            let mut v = s.clone();
            v.index = streams.len() as i64;
            streams.push(v);
        }
    }
    let dub = MediaStream {
        type_: Some(MediaStreamType::Audio),
        index: streams.len() as i64,
        codec: Some("aac".into()),
        language: Some("vie".into()),
        channels: Some(2),
        sample_rate: Some(44100),
        is_default: Some(true),
        is_external: false,
        display_title: Some(format!(
            "Vietnamese · Thuyết Minh ({provider}) - AAC - Stereo - Default"
        )),
        ..Default::default()
    };
    streams.push(dub);
    for s in &hq.media_streams {
        if matches!(s.type_, Some(MediaStreamType::Audio)) {
            let mut a = s.clone();
            a.index = streams.len() as i64;
            a.is_default = Some(false);
            if let Some(dt) = a
                .display_title
                .as_mut()
            {
                *dt = dt
                    .replace(" - Default", "")
                    .to_string();
            }
            streams.push(a);
        }
    }
    // Embedded subtitle tracks stay listed (MPEG-TS can't carry text, so the
    // mux has none): remux extracts them from the HQ file itself — its URL is
    // the mux URL's `video` query parameter (see api/subtitles.rs). Indices
    // are offset so they never collide with the rebuilt audio indices; only
    // their relative order matters for the `0:s:<ordinal>` map.
    for s in &hq.media_streams {
        if matches!(s.type_, Some(MediaStreamType::Subtitle)) && !s.is_external {
            let mut t = s.clone();
            t.index = SUBTITLE_INDEX_OFFSET + s.index;
            t.is_default = Some(false);
            streams.push(t);
        }
    }
    probe.media_streams = streams;
    probe
}

/// Subtitle streams on a "[+VN dub]" row keep the HQ's ordering at this offset.
pub(crate) const SUBTITLE_INDEX_OFFSET: i64 = 100;

/// Is this a muxer master-playlist path (a "[+VN dub]" row's source path)?
pub(crate) fn is_mux_path(path: Option<&str>) -> bool {
    path.is_some_and(|p| p.contains("/mux/") && p.contains("master.m3u8"))
}

/// The HQ release a "[+VN dub]" row was built from: the muxer URL carries it
/// as `?video=<url>`. Subtitle extraction reads the HQ file, not the mux.
pub(crate) fn hq_url_of(stream: &db::Media) -> Option<String> {
    let url = http_url(stream)?;
    if !url.contains("/mux/") {
        return None;
    }
    url::Url::parse(url)
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == "video")
        .map(|(_, v)| v.into_owned())
}

/// Make sure a "[+VN dub]" stream row exists for every (HQ source, vnphim dub)
/// pair the muxer has accepted, creating/refreshing rows as needed. Returns
/// the rows that were added or updated (already-present rows are re-upserted
/// so their `updated_at` follows the parent's refresh). `wait_secs` bounds
/// how long a still-running preparation is waited for — 0 on background
/// refreshes, a few seconds on a playback request.
/// Muxer priorities (lower = sooner). The pair's rank within its episode is
/// added on top, so every episode's FIRST dub version is prepared before
/// anyone's second variant.
pub(crate) const PRIORITY_PLAYBACK: u32 = 0;
pub(crate) const PRIORITY_WALK: u32 = 100;
pub(crate) const PRIORITY_BACKGROUND: u32 = 300;
/// Variants (second dub provider / second-best release) of an episode rank
/// behind the first pairs of the whole walk (up to 12 episodes).
const VARIANT_PENALTY: u32 = 50;

pub(crate) async fn ensure_dub_rows(
    ctx: &AppContext,
    media: &db::Media,
    streams: &[db::Media],
    wait_secs: u64,
    priority: u32,
) -> Vec<db::Media> {
    let Some(cfg) = DubmuxConfig::from(&ctx.config) else {
        return vec![];
    };
    if !matches!(media.kind, db::MediaKind::Movie | db::MediaKind::Episode) {
        return vec![];
    }
    // Existing dub rows come from the DB, not from `streams`: on a refresh
    // the caller passes the freshly fetched addon list, which never
    // contains our synthetic rows (that is exactly when carry-over matters).
    let stored = existing_dub_rows(ctx, media).await;
    let existing: Vec<&db::Media> = streams
        .iter()
        .filter(|s| is_dubmux_row(s))
        .chain(
            stored
                .iter()
                .filter(|r| {
                    !streams
                        .iter()
                        .any(|s| s.id == r.id)
                }),
        )
        .collect();
    let dubs: Vec<(&db::Media, String, String)> = streams
        .iter()
        .filter(|s| !is_dubmux_row(s))
        .filter_map(|s| {
            let name = filename(s)?;
            let provider = dub_provider(name)?;
            http_url(s)?;
            Some((s, provider, dub_id(media, name)))
        })
        .take(MAX_DUBS)
        .collect();
    let hqs: Vec<&db::Media> = streams
        .iter()
        .filter(|s| is_hq_candidate(s))
        .take(MAX_HQ)
        .collect();
    if hqs.is_empty() {
        return vec![];
    }
    let now = chrono::Utc::now().naive_utc();
    if dubs.is_empty() {
        // The dub addon answered nothing this time (vnphim's upstream or
        // its VN proxy was down): the muxes already made are still good,
        // so keep the rows that were built on releases still listed.
        return carry_over(ctx, media, &existing, &hqs, &[], now).await;
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .build()
        .unwrap_or_default();
    let mut rows = Vec::new();
    let mut rejected: Vec<Uuid> = Vec::new();
    let mut wait = wait_secs;
    let mut failures = 0u32;
    let mut pairs_seen: u32 = 0;
    'pairs: for hq in hqs
        .iter()
        .copied()
    {
        let hq_url = http_url(hq).unwrap();
        let hq_id = hq
            .id
            .simple()
            .to_string();
        for (dub, provider, dub_id) in &dubs {
            let dub_url = http_url(dub).unwrap();
            let pair_priority = if pairs_seen == 0 {
                priority
            } else {
                priority + VARIANT_PENALTY + pairs_seen.min(40)
            };
            pairs_seen += 1;
            let reply = match prepare(
                &client, &cfg, dub_id, dub_url, &hq_id, hq_url, wait, pair_priority,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    // A busy muxer answers late, not wrong: the job it was
                    // asked for keeps running server-side. Skip this pair
                    // and give up on the walk only if it keeps failing.
                    failures += 1;
                    warn!(item = %media.id, failures, "dubmux prepare failed: {e:#}");
                    wait = 0;
                    if failures >= 2 {
                        break 'pairs;
                    }
                    continue;
                }
            };
            // Only the first pair spends the caller's wait budget; the muxer
            // runs the other preparations concurrently anyway.
            wait = 0;
            if reply.status != "ready" {
                debug!(item = %media.id, provider, hq = %hq.id, status = reply.status,
                       stage = ?reply.stage, "dubmux pair not ready");
                if reply.status == "rejected" {
                    rejected.push(row_id(media, dub_id, &hq.id));
                }
                continue;
            }
            let mut row = hq.clone();
            row.id = row_id(media, dub_id, &hq.id);
            row.parent_id = Some(media.id);
            row.idx = Some(-(rows.len() as i64) - 1);
            row.created_at = now;
            row.updated_at = now;
            row.title = format!("[+VN dub · {provider}] {}", hq.title);
            if let Some(si) = row
                .stream_info
                .as_mut()
            {
                let stem = filename(hq)
                    .and_then(|f| {
                        f.rsplit_once('.')
                            .map(|(s, _)| s)
                    })
                    .unwrap_or("stream");
                si.filename = Some(format!("{stem}.VNDub-{provider}.m3u8"));
                si.descriptor = StreamDescriptor::Http {
                    url: format!(
                        "{}/mux/{dub_id}/{hq_id}/master.m3u8?video={}",
                        cfg.public,
                        urlencoding::encode(hq_url)
                    ),
                    request_headers: Default::default(),
                    response_headers: Default::default(),
                };
            }
            // Even a filename-guess probe of the HQ gives the row its track
            // list (dub first, default) from the moment it exists; the next
            // refresh rebuilds it from the HQ's real probe. Without it a
            // just-created row showed no Vietnamese track until then.
            row.probe_data = hq
                .probe_data
                .as_ref()
                .map(|p| mux_probe(p, provider));
            rows.push(row);
            // HQ releases are walked in quality order, so the cap keeps the
            // best video and its dub variants (backups against a bad dub)
            // rather than one dub across every release.
            if rows.len()
                >= ctx
                    .config
                    .dubmux_max_rows as usize
            {
                break 'pairs;
            }
        }
    }
    // Rows this walk could not rebuild (muxer busy, a pair not answered in
    // time) but which were fine before stay, unless the muxer now rejects
    // the pair or the cap is reached.
    let max_rows = ctx
        .config
        .dubmux_max_rows as usize;
    if rows.len() < max_rows {
        let have: Vec<Uuid> = rows
            .iter()
            .map(|r| r.id)
            .collect();
        let keep: Vec<&db::Media> = existing
            .iter()
            .copied()
            .filter(|r| !have.contains(&r.id))
            .collect();
        for row in carried_rows(&keep, &hqs, &rejected, now) {
            if rows.len() >= max_rows {
                break;
            }
            debug!(item = %media.id, row = %row.id, "dubmux row carried over");
            rows.push(row);
        }
    }
    if rows.is_empty() {
        return rows;
    }
    // Sources sort by idx ascending, so the first-built row (best HQ
    // release) gets the most negative idx.
    let n = rows.len() as i64;
    for (i, row) in rows
        .iter_mut()
        .enumerate()
    {
        row.idx = Some(i as i64 - n);
    }
    if let Err(e) = db::Media::upsert(&ctx.db, &rows).await {
        warn!(item = %media.id, "dubmux row upsert failed: {e:#}");
        return vec![];
    }
    info!(item = %media.id, rows = rows.len(), "dubmux rows ready");
    // A playback request (wait > 0) means the viewer is about to pick one of
    // these rows: start every mux now so the player's GET finds a finished
    // VOD playlist instead of an in-progress EVENT one (VidHub shows those
    // as a 0-length live stream and stops after a few segments).
    if wait_secs > 0 {
        let urls: Vec<String> = rows
            .iter()
            .filter_map(http_url)
            .map(|u| u.replacen(cfg.public, cfg.api, 1))
            .collect();
        let item = media.id;
        tokio::spawn(async move {
            for url in urls {
                start_mux(&url, item).await;
            }
        });
    }
    rows
}

/// Every "[+VN dub]" row stored for an item, fresh or stale (`Media::streams`
/// hides rows older than the last refresh, which is what carry-over needs).
async fn existing_dub_rows(ctx: &AppContext, media: &db::Media) -> Vec<db::Media> {
    let ids: Vec<Uuid> = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM media WHERE parent_id = ? AND title LIKE '[+VN dub%'",
    )
    .bind(media.id)
    .fetch_all(&ctx.db)
    .await
    .unwrap_or_default();
    let mut rows = Vec::new();
    for id in ids {
        if let Ok(Some(row)) = db::Media::get_by_id(&ctx.db, &id).await {
            if is_dubmux_row(&row) {
                rows.push(row);
            }
        }
    }
    rows
}

/// The HQ source id a dub row was built on (the `/mux/<dub>/<hq>/` path).
fn hq_id_of(row: &db::Media) -> Option<Uuid> {
    let url = http_url(row)?;
    let rest = url
        .split("/mux/")
        .nth(1)?;
    let hq = rest
        .split('/')
        .nth(1)?;
    Uuid::parse_str(hq).ok()
}

/// Existing dub rows worth keeping: their HQ release is still listed and the
/// muxer has not rejected the pair. The mux URL's `?video=` is refreshed to
/// the release's current URL so a swept mux can be rebuilt.
fn carried_rows(
    existing: &[&db::Media],
    hqs: &[&db::Media],
    rejected: &[Uuid],
    now: chrono::NaiveDateTime,
) -> Vec<db::Media> {
    let mut out = Vec::new();
    for row in existing {
        if rejected.contains(&row.id) {
            continue;
        }
        let Some(hq_id) = hq_id_of(row) else {
            continue;
        };
        let Some(hq) = hqs
            .iter()
            .find(|h| h.id == hq_id)
        else {
            continue;
        };
        let Some(hq_url) = http_url(hq) else {
            continue;
        };
        let mut row = (*row).clone();
        row.updated_at = now;
        if let Some(si) = row
            .stream_info
            .as_mut()
        {
            if let StreamDescriptor::Http { url, .. } = &mut si.descriptor {
                if let Some((base, _)) = url.split_once("?video=") {
                    *url = format!("{base}?video={}", urlencoding::encode(hq_url));
                }
            }
        }
        out.push(row);
    }
    out
}

async fn carry_over(
    ctx: &AppContext,
    media: &db::Media,
    existing: &[&db::Media],
    hqs: &[&db::Media],
    rejected: &[Uuid],
    now: chrono::NaiveDateTime,
) -> Vec<db::Media> {
    let rows = carried_rows(existing, hqs, rejected, now);
    if rows.is_empty() {
        return rows;
    }
    if let Err(e) = db::Media::upsert(&ctx.db, &rows).await {
        warn!(item = %media.id, "dubmux carry-over upsert failed: {e:#}");
        return vec![];
    }
    info!(item = %media.id, rows = rows.len(), "dubmux rows carried over (no dub streams listed)");
    rows
}

/// Ask the muxer to start (or confirm) the mux behind a dub row's master URL
/// without waiting for it. `url` must already point at the API host.
pub(crate) async fn start_mux(url: &str, item: Uuid) {
    let start = url.replacen("/master.m3u8", "/start", 1);
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default();
    match client
        .post(&start)
        .send()
        .await
    {
        Ok(r)
            if r.status()
                .is_success() =>
        {
            debug!(item = %item, "dubmux mux started")
        }
        Ok(r) => warn!(item = %item, status = %r.status(), "dubmux mux start refused"),
        Err(e) => warn!(item = %item, "dubmux mux start failed: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_parsed_from_release_name() {
        assert_eq!(
            dub_provider(
                "Dau.Xuan.Tuoi.Sang.2026.S01E02.1080p.WEB-DL.Vietnamese.ThuyetMinh.hotphim.mp4"
            ),
            Some("hotphim".into())
        );
        assert_eq!(
            dub_provider(
                "X.S01E02.1080p.WEB-DL.Vietnamese.ThuyetMinh.kkphim.ProxiedVN.mp4"
            ),
            Some("kkphim".into())
        );
        assert_eq!(
            dub_provider(
                "Pham.Nhan.Tu.Tien.S01E191.2160p.WEB-DL.Vietnamese.LongTieng.yanhh3d.mp4"
            ),
            Some("yanhh3d".into())
        );
        assert_eq!(
            dub_provider("X.S01E02.1080p.WEB-DL.Chinese.Vietsub.hotphim.mp4"),
            None
        );
        assert_eq!(
            dub_provider("The.Love.Hypothesis.2026.1080p.AMZN.WEB-DL-FLUX.mkv"),
            None
        );
    }

    fn http_media(url: &str, filename: &str) -> db::Media {
        db::Media {
            stream_info: Some(crate::stream::StreamInfo {
                descriptor: StreamDescriptor::Http {
                    url: url.into(),
                    request_headers: Default::default(),
                    response_headers: Default::default(),
                },
                filename: Some(filename.into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn hq_candidates_exclude_vnphim_hls_and_dub_rows() {
        assert!(is_hq_candidate(&http_media(
            "https://aiostreams.example/api/v1/debrid/playback/x/Y.mkv",
            "The.Early.Spring.S01E02.1080p.WEB-DL.mkv"
        )));
        assert!(!is_hq_candidate(&http_media(
            "https://vnphim.uduchi.com/p/b.token.m3u8/X.m3u8",
            "X.S01E02.1080p.WEB-DL.Vietnamese.ThuyetMinh.hotphim.mp4"
        )));
        assert!(!is_hq_candidate(&http_media(
            "https://dubmux.example/mux/a/b/master.m3u8",
            "The.Early.Spring.S01E02.1080p.WEB-DL.VNDub-hotphim.m3u8"
        )));
    }

    #[test]
    fn mux_probe_puts_dub_first_as_default_and_keeps_original_audio() {
        let hq = api::MediaSourceInfo {
            container: Some(VideoContainer::Mkv),
            media_streams: vec![
                MediaStream {
                    type_: Some(MediaStreamType::Video),
                    index: 0,
                    codec: Some("h264".into()),
                    ..Default::default()
                },
                MediaStream {
                    type_: Some(MediaStreamType::Audio),
                    index: 1,
                    codec: Some("eac3".into()),
                    language: Some("zho".into()),
                    is_default: Some(true),
                    display_title: Some("Chinese - Default".into()),
                    ..Default::default()
                },
                MediaStream {
                    type_: Some(MediaStreamType::Subtitle),
                    index: 2,
                    codec: Some("subrip".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let p = mux_probe(&hq, "hotphim");
        assert_eq!(p.container, Some(VideoContainer::Other("hls".into())));
        let kinds: Vec<_> = p
            .media_streams
            .iter()
            .map(|s| {
                (
                    s.index,
                    s.type_
                        .clone(),
                    s.language
                        .clone(),
                    s.is_default,
                )
            })
            .collect();
        assert_eq!(kinds.len(), 4);
        assert!(matches!(kinds[0], (0, Some(MediaStreamType::Video), _, _)));
        assert_eq!(
            kinds[1]
                .2
                .as_deref(),
            Some("vie")
        );
        assert_eq!(kinds[1].3, Some(true));
        assert_eq!(
            kinds[2]
                .2
                .as_deref(),
            Some("zho")
        );
        assert_eq!(kinds[2].3, Some(false));
        let subs: Vec<_> = p
            .media_streams
            .iter()
            .filter(|s| matches!(s.type_, Some(MediaStreamType::Subtitle)))
            .collect();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].index, SUBTITLE_INDEX_OFFSET + 2);
        assert_eq!(
            subs[0]
                .codec
                .as_deref(),
            Some("subrip")
        );
    }

    #[test]
    fn hq_url_is_recovered_from_the_mux_url() {
        let row = http_media(
            "https://dubmux.example/mux/d/h/master.m3u8?video=https%3A%2F%2Fcdn.example%2Fa.mkv%3Ft%3D1",
            "X.VNDub-hotphim.m3u8",
        );
        assert_eq!(
            hq_url_of(&row).as_deref(),
            Some("https://cdn.example/a.mkv?t=1")
        );
        assert_eq!(
            hq_url_of(&http_media("https://cdn.example/a.mkv", "a.mkv")),
            None
        );
    }
}

// ------------------------------------------------------------------------
// One-shot prefetch on playback start: prepare the dubs of the upcoming
// episodes (extract + match on the seedbox, no muxing) and pre-mux the very
// next one so it starts instantly. Runs once per series per hour, off the
// request path, sequentially with a small gap so addon/muxer load stays flat.

static PREFETCHED: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<Uuid, std::time::Instant>>,
> = std::sync::LazyLock::new(Default::default);

pub(crate) fn spawn_prefetch_upcoming(ctx: AppContext, media: db::Media, user: Uuid) {
    let Some(_) = DubmuxConfig::from(&ctx.config) else {
        return;
    };
    if media.kind != db::MediaKind::Episode
        || ctx
            .config
            .dubmux_prefetch_episodes
            == 0
    {
        return;
    }
    let Some(series_id) = media.grandparent_id else {
        return;
    };
    {
        let mut seen = PREFETCHED
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        seen.retain(|_, t| t.elapsed() < Duration::from_secs(3600));
        if seen.contains_key(&series_id) {
            return;
        }
        seen.insert(series_id, std::time::Instant::now());
    }
    tokio::spawn(async move {
        if let Err(e) = prefetch_upcoming(&ctx, &media, series_id, user).await {
            warn!(series = %series_id, "dub prefetch failed: {e:#}");
        }
    });
}

async fn prefetch_upcoming(
    ctx: &AppContext,
    media: &db::Media,
    series_id: Uuid,
    user: Uuid,
) -> anyhow::Result<()> {
    let limit = ctx
        .config
        .dubmux_prefetch_episodes as i64;
    let next = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM media WHERE kind = 'episode' AND grandparent_id = ? \
         AND (parent_idx > ? OR (parent_idx = ? AND idx > ?)) \
         ORDER BY parent_idx, idx LIMIT ?",
    )
    .bind(series_id)
    .bind(
        media
            .parent_idx
            .unwrap_or(0),
    )
    .bind(
        media
            .parent_idx
            .unwrap_or(0),
    )
    .bind(
        media
            .idx
            .unwrap_or(0),
    )
    .bind(limit)
    .fetch_all(&ctx.db)
    .await?;
    // The two episodes before the current one go last: a rewatch/"what did I
    // miss" is plausible but far less likely than pressing next.
    let previous = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM media WHERE kind = 'episode' AND grandparent_id = ? \
         AND (parent_idx < ? OR (parent_idx = ? AND idx < ?)) \
         ORDER BY parent_idx DESC, idx DESC LIMIT ?",
    )
    .bind(series_id)
    .bind(
        media
            .parent_idx
            .unwrap_or(0),
    )
    .bind(
        media
            .parent_idx
            .unwrap_or(0),
    )
    .bind(
        media
            .idx
            .unwrap_or(0),
    )
    .bind(
        ctx.config
            .dubmux_prefetch_previous as i64,
    )
    .fetch_all(&ctx.db)
    .await?;
    let upcoming = next.len();
    let order: Vec<Uuid> = next
        .into_iter()
        .chain(previous)
        .collect();
    info!(series = %series_id, upcoming, total = order.len(), "dub prefetch: walking episodes");
    for (offset, id) in order
        .into_iter()
        .enumerate()
    {
        let Some(mut ep) = db::Media::get_by_id(&ctx.db, &id).await? else {
            continue;
        };
        // refresh_streams honours the freshness window and runs the dub hook
        // (wait 0, background priority) itself; stale lists cost one addon
        // round-trip per episode.
        if let Err(e) = ctx
            .addons
            .refresh_streams(&mut ep, ctx, Some(user))
            .await
        {
            warn!(episode = %id, "dub prefetch: stream refresh failed: {e:#}");
            continue;
        }
        // Re-submit this episode's pairs at walk priority (next episodes
        // first, previous ones last): the muxer bumps queued jobs in place.
        if let Ok(streams) = ep
            .streams(&ctx.db)
            .await
        {
            let prio = PRIORITY_WALK + offset as u32;
            let _ = ensure_dub_rows(ctx, &ep, &streams, 0, prio).await;
        }
        if offset == 0
            && ctx
                .config
                .dubmux_premux_next
        {
            premux_first_pair(ctx, &mut ep, user).await;
        }
        // The viewer will most likely get to these episodes: keep their
        // finished muxes from aging out before the one being watched.
        touch_episode_muxes(ctx, &mut ep).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Ok(())
}

/// Extend the retention of every finished mux behind an episode's dub rows
/// (muxer `POST …/touch`; never starts a mux).
async fn touch_episode_muxes(ctx: &AppContext, ep: &mut db::Media) {
    let Ok(streams) = ep
        .streams(&ctx.db)
        .await
    else {
        return;
    };
    touch_muxes(ctx, ep.id, &streams).await;
}

/// The persistent background refresh queue's share of the dub work, run after
/// an episode's streams were refreshed (the refresh itself already created or
/// carried over the dub rows): keep the finished muxes of everything the queue
/// tracks alive, and — while the series is being watched — make sure the best
/// pair of each episode is muxed, so a dropped mux or a row that appeared
/// late gets built without waiting for a playback. Returns the dub rows.
pub(crate) async fn background_episode_hook(
    ctx: &AppContext,
    ep: &mut db::Media,
    premux: bool,
) -> Vec<db::Media> {
    let Ok(streams) = ep
        .streams(&ctx.db)
        .await
    else {
        return vec![];
    };
    touch_muxes(ctx, ep.id, &streams).await;
    let rows: Vec<db::Media> = streams
        .into_iter()
        .filter(is_dubmux_row)
        .collect();
    if premux {
        if let Some((cfg, url)) = DubmuxConfig::from(&ctx.config).zip(
            rows.first()
                .and_then(http_url),
        ) {
            start_mux(&url.replacen(cfg.public, cfg.api, 1), ep.id).await;
        }
    }
    rows
}

async fn touch_muxes(ctx: &AppContext, item: Uuid, streams: &[db::Media]) {
    let Some(cfg) = DubmuxConfig::from(&ctx.config) else {
        return;
    };
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    for url in streams
        .iter()
        .filter(|s| is_dubmux_row(s))
        .filter_map(http_url)
    {
        let touch = url
            .replacen(cfg.public, cfg.api, 1)
            .replacen("/master.m3u8", "/touch", 1);
        let touch = touch
            .split('?')
            .next()
            .unwrap_or(&touch)
            .to_string();
        if let Err(e) = client
            .post(&touch)
            .send()
            .await
        {
            debug!(item = %item, "dubmux touch failed: {e:#}");
        }
    }
}

/// Wait for the next episode's preparations and hit the muxer's master
/// playlist for the first accepted pair so the whole episode is muxed and
/// cached before the viewer gets there.
async fn premux_first_pair(ctx: &AppContext, ep: &mut db::Media, user: Uuid) {
    let Some(cfg) = DubmuxConfig::from(&ctx.config) else {
        return;
    };
    let Ok(streams) = ep
        .streams(&ctx.db)
        .await
    else {
        return;
    };
    let mut rows = ensure_dub_rows(ctx, ep, &streams, 45, PRIORITY_WALK).await;
    if rows.is_empty() {
        rows = streams
            .iter()
            .filter(|s| is_dubmux_row(s))
            .cloned()
            .collect();
    }
    let Some(url) = rows
        .first()
        .and_then(http_url)
    else {
        debug!(episode = %ep.id, "dub prefetch: no accepted pair to pre-mux");
        return;
    };
    // Reach the muxer over the tailnet API host, not the public hostname.
    let url = url.replacen(cfg.public, cfg.api, 1);
    info!(episode = %ep.id, user = %user, "dub prefetch: starting next episode mux");
    start_mux(&url, ep.id).await;
}
