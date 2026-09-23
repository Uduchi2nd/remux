use anyhow::Result;
use chrono::{Duration, Utc};
use sqlx::SqlitePool;
use uuid::Uuid;

pub const PRIORITY_NEXT_EPISODE: i64 = 200;
pub const PRIORITY_CURRENT_EPISODE: i64 = 100;
pub const PRIORITY_UPCOMING_EPISODE: i64 = 80;
pub const PRIORITY_RECENT_EPISODE: i64 = 40;

#[derive(Debug, Clone)]
pub struct StreamRefreshJob {
    pub user_id: Uuid,
    pub media_id: Uuid,
    pub series_id: Option<Uuid>,
    pub attempts: i64,
    pub priority: i64,
}

pub async fn enqueue_stream_refresh(
    db: &SqlitePool,
    user_id: Uuid,
    media_id: Uuid,
    series_id: Option<Uuid>,
    priority: i64,
) -> Result<()> {
    let now = Utc::now().naive_utc();
    let run_after = now.to_string();
    sqlx::query(
        "INSERT INTO background_stream_refresh_jobs \
         (user_id, media_id, series_id, priority, run_after, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(user_id, media_id) DO UPDATE SET \
           series_id = COALESCE(excluded.series_id, background_stream_refresh_jobs.series_id), \
           priority = excluded.priority, \
           run_after = MIN(background_stream_refresh_jobs.run_after, excluded.run_after), \
           updated_at = excluded.updated_at",
    )
    .bind(user_id)
    .bind(media_id)
    .bind(series_id)
    .bind(priority)
    .bind(run_after)
    .bind(now.to_string())
    .bind(now.to_string())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn claim_due_stream_refresh(db: &SqlitePool) -> Result<Option<StreamRefreshJob>> {
    let mut tx = db.begin().await?;
    let now = Utc::now().naive_utc();
    let now_s = now.to_string();
    let row = sqlx::query_as::<_, (Uuid, Uuid, Option<Uuid>, i64, i64)>(
        "SELECT user_id, media_id, series_id, attempts, priority \
         FROM background_stream_refresh_jobs \
         WHERE run_after <= ? AND (lease_until IS NULL OR lease_until <= ?) \
         ORDER BY priority DESC, run_after ASC LIMIT 1",
    )
    .bind(&now_s)
    .bind(&now_s)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((user_id, media_id, series_id, attempts, priority)) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    let lease_until = (now + Duration::minutes(5)).to_string();
    sqlx::query(
        "UPDATE background_stream_refresh_jobs \
         SET lease_until = ?, updated_at = ? \
         WHERE user_id = ? AND media_id = ?",
    )
    .bind(lease_until)
    .bind(now_s)
    .bind(user_id)
    .bind(media_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(Some(StreamRefreshJob {
        user_id,
        media_id,
        series_id,
        attempts,
        priority,
    }))
}

pub async fn finish_stream_refresh(
    db: &SqlitePool,
    job: &StreamRefreshJob,
    active_series: bool,
) -> Result<()> {
    let now = Utc::now().naive_utc();
    if active_series && job.series_id.is_some() {
        // Spread work across the 12–14 minute pre-expiry window. Stable-ish
        // per-row jitter prevents every queued item from refreshing together.
        let jitter_secs = if job.priority >= PRIORITY_NEXT_EPISODE {
            0
        } else {
            (job.media_id.as_bytes()[0] as i64) % 121
        };
        let next = now + Duration::minutes(12) + Duration::seconds(jitter_secs);
        sqlx::query(
            "UPDATE background_stream_refresh_jobs \
             SET run_after = ?, lease_until = NULL, attempts = 0, last_error = NULL, updated_at = ? \
             WHERE user_id = ? AND media_id = ?",
        )
        .bind(next.to_string())
        .bind(now.to_string())
        .bind(job.user_id)
        .bind(job.media_id)
        .execute(db)
        .await?;
    } else {
        sqlx::query(
            "DELETE FROM background_stream_refresh_jobs WHERE user_id = ? AND media_id = ?",
        )
        .bind(job.user_id)
        .bind(job.media_id)
        .execute(db)
        .await?;
    }
    Ok(())
}

pub async fn fail_stream_refresh(
    db: &SqlitePool,
    job: &StreamRefreshJob,
    error: &str,
) -> Result<()> {
    let now = Utc::now().naive_utc();
    let backoff = (30_i64 * 2_i64.pow(job.attempts.min(8) as u32)).min(900);
    let next = now + Duration::seconds(backoff);
    sqlx::query(
        "UPDATE background_stream_refresh_jobs \
         SET run_after = ?, lease_until = NULL, attempts = attempts + 1, \
             last_error = ?, updated_at = ? WHERE user_id = ? AND media_id = ?",
    )
    .bind(next.to_string())
    .bind(error.chars().take(400).collect::<String>())
    .bind(now.to_string())
    .bind(job.user_id)
    .bind(job.media_id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn series_recently_played(
    db: &SqlitePool,
    user_id: Uuid,
    series_id: Uuid,
) -> Result<bool> {
    let cutoff = (Utc::now() - Duration::hours(24)).naive_utc();
    let found: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM user_media_state ums \
         JOIN media m ON m.id = ums.media_id \
         WHERE ums.user_id = ? AND m.grandparent_id = ? \
           AND m.kind = 'episode' AND ums.last_played_at >= ? LIMIT 1",
    )
    .bind(user_id)
    .bind(series_id)
    .bind(cutoff)
    .fetch_optional(db)
    .await?;
    Ok(found.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> SqlitePool {
        let db = crate::db::connect("sqlite::memory:", 10_000).await.unwrap();
        crate::db::migrate(&db).await.unwrap();
        db
    }

    #[tokio::test]
    async fn stream_refresh_jobs_deduplicate_and_lease() {
        let db = test_db().await;
        let user = Uuid::new_v4();
        let item = Uuid::new_v4();
        let series = Uuid::new_v4();
        enqueue_stream_refresh(&db, user, item, Some(series), 20).await.unwrap();
        enqueue_stream_refresh(&db, user, item, Some(series), 80).await.unwrap();
        enqueue_stream_refresh(&db, user, item, Some(series), 40).await.unwrap();

        let stored_priority: i64 = sqlx::query_scalar(
            "SELECT priority FROM background_stream_refresh_jobs WHERE user_id = ? AND media_id = ?",
        )
        .bind(user)
        .bind(item)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(stored_priority, 40, "a new playback scope must replace stale priority");

        let claimed = claim_due_stream_refresh(&db).await.unwrap().unwrap();
        assert_eq!(claimed.user_id, user);
        assert_eq!(claimed.media_id, item);
        assert_eq!(claimed.series_id, Some(series));
        assert!(claim_due_stream_refresh(&db).await.unwrap().is_none());

        finish_stream_refresh(&db, &claimed, false).await.unwrap();
        assert!(claim_due_stream_refresh(&db).await.unwrap().is_none());
    }
}
