//! Tier classifier + dispatcher.
//!
//! Classifies an `until` timestamp into Hot / Warm / Cold, then
//! dispatches to the impl wired up for that tier. The default windows
//! match ADR-004:
//!   - Hot: now-back to `now - 90d`
//!   - Warm: `now - 90d` to `now - 5y`
//!   - Cold: older than `now - 5y`
//!
//! Per-schema retention windows (per `timeMachine.storage.hot.retention`
//! on `SchemaDefinition`) are a Phase 4 follow-up — the registry would
//! need to plumb the value down. For now the windows are platform-wide.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use super::event_reader::{EventReader, EventRow, TierError};

/// Which tier a timestamp falls into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Hot,
    Warm,
    Cold,
}

#[derive(Debug, Clone, Copy)]
pub struct TierWindows {
    pub hot_days: i64,
    pub warm_years: i64,
}

impl Default for TierWindows {
    fn default() -> Self {
        Self { hot_days: 90, warm_years: 5 }
    }
}

impl TierWindows {
    pub fn classify(&self, now: DateTime<Utc>, until: DateTime<Utc>) -> Tier {
        let hot_floor = now - Duration::days(self.hot_days);
        let warm_floor = now - Duration::days(self.warm_years * 365);
        if until >= hot_floor {
            Tier::Hot
        } else if until >= warm_floor {
            Tier::Warm
        } else {
            Tier::Cold
        }
    }
}

pub struct TieredEventReader {
    hot: Arc<dyn EventReader>,
    /// `None` when warm-tier is not configured for this deployment.
    /// Warm-tier requests then return `TierError::WarmNotConfigured`
    /// rather than silently falling back to hot.
    warm: Option<Arc<dyn EventReader>>,
    windows: TierWindows,
}

impl std::fmt::Debug for TieredEventReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TieredEventReader")
            .field("hot", &"<EventReader>")
            .field("warm", &self.warm.as_ref().map(|_| "<EventReader>").unwrap_or("<none>"))
            .field("windows", &self.windows)
            .finish()
    }
}

impl TieredEventReader {
    pub fn new(hot: Arc<dyn EventReader>, warm: Option<Arc<dyn EventReader>>) -> Self {
        Self {
            hot,
            warm,
            windows: TierWindows::default(),
        }
    }

    pub fn with_windows(mut self, windows: TierWindows) -> Self {
        self.windows = windows;
        self
    }

    /// Expose tier classification so handlers can short-circuit
    /// tier-specific responses (e.g. cold returns 202 before any
    /// reader work happens).
    pub fn classify(&self, until: DateTime<Utc>) -> Tier {
        self.windows.classify(Utc::now(), until)
    }
}

#[async_trait]
impl EventReader for TieredEventReader {
    async fn events_for(
        &self,
        path: &str,
        entity_id: Uuid,
        until: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<EventRow>, TierError> {
        match self.classify(until) {
            Tier::Hot => self.hot.events_for(path, entity_id, until, limit).await,
            Tier::Warm => {
                let Some(warm) = self.warm.as_ref() else {
                    return Err(TierError::WarmNotConfigured);
                };
                warm.events_for(path, entity_id, until, limit).await
            }
            Tier::Cold => Err(TierError::ColdNotSupported),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn classifies_recent_as_hot() {
        let now = Utc::now();
        let t = TierWindows::default().classify(now, now - Duration::days(5));
        assert_eq!(t, Tier::Hot);
    }

    #[test]
    fn classifies_six_months_old_as_warm() {
        let now = Utc::now();
        let t = TierWindows::default().classify(now, now - Duration::days(180));
        assert_eq!(t, Tier::Warm);
    }

    #[test]
    fn classifies_ancient_as_cold() {
        let now = Utc::now();
        let t = TierWindows::default().classify(now, now - Duration::days(365 * 6));
        assert_eq!(t, Tier::Cold);
    }

    #[test]
    fn hot_warm_boundary_inclusive_on_hot_side() {
        // Exactly at the 90-day mark counts as Hot (>= hot_floor).
        let now = Utc::now();
        let exactly_90 = now - Duration::days(90);
        let t = TierWindows::default().classify(now, exactly_90);
        assert_eq!(t, Tier::Hot);
    }

    #[test]
    fn just_inside_warm_boundary() {
        // 90 days + 1 second past = warm.
        let now = Utc::now();
        let t = TierWindows::default()
            .classify(now, now - Duration::days(90) - Duration::seconds(1));
        assert_eq!(t, Tier::Warm);
    }
}
