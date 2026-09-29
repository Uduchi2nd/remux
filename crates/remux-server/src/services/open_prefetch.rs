//! PATCH (uduchi2nd, 2026-09-29): fetch the stream list when a title's page is
//! opened, before the viewer presses play.
//!
//! A cold PlaybackInfo spent 10–16 s waiting for the AIOStreams stream list.
//! Clients that fetch the item document without `MediaSources` (Infuse,
//! VidHub) never triggered that fetch until play. Opening an episode/movie
//! page now refreshes its list in the background (honouring the list TTL),
//! and opening a show page does the same for the episode most likely to be
//! played next. A list fetch mints no debrid links (only playback does), but
//! counts against AIOStreams' request limit (20 per 15 s per IP), so
//! prefetches run one at a time with a gap and each item at most once per
//! 10 minutes.

use crate::{AppContext, db};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{debug, warn};
use uuid::Uuid;

const PER_ITEM: Duration = Duration::from_secs(600);
const GAP: Duration = Duration::from_secs(1);

static SEEN: Mutex<Option<HashMap<Uuid, Instant>>> = Mutex::new(None);
static ONE_AT_A_TIME: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

fn first_time(id: Uuid) -> bool {
    let Ok(mut guard) = SEEN.lock() else {
        return false;
    };
    let map = guard.get_or_insert_with(Default::default);
    let now = Instant::now();
    map.retain(|_, t| now.duration_since(*t) < PER_ITEM);
    if map.contains_key(&id) {
        return false;
    }
    map.insert(id, now);
    true
}

/// Called when an item document is served; returns at once.
pub(crate) fn prefetch_on_open(ctx: &AppContext, media: &db::Media, user: Uuid) {
    if !matches!(
        media.kind,
        db::MediaKind::Movie | db::MediaKind::Episode | db::MediaKind::Series
    ) || !first_time(media.id)
    {
        return;
    }
    let ctx = ctx.clone();
    let media = media.clone();
    tokio::spawn(async move {
        let target = if media.kind == db::MediaKind::Series {
            match next_episode(&ctx, media.id, user).await {
                Some(ep) if first_time(ep.id) => ep,
                _ => return,
            }
        } else {
            media
        };
        let Ok(_permit) = ONE_AT_A_TIME
            .acquire()
            .await
        else {
            return;
        };
        let mut target = target;
        if let Err(e) = ctx
            .addons
            .refresh_streams(&mut target, &ctx, Some(user))
            .await
        {
            warn!(item = %target.id, "open prefetch: stream refresh failed: {e:#}");
        } else {
            debug!(item = %target.id, "open prefetch: stream list ready");
            crate::services::dubmux::prepare_on_open(&ctx, &target, user);
        }
        tokio::time::sleep(GAP).await;
    });
}

/// The episode the viewer most likely plays next: the one in progress, else
/// the one after the last finished, else the first regular episode.
async fn next_episode(ctx: &AppContext, series_id: Uuid, user: Uuid) -> Option<db::Media> {
    let last: Option<(Uuid, i64, i64, i64)> = sqlx::query_as(
        "SELECT m.id, COALESCE(m.parent_idx, 0), COALESCE(m.idx, 0), COALESCE(s.play_count, 0) \
         FROM user_media_state s JOIN media m ON m.id = s.media_id \
         WHERE s.user_id = ? AND m.grandparent_id = ? AND m.kind = 'episode' \
         AND (COALESCE(s.play_count, 0) > 0 OR COALESCE(s.playback_position, 0) > 0) \
         ORDER BY COALESCE(s.last_played_at, s.played_at) DESC LIMIT 1",
    )
    .bind(user)
    .bind(series_id)
    .fetch_optional(&ctx.db)
    .await
    .ok()
    .flatten();
    let id: Option<Uuid> = match last {
        Some((id, _, _, 0)) => Some(id),
        Some((_, season, ep, _)) => sqlx::query_scalar(
            "SELECT id FROM media WHERE kind = 'episode' AND grandparent_id = ? \
             AND (parent_idx > ? OR (parent_idx = ? AND idx > ?)) \
             ORDER BY parent_idx, idx LIMIT 1",
        )
        .bind(series_id)
        .bind(season)
        .bind(season)
        .bind(ep)
        .fetch_optional(&ctx.db)
        .await
        .ok()
        .flatten(),
        None => sqlx::query_scalar(
            "SELECT id FROM media WHERE kind = 'episode' AND grandparent_id = ? \
             AND COALESCE(parent_idx, 0) > 0 ORDER BY parent_idx, idx LIMIT 1",
        )
        .bind(series_id)
        .fetch_optional(&ctx.db)
        .await
        .ok()
        .flatten(),
    };
    db::Media::get_by_id(&ctx.db, &id?)
        .await
        .ok()
        .flatten()
}
