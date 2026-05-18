//! Cold tier — Phase 4.7 stub.
//!
//! Real Glacier retrieval is deferred; the contract this stub
//! establishes is: a cold-tier time-machine request returns 202 with
//! a `job_id` immediately and a completion notification path (webhook,
//! email) ships later. The in-memory job store here is just enough
//! for callers to receive a stable ID and check it back; no actual
//! retrieval happens.

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Serialize;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
pub struct ColdJob {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub path: String,
    pub entity_id: Uuid,
    pub until: DateTime<Utc>,
    pub status: ColdJobStatus,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ColdJobStatus {
    Accepted,
    // Future states (Retrieving, Ready, Failed) land when the real
    // Glacier integration ships. Listed here so the serialised shape
    // is stable from day one.
}

#[derive(Debug, Default)]
pub struct ColdJobStore {
    jobs: DashMap<Uuid, ColdJob>,
}

impl ColdJobStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { jobs: DashMap::new() })
    }

    pub fn create(&self, path: String, entity_id: Uuid, until: DateTime<Utc>) -> ColdJob {
        let job = ColdJob {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            path,
            entity_id,
            until,
            status: ColdJobStatus::Accepted,
        };
        self.jobs.insert(job.id, job.clone());
        job
    }

    pub fn get(&self, id: Uuid) -> Option<ColdJob> {
        self.jobs.get(&id).map(|r| r.clone())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn creates_and_retrieves_job() {
        let store = ColdJobStore::new();
        let entity = Uuid::new_v4();
        let until = Utc::now();
        let job = store.create("a/b/c".into(), entity, until);
        let fetched = store.get(job.id).unwrap();
        assert_eq!(fetched.id, job.id);
        assert_eq!(fetched.path, "a/b/c");
        assert_eq!(fetched.entity_id, entity);
    }
}
