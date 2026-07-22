use crate::channel::{ChannelLease, ChannelManager};
use crate::config::{Config, PullSource};
use crate::dash::DashPackager;
use crate::demux::FlvDemux;
use anyhow::{Context, Result, bail};
use bytes::BytesMut;
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{
    ClientSession, ClientSessionConfig, ClientSessionEvent, ClientSessionResult,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, sleep};
use tracing::{error, info, warn};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Origin may keep the TCP/RTMP session alive (pings) after a host restart
/// without sending A/V. Force reconnect so [`DashPackager::prepare_for_reconnect`]
/// can open a fresh CMAF generation.
const MEDIA_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const HANDSHAKE_BUF_LIMIT: usize = 64 * 1024;

/// Run all configured pull sources. Never returns (keeps process alive).
pub async fn run_all(cfg: Arc<Config>, channels: ChannelManager) -> Result<()> {
    if cfg.pull.is_empty() {
        info!("no pull sources configured");
        loop {
            sleep(Duration::from_secs(3600)).await;
        }
    }

    for src in cfg.pull.clone() {
        let cfg = Arc::clone(&cfg);
        let channels = channels.clone();
        tokio::spawn(async move {
            run_one(cfg, channels, src).await;
        });
    }

    // Park forever — individual workers self-heal via reconnect loops.
    loop {
        sleep(Duration::from_secs(3600)).await;
    }
}

/// Acquire a channel lease for one pull source, then run its reconnect loop.
async fn run_one(cfg: Arc<Config>, channels: ChannelManager, src: PullSource) {
    let Some(acquired) = channels.try_acquire(&src.channel) else {
        error!(
            channel = %src.channel,
            "pull channel already in use; will retry acquire"
        );
        // Keep trying so a transient publish conflict does not kill pull forever.
        let reconnect = Duration::from_secs(src.reconnect_secs.max(1));
        loop {
            sleep(reconnect).await;
            if let Some(acquired) = channels.try_acquire(&src.channel) {
                info!(channel = %src.channel, "pull channel acquired after wait");
                run_with_lease(cfg, src, acquired.lease).await;
                return;
            }
        }
    };

    run_with_lease(cfg, src, acquired.lease).await;
}

/// Keep pulling a remote RTMP source under an exclusive channel lease, reconnecting on failure.
///
/// The packager instance (and segment **numbering**) lives for the lease, but each
/// RTMP session gets a **fresh CMAF generation** after disconnect: origin restarts
/// reset `tfdt` / init even when codec strings look identical, so reusing the old
/// Segmenter leaves a ghost MPD with 404 segments.
async fn run_with_lease(cfg: Arc<Config>, src: PullSource, lease: Arc<ChannelLease>) {
    info!(
        channel = %src.channel,
        url = %src.url,
        "pull worker started → http://{}:{}/live/{}/index.mpd",
        cfg.dash.listen,
        cfg.dash.port,
        src.channel
    );

    let dir = match ChannelManager::ensure_channel_dir(&cfg, &src.channel) {
        Ok(d) => d,
        Err(err) => {
            error!(channel = %src.channel, "ensure channel dir failed: {err:#}");
            return;
        }
    };
    let mut packager = match DashPackager::resume(dir, &cfg.cache) {
        Ok(p) => p,
        Err(err) => {
            error!(channel = %src.channel, "packager init failed: {err:#}");
            return;
        }
    };

    let reconnect = Duration::from_secs(src.reconnect_secs.max(1));
    loop {
        match pull_session(&src, Arc::clone(&lease), &mut packager).await {
            Ok(()) => warn!(channel = %src.channel, "pull session ended"),
            Err(err) => warn!(channel = %src.channel, "pull session error: {err:#}"),
        }
        packager.prepare_for_reconnect().await;
        info!(
            channel = %src.channel,
            secs = reconnect.as_secs(),
            "reconnecting pull source"
        );
        sleep(reconnect).await;
    }
}

/// Connect to one remote RTMP URL, handshake, play the stream, and package DASH until disconnect.
async fn pull_session(
    src: &PullSource,
    _lease: Arc<ChannelLease>,
    packager: &mut DashPackager,
) -> Result<()> {
    let parsed = src.parse_url()?;
    let addr = format!("{}:{}", parsed.host, parsed.port);
    info!(
        channel = %src.channel,
        %addr,
        app = %parsed.app,
        stream = %parsed.stream_key,
        "connecting pull"
    );

    let mut socket = timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr))
        .await
        .with_context(|| format!("connect timeout {addr}"))?
        .with_context(|| format!("connect {addr}"))?;

    // --- Client handshake ---
    let mut handshake = Handshake::new(PeerType::Client);
    let c0c1 = handshake.generate_outbound_p0_and_p1()?;
    socket.write_all(&c0c1).await?;

    let mut buf = BytesMut::with_capacity(4096);
    let leftover = loop {
        if buf.len() > HANDSHAKE_BUF_LIMIT {
            bail!("handshake buffer overflow");
        }
        let n = timeout(READ_TIMEOUT, socket.read_buf(&mut buf))
            .await
            .context("handshake read timeout")??;
        if n == 0 {
            bail!("server closed during handshake");
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

    let mut client_cfg = ClientSessionConfig::new();
    client_cfg.tc_url = Some(parsed.tc_url.clone());
    let (mut session, initial) = ClientSession::new(client_cfg)?;
    write_client_results(&mut socket, &initial).await?;

    let connect_result = session.request_connection(parsed.app.clone())?;
    write_one(&mut socket, &connect_result).await?;

    let mut demux = FlvDemux::new();

    let mut connected = false;
    let mut playing = false;
    let mut last_media_at = Instant::now();

    if !leftover.is_empty() {
        let results = session.handle_input(&leftover)?;
        process_client_results(
            &mut socket,
            &mut session,
            &mut demux,
            packager,
            &parsed.stream_key,
            &mut connected,
            &mut playing,
            &mut last_media_at,
            results,
        )
        .await?;
    }

    let mut read_buf = vec![0u8; 64 * 1024];
    loop {
        if playing && last_media_at.elapsed() > MEDIA_IDLE_TIMEOUT {
            bail!(
                "pull media idle timeout ({MEDIA_IDLE_TIMEOUT:?}) — forcing reconnect for generation reset"
            );
        }

        let read_budget = if playing {
            // Wake often enough for MEDIA_IDLE_TIMEOUT and A/V tfdt skew timer.
            READ_TIMEOUT
                .min(MEDIA_IDLE_TIMEOUT)
                .min(Duration::from_secs(2))
        } else {
            READ_TIMEOUT
        };
        let n = match timeout(read_budget, socket.read(&mut read_buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => n,
            Ok(Err(err)) => return Err(err.into()),
            Err(_) if playing && last_media_at.elapsed() > MEDIA_IDLE_TIMEOUT => {
                bail!(
                    "pull media idle timeout ({MEDIA_IDLE_TIMEOUT:?}) — forcing reconnect for generation reset"
                );
            }
            Err(_) if playing => {
                packager.check_av_skew_on_disk();
                continue;
            }
            Err(_) => bail!("pull read idle timeout ({READ_TIMEOUT:?})"),
        };

        let results = match session.handle_input(&read_buf[..n]) {
            Ok(r) => r,
            Err(err) => {
                warn!("pull RTMP parse error: {err:#}");
                break;
            }
        };
        process_client_results(
            &mut socket,
            &mut session,
            &mut demux,
            packager,
            &parsed.stream_key,
            &mut connected,
            &mut playing,
            &mut last_media_at,
            results,
        )
        .await?;
    }

    for au in demux.flush() {
        let _ = packager.handle_au(au);
    }
    Ok(())
}

/// Write outbound RTMP packets and dispatch raised client-session events from one input batch.
async fn process_client_results(
    socket: &mut TcpStream,
    session: &mut ClientSession,
    demux: &mut FlvDemux,
    packager: &mut DashPackager,
    stream_key: &str,
    connected: &mut bool,
    playing: &mut bool,
    last_media_at: &mut Instant,
    results: Vec<ClientSessionResult>,
) -> Result<()> {
    for result in results {
        match result {
            ClientSessionResult::OutboundResponse(packet) => {
                socket.write_all(&packet.bytes).await?;
            }
            ClientSessionResult::RaisedEvent(event) => {
                handle_client_event(
                    socket,
                    session,
                    demux,
                    packager,
                    stream_key,
                    connected,
                    playing,
                    last_media_at,
                    event,
                )
                .await?;
            }
            ClientSessionResult::UnhandleableMessageReceived(_) => {}
        }
    }
    Ok(())
}

/// Handle a single RTMP client event (connect/play accept, A/V data) for the pull path.
async fn handle_client_event(
    socket: &mut TcpStream,
    session: &mut ClientSession,
    demux: &mut FlvDemux,
    packager: &mut DashPackager,
    stream_key: &str,
    connected: &mut bool,
    playing: &mut bool,
    last_media_at: &mut Instant,
    event: ClientSessionEvent,
) -> Result<()> {
    match event {
        ClientSessionEvent::ConnectionRequestAccepted => {
            info!(stream = %stream_key, "pull connect accepted");
            *connected = true;
            let play = session.request_playback(stream_key.to_string())?;
            write_one(socket, &play).await?;
        }
        ClientSessionEvent::ConnectionRequestRejected { description } => {
            bail!("pull connect rejected: {description}");
        }
        ClientSessionEvent::PlaybackRequestAccepted => {
            info!(stream = %stream_key, "pull playback accepted");
            *playing = true;
            *last_media_at = Instant::now();
        }
        ClientSessionEvent::VideoDataReceived { data, timestamp } => {
            *last_media_at = Instant::now();
            match demux.push_video(&data, timestamp.value) {
                Ok(aus) => {
                    for au in aus {
                        let _ = packager.handle_au(au);
                    }
                }
                Err(err) => warn!("pull video demux skipped: {err:#}"),
            }
        }
        ClientSessionEvent::AudioDataReceived { data, timestamp } => {
            *last_media_at = Instant::now();
            match demux.push_audio(&data, timestamp.value) {
                Ok(aus) => {
                    for au in aus {
                        let _ = packager.handle_au(au);
                    }
                }
                Err(err) => warn!("pull audio demux skipped: {err:#}"),
            }
        }
        ClientSessionEvent::StreamMetadataReceived { .. }
        | ClientSessionEvent::AcknowledgementReceived { .. }
        | ClientSessionEvent::PingResponseReceived { .. }
        | ClientSessionEvent::PublishRequestAccepted
        | ClientSessionEvent::UnhandleableAmf0Command { .. }
        | ClientSessionEvent::UnknownTransactionResultReceived { .. }
        | ClientSessionEvent::UnhandleableOnStatusCode { .. } => {}
    }

    let _ = connected;
    Ok(())
}

/// Send all outbound packets from a batch of client session results to the socket.
async fn write_client_results(socket: &mut TcpStream, results: &[ClientSessionResult]) -> Result<()> {
    for r in results {
        write_one(socket, r).await?;
    }
    Ok(())
}

/// Write a single client-session result to the socket when it carries an outbound packet.
async fn write_one(socket: &mut TcpStream, result: &ClientSessionResult) -> Result<()> {
    if let ClientSessionResult::OutboundResponse(packet) = result {
        socket.write_all(&packet.bytes).await?;
    }
    Ok(())
}
