use async_trait::async_trait;
use chrono::{Duration, Utc};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    AppContext, db,
    signals::{DeliveryMode, Event, EventType, PlaybackContext, Subscriber},
};

pub struct BackgroundPrepareSubscriber {
    pub ctx: AppContext,
}

impl BackgroundPrepareSubscriber {
    pub fn start_worker(ctx: AppContext) {
        if !ctx.config.background_stream_refresh_enabled {
            return;
        }
        tokio::spawn(async move {
            loop {
                match db::claim_due_stream_refresh(&ctx.db).await {
                    Ok(Some(job)) => {
                        let result = async {
                            let Some(mut media) = db::Media::get_by_id(&ctx.db, &job.media_id)
                                .await?
                            else {
                                db::finish_stream_refresh(&ctx.db, &job, false).await?;
                                return Ok::<_, anyhow::Error>(());
                            };
                            ctx.addons
                                .refresh_streams_background(&mut media, &ctx, Some(job.user_id))
                                .await?;
                            let active = match job.series_id {
                                Some(series_id) => {
                                    db::series_recently_played(&ctx.db, job.user_id, series_id).await?
                                }
                                None => false,
                            };
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
                                Some(series_id) => {
                                    db::series_recently_played(&ctx.db, job.user_id, series_id)
                                        .await
                                        .unwrap_or(false)
                                }
                                None => false,
                            };
                            let reschedule = if still_active {
                                db::fail_stream_refresh(&ctx.db, &job, &error.to_string()).await
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
                    Ok(None) => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
                    Err(error) => {
                        warn!(error = %error, "background stream refresh queue unavailable");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }
        });
    }

    async fn enqueue_playback_scope(&self, playback: &PlaybackContext) -> anyhow::Result<()> {
        let Some(media) = db::Media::get_by_id(&self.ctx.db, &playback.media_id).await? else {
            return Ok(());
        };
        let mut targets = vec![(media.id, media.grandparent_id, 100_i64)];

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
                targets.extend(next.into_iter().map(|id| (id, Some(series_id), 80)));

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
                targets.extend(recent.into_iter().map(|id| (id, Some(series_id), 40)));
            }
        }

        for (media_id, series_id, priority) in targets {
            db::enqueue_stream_refresh(
                &self.ctx.db,
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
        if !self.ctx.config.background_stream_refresh_enabled {
            return Ok(());
        }
        if let Event::PlaybackStarted(playback) = event {
            self.enqueue_playback_scope(&playback).await?;
        }
        Ok(())
    }
}
