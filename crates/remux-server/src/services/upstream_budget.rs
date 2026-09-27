//! PATCH (uduchi2nd): one small, shared budget for BACKGROUND work that makes
//! the debrid service mint a download link (probing a TorBox/AIOStreams
//! version asks TorBox for a link), plus a circuit breaker.
//!
//! 2026-09-27: the persistent refresh queue re-probed every version of every
//! tracked episode every 12 minutes (~15 TorBox link requests a minute). Once
//! TorBox rate-limited the account, AIOStreams answered every playback URL
//! with its 2-minute `429.mp4` placeholder — for the viewer too — and the
//! background probes kept the limit alive for hours.
//!
//! Rules:
//! - background probes are paced to one per [`BACKGROUND_GAP`];
//! - several placeholder-length answers in a short window trip the breaker:
//!   all background probing stops for 30 min, doubling on each consecutive
//!   trip up to 4 h; a successful probe resets the backoff;
//! - playback (PlaybackInfo) never waits on any of this.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Minimum spacing between two background probes.
pub(crate) const BACKGROUND_GAP: Duration = Duration::from_secs(30);
const FIRST_OPEN: Duration = Duration::from_secs(30 * 60);
const MAX_OPEN: Duration = Duration::from_secs(4 * 3600);
/// AIOStreams' static error clips are ~2 minutes long.
const PLACEHOLDER_MIN_SECS: i64 = 110;
const PLACEHOLDER_MAX_SECS: i64 = 130;
/// This many placeholder answers within [`TRIP_WINDOW`] trip the breaker (a
/// single short release is not a rate limit).
const TRIP_COUNT: usize = 3;
const TRIP_WINDOW: Duration = Duration::from_secs(5 * 60);

struct State {
    open_until: Option<Instant>,
    last_open: Duration,
    recent_placeholders: Vec<Instant>,
}

static STATE: Mutex<State> = Mutex::new(State {
    open_until: None,
    last_open: Duration::ZERO,
    recent_placeholders: Vec::new(),
});

static NEXT_SLOT: tokio::sync::Mutex<Option<Instant>> = tokio::sync::Mutex::const_new(None);

/// True while background work that mints debrid links must not run.
pub(crate) fn breaker_open() -> bool {
    STATE
        .lock()
        .map(|s| {
            s.open_until
                .is_some_and(|t| Instant::now() < t)
        })
        .unwrap_or(false)
}

/// Seconds until the breaker closes (0 when closed), for logs/diagnostics.
pub(crate) fn breaker_remaining_secs() -> u64 {
    STATE
        .lock()
        .ok()
        .and_then(|s| s.open_until)
        .map(|t| {
            t.saturating_duration_since(Instant::now())
                .as_secs()
        })
        .unwrap_or(0)
}

fn next_open(last: Duration) -> Duration {
    if last.is_zero() {
        FIRST_OPEN
    } else {
        (last * 2).min(MAX_OPEN)
    }
}

/// Trip the breaker now (consecutive trips double the pause, up to 4 h).
pub(crate) fn trip(reason: &str) {
    let Ok(mut s) = STATE.lock() else {
        return;
    };
    if s.open_until
        .is_some_and(|t| Instant::now() < t)
    {
        return;
    }
    let open = next_open(s.last_open);
    s.last_open = open;
    s.open_until = Some(Instant::now() + open);
    s.recent_placeholders
        .clear();
    tracing::warn!(
        reason,
        pause_mins = open.as_secs() / 60,
        "upstream rate limit suspected: background probing paused"
    );
}

/// A probe produced media of `probed_secs` duration that failed the
/// short-stream check. Placeholder-length answers count toward a trip.
pub(crate) fn note_short_probe(probed_secs: i64) {
    if !(PLACEHOLDER_MIN_SECS..=PLACEHOLDER_MAX_SECS).contains(&probed_secs) {
        return;
    }
    let trip_now = {
        let Ok(mut s) = STATE.lock() else {
            return;
        };
        let now = Instant::now();
        s.recent_placeholders
            .retain(|t| now.duration_since(*t) < TRIP_WINDOW);
        s.recent_placeholders
            .push(now);
        s.recent_placeholders
            .len()
            >= TRIP_COUNT
    };
    if trip_now {
        trip("repeated placeholder-length probe results (debrid rate limit)");
    }
}

/// A probe returned real media: the upstream is answering again.
pub(crate) fn note_success() {
    if let Ok(mut s) = STATE.lock()
        && s.open_until
            .is_none_or(|t| Instant::now() >= t)
    {
        s.last_open = Duration::ZERO;
        s.recent_placeholders
            .clear();
    }
}

/// Wait for the next background probe slot. Returns false (without waiting
/// further) when the breaker is open — the caller must skip the probe.
pub(crate) async fn background_slot() -> bool {
    if breaker_open() {
        return false;
    }
    let mut next = NEXT_SLOT
        .lock()
        .await;
    if let Some(at) = *next {
        let now = Instant::now();
        if at > now {
            tokio::time::sleep(at - now).await;
        }
    }
    if breaker_open() {
        return false;
    }
    *next = Some(Instant::now() + BACKGROUND_GAP);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(next_open(Duration::ZERO), FIRST_OPEN);
        assert_eq!(next_open(FIRST_OPEN), FIRST_OPEN * 2);
        assert_eq!(next_open(MAX_OPEN), MAX_OPEN);
        assert_eq!(next_open(Duration::from_secs(3 * 3600)), MAX_OPEN);
    }

    #[test]
    fn only_placeholder_length_counts() {
        // outside the placeholder window: never trips
        for _ in 0..10 {
            note_short_probe(45);
            note_short_probe(170);
        }
        assert!(!breaker_open());
        for _ in 0..TRIP_COUNT {
            note_short_probe(120);
        }
        assert!(breaker_open());
        assert!(breaker_remaining_secs() > 0);
    }
}
