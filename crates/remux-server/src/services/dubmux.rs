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

const MAX_HQ: usize = 2;
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

fn is_dubmux_row(stream: &db::Media) -> bool {
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
) -> anyhow::Result<PrepareReply> {
    let body = serde_json::json!({
        "dub": {"id": dub_id, "url": dub_url},
        "video": {"id": hq_id, "url": hq_url},
        "wait": wait,
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
    probe.media_streams = streams;
    probe
}

/// Make sure a "[+VN dub]" stream row exists for every (HQ source, vnphim dub)
/// pair the muxer has accepted, creating/refreshing rows as needed. Returns
/// the rows that were added or updated (already-present rows are re-upserted
/// so their `updated_at` follows the parent's refresh). `wait_secs` bounds
/// how long a still-running preparation is waited for — 0 on background
/// refreshes, a few seconds on a playback request.
pub(crate) async fn ensure_dub_rows(
    ctx: &AppContext,
    media: &db::Media,
    streams: &[db::Media],
    wait_secs: u64,
) -> Vec<db::Media> {
    let Some(cfg) = DubmuxConfig::from(&ctx.config) else {
        return vec![];
    };
    if !matches!(media.kind, db::MediaKind::Movie | db::MediaKind::Episode) {
        return vec![];
    }
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
    if dubs.is_empty() {
        return vec![];
    }
    let hqs: Vec<&db::Media> = streams
        .iter()
        .filter(|s| is_hq_candidate(s))
        .take(MAX_HQ)
        .collect();
    if hqs.is_empty() {
        return vec![];
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .build()
        .unwrap_or_default();
    let now = chrono::Utc::now().naive_utc();
    let mut rows = Vec::new();
    let mut wait = wait_secs;
    for hq in hqs {
        let hq_url = http_url(hq).unwrap();
        let hq_id = hq
            .id
            .simple()
            .to_string();
        for (dub, provider, dub_id) in &dubs {
            let dub_url = http_url(dub).unwrap();
            let reply =
                match prepare(&client, &cfg, dub_id, dub_url, &hq_id, hq_url, wait)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(item = %media.id, "dubmux prepare failed: {e:#}");
                        return rows;
                    }
                };
            // Only the first pair spends the caller's wait budget; the muxer
            // runs the other preparations concurrently anyway.
            wait = 0;
            if reply.status != "ready" {
                debug!(item = %media.id, provider, hq = %hq.id, status = reply.status,
                       stage = ?reply.stage, "dubmux pair not ready");
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
            row.probe_data = hq
                .probe_data
                .as_ref()
                .filter(|p| !p.is_filename_guess())
                .map(|p| mux_probe(p, provider));
            rows.push(row);
        }
    }
    if rows.is_empty() {
        return rows;
    }
    if let Err(e) = db::Media::upsert(&ctx.db, &rows).await {
        warn!(item = %media.id, "dubmux row upsert failed: {e:#}");
        return vec![];
    }
    info!(item = %media.id, rows = rows.len(), "dubmux rows ready");
    rows
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
        assert_eq!(kinds.len(), 3);
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
        assert!(
            !p.media_streams
                .iter()
                .any(|s| matches!(s.type_, Some(MediaStreamType::Subtitle)))
        );
    }
}
