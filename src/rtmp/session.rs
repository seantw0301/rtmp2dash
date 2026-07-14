use crate::channel::{ChannelLease, ChannelManager};
use crate::config::Config;
use crate::dash::DashPackager;
use crate::demux::FlvDemux;
use anyhow::{Context, Result};
use bytes::BytesMut;
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, warn};

const HANDSHAKE_BUF_LIMIT: usize = 64 * 1024;

/// Accept RTMP publish connections and spawn one task per peer.
pub async fn run(cfg: Arc<Config>, channels: ChannelManager) -> Result<()> {
    let addr = cfg.rtmp_addr()?;
    let listener = TcpListener::bind(addr).await?;
    info!(
        "RTMP listening on rtmp://{addr}/{}/<channel_id> (multi-channel)",
        cfg.rtmp.app
    );

    loop {
        match listener.accept().await {
            Ok((socket, peer)) => {
                let cfg = Arc::clone(&cfg);
                let channels = channels.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_connection(socket, cfg, channels).await {
                        warn!(%peer, "RTMP connection ended: {err:#}");
                    }
                });
            }
            Err(err) => {
                // Transient accept errors must not kill the publish server.
                warn!("RTMP accept error: {err:#}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Ensures channel lease is released if the connection ends unexpectedly.
struct PublishGuard {
    channels: ChannelManager,
    channel: Option<String>,
    _lease: Option<Arc<ChannelLease>>,
}

impl PublishGuard {
    /// Create a guard that owns no channel lease yet.
    fn new(channels: ChannelManager) -> Self {
        Self {
            channels,
            channel: None,
            _lease: None,
        }
    }

    /// Claim exclusive publish rights for `channel`, releasing any previous lease on this guard.
    fn take_over(&mut self, channel: String, lease: Arc<ChannelLease>) {
        // Replace any previous (shouldn't happen on one connection).
        if let Some(old) = self.channel.take() {
            self.channels.release(&old);
        }
        self.channel = Some(channel);
        self._lease = Some(lease);
    }

    /// Release the current channel lease without waiting for Drop.
    fn clear(&mut self) {
        if let Some(ch) = self.channel.take() {
            self.channels.release(&ch);
        }
        self._lease = None;
    }
}

impl Drop for PublishGuard {
    /// Release the channel lease if the connection ends while still publishing.
    fn drop(&mut self) {
        if let Some(ch) = self.channel.take() {
            self.channels.release(&ch);
            info!(channel = %ch, "publisher lease released (drop)");
        }
    }
}

/// Handshake and run one RTMP publish session until the client disconnects.
async fn handle_connection(
    mut socket: TcpStream,
    cfg: Arc<Config>,
    channels: ChannelManager,
) -> Result<()> {
    let peer = socket.peer_addr().ok();
    info!(?peer, "RTMP client connected");

    let mut guard = PublishGuard::new(channels.clone());

    // --- Handshake ---
    let mut handshake = Handshake::new(PeerType::Server);
    let mut buf = BytesMut::with_capacity(4096);
    let leftover = loop {
        if buf.len() > HANDSHAKE_BUF_LIMIT {
            anyhow::bail!("handshake buffer overflow");
        }
        let n = socket.read_buf(&mut buf).await?;
        if n == 0 {
            anyhow::bail!("client closed during handshake");
        }
        match handshake.process_bytes(&buf)? {
            HandshakeProcessResult::InProgress { response_bytes } => {
                if !response_bytes.is_empty() {
                    socket.write_all(&response_bytes).await?;
                }
                buf.clear();
            }
            HandshakeProcessResult::Completed {
                response_bytes,
                remaining_bytes,
            } => {
                if !response_bytes.is_empty() {
                    socket.write_all(&response_bytes).await?;
                }
                break remaining_bytes;
            }
        }
    };

    let (mut session, initial) = ServerSession::new(ServerSessionConfig::new())?;
    write_results(&mut socket, &initial).await?;

    let mut demux = FlvDemux::new();
    let mut packager: Option<DashPackager> = None;

    if !leftover.is_empty() {
        let results = session.handle_input(&leftover)?;
        process_session_results(
            &mut socket,
            &mut session,
            &cfg,
            &channels,
            &mut demux,
            &mut packager,
            &mut guard,
            results,
        )
        .await?;
    }

    let mut read_buf = vec![0u8; 64 * 1024];
    loop {
        let n = socket.read(&mut read_buf).await?;
        if n == 0 {
            break;
        }
        match session.handle_input(&read_buf[..n]) {
            Ok(results) => {
                process_session_results(
                    &mut socket,
                    &mut session,
                    &cfg,
                    &channels,
                    &mut demux,
                    &mut packager,
                    &mut guard,
                    results,
                )
                .await?;
            }
            Err(err) => {
                warn!("RTMP session parse error (closing connection): {err:#}");
                break;
            }
        }
    }

    for au in demux.flush() {
        if let Some(p) = packager.as_mut() {
            let _ = p.handle_au(au);
        }
    }
    if let Some(mut p) = packager.take() {
        p.finish().await;
    }
    if let Some(ch) = guard.channel.clone() {
        info!(channel = %ch, "publisher disconnected");
    }
    guard.clear();
    Ok(())
}

/// Write outbound packets and dispatch raised server-session events from one input batch.
async fn process_session_results(
    socket: &mut TcpStream,
    session: &mut ServerSession,
    cfg: &Config,
    channels: &ChannelManager,
    demux: &mut FlvDemux,
    packager: &mut Option<DashPackager>,
    guard: &mut PublishGuard,
    results: Vec<ServerSessionResult>,
) -> Result<()> {
    for result in results {
        match result {
            ServerSessionResult::OutboundResponse(packet) => {
                socket.write_all(&packet.bytes).await?;
            }
            ServerSessionResult::RaisedEvent(event) => {
                handle_event(
                    socket,
                    session,
                    cfg,
                    channels,
                    demux,
                    packager,
                    guard,
                    event,
                )
                .await?;
            }
            ServerSessionResult::UnhandleableMessageReceived(_) => {}
        }
    }
    Ok(())
}

/// Handle a single RTMP server event (connect/publish/A/V) for the publish path.
async fn handle_event(
    socket: &mut TcpStream,
    session: &mut ServerSession,
    cfg: &Config,
    channels: &ChannelManager,
    demux: &mut FlvDemux,
    packager: &mut Option<DashPackager>,
    guard: &mut PublishGuard,
    event: ServerSessionEvent,
) -> Result<()> {
    match event {
        ServerSessionEvent::ConnectionRequested {
            request_id,
            app_name,
        } => {
            if app_name != cfg.rtmp.app {
                warn!(%app_name, expected = %cfg.rtmp.app, "rejecting connect: wrong app");
                let results = session.reject_request(
                    request_id,
                    "NetConnection.Connect.Rejected",
                    &format!("app must be '{}'", cfg.rtmp.app),
                )?;
                write_results(socket, &results).await?;
            } else {
                let results = session.accept_request(request_id)?;
                write_results(socket, &results).await?;
            }
        }

        ServerSessionEvent::PublishStreamRequested {
            request_id,
            app_name,
            stream_key,
            mode: _,
        } => {
            if app_name != cfg.rtmp.app {
                let results = session.reject_request(
                    request_id,
                    "NetStream.Publish.BadName",
                    "invalid application",
                )?;
                write_results(socket, &results).await?;
                return Ok(());
            }
            if !is_safe_channel(&stream_key) {
                let results = session.reject_request(
                    request_id,
                    "NetStream.Publish.BadName",
                    "invalid stream key / channel id",
                )?;
                write_results(socket, &results).await?;
                return Ok(());
            }

            match channels.try_acquire(&stream_key) {
                Some(acquired) => {
                    let dir = ChannelManager::ensure_channel_dir(cfg, &stream_key)
                        .context("create channel cache dir")?;
                    match DashPackager::new(dir, &cfg.cache) {
                        Ok(pkg) => {
                            *packager = Some(pkg);
                            guard.take_over(stream_key.clone(), acquired);
                            let results = session.accept_request(request_id)?;
                            write_results(socket, &results).await?;
                            info!(
                                channel = %stream_key,
                                segment_secs = cfg.cache.segment_duration_secs,
                                "publish accepted"
                            );
                        }
                        Err(err) => {
                            error!("packager init failed: {err:#}");
                            let results = session.reject_request(
                                request_id,
                                "NetStream.Publish.Failed",
                                "packager init failed",
                            )?;
                            write_results(socket, &results).await?;
                        }
                    }
                }
                None => {
                    warn!(channel = %stream_key, "channel already has an active source");
                    let results = session.reject_request(
                        request_id,
                        "NetStream.Publish.BadName",
                        "channel already publishing",
                    )?;
                    write_results(socket, &results).await?;
                }
            }
        }

        ServerSessionEvent::PublishStreamFinished {
            app_name: _,
            stream_key,
        } => {
            info!(channel = %stream_key, "publish finished");
            for au in demux.flush() {
                if let Some(p) = packager.as_mut() {
                    let _ = p.handle_au(au);
                }
            }
            if let Some(mut p) = packager.take() {
                p.finish().await;
            }
            guard.clear();
        }

        ServerSessionEvent::VideoDataReceived {
            data,
            timestamp,
            ..
        } => {
            // Live: never tear down the connection for a bad tag.
            match demux.push_video(&data, timestamp.value) {
                Ok(aus) => {
                    for au in aus {
                        if let Some(p) = packager.as_mut() {
                            let _ = p.handle_au(au);
                        }
                    }
                }
                Err(err) => warn!("video demux error (skipped): {err:#}"),
            }
        }

        ServerSessionEvent::AudioDataReceived {
            data,
            timestamp,
            ..
        } => {
            match demux.push_audio(&data, timestamp.value) {
                Ok(aus) => {
                    for au in aus {
                        if let Some(p) = packager.as_mut() {
                            let _ = p.handle_au(au);
                        }
                    }
                }
                Err(err) => warn!("audio demux error (skipped): {err:#}"),
            }
        }

        ServerSessionEvent::ReleaseStreamRequested { .. } => {}

        ServerSessionEvent::StreamMetadataChanged { .. }
        | ServerSessionEvent::ClientChunkSizeChanged { .. }
        | ServerSessionEvent::AcknowledgementReceived { .. }
        | ServerSessionEvent::PingResponseReceived { .. }
        | ServerSessionEvent::UnhandleableAmf0Command { .. } => {}

        ServerSessionEvent::PlayStreamRequested { request_id, .. } => {
            let results = session.reject_request(
                request_id,
                "NetStream.Play.Failed",
                "this server is publish-only (use DASH HTTP for playback)",
            )?;
            write_results(socket, &results).await?;
        }

        ServerSessionEvent::PlayStreamFinished { .. } => {}
    }
    Ok(())
}

/// Send all outbound packets from a batch of server session results to the socket.
async fn write_results(socket: &mut TcpStream, results: &[ServerSessionResult]) -> Result<()> {
    for result in results {
        if let ServerSessionResult::OutboundResponse(packet) = result {
            socket.write_all(&packet.bytes).await?;
        }
    }
    Ok(())
}

/// Return true if `channel` is a safe path segment for cache and HTTP URLs.
fn is_safe_channel(channel: &str) -> bool {
    !channel.is_empty()
        && channel.len() <= 128
        && channel
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}
