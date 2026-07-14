use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc as async_mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

const WRITE_QUEUE_DEPTH: usize = 64;

enum WriteJob {
    Bytes { path: PathBuf, data: Vec<u8> },
    Delete { path: PathBuf },
}

/// Per-channel async disk writer: RTMP ingest enqueues; a background task drains
/// via `spawn_blocking` so the read loop never blocks on `std::fs` I/O.
pub struct PackagerWriter {
    out_dir: PathBuf,
    tx: Option<async_mpsc::Sender<WriteJob>>,
    join: Option<JoinHandle<()>>,
}

impl PackagerWriter {
    pub fn spawn(out_dir: PathBuf) -> Self {
        let (tx, mut rx) = async_mpsc::channel(WRITE_QUEUE_DEPTH);
        let join = tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                match job {
                    WriteJob::Bytes { path, data } => {
                        let file_path = path.display().to_string();
                        let write_result =
                            tokio::task::spawn_blocking(move || atomic_write(&path, &data)).await;
                        match write_result {
                            Ok(Ok(())) => {}
                            Ok(Err(err)) => {
                                warn!(file = %file_path, "disk write failed: {err:#}");
                            }
                            Err(err) => {
                                warn!(file = %file_path, "write task join failed: {err:#}");
                            }
                        }
                    }
                    WriteJob::Delete { path } => {
                        if let Err(err) = tokio::fs::remove_file(&path).await {
                            if err.kind() != std::io::ErrorKind::NotFound {
                                warn!(file = %path.display(), "cache delete failed: {err}");
                            }
                        }
                    }
                }
            }
        });
        Self {
            out_dir,
            tx: Some(tx),
            join: Some(join),
        }
    }

    pub fn enqueue(&self, name: &str, data: Vec<u8>) {
        let Some(tx) = &self.tx else {
            warn!(file = %name, "packager writer shut down; dropped write");
            return;
        };
        let path = self.out_dir.join(name);
        let job = WriteJob::Bytes { path, data };
        if let Err(err) = tx.try_send(job) {
            match err {
                async_mpsc::error::TrySendError::Full(job) => {
                    warn!("packager write queue full; applying backpressure");
                    if tokio::task::block_in_place(|| tx.blocking_send(job)).is_err() {
                        warn!(file = %name, "packager writer closed; dropped write");
                    }
                }
                async_mpsc::error::TrySendError::Closed(_) => {
                    warn!(file = %name, "packager writer closed; dropped write");
                }
            }
        }
    }

    /// Queue removal after all older writes, keeping the live cache window bounded.
    pub fn delete(&self, name: &str) {
        let Some(tx) = &self.tx else {
            return;
        };
        let path = self.out_dir.join(name);
        if tx.try_send(WriteJob::Delete { path }).is_err() {
            // The TTL janitor is the fallback when a saturated writer cannot
            // accept an immediate sliding-window deletion.
            warn!(file = %name, "cache delete queue busy; janitor will remove it");
        }
    }

    /// Close the sender without waiting for the background task (Drop / best-effort).
    pub fn shutdown(&mut self) {
        self.tx.take();
    }

    /// Close the sender and wait until every queued write/delete has finished.
    /// Call this before replacing a packager so an old MPD cannot land after a wipe.
    pub async fn shutdown_and_drain(&mut self) {
        self.tx.take();
        if let Some(join) = self.join.take() {
            if let Err(err) = join.await {
                warn!("packager writer task join failed: {err:#}");
            }
        }
    }
}

impl Drop for PackagerWriter {
    fn drop(&mut self) {
        self.tx.take();
        // JoinHandle is dropped without awaiting — Tokio cancels on Abort,
        // but the channel close already lets the task exit after draining.
        // Prefer `shutdown_and_drain` at explicit finish points.
    }
}

/// Write `data` via a temp file then rename, so readers never see a partial file.
pub(crate) fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(err.into())
        }
    }
}

/// Best-effort clear of leftover segment files from a previous run (publish startup only).
pub(crate) fn clear_channel_dir(out_dir: &Path) -> Result<()> {
    fs::create_dir_all(out_dir)
        .with_context(|| format!("create channel dir {}", out_dir.display()))?;
    if let Ok(entries) = fs::read_dir(out_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let _ = fs::remove_file(&path);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_and_drain_flushes_queued_writes() {
        let dir = tempdir().unwrap();
        let mut writer = PackagerWriter::spawn(dir.path().to_path_buf());
        writer.enqueue("a.bin", b"hello".to_vec());
        writer.enqueue("b.bin", b"world".to_vec());
        writer.shutdown_and_drain().await;
        assert_eq!(fs::read(dir.path().join("a.bin")).unwrap(), b"hello");
        assert_eq!(fs::read(dir.path().join("b.bin")).unwrap(), b"world");
    }
}
