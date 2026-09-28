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
    // vnphim's own rows are recognised by their release-name tokens now that
    // their URLs are opaque encrypted MediaFlow addresses.
    let vn_source = [
        ".kkphim.",
        ".ophim.",
        ".hotphim.",
        ".yanhh3d.",
        "proxiedvn",
        ".vietsub.",
    ]
    .iter()
    .any(|t| lower.contains(t));
    // Usenet releases (NzbDAV) are named like scene releases with NO file
    // extension; only playlist-style names are not files.
    let file_like = lower.ends_with(".mkv")
        || lower.ends_with(".mp4")
        || !lower
            .rsplit('.')
            .next()
            .is_some_and(|ext| {
                matches!(
                    ext,
                    "m3u8"
                        | "m3u"
                        | "ts"
                        | "strm"
                        | "avi"
                        | "wmv"
                        | "flv"
                        | "iso"
                        | "rar"
                )
            });
    !is_dubmux_row(stream)
        && !url.contains("vnphim")
        && !vn_source
        && dub_provider(name).is_none()
        && file_like
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
    #[serde(default)]
    result: Option<serde_json::Value>,
}

impl PrepareReply {
    /// The muxer decode-checked this pair's video (its copy of the release,
    /// or the first segments of a session built from it).
    fn video_checked(&self) -> bool {
        self.result
            .as_ref()
            .and_then(|r| r.get("video_check"))
            .and_then(|v| v.as_str())
            == Some("ok")
    }
}

/// PATCH (uduchi2nd, user decision 2026-09-28): the Vietnamese-dub mux is only
/// for Asian-originated titles. Judged on the title's (for an episode: its
/// series') original language, falling back to its country; unknown origin
/// means no dub rows. Vietnamese originals are excluded (their audio already
/// is Vietnamese).
pub(crate) fn is_asian_origin(
    original_language: Option<&str>,
    country: Option<&str>,
) -> bool {
    const LANGS: &[&str] = &[
        "zh", "cn", "yue", "ko", "ja", "th", "id", "ms", "tl", "fil", "hi", "ta", "te",
        "ml", "kn", "bn", "mr", "ur", "pa", "my", "km", "lo", "mn", "ne", "si",
    ];
    const COUNTRIES: &[&str] = &[
        "CN",
        "HK",
        "TW",
        "MO",
        "KR",
        "KP",
        "JP",
        "TH",
        "ID",
        "MY",
        "SG",
        "PH",
        "IN",
        "PK",
        "BD",
        "LK",
        "NP",
        "MM",
        "KH",
        "LA",
        "MN",
        "BT",
        "BN",
        "CHINA",
        "HONG KONG",
        "TAIWAN",
        "MACAU",
        "SOUTH KOREA",
        "KOREA",
        "NORTH KOREA",
        "JAPAN",
        "THAILAND",
        "INDONESIA",
        "MALAYSIA",
        "SINGAPORE",
        "PHILIPPINES",
        "INDIA",
        "PAKISTAN",
        "BANGLADESH",
        "SRI LANKA",
        "NEPAL",
        "MYANMAR",
        "CAMBODIA",
        "LAOS",
        "MONGOLIA",
    ];
    if let Some(lang) = original_language
        .map(|l| {
            l.trim()
                .to_ascii_lowercase()
        })
        .filter(|l| !l.is_empty())
    {
        let base = lang
            .split(['-', '_'])
            .next()
            .unwrap_or("");
        return LANGS.contains(&base);
    }
    // No language: the country field (codes or full names, comma-separated);
    // every listed country must be Asian (a US/UK co-production does not
    // qualify).
    let Some(countries) = country
        .map(str::trim)
        .filter(|c| !c.is_empty())
    else {
        return false;
    };
    let mut any = false;
    for c in countries.split(',') {
        let c = c
            .trim()
            .to_ascii_uppercase();
        if c.is_empty() {
            continue;
        }
        if !COUNTRIES.contains(&c.as_str()) {
            return false;
        }
        any = true;
    }
    any
}

/// The title whose origin decides: an episode's series, else the item itself.
async fn origin_title(ctx: &AppContext, media: &db::Media) -> Option<db::Media> {
    match media.kind {
        db::MediaKind::Episode => match media.grandparent_id {
            Some(id) => db::Media::get_by_id(&ctx.db, &id)
                .await
                .ok()
                .flatten(),
            None => None,
        },
        _ => Some(media.clone()),
    }
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
    cached_only: bool,
) -> anyhow::Result<PrepareReply> {
    let body = serde_json::json!({
        "dub": {"id": dub_id, "url": dub_url},
        "video": {"id": hq_id, "url": hq_url},
        "wait": wait,
        "priority": priority,
        "cached_only": cached_only,
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

/// Sources sort by idx ascending. Rows whose video the muxer has decode-
/// checked go first (the first-built = best HQ release gets the most negative
/// idx); unchecked rows go after every release, so a broken row is never a
/// player's default. They move up once a check passes.
const UNCHECKED_IDX_BASE: i64 = 100_000;

fn order_rows(rows: &mut [db::Media], checked: &[Uuid]) {
    let n_checked = rows
        .iter()
        .filter(|r| checked.contains(&r.id))
        .count() as i64;
    let (mut c, mut u) = (0_i64, 0_i64);
    for row in rows.iter_mut() {
        if checked.contains(&row.id) {
            row.idx = Some(c - n_checked);
            c += 1;
        } else {
            row.idx = Some(UNCHECKED_IDX_BASE + u);
            u += 1;
        }
    }
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
/// The episode after the one being played: below the current episode, above
/// everything else (item-open pairs of browsed titles, the rest of the walk,
/// background, cache warming). Gets the muxer's express slot too (≤ 9).
pub(crate) const PRIORITY_NEXT: u32 = 2;
pub(crate) const PRIORITY_BACKGROUND: u32 = 300;
/// Variants (second dub provider / second-best release) of an episode rank
/// behind the first pairs of the whole walk (up to 12 episodes).
const VARIANT_PENALTY: u32 = 50;
/// Priorities at or below this are the episode being played and the next one.
const PRIORITY_LIVE_MAX: u32 = 9;
/// New (uncached) preparations per episode at any other priority.
const BACKGROUND_NEW_PAIRS: u32 = 2;

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
    let origin = origin_title(ctx, media).await;
    if !origin
        .as_ref()
        .is_some_and(|t| {
            is_asian_origin(
                t.original_language
                    .as_deref(),
                t.country
                    .as_deref(),
            )
        })
    {
        debug!(item = %media.id, "dubmux skipped: not an Asian-originated title");
        // rows made before this rule (e.g. Reacher) go now
        for row in existing_dub_rows(ctx, media).await {
            if db::Media::delete(&ctx.db, &row.id)
                .await
                .is_ok()
            {
                info!(item = %media.id, row = %row.id, "dubmux row removed: title not Asian-originated");
            }
        }
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
    let mut checked: Vec<Uuid> = Vec::new();
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
            // PATCH (uduchi2nd): outside playback / the next episode, only the
            // first BACKGROUND_NEW_PAIRS pairs of an episode may start a new
            // preparation (each costs a debrid link); later pairs are only
            // reported when the muxer already has them.
            let cached_only =
                priority > PRIORITY_LIVE_MAX && pairs_seen >= BACKGROUND_NEW_PAIRS;
            pairs_seen += 1;
            let reply = match prepare(
                &client,
                &cfg,
                dub_id,
                dub_url,
                &hq_id,
                hq_url,
                wait,
                pair_priority,
                cached_only,
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
            if reply.video_checked() {
                checked.push(row.id);
            }
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
            let rebuilt = hq
                .probe_data
                .as_ref()
                .map(|p| mux_probe(p, provider))
                .unwrap_or_else(|| minimal_probe(provider));
            let stored_probe = stored
                .iter()
                .find(|r| r.id == row.id)
                .and_then(|r| {
                    r.probe_data
                        .clone()
                });
            row.probe_data = Some(richer_probe(rebuilt, stored_probe));
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
    // A pair the muxer now rejects (e.g. its video turned out not to decode)
    // loses its row immediately rather than at the next stream refresh — a
    // broken row sorted first is every player's default choice.
    for id in stored
        .iter()
        .map(|r| r.id)
        .filter(|id| rejected.contains(id))
    {
        if let Err(e) = db::Media::delete(&ctx.db, &id).await {
            warn!(item = %media.id, row = %id, "dubmux rejected row delete failed: {e:#}");
        } else {
            info!(item = %media.id, row = %id, "dubmux row removed: pair rejected");
        }
    }
    if rows.is_empty() {
        return rows;
    }
    order_rows(&mut rows, &checked);
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

/// Provider label of a "[+VN dub · <provider>] …" row title.
fn row_provider(row: &db::Media) -> String {
    row.title
        .split("[+VN dub · ")
        .nth(1)
        .and_then(|r| {
            r.split(']')
                .next()
        })
        .unwrap_or("vnphim")
        .to_string()
}

pub(crate) fn row_provider_pub(row: &db::Media) -> String {
    row_provider(row)
}

/// A dub row's track list when its HQ release has no probe yet: the video
/// as a guess plus the dub as the default `vie` track. Rows must never be
/// live-probed (the master would start a mux and the result races the
/// muxer), so every row carries SOME probe from the moment it exists.
/// Give a legacy dub row (created before rows carried a track list) the
/// minimal one it is presented with. PlaybackInfo used to build it on the fly
/// without saving it, so the subtitle route saw no tracks and numbered the
/// external subtitles from 0 while PlaybackInfo advertised them from 2 →
/// "subtitle stream not found" and blank subtitles in VidHub (2026-09-28).
/// Returns true when the row was filled (the caller persists it).
/// A dub row's track list must never shrink: rebuilding it from a release
/// that is not (yet) ffprobed — only a filename guess, or no probe at all —
/// dropped the release's subtitle tracks, renumbering the addon subtitles
/// (102+ → 2), and the next rebuild from the probed release switched back.
/// Players remember one numbering and fetch tracks under it after the flip →
/// "subtitle stream not found" (The Early Spring E01/E19, 2026-09-28).
fn richer_probe(
    rebuilt: api::MediaSourceInfo,
    stored: Option<api::MediaSourceInfo>,
) -> api::MediaSourceInfo {
    match stored {
        Some(old)
            if old
                .media_streams
                .len()
                > rebuilt
                    .media_streams
                    .len() =>
        {
            old
        }
        _ => rebuilt,
    }
}

pub(crate) fn fill_row_probe(row: &mut db::Media) -> bool {
    if is_dubmux_row(row)
        && row
            .probe_data
            .is_none()
    {
        row.probe_data = Some(minimal_probe(&row_provider_pub(row)));
        return true;
    }
    false
}

pub(crate) fn minimal_probe(provider: &str) -> api::MediaSourceInfo {
    let mut base = api::MediaSourceInfo::default();
    base.media_streams = vec![MediaStream {
        type_: Some(MediaStreamType::Video),
        index: 0,
        is_default: Some(true),
        ..Default::default()
    }];
    mux_probe(&base, provider)
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
        // Rebuild the track list from the release's CURRENT probe: a row
        // carried over from before its release was probed (or from a build
        // that left rows without one) would otherwise be live-probed on
        // play — the mux master answers only once the mux is done and the
        // first PlaybackInfo listed no Vietnamese track (Pursuit of Jade E12).
        let provider = row_provider(&row);
        let rebuilt = hq
            .probe_data
            .as_ref()
            .map(|p| mux_probe(p, &provider))
            .unwrap_or_else(|| minimal_probe(&provider));
        row.probe_data = Some(richer_probe(
            rebuilt,
            row.probe_data
                .take(),
        ));
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

/// Priority for pairs requested because the viewer opened the item's detail
/// page: ahead of the walk, behind an actual playback request.
pub(crate) const PRIORITY_OPEN: u32 = 10;

/// Opening an episode/movie is the earliest signal that it may be played:
/// prepare its dub pairs and pre-mux the best one right away (cold path is
/// 3–6 min end to end, far longer than a player waits at PlaybackInfo), so
/// by the time the viewer presses play the row usually exists. Throttled to
/// once per item per 10 min; runs in the background, never delays the page.
pub(crate) fn prepare_on_open(ctx: &AppContext, media: &db::Media, user: Uuid) {
    use std::sync::Mutex;
    use std::time::{Duration as StdDuration, Instant};
    static SEEN: Mutex<Option<std::collections::HashMap<Uuid, Instant>>> =
        Mutex::new(None);
    if DubmuxConfig::from(&ctx.config).is_none()
        || !matches!(media.kind, db::MediaKind::Movie | db::MediaKind::Episode)
    {
        return;
    }
    {
        let mut guard = match SEEN.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let map = guard.get_or_insert_with(Default::default);
        let now = Instant::now();
        map.retain(|_, t| now.duration_since(*t) < StdDuration::from_secs(3600));
        if map
            .get(&media.id)
            .is_some_and(|t| now.duration_since(*t) < StdDuration::from_secs(600))
        {
            return;
        }
        map.insert(media.id, now);
    }
    let ctx = ctx.clone();
    let mut media = media.clone();
    tokio::spawn(async move {
        let Ok(streams) = media
            .streams(&ctx.db)
            .await
        else {
            return;
        };
        let rows = ensure_dub_rows(&ctx, &media, &streams, 0, PRIORITY_OPEN).await;
        let Some(cfg) = DubmuxConfig::from(&ctx.config) else {
            return;
        };
        if let Some(url) = rows
            .first()
            .and_then(http_url)
        {
            start_mux(&url.replacen(cfg.public, cfg.api, 1), media.id).await;
        }
        debug!(item = %media.id, user = %user, rows = rows.len(), "dub prepare on open");
    });
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
    fn asian_origin_by_language_then_country() {
        assert!(is_asian_origin(Some("zh"), Some("CN")));
        assert!(is_asian_origin(Some("ko"), None));
        assert!(is_asian_origin(Some("ja"), Some("US")));
        assert!(!is_asian_origin(Some("en"), Some("US")));
        assert!(!is_asian_origin(Some("en"), Some("CN")));
        assert!(!is_asian_origin(Some("vi"), Some("VN")));
        assert!(is_asian_origin(None, Some("SOUTH KOREA")));
        assert!(is_asian_origin(None, Some("CN, HK")));
        assert!(!is_asian_origin(
            None,
            Some("UNITED KINGDOM, UNITED STATES OF AMERICA")
        ));
        assert!(!is_asian_origin(None, Some("CN, US")));
        assert!(!is_asian_origin(None, None));
        assert!(!is_asian_origin(Some(""), Some("")));
    }

    #[test]
    fn row_track_list_never_shrinks() {
        let guess = minimal_probe("hotphim");
        let mut full = minimal_probe("hotphim");
        for i in 0..5 {
            full.media_streams
                .push(MediaStream {
                    type_: Some(MediaStreamType::Subtitle),
                    index: 102 + i,
                    ..Default::default()
                });
        }
        let n = full
            .media_streams
            .len();
        assert_eq!(
            richer_probe(guess.clone(), Some(full.clone()))
                .media_streams
                .len(),
            n
        );
        assert_eq!(
            richer_probe(full.clone(), Some(guess.clone()))
                .media_streams
                .len(),
            n
        );
        assert_eq!(
            richer_probe(guess.clone(), None)
                .media_streams
                .len(),
            guess
                .media_streams
                .len()
        );
    }

    #[test]
    fn unchecked_rows_sort_after_releases() {
        let mk = |n: u128| {
            let mut m = db::Media::default();
            m.id = Uuid::from_u128(n);
            m
        };
        let mut rows = vec![mk(1), mk(2), mk(3)];
        order_rows(&mut rows, &[Uuid::from_u128(2)]);
        assert_eq!(rows[1].idx, Some(-1));
        assert_eq!(rows[0].idx, Some(UNCHECKED_IDX_BASE));
        assert_eq!(rows[2].idx, Some(UNCHECKED_IDX_BASE + 1));
        let mut rows = vec![mk(1), mk(2)];
        order_rows(&mut rows, &[Uuid::from_u128(1), Uuid::from_u128(2)]);
        assert_eq!((rows[0].idx, rows[1].idx), (Some(-2), Some(-1)));
    }

    #[test]
    fn hq_candidate_accepts_extensionless_usenet_names_and_rejects_vn_sources() {
        fn mk(name: &str, url: &str) -> db::Media {
            let mut m = db::Media::default();
            m.stream_info = Some(crate::stream::StreamInfo {
                filename: Some(name.into()),
                descriptor: StreamDescriptor::Http {
                    url: url.into(),
                    request_headers: Default::default(),
                    response_headers: Default::default(),
                },
                ..Default::default()
            });
            m
        }
        assert!(is_hq_candidate(&mk(
            "The.Captain.2019.1080p.BluRay.DD+5.1.x264-PTer",
            "https://usenet.example/x"
        )));
        assert!(is_hq_candidate(&mk(
            "Yolo.2024.1080p.WEB-DL.mkv",
            "https://cdn.example/y"
        )));
        assert!(!is_hq_candidate(&mk(
            "Nguoi.Ban.S01E01.1080p.WEB-DL.Chinese.Vietsub.kkphim.ProxiedVN.mp4",
            "https://mf.example/_token_x/proxy/hls/manifest.m3u8"
        )));
        assert!(!is_hq_candidate(&mk(
            "Tien.Nghich.S01E101.1080p.WEB-DL.Chinese.Vietsub.yanhh3d.mp4",
            "https://mf.example/_token_y/proxy/hls/manifest.m3u8"
        )));
        assert!(!is_hq_candidate(&mk(
            "Some.Show.S01E01.m3u8",
            "https://x.example/a.m3u8"
        )));
    }

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
        // Re-submit this episode's pairs: the NEXT episode right behind the
        // current one, the rest at walk priority (upcoming first, previous
        // ones last); the muxer bumps queued jobs in place.
        if let Ok(streams) = ep
            .streams(&ctx.db)
            .await
        {
            let prio = if offset == 0 {
                PRIORITY_NEXT
            } else {
                PRIORITY_WALK + offset as u32
            };
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
