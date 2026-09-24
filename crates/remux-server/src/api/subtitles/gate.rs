//! Eligible external subtitles are prepared in the background; video
//! delivery and subtitle-text delivery both bound their wait on alignment to
//! Config.subtitle_alignment_wait_seconds (default 2s) and never fail the
//! request over it — see [`alignment::resolve`] for the actual bounded wait
//! and [`prepare_in_background`] for the fire-and-forget PlaybackInfo warm-up.
use super::{alignment, lang_to_two_letter};
use crate::{AppContext, AppState, api, db};
use std::{
    collections::HashSet,
    sync::{LazyLock, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;
use uuid::Uuid;

type PrepareKey = (Uuid, Uuid, Option<Uuid>);

static PREPARING: LazyLock<Mutex<HashSet<PrepareKey>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
static PREPARE_LIMIT: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(2));

/// Warm alignment caches without making playback metadata or video wait for
/// subtitle providers. Duplicate requests for the same user's source share one
/// preparation task; work is bounded so browsing many items can't fan out.
pub(crate) fn prepare_in_background(
    state: &AppState,
    source: &db::Media,
    item: Uuid,
    user: Option<Uuid>,
) {
    let Some(probe) = source.probe_data.as_ref() else {
        return;
    };
    if state
        .ctx
        .config
        .subtitle_alignment_gate_base_url
        .is_none()
        || !["vi", "en"].iter().any(|language| eligible(&probe.media_streams, Some(language)))
    {
        return;
    }

    let key = (item, source.id, user);
    let Ok(mut preparing) = PREPARING.lock() else {
        return;
    };
    if !preparing.insert(key) {
        return;
    }
    drop(preparing);

    let state = state.clone();
    let source = source.clone();
    tokio::spawn(async move {
        let result = match PREPARE_LIMIT.acquire().await {
            Ok(_permit) => ensure_ready(&state, &source, item, user).await,
            Err(_) => Err(anyhow::anyhow!("subtitle preparation limit unavailable")),
        };
        if result.is_err() {
            tracing::warn!(
                item = %item,
                media_source = %source.id,
                "background subtitle alignment is not ready"
            );
        }
        if let Ok(mut preparing) = PREPARING.lock() {
            preparing.remove(&key);
        }
    });
}

pub(super) fn raw_key(descriptor: &crate::stream::StreamDescriptor) -> String {
    let mut value = serde_json::to_value(descriptor).unwrap_or_default();
    value.sort_all_objects();
    format!(
        "subtitle-valid-raw:{}",
        Uuid::new_v5(
            &Uuid::nil(),
            value
                .to_string()
                .as_bytes()
        )
    )
}

pub(super) fn invalid_key(descriptor: &crate::stream::StreamDescriptor) -> String {
    format!("subtitle-invalid-raw:{}", raw_key(descriptor).trim_start_matches("subtitle-valid-raw:"))
}

pub(super) fn known_invalid(
    ctx: &AppContext,
    descriptor: &crate::stream::StreamDescriptor,
) -> bool {
    ctx.store
        .get::<bool>(&invalid_key(descriptor))
        .is_some_and(|invalid| *invalid)
}

pub(super) fn valid_subtitle(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    if text
        .trim_start()
        .starts_with(['{', '<'])
    {
        return false;
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&crate::conversions::srt_to_jellyfin_json(text))
            .unwrap_or_default();
    parsed["TrackEvents"]
        .as_array()
        .is_some_and(|events| {
            events
                .iter()
                .any(|e| {
                    e["Text"]
                        .as_str()
                        .is_some_and(|t| {
                            !t.trim()
                                .is_empty()
                        })
                        && e["StartPositionTicks"]
                            .as_i64()
                            .zip(e["EndPositionTicks"].as_i64())
                            .is_some_and(|(a, b)| a >= 0 && b > a)
                })
        })
        || text
            .lines()
            .any(|line| {
                line.starts_with("Dialogue:")
                    && line
                        .split(',')
                        .count()
                        >= 10
            })
}

fn has_reference(streams: &[api::MediaStream]) -> bool {
    streams
        .iter()
        .any(|s| {
            matches!(s.type_, Some(api::MediaStreamType::Subtitle))
                && !s.is_external
                && !s.is_forced
                && alignment::text_codec(
                    s.codec
                        .as_deref(),
                )
        })
}
fn eligible(streams: &[api::MediaStream], language: Option<&str>) -> bool {
    has_reference(streams)
        && alignment::alignment_skip_reason(language, streams).is_none()
}

pub(crate) fn advertise(
    ctx: &AppContext,
    source: &mut api::MediaSourceInfo,
    item: Uuid,
    token: &str,
) {
    let Some(base) = &ctx
        .config
        .subtitle_alignment_gate_base_url
    else {
        return;
    };
    if !source
        .media_streams
        .iter()
        .any(|s| {
            s.is_external
                && eligible(
                    &source.media_streams,
                    s.language
                        .as_deref(),
                )
        })
    {
        return;
    }
    source.path = Some(format!(
        "{}/remux/subtitle-ready/{item}/{}/stream?ApiKey={token}",
        base.trim_end_matches('/'),
        source.id
    ));
    source.is_remote = true;
    source.protocol = api::MediaProtocol::Http;
}

pub(crate) async fn ensure_ready(
    state: &AppState,
    source: &db::Media,
    item: Uuid,
    user: Option<Uuid>,
) -> anyhow::Result<()> {
    if state
        .ctx
        .config
        .subtitle_alignment_gate_base_url
        .is_none()
    {
        return Ok(());
    }
    let Some(probe) = source
        .probe_data
        .as_ref()
    else {
        return Ok(());
    };
    if !has_reference(&probe.media_streams) {
        return Ok(());
    }
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut media = db::Media::get_by_id(
            &state
                .ctx
                .db,
            &item,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("subtitle item unavailable"))?;
        let subs = state
            .ctx
            .addons
            .fetch_subtitles(
                &mut media,
                &state
                    .ctx
                    .db,
                false,
                user,
            )
            .await;
        let settings = db::Settings::get_config_or_default(
            &state
                .ctx
                .db,
        )
        .await;
        let source_info = api::MediaSourceInfo::from(source.clone());
        let mut subs: Vec<_> = super::scored_external_subtitles(
            &subs,
            &settings
                .subtitle_languages
                .unwrap_or_default(),
            &source_info.name,
            &source_info.path,
        )
        .into_iter()
        .cloned()
        .collect();
        if let Some(torrent) = state
            .ctx
            .torrent
            .read()
            .await
            .clone()
        {
            if let Some(info) = source
                .stream_info
                .as_ref()
            {
                subs.extend(info.subtitle_sidecars(&torrent));
            }
        }
        for language in ["vi", "en"] {
            if !eligible(&probe.media_streams, Some(language)) {
                continue;
            }
            let candidates: Vec<_> = subs
                .iter()
                .filter(|s| {
                    s.lang
                        .as_deref()
                        .and_then(lang_to_two_letter)
                        .as_deref()
                        == Some(language)
                        && s.url
                            .is_some()
                })
                .collect();
            if candidates.is_empty() {
                continue;
            }
            // Prepare every advertised matching track: a player may select any of them.
            for sub in candidates {
                let descriptor = sub
                    .url
                    .as_ref()
                    .unwrap();
                let bytes = match super::fetch_external_subtitle_bytes(state, descriptor).await {
                    Ok(bytes) => bytes,
                    Err(_) if known_invalid(&state.ctx, descriptor) => continue,
                    Err(error) => return Err(error),
                };
                loop {
                    let response = super::aligned_external_response(
                        state,
                        source,
                        item,
                        Some(&sub.id),
                        None,
                        None,
                        descriptor,
                        bytes.clone(),
                        sub.lang
                            .as_deref(),
                        "vtt",
                        false,
                    )
                    .await;
                    let status = response
                        .headers()
                        .get("X-Remux-Subtitle-Alignment")
                        .and_then(|v| {
                            v.to_str()
                                .ok()
                        })
                        .unwrap_or("unavailable");
                    match status {
                        "aligned" | "unchanged" => break,
                        "pending" | "wait-timeout" => {
                            tokio::time::sleep(Duration::from_millis(100)).await
                        }
                        _ => {
                            return Err(anyhow::anyhow!(
                                "external subtitle alignment is not ready"
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!("subtitle preparation exceeded 60 seconds; retry playback")
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_error_and_empty_payloads() {
        for data in [
            b"WEBVTT\n\n".as_slice(),
            br##"{"error":"#75 Bad Request"}"##,
            b"<html>error</html>",
            b"1\n00:00:02,000 --> 00:00:01,000\nbad\n",
        ] {
            assert!(!valid_subtitle(data));
        }
        assert!(valid_subtitle(b"1\n00:00:01,000 --> 00:00:02,000\nHello\n"));
        assert!(valid_subtitle(
            b"WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHello\n"
        ));
    }
    #[test]
    fn only_missing_target_language_with_text_reference_requires_gate() {
        let mut reference = api::MediaStream {
            type_: Some(api::MediaStreamType::Subtitle),
            codec: Some("srt".into()),
            language: Some("eng".into()),
            ..Default::default()
        };
        assert!(eligible(&[reference.clone()], Some("vie")));
        assert!(!eligible(&[reference.clone()], Some("eng")));
        assert!(!eligible(&[reference.clone()], Some("fra")));
        reference.is_forced = true;
        assert!(!eligible(&[reference], Some("vie")));
    }
    #[tokio::test]
    async fn unavailable_alignment_still_serves_the_original_subtitle() {
        let (_, guard) =
            crate::integration_test::new_test_server_with_config(crate::Config {
                database_url: Some("sqlite::memory:".into()),
                torrent_http_port: None,
                disable_dht: true,
                subtitle_alignment_gate_base_url: Some("https://example.test".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let state = AppState {
            ctx: guard
                .0
                .clone(),
            tasks: crate::tasks::TaskService::new(
                guard
                    .0
                    .clone(),
            )
            .await
            .unwrap(),
        };
        let source = db::Media {
            probe_data: Some(api::MediaSourceInfo {
                media_streams: vec![api::MediaStream {
                    type_: Some(api::MediaStreamType::Subtitle),
                    codec: Some("srt".into()),
                    language: Some("eng".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let response = super::super::aligned_external_response(
            &state,
            &source,
            Uuid::new_v4(),
            None,
            None,
            None,
            &crate::stream::StreamDescriptor::Local("unused.srt".into()),
            axum::body::Bytes::from_static(
                b"1\n00:00:01,000 --> 00:00:02,000\nOriginal text\n",
            ),
            Some("vie"),
            "vtt",
            false,
        )
        .await;
        // No subtitle_alignment_url/token is configured in this test, so
        // alignment is simply "disabled" — the request must never block on
        // it or fail the response; it serves the already-validated original.
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(
            response.headers()["X-Remux-Subtitle-Alignment"],
            "disabled"
        );
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("Original text"));
        // The separate PlaybackInfo warm-up path still surfaces a real error
        // (e.g. an unknown item) rather than silently doing nothing.
        assert!(
            ensure_ready(&state, &source, Uuid::new_v4(), None)
                .await
                .is_err()
        );
    }
}
