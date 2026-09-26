use async_trait::async_trait;
use chrono::{Duration, Utc};
use futures::{StreamExt, stream};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    AppContext,
    db::{
        self, PRIORITY_CURRENT_EPISODE, PRIORITY_NEXT_EPISODE, PRIORITY_RECENT_EPISODE,
        PRIORITY_UPCOMING_EPISODE,
    },
    signals::{DeliveryMode, Event, EventType, PlaybackContext, Subscriber},
};

fn add_refresh_target(
    targets: &mut Vec<(Uuid, Option<Uuid>, i64)>,
    media_id: Uuid,
    series_id: Option<Uuid>,
    priority: i64,
) {
    if let Some((_, _, existing_priority)) = targets
        .iter_mut()
        .find(|(id, _, _)| *id == media_id)
    {
        *existing_priority = (*existing_priority).max(priority);
    } else {
        targets.push((media_id, series_id, priority));
    }
}

const BACKGROUND_PROBE_CONCURRENCY: usize = 2;

fn has_expected_media_stream(
    item_kind: &db::MediaKind,
    source: &crate::api::MediaSourceInfo,
) -> bool {
    match item_kind {
        db::MediaKind::Movie | db::MediaKind::Episode => source
            .video_stream()
            .is_some(),
        db::MediaKind::Track => source
            .audio_stream()
            .is_some(),
        _ => {
            source
                .video_stream()
                .is_some()
                || source
                    .audio_stream()
                    .is_some()
        }
    }
}

async fn probe_background_candidate(
    ctx: AppContext,
    source: db::Media,
    item_kind: db::MediaKind,
    timeout_secs: u64,
    timeout_p2p_secs: u64,
    port: u16,
) -> bool {
    let Some(stream_info) = source
        .stream_info
        .as_ref()
    else {
        crate::playback::probe::record_probe_verification(&source, false);
        return false;
    };
    let url = stream_info
        .descriptor
        .server_input(source.id, port);
    let timeout = if stream_info.is_p2p() {
        timeout_p2p_secs
    } else {
        timeout_secs
    };

    // Force a fresh media probe rather than trusting version metadata from
    // RemuxDB. probe_stream persists successful ffprobe results for playback.
    let mut probe_source = source.clone();
    probe_source.probe_data = None;
    let pool = [probe_source.clone()];
    let result = crate::playback::probe::probe_stream(
        &probe_source,
        Some(url),
        false,
        timeout,
        false,
        0,
        &pool,
        true,
        port,
        &ctx.db,
    )
    .await;

    match result {
        Ok((probe, _)) => {
            let playable = has_expected_media_stream(&item_kind, &probe);
            crate::playback::probe::record_probe_verification(&source, playable);
            playable
        }
        Err(_) => {
            crate::playback::probe::record_probe_verification(&source, false);
            false
        }
    }
}

async fn probe_background_streams(
    ctx: &AppContext,
    item: &mut db::Media,
) -> anyhow::Result<(usize, usize)> {
    let sources: Vec<db::Media> = item
        .streams(&ctx.db)
        .await?
        .into_iter()
        // PATCH (uduchi2nd): a "[+VN dub]" row already carries a synthesized
        // probe, and ffprobe-ing its master playlist would make the seedbox
        // mux the whole episode just to verify it — 100+ muxes / 300 GB in
        // one prefetch sweep. Its HQ source is verified on its own.
        .filter(|s| !crate::services::dubmux::is_dubmux_row(s))
        .collect();
    let checked = sources.len();
    let item_kind = item
        .kind
        .clone();
    let probe_config = db::Settings::get_config_or_default(&ctx.db).await;
    let timeout_secs = probe_config
        .probe_timeout_secs
        .unwrap_or(20) as u64;
    let timeout_p2p_secs = probe_config
        .probe_timeout_p2p_secs
        .unwrap_or(60) as u64;
    let port = ctx
        .config
        .port;

    let results = stream::iter(
        sources
            .into_iter()
            .map(|source| {
                let ctx = ctx.clone();
                let item_kind = item_kind.clone();
                async move {
                    probe_background_candidate(
                        ctx,
                        source,
                        item_kind,
                        timeout_secs,
                        timeout_p2p_secs,
                        port,
                    )
                    .await
                }
            }),
    )
    .buffer_unordered(BACKGROUND_PROBE_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    Ok((
        checked,
        results
            .into_iter()
            .filter(|playable| *playable)
            .count(),
    ))
}

pub struct BackgroundPrepareSubscriber {
    pub ctx: AppContext,
}

impl BackgroundPrepareSubscriber {
    pub fn start_worker(ctx: AppContext) {
        if !ctx
            .config
            .background_stream_refresh_enabled
        {
            return;
        }
        tokio::spawn(async move {
            loop {
                match db::claim_due_stream_refresh(&ctx.db).await {
                    Ok(Some(job)) => {
                        let result = async {
                            let Some(mut media) =
                                db::Media::get_by_id(&ctx.db, &job.media_id).await?
                            else {
                                db::finish_stream_refresh(&ctx.db, &job, false).await?;
                                return Ok::<_, anyhow::Error>(());
                            };
                            ctx.addons
                                .refresh_streams_background(
                                    &mut media,
                                    &ctx,
                                    Some(job.user_id),
                                )
                                .await?;
                            let (checked, playable) =
                                probe_background_streams(&ctx, &mut media).await?;
                            debug!(
                                item = %media.id,
                                checked,
                                playable,
                                "background stream verification finished"
                            );
                            let active = match job.series_id {
                                Some(series_id) => {
                                    db::series_recently_played(
                                        &ctx.db,
                                        job.user_id,
                                        series_id,
                                    )
                                    .await?
                                }
                                None => false,
                            };
                            // PATCH (uduchi2nd): the persistent queue also
                            // keeps dub muxes alive (and, for a series being
                            // watched, muxed) and pre-aligns the external
                            // subtitles of the sources a player would pick —
                            // the same work the on-play walk does, but
                            // retried on every refresh, so anything that
                            // dropped gets rebuilt eventually.
                            let dub_rows =
                                crate::services::dubmux::background_episode_hook(
                                    &ctx, &mut media, active,
                                )
                                .await;
                            if let Some(state) = crate::APP_STATE.get() {
                                let mut candidates: Vec<db::Media> = dub_rows;
                                if let Ok(streams) = media
                                    .streams(&ctx.db)
                                    .await
                                {
                                    candidates.extend(
                                        streams
                                            .into_iter()
                                            .filter(|s| {
                                                !crate::services::dubmux::is_dubmux_row(
                                                    s,
                                                )
                                            })
                                            .take(1),
                                    );
                                }
                                for source in candidates
                                    .iter()
                                    .take(2)
                                {
                                    crate::api::subtitles::gate::prepare_in_background(
                                        state,
                                        source,
                                        media.id,
                                        Some(job.user_id),
                                    );
                                }
                            }
                            db::finish_stream_refresh(&ctx.db, &job, active).await?;
                            Ok(())
                        }
                        .await;

                        if let Err(error) = result {
                            warn!(
                                item = %job.media_id,
                                error = %error,
                                "background stream refresh failed"
                            );
                            let still_active = match job.series_id {
                                Some(series_id) => db::series_recently_played(
                                    &ctx.db,
                                    job.user_id,
                                    series_id,
                                )
                                .await
                                .unwrap_or(false),
                                None => false,
                            };
                            let reschedule = if still_active {
                                db::fail_stream_refresh(
                                    &ctx.db,
                                    &job,
                                    &error.to_string(),
                                )
                                .await
                            } else {
                                db::finish_stream_refresh(&ctx.db, &job, false).await
                            };
                            if let Err(db_error) = reschedule {
                                warn!(
                                    item = %job.media_id,
                                    error = %db_error,
                                    "failed to reschedule stream refresh"
                                );
                            }
                        }
                        // Bound aggregate addon load when playback queues many
                        // recently watched episodes at once.
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                    Ok(None) => {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await
                    }
                    Err(error) => {
                        warn!(error = %error, "background stream refresh queue unavailable");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
        });
    }

    async fn enqueue_playback_scope(
        &self,
        playback: &PlaybackContext,
    ) -> anyhow::Result<()> {
        let Some(media) = db::Media::get_by_id(
            &self
                .ctx
                .db,
            &playback.media_id,
        )
        .await?
        else {
            return Ok(());
        };
        let mut targets = Vec::new();
        add_refresh_target(
            &mut targets,
            media.id,
            media.grandparent_id,
            PRIORITY_CURRENT_EPISODE,
        );

        if media.kind == db::MediaKind::Episode {
            if let Some(series_id) = media.grandparent_id {
                // Prepare the next three available episodes in library order.
                let next = sqlx::query_scalar::<_, Uuid>(
                    "SELECT id FROM media WHERE kind = 'episode' AND grandparent_id = ? \
                     AND (parent_idx > ? OR (parent_idx = ? AND idx > ?)) \
                     ORDER BY parent_idx, idx LIMIT 3",
                )
                .bind(series_id)
                .bind(media.parent_idx.unwrap_or(0))
                .bind(media.parent_idx.unwrap_or(0))
                .bind(media.idx.unwrap_or(0))
                .fetch_all(&self.ctx.db)
                .await?;
                for (offset, id) in next
                    .into_iter()
                    .enumerate()
                {
                    let priority = if offset == 0 {
                        PRIORITY_NEXT_EPISODE
                    } else {
                        PRIORITY_UPCOMING_EPISODE
                    };
                    add_refresh_target(&mut targets, id, Some(series_id), priority);
                }

                // Keep all episodes played by this user in the last week warm,
                // bounded to prevent an unusually large history from flooding the queue.
                let cutoff = (Utc::now() - Duration::days(7)).naive_utc();
                let recent = sqlx::query_scalar::<_, Uuid>(
                    "SELECT m.id FROM media m JOIN user_media_state ums ON ums.media_id = m.id \
                     WHERE m.kind = 'episode' AND m.grandparent_id = ? AND ums.user_id = ? \
                       AND ums.last_played_at >= ? ORDER BY ums.last_played_at DESC LIMIT 100",
                )
                .bind(series_id)
                .bind(playback.user_id)
                .bind(cutoff)
                .fetch_all(&self.ctx.db)
                .await?;
                for id in recent {
                    add_refresh_target(
                        &mut targets,
                        id,
                        Some(series_id),
                        PRIORITY_RECENT_EPISODE,
                    );
                }
            }
        }

        // PATCH (uduchi2nd): one-shot VN-dub prefetch for the upcoming
        // episodes (seedbox extract + match; next episode also pre-muxed).
        // Separate from the refresh queue on purpose: that one re-polls its
        // targets every 12–14 min, which is the wrong cadence for a 10-deep
        // walk of addon calls.
        crate::services::dubmux::spawn_prefetch_upcoming(
            self.ctx
                .clone(),
            media.clone(),
            playback.user_id,
        );

        for (media_id, series_id, priority) in targets {
            db::enqueue_stream_refresh(
                &self
                    .ctx
                    .db,
                playback.user_id,
                media_id,
                series_id,
                priority,
            )
            .await?;
        }
        debug!(
            item = %media.id,
            user = %playback.user_id,
            "queued playback-scope stream preparation"
        );
        Ok(())
    }
}

#[async_trait]
impl Subscriber for BackgroundPrepareSubscriber {
    fn key(&self) -> &'static str {
        "background_prepare"
    }

    fn events(&self) -> &[EventType] {
        &[EventType::PlaybackStarted]
    }

    fn delivery_mode(&self) -> DeliveryMode {
        DeliveryMode::Transient
    }

    async fn handle(&self, event: Event) -> anyhow::Result<()> {
        if !self
            .ctx
            .config
            .background_stream_refresh_enabled
        {
            return Ok(());
        }
        if let Event::PlaybackStarted(playback) = event {
            self.enqueue_playback_scope(&playback)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_episode_keeps_top_priority_when_also_in_recent_history() {
        let media_id = Uuid::new_v4();
        let series_id = Uuid::new_v4();
        let mut targets = Vec::new();

        add_refresh_target(
            &mut targets,
            media_id,
            Some(series_id),
            PRIORITY_NEXT_EPISODE,
        );
        add_refresh_target(
            &mut targets,
            media_id,
            Some(series_id),
            PRIORITY_RECENT_EPISODE,
        );

        assert_eq!(
            targets,
            vec![(media_id, Some(series_id), PRIORITY_NEXT_EPISODE)]
        );
        assert!(PRIORITY_NEXT_EPISODE > PRIORITY_CURRENT_EPISODE);
        assert!(PRIORITY_NEXT_EPISODE > PRIORITY_UPCOMING_EPISODE);
    }
}
