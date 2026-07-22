use crate::config::Config;
use crate::debug_ndjson::agent_log;
use dashmap::DashMap;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::time::{sleep, Instant};

/// Tracks concurrent live publishes. Many channels may stream at once;
/// each channel_id accepts only one active publisher.
#[derive(Clone, Default)]
pub struct ChannelManager {
    publishers: Arc<DashMap<String, ChannelSlot>>,
}

struct ChannelSlot {
    lease: Arc<ChannelLease>,
    kick: Arc<KickSignal>,
}

/// Exclusive publish/pull rights for one channel id.
pub struct ChannelLease {
    active: AtomicBool,
}

/// Sticky takeover flag polled by the active publisher/pull session.
pub struct KickSignal {
    flagged: AtomicBool,
}

impl KickSignal {
    fn new() -> Self {
        Self {
            flagged: AtomicBool::new(false),
        }
    }

    /// Mark this lease for takeover.
    pub fn signal(&self) {
        self.flagged.store(true, Ordering::SeqCst);
    }

    /// Return true if a takeover was requested.
    pub fn is_signaled(&self) -> bool {
        self.flagged.load(Ordering::SeqCst)
    }

    /// Wait until [`Self::signal`] is called (polls; never loses the sticky flag).
    pub async fn wait(&self) {
        while !self.is_signaled() {
            sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Result of a successful [`ChannelManager::try_acquire`].
pub struct AcquiredChannel {
    pub lease: Arc<ChannelLease>,
    /// Signaled when another ingest requests takeover of this channel.
    pub kick: Arc<KickSignal>,
}

impl ChannelLease {
    /// Create an active lease token for exclusive publish rights on a channel.
    fn new(_channel_id: String) -> Self {
        Self {
            active: AtomicBool::new(true),
        }
    }
}

impl Drop for ChannelLease {
    /// Mark the lease inactive so the manager can reclaim the channel id.
    fn drop(&mut self) {
        self.active.store(false, Ordering::SeqCst);
    }
}

impl ChannelManager {
    /// Create an empty multi-channel publish registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to claim exclusive publish rights for `channel_id`.
    /// Returns `None` if another publisher is already live on this channel.
    pub fn try_acquire(&self, channel_id: &str) -> Option<AcquiredChannel> {
        self.publishers
            .retain(|_, slot| slot.lease.active.load(Ordering::SeqCst));

        if self.publishers.contains_key(channel_id) {
            let lease_active_flag = self
                .publishers
                .get(channel_id)
                .map(|s| s.lease.active.load(Ordering::SeqCst));
            let active = self.list_active();
            // #region agent log
            agent_log(
                "A",
                "channel/manager.rs:try_acquire",
                "acquire denied: channel already leased",
                json!({
                    "channel": channel_id,
                    "active": active,
                    "lease_active_flag": lease_active_flag,
                }),
            );
            // #endregion
            return None;
        }

        let lease = Arc::new(ChannelLease::new(channel_id.to_string()));
        let kick = Arc::new(KickSignal::new());
        match self.publishers.entry(channel_id.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(_) => {
                let active = self.list_active();
                // #region agent log
                agent_log(
                    "D",
                    "channel/manager.rs:try_acquire",
                    "acquire lost race on vacant->occupied",
                    json!({ "channel": channel_id, "active": active }),
                );
                // #endregion
                None
            }
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                slot.insert(ChannelSlot {
                    lease: Arc::clone(&lease),
                    kick: Arc::clone(&kick),
                });
                let active = self.list_active();
                // #region agent log
                agent_log(
                    "A",
                    "channel/manager.rs:try_acquire",
                    "acquire ok",
                    json!({ "channel": channel_id, "active": active }),
                );
                // #endregion
                Some(AcquiredChannel { lease, kick })
            }
        }
    }

    /// Ask the current lease holder to exit so a new publisher can take over.
    pub fn signal_kick(&self, channel_id: &str) -> bool {
        // IMPORTANT: drop the DashMap ref BEFORE list_active()/retain — holding a
        // shard lock across retain deadlocks the whole process (HTTP included).
        let kick = {
            let Some(slot) = self.publishers.get(channel_id) else {
                return false;
            };
            Arc::clone(&slot.kick)
        };
        kick.signal();
        let active = self.list_active();
        // #region agent log
        agent_log(
            "A",
            "channel/manager.rs:signal_kick",
            "kick signaled for takeover",
            json!({ "channel": channel_id, "active": active }),
        );
        // #endregion
        true
    }

    /// Kick any existing holder and retry acquire for up to `wait`.
    pub async fn acquire_with_takeover(
        &self,
        channel_id: &str,
        wait: Duration,
    ) -> Option<AcquiredChannel> {
        if let Some(acquired) = self.try_acquire(channel_id) {
            return Some(acquired);
        }

        let kicked = self.signal_kick(channel_id);
        let active = self.list_active();
        // #region agent log
        agent_log(
            "A",
            "channel/manager.rs:acquire_with_takeover",
            "takeover waiting for prior lease release",
            json!({
                "channel": channel_id,
                "kicked": kicked,
                "wait_ms": wait.as_millis() as u64,
                "active": active,
            }),
        );
        // #endregion

        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            sleep(Duration::from_millis(50)).await;
            if let Some(acquired) = self.try_acquire(channel_id) {
                let active = self.list_active();
                // #region agent log
                agent_log(
                    "A",
                    "channel/manager.rs:acquire_with_takeover",
                    "takeover acquire ok",
                    json!({ "channel": channel_id, "active": active }),
                );
                // #endregion
                return Some(acquired);
            }
        }

        let active = self.list_active();
        // #region agent log
        agent_log(
            "A",
            "channel/manager.rs:acquire_with_takeover",
            "takeover timed out",
            json!({ "channel": channel_id, "active": active }),
        );
        // #endregion
        None
    }

    /// Drop exclusive publish rights for `channel_id` so another source may acquire it.
    pub fn release(&self, channel_id: &str) {
        let removed = self.publishers.remove(channel_id).is_some();
        let active_after = self.list_active();
        // #region agent log
        agent_log(
            "A",
            "channel/manager.rs:release",
            "lease release called",
            json!({
                "channel": channel_id,
                "removed": removed,
                "active_after": active_after,
            }),
        );
        // #endregion
    }

    /// Return channel ids that currently hold an active publish/pull lease.
    pub fn list_active(&self) -> Vec<String> {
        self.publishers
            .retain(|_, slot| slot.lease.active.load(Ordering::SeqCst));
        let mut ids: Vec<String> = self
            .publishers
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        ids.sort();
        ids
    }

    /// Ensure `cache/live/<channel_id>` exists and return its path.
    pub fn ensure_channel_dir(cfg: &Config, channel_id: &str) -> std::io::Result<std::path::PathBuf> {
        let dir = cfg.channel_dir(channel_id);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}
