use crate::channel::{AcquiredChannel, ChannelManager};
use crate::config::Config;
use crate::dash::DashPackager;
use crate::debug_ndjson::agent_log;
use crate::demux::FlvDemux;
use anyhow::{Context, Result};
use bytes::BytesMut;
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tracing::{error, info, warn};

const HANDSHAKE_BUF_LIMIT: usize = 64 * 1024;
/// Match pull-path behavior: a TCP session that stops sending A/V must not
/// hold the exclusive channel lease forever (blocks republish / stress restart).
/// 45s was too aggressive for live encoder stalls — publishers briefly paused,
/// lease released, then re-push restarted at seg_1 and tore down /mpegts clients.
const MEDIA_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const READ_WAKE_TIMEOUT: Duration = Duration::from_secs(5);
const TAKEOVER_WAIT: Duration = Duration::from_secs(2);

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
    _lease: Option<Arc<crate::channel::ChannelLease>>,
    kick: Option<Arc<crate::channel::KickSignal>>,
}

impl PublishGuard {
    /// Create a guard that owns no channel lease yet.
    fn new(channels: ChannelManager) -> Self {
        Self {
            channels,
            channel: None,
            _lease: None,
            kick: None,
        }
    }

    /// Claim exclusive publish rights for `channel`, releasing any previous lease on this guard.
    fn take_over(&mut self, channel: String, acquired: AcquiredChannel) {
        // Replace any previous (shouldn't happen on one connection).
        if let Some(old) = self.channel.take() {
            self.channels.release(&old);
        }
        self.channel = Some(channel);
        self._lease = Some(acquired.lease);
        self.kick = Some(acquired.kick);
    }

    /// Release the current channel lease without waiting for Drop.
    fn clear(&mut self) {
        if let Some(ch) = self.channel.take() {
            self.channels.release(&ch);
        }
        self._lease = None;
        self.kick = None;
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
    let conn_t0 = Instant::now();
    // #region agent log
    agent_log(
        "C",
        "rtmp/session.rs:handle_connection",
        "rtmp client connected",
        json!({ "peer": peer.map(|p| p.to_string()), "active": channels.list_active() }),
    );
    // #endregion

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
    // Before the first A/V frame, do not enforce media-idle (codecs/config may
    // take a moment). After publishing starts, treat silent TCP as dead.
    let mut last_media: Option<Instant> = None;
    let mut publishing = false;
    let mut eof_reason = "unknown";
    loop {
        if publishing {
            if let Some(t0) = last_media {
                if t0.elapsed() > MEDIA_IDLE_TIMEOUT {
                    warn!(
                        channel = guard.channel.as_deref().unwrap_or("-"),
                        "publish media idle timeout ({MEDIA_IDLE_TIMEOUT:?}) — releasing lease"
                    );
                    eof_reason = "media_idle";
                    // #region agent log
                    agent_log(
                        "E",
                        "rtmp/session.rs:handle_connection",
                        "media idle timeout; releasing lease early",
                        json!({
                            "peer": peer.map(|p| p.to_string()),
                            "channel": guard.channel.clone(),
                            "idle_ms": t0.elapsed().as_millis() as u64,
                        }),
                    );
                    // #endregion
                    // Free the channel immediately so a replacement push can acquire.
                    guard.clear();
                    break;
                }
            }
        }

        // Wake often enough for media-idle + A/V tfdt skew timer checks.
        let skew_wake = Duration::from_secs(cfg.cache.av_tfdt_check_interval_secs.max(1));
        let read_budget = if publishing {
            skew_wake.min(READ_WAKE_TIMEOUT).min(MEDIA_IDLE_TIMEOUT)
        } else {
            Duration::from_secs(60)
        };

        if let Some(kick) = guard.kick.as_ref() {
            if kick.is_signaled() {
                warn!(
                    channel = guard.channel.as_deref().unwrap_or("-"),
                    "publisher kicked for channel takeover"
                );
                eof_reason = "kicked";
                // #region agent log
                agent_log(
                    "A",
                    "rtmp/session.rs:handle_connection",
                    "kicked for takeover; releasing lease early",
                    json!({
                        "peer": peer.map(|p| p.to_string()),
                        "channel": guard.channel.clone(),
                    }),
                );
                // #endregion
                guard.clear();
                break;
            }
        }

        let n = {
            let kick = guard.kick.clone();
            let read_fut = timeout(read_budget, socket.read(&mut read_buf));
            if let Some(kick) = kick {
                tokio::select! {
                    biased;
                    _ = kick.wait() => {
                        warn!(
                            channel = guard.channel.as_deref().unwrap_or("-"),
                            "publisher kicked for channel takeover"
                        );
                        eof_reason = "kicked";
                        // #region agent log
                        agent_log(
                            "A",
                            "rtmp/session.rs:handle_connection",
                            "kicked for takeover; releasing lease early",
                            json!({
                                "peer": peer.map(|p| p.to_string()),
                                "channel": guard.channel.clone(),
                            }),
                        );
                        // #endregion
                        guard.clear();
                        None
                    }
                    read = read_fut => match read {
                        Ok(Ok(n)) => Some(n),
                        Ok(Err(err)) => return Err(err.into()),
                        Err(_) => Some(usize::MAX), // wake sentinel
                    },
                }
            } else {
                match read_fut.await {
                    Ok(Ok(n)) => Some(n),
                    Ok(Err(err)) => return Err(err.into()),
                    Err(_) => Some(usize::MAX),
                }
            }
        };

        let Some(n) = n else {
            break; // kicked
        };
        if n == usize::MAX {
            // Timed wake: idle lease check (above) + A/V tfdt skew timer.
            if let Some(p) = packager.as_mut() {
                p.check_av_skew_on_disk();
            }
            continue;
        }
        if n == 0 {
            eof_reason = "tcp_eof";
            break;
        }
        match session.handle_input(&read_buf[..n]) {
            Ok(results) => {
                let had_av = results.iter().any(|r| {
                    matches!(
                        r,
                        ServerSessionResult::RaisedEvent(
                            ServerSessionEvent::VideoDataReceived { .. }
                                | ServerSessionEvent::AudioDataReceived { .. }
                        )
                    )
                });
                if had_av {
                    last_media = Some(Instant::now());
                    publishing = true;
                }
                if results.iter().any(|r| {
                    matches!(
                        r,
                        ServerSessionResult::RaisedEvent(
                            ServerSessionEvent::PublishStreamRequested { .. }
                        )
                    )
                }) {
                    publishing = true;
                    if last_media.is_none() {
                        last_media = Some(Instant::now());
                    }
                }
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
                eof_reason = "parse_error";
                // #region agent log
                agent_log(
                    "E",
                    "rtmp/session.rs:handle_connection",
                    "session parse error",
                    json!({
                        "peer": peer.map(|p| p.to_string()),
                        "channel": guard.channel.clone(),
                        "error": format!("{err:#}"),
                        "idle_ms": last_media.map(|t| t.elapsed().as_millis() as u64),
                    }),
                );
                // #endregion
                break;
            }
        }
    }

    // #region agent log
    agent_log(
        "A",
        "rtmp/session.rs:handle_connection",
        "publisher loop exit; about to finish packager",
        json!({
            "peer": peer.map(|p| p.to_string()),
            "channel": guard.channel.clone(),
            "eof_reason": eof_reason,
            "conn_ms": conn_t0.elapsed().as_millis() as u64,
            "idle_ms": last_media.map(|t| t.elapsed().as_millis() as u64),
            "has_packager": packager.is_some(),
            "active": channels.list_active(),
        }),
    );
    // #endregion

    for au in demux.flush() {
        if let Some(p) = packager.as_mut() {
            let _ = p.handle_au(au);
        }
    }
    if let Some(mut p) = packager.take() {
        let finish_t0 = Instant::now();
        p.finish().await;
        // #region agent log
        agent_log(
            "B",
            "rtmp/session.rs:handle_connection",
            "packager finish completed",
            json!({
                "channel": guard.channel.clone(),
                "finish_ms": finish_t0.elapsed().as_millis() as u64,
                "eof_reason": eof_reason,
            }),
        );
        // #endregion
    }
    if let Some(ch) = guard.channel.clone() {
        info!(channel = %ch, "publisher disconnected");
    }
    guard.clear();
    // #region agent log
    agent_log(
        "A",
        "rtmp/session.rs:handle_connection",
        "publisher cleaned up (lease cleared)",
        json!({
            "peer": peer.map(|p| p.to_string()),
            "eof_reason": eof_reason,
            "active_after": channels.list_active(),
        }),
    );
    // #endregion
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
                // #region agent log
                agent_log(
                    "C",
                    "rtmp/session.rs:ConnectionRequested",
                    "reject connect wrong app",
                    json!({ "app": app_name, "expected": cfg.rtmp.app }),
                );
                // #endregion
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
            // #region agent log
            agent_log(
                "C",
                "rtmp/session.rs:PublishStreamRequested",
                "publish requested",
                json!({
                    "app": app_name,
                    "channel": stream_key,
                    "active": channels.list_active(),
                }),
            );
            // #endregion
            if app_name != cfg.rtmp.app {
                // #region agent log
                agent_log(
                    "C",
                    "rtmp/session.rs:PublishStreamRequested",
                    "reject publish invalid app",
                    json!({ "app": app_name, "channel": stream_key }),
                );
                // #endregion
                let results = session.reject_request(
                    request_id,
                    "NetStream.Publish.BadName",
                    "invalid application",
                )?;
                write_results(socket, &results).await?;
                return Ok(());
            }
            if !is_safe_channel(&stream_key) {
                // #region agent log
                agent_log(
                    "C",
                    "rtmp/session.rs:PublishStreamRequested",
                    "reject publish unsafe channel",
                    json!({ "channel": stream_key }),
                );
                // #endregion
                let results = session.reject_request(
                    request_id,
                    "NetStream.Publish.BadName",
                    "invalid stream key / channel id",
                )?;
                write_results(socket, &results).await?;
                return Ok(());
            }

            // Prefer immediate acquire; if another zombie/stalled publisher still
            // holds the lease, kick it and wait briefly (live push takeover).
            match channels
                .acquire_with_takeover(&stream_key, TAKEOVER_WAIT)
                .await
            {
                Some(acquired) => {
                    let dir = ChannelManager::ensure_channel_dir(cfg, &stream_key)
                        .context("create channel cache dir")?;
                    // Resume numbering across publisher reconnects / idle re-push.
                    // `new()` clears the channel and restarts at seg_1, which makes
                    // downstream trans_server treat the playlist as a generation
                    // reset (segment numbering regressed) and tear down /mpegts —
                    // STB clients then see Input Exception / Eof loops (ttv/haka).
                    match DashPackager::resume(dir, &cfg.cache) {
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
                            // #region agent log
                            agent_log(
                                "A",
                                "rtmp/session.rs:PublishStreamRequested",
                                "publish accepted",
                                json!({ "channel": stream_key, "active": channels.list_active() }),
                            );
                            // #endregion
                        }
                        Err(err) => {
                            error!("packager init failed: {err:#}");
                            // #region agent log
                            agent_log(
                                "B",
                                "rtmp/session.rs:PublishStreamRequested",
                                "packager init failed; releasing acquire",
                                json!({
                                    "channel": stream_key,
                                    "error": format!("{err:#}"),
                                    "active": channels.list_active(),
                                }),
                            );
                            // #endregion
                            // Drop kick/lease from map — acquired going out of scope is not
                            // enough while the map still holds an Arc clone.
                            channels.release(&stream_key);
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
                    // #region agent log
                    agent_log(
                        "A",
                        "rtmp/session.rs:PublishStreamRequested",
                        "reject publish: channel already publishing (takeover failed)",
                        json!({
                            "channel": stream_key,
                            "active": channels.list_active(),
                        }),
                    );
                    // #endregion
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
            // #region agent log
            agent_log(
                "A",
                "rtmp/session.rs:PublishStreamFinished",
                "publish finished event",
                json!({ "channel": stream_key }),
            );
            // #endregion
            for au in demux.flush() {
                if let Some(p) = packager.as_mut() {
                    let _ = p.handle_au(au);
                }
            }
            if let Some(mut p) = packager.take() {
                let finish_t0 = Instant::now();
                // Flush any tail, then wipe like pull reconnect so source/edge
                // cannot keep serving the pre-restart window across a republish.
                p.finish().await;
                p.prepare_for_reconnect().await;
                // #region agent log
                agent_log(
                    "B",
                    "rtmp/session.rs:PublishStreamFinished",
                    "packager finish+wipe after PublishStreamFinished",
                    json!({
                        "channel": stream_key,
                        "finish_ms": finish_t0.elapsed().as_millis() as u64,
                    }),
                );
                // #endregion
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
