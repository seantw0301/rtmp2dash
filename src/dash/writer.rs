use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio::sync::mpsc as async_mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

const WRITE_QUEUE_DEPTH: usize = 64;

enum WriteJob {
    Bytes { path: PathBuf, data: Vec<u8> },
    Delete { path: PathBuf },
}

struct WriterState {
    tx: Option<async_mpsc::Sender<WriteJob>>,
    join: Option<JoinHandle<()>>,
    /// When true, enqueue/delete must not respawn (intentional shutdown).
    shutdown: bool,
}

/// Per-channel async disk writer: RTMP ingest enqueues; a background task drains
/// via `spawn_blocking` so the read loop never blocks on `std::fs` I/O.
pub struct PackagerWriter {
    out_dir: PathBuf,
    state: Mutex<WriterState>,
}

impl PackagerWriter {
    pub fn spawn(out_dir: PathBuf) -> Self {
        let (tx, join) = spawn_writer_task();
        Self {
            out_dir,
            state: Mutex::new(WriterState {
                tx: Some(tx),
                join: Some(join),
                shutdown: false,
            }),
        }
    }

    pub fn enqueue(&self, name: &str, data: Vec<u8>) {
        let path = self.out_dir.join(name);
        let job = WriteJob::Bytes {
            path,
            data,
        };
        if !self.send_job(name, job) {
            warn!(file = %name, "packager writer unavailable; dropped write");
        }
    }

    /// Queue removal after all older writes, keeping the live cache window bounded.
    pub fn delete(&self, name: &str) {
        let path = self.out_dir.join(name);
        let job = WriteJob::Delete { path };
        if !self.send_job(name, job) {
            // The TTL janitor is the fallback when a saturated/dead writer cannot
            // accept an immediate sliding-window deletion.
            warn!(file = %name, "cache delete queue busy; janitor will remove it");
        }
    }

    /// Close the sender without waiting for the background task (Drop / best-effort).
    pub fn shutdown(&mut self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.shutdown = true;
        state.tx.take();
    }

    /// Close the sender and wait until every queued write/delete has finished.
    /// Call this before replacing a packager so an old MPD cannot land after a wipe.
    pub async fn shutdown_and_drain(&mut self) {
        let join = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            state.shutdown = true;
            state.tx.take();
            state.join.take()
        };
        if let Some(join) = join {
            if let Err(err) = join.await {
                warn!("packager writer task join failed: {err:#}");
            }
        }
    }

    fn send_job(&self, name: &str, job: WriteJob) -> bool {
        let tx = match self.ensure_sender(name) {
            Some(tx) => tx,
            None => return false,
        };
        match tx.try_send(job) {
            Ok(()) => true,
            Err(async_mpsc::error::TrySendError::Full(job)) => {
                warn!("packager write queue full; applying backpressure");
                match tokio::task::block_in_place(|| tx.blocking_send(job)) {
                    Ok(()) => true,
                    Err(async_mpsc::error::SendError(job)) => {
                        // Receiver died while blocked — respawn once and retry.
                        warn!(file = %name, "packager writer closed under backpressure; respawning");
                        self.respawn_sender();
                        let Some(tx2) = self.ensure_sender(name) else {
                            return false;
                        };
                        tx2.try_send(job).is_ok()
                    }
                }
            }
            Err(async_mpsc::error::TrySendError::Closed(job)) => {
                warn!(file = %name, "packager writer closed; respawning");
                self.respawn_sender();
                let Some(tx2) = self.ensure_sender(name) else {
                    return false;
                };
                tx2.try_send(job).is_ok()
            }
        }
    }

    fn ensure_sender(&self, name: &str) -> Option<async_mpsc::Sender<WriteJob>> {
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        if state.shutdown {
            warn!(file = %name, "packager writer shut down; dropped write");
            return None;
        }
        if let Some(tx) = state.tx.as_ref() {
            if !tx.is_closed() {
                return Some(tx.clone());
            }
        }
        // Task gone while we still intend to write — recreate instead of silent drop.
        warn!(file = %name, "packager writer task dead; respawning");
        if let Some(join) = state.join.take() {
            join.abort();
        }
        let (tx, join) = spawn_writer_task();
        state.tx = Some(tx.clone());
        state.join = Some(join);
        Some(tx)
    }

    fn respawn_sender(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.shutdown {
            return;
        }
        if let Some(join) = state.join.take() {
            join.abort();
        }
        let (tx, join) = spawn_writer_task();
        state.tx = Some(tx);
        state.join = Some(join);
    }
}

impl Drop for PackagerWriter {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.shutdown = true;
            state.tx.take();
            // Abort the drain task; prefer `shutdown_and_drain` at explicit finish points.
            if let Some(join) = state.join.take() {
                join.abort();
            }
        }
    }
}

fn spawn_writer_task() -> (async_mpsc::Sender<WriteJob>, JoinHandle<()>) {
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
    (tx, join)
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_writer_respawns_and_accepts_writes() {
        let dir = tempdir().unwrap();
        let writer = PackagerWriter::spawn(dir.path().to_path_buf());
        // Simulate background task death while PackagerWriter still lives.
        {
            let mut state = writer.state.lock().unwrap();
            state.tx.take();
            if let Some(join) = state.join.take() {
                join.abort();
            }
        }
        writer.enqueue("recovered.bin", b"ok".to_vec());
        // Allow the new drain task to flush.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            fs::read(dir.path().join("recovered.bin")).unwrap(),
            b"ok"
        );
    }
}
