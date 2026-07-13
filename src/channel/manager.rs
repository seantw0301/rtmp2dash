use crate::config::Config;
use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Tracks concurrent live publishes. Many channels may stream at once;
/// each channel_id accepts only one active publisher.
#[derive(Clone, Default)]
pub struct ChannelManager {
    publishers: Arc<DashMap<String, Arc<ChannelLease>>>,
}

pub struct ChannelLease {
    active: AtomicBool,
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
    pub fn try_acquire(&self, channel_id: &str) -> Option<Arc<ChannelLease>> {
        self.publishers
            .retain(|_, lease| lease.active.load(Ordering::SeqCst));

        if self.publishers.contains_key(channel_id) {
            return None;
        }

        let lease = Arc::new(ChannelLease::new(channel_id.to_string()));
        match self.publishers.entry(channel_id.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(_) => None,
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                slot.insert(Arc::clone(&lease));
                Some(lease)
            }
        }
    }

    /// Drop exclusive publish rights for `channel_id` so another source may acquire it.
    pub fn release(&self, channel_id: &str) {
        self.publishers.remove(channel_id);
    }

    /// Return channel ids that currently hold an active publish/pull lease.
    pub fn list_active(&self) -> Vec<String> {
        self.publishers
            .retain(|_, lease| lease.active.load(Ordering::SeqCst));
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
