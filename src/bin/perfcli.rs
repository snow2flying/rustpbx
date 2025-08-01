use anyhow::Result;
use clap::Parser;
use dotenv::dotenv;
use futures::{SinkExt, StreamExt};
use rustpbx::{
    event::SessionEvent,
    handler::{CallOption, Command},
    media::{
        codecs::{g722::G722Encoder, resample::resample_mono, CodecType, Encoder},
        negotiate::strip_ipv6_candidates,
        track::{file::read_wav_file, webrtc::WebrtcTrack},
        vad::VADOption,
    },
    version,
};
use std::{
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};
use tokio::{select, time};
use tokio_tungstenite::tungstenite;
use tracing::{error, info, level_filters::LevelFilter};
use uuid::Uuid;
use webrtc::{
    api::APIBuilder,
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::{rtp_receiver::RTCRtpReceiver, RTCRtpTransceiver},
    track::{
        track_local::track_local_static_sample::TrackLocalStaticSample, track_remote::TrackRemote,
    },
};

struct AppState {
    alive: AtomicU32,
    rx_bytes: AtomicU64,
    tx_bytes: AtomicU64,
}

#[derive(Parser, Debug, Clone)]
#[command(
    author,
    version = version::get_short_version(),
    about = "A WebRTC performance test client with RTCP packet loss monitoring",
    long_about = version::get_version_info()
)]
struct Cli {
    /// Number of concurrent connections
    #[clap(long, help = "Number of concurrent connections", default_value = "10")]
    clients: u32,

    /// Endpoint of the server
    #[clap(
        long,
        help = "Endpoint of the server",
        default_value = "ws://localhost:8080/call/webrtc"
    )]
    endpoint: String,

    /// Path to the file to play
    #[clap(long, help = "Url to the file to play(Server side)")]
    play_file: String,

    /// Path to the file to play
    #[clap(long, help = "Path to the file to play(Client side)")]
    input_file: String,

    /// Verbose
    #[clap(long, help = "Verbose")]
    verbose: bool,

    /// Codec type
    #[clap(long, help = "Codec type", default_value = "g722")]
    codec: String,

    /// VAD
    #[clap(long, help = "VAD", default_value = "true")]
    vad: Option<bool>,

    /// Denoising
    #[clap(long, help = "Denoising", default_value = "true")]
    denoise: Option<bool>,
}

async fn serve_client(codec: CodecType, cli: Cli, id: u32, state: Arc<AppState>) -> Result<()> {
    // Create WebRTC client
    let media_engine = WebrtcTrack::get_media_engine(Some(codec))?;
    let api = APIBuilder::new().with_media_engine(media_engine).build();
    let config = RTCConfiguration {
        ..Default::default()
    };

    let peer_connection = Arc::new(api.new_peer_connection(config).await?);
    // Create an audio track
    let track = WebrtcTrack::create_audio_track(codec, None);
    // Add the track to the peer connection
    let _rtp_sender = peer_connection
        .add_track(Arc::clone(&track) as Arc<TrackLocalStaticSample>)
        .await?;
    // Create a channel to collect ICE candidates
    let (ice_candidates_tx, mut ice_candidates_rx) = tokio::sync::mpsc::channel(1);
    let ice_candidates_tx = Arc::new(tokio::sync::Mutex::new(ice_candidates_tx));

    // Set up ICE candidate handler
    let ice_candidates_tx_clone = Arc::clone(&ice_candidates_tx);
    peer_connection.on_ice_candidate(Box::new(
        move |candidate: Option<webrtc::ice_transport::ice_candidate::RTCIceCandidate>| {
            let ice_candidates_tx_clone = ice_candidates_tx_clone.clone();
            Box::pin(async move {
                if candidate.is_none() {
                    ice_candidates_tx_clone.lock().await.send(()).await.ok();
                }
            })
        },
    ));
    let state_ref = state.clone();
    peer_connection.on_track(Box::new(
        move |track: Arc<TrackRemote>,
              _receiver: Arc<RTCRtpReceiver>,
              _transceiver: Arc<RTCRtpTransceiver>| {
            info!("on_track received: {}", track.codec().capability.mime_type,);
            let state_ref = state_ref.clone();
            Box::pin(async move {
                loop {
                    match track.read_rtp().await {
                        Ok(packet) => {
                            state_ref
                                .rx_bytes
                                .fetch_add(packet.0.payload.len() as u64, Ordering::Relaxed);
                        }
                        Err(e) => {
                            error!("Failed to read RTP packet: {}", e);
                            break;
                        }
                    }
                }
            })
        },
    ));

    // Create offer
    let offer = peer_connection.create_offer(None).await?;
    peer_connection.set_local_description(offer.clone()).await?;

    select! {
        _ = ice_candidates_rx.recv() => {
            info!("ICE gathering ok");
        }
        _ = time::sleep(std::time::Duration::from_secs(3)) => {
            error!("ICE gathering timeout");
        }
    }

    let offer = peer_connection
        .local_description()
        .await
        .ok_or(anyhow::anyhow!("Failed to get local description"))?;

    // Connect to WebSocket
    let session_id = Uuid::new_v4().to_string();
    let url = format!("{}?id=prefcli_{}_{}", cli.endpoint, session_id, id);
    info!(id, "Connecting to WebSocket: {}", url);
    let ws_stream = match tokio_tungstenite::connect_async(url).await {
        Ok((ws_stream, resp)) => {
            info!(id, "Connected to WebSocket: {}", resp.status());
            ws_stream
        }
        Err(e) => {
            match e {
                tungstenite::Error::Http(resp) => {
                    let body = resp.body();
                    let body_str = String::from_utf8_lossy(&body.as_ref().unwrap());
                    error!(id, "Failed to connect to WebSocket: {}", body_str);
                }
                _ => {
                    error!(id, "Failed to connect to WebSocket: {:?}", e);
                }
            }
            return Err(anyhow::anyhow!("Failed to connect to WebSocket"));
        }
    };

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Create the invite command with proper options
    let option = CallOption {
        offer: Some(strip_ipv6_candidates(&offer.sdp)),
        vad: if cli.vad.unwrap_or(false) {
            Some(VADOption::default())
        } else {
            None
        },
        denoise: if cli.denoise.unwrap_or(false) {
            Some(true)
        } else {
            None
        },
        ..Default::default()
    };

    let command = Command::Invite { option };
    // Send the invite command
    let command_str = serde_json::to_string_pretty(&command)?;
    ws_sender
        .send(tungstenite::Message::Text(command_str.into()))
        .await?;

    // Wait for transcription event
    let recv_event_loop = async move {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            let event: SessionEvent =
                serde_json::from_str(&msg.to_string()).expect("Failed to parse event");
            match event {
                SessionEvent::Answer { sdp, .. } => {
                    info!(id, "Received answer: {}", sdp);
                    let offer = RTCSessionDescription::answer(sdp)?;
                    match peer_connection.set_remote_description(offer).await {
                        Ok(_) => {
                            info!(id, "Set remote description ok");
                        }
                        Err(e) => {
                            error!(id, "Set remote description failed: {}", e);
                        }
                    }
                    ws_sender
                        .send(tungstenite::Message::Text(
                            serde_json::to_string_pretty(&Command::Play {
                                url: cli.play_file.clone(),
                                auto_hangup: None,
                                wait_input_timeout: None,
                            })?
                            .into(),
                        ))
                        .await?;
                }
                SessionEvent::Error { error, .. } => {
                    error!(id, "Received error: {}", error);
                    break;
                }
                SessionEvent::TrackEnd {
                    track_id, duration, ..
                } => {
                    if track_id == "server-side-track" {
                        info!(id, "Play file done, duration: {:?}", duration);
                        ws_sender
                            .send(tungstenite::Message::Text(
                                serde_json::to_string_pretty(&Command::Play {
                                    url: cli.play_file.clone(),
                                    auto_hangup: None,
                                    wait_input_timeout: None,
                                })?
                                .into(),
                            ))
                            .await?;
                    }
                }
                SessionEvent::Speaking { .. } | SessionEvent::Silence { .. } => {}
                _ => {
                    info!(id, "Received unexpected event: {:?}", event);
                    continue;
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    let state_ref = state.clone();
    let send_audio_loop = async move {
        // Read test audio file
        let (mut audio_samples, sample_rate) =
            read_wav_file(&cli.input_file).expect("Failed to read input file");
        let mut ticker = time::interval(Duration::from_millis(20));
        let mut encoder = G722Encoder::new();
        let mut packet_timestamp = 0;
        let chunk_size = if encoder.sample_rate() == 16000 {
            320
        } else {
            160
        };
        if sample_rate != encoder.sample_rate() {
            audio_samples = resample_mono(&audio_samples, sample_rate, encoder.sample_rate());
        }
        loop {
            for chunk in audio_samples.chunks(chunk_size) {
                let encoded = encoder.encode(&chunk);
                let sample = webrtc::media::Sample {
                    data: encoded.into(),
                    duration: Duration::from_millis(20),
                    timestamp: SystemTime::now(),
                    packet_timestamp,
                    ..Default::default()
                };
                track.write_sample(&sample).await.unwrap();
                state_ref
                    .tx_bytes
                    .fetch_add(sample.data.len() as u64, Ordering::Relaxed);
                packet_timestamp += chunk_size as u32;
                ticker.tick().await;
            }
        }
    };

    state.alive.fetch_add(1, Ordering::Relaxed);

    select! {
        r = recv_event_loop => {
            info!(id, "recv_event_loop completed {:?}", r);
        }
        _ = send_audio_loop => {
        }
    }
    state.alive.fetch_sub(1, Ordering::Relaxed);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    sqlx::any::install_default_drivers();

    let cli = Cli::parse();

    // Set up logging
    tracing_subscriber::fmt()
        .with_max_level(if cli.verbose {
            LevelFilter::DEBUG
        } else {
            LevelFilter::ERROR
        })
        .with_file(true)
        .with_line_number(true)
        .try_init()
        .ok();
    dotenv().ok();

    info!("Clients: {}", cli.clients);

    let mut handles = Vec::new();
    let state = Arc::new(AppState {
        alive: AtomicU32::new(0),
        rx_bytes: AtomicU64::new(0),
        tx_bytes: AtomicU64::new(0),
    });
    let codec = match cli.codec.as_str() {
        "g722" => CodecType::G722,
        "opus" => CodecType::Opus,
        "pcmu" => CodecType::PCMU,
        _ => return Err(anyhow::anyhow!("Invalid codec type")),
    };
    for id in 0..cli.clients {
        let cli = cli.clone();
        let state = state.clone();
        handles.push(tokio::spawn(async move {
            loop {
                match serve_client(codec, cli.clone(), id, state.clone()).await {
                    Ok(_) => {
                        info!(id, "Client session completed normally");
                    }
                    Err(e) => {
                        error!(id, "Client session failed: {}", e);
                    }
                }
                time::sleep(Duration::from_millis(rand::random::<u64>() % 5000)).await;
            }
        }));
    }

    println!(
        "perfcli started, clients: {}, codec: {:?}, vad: {}, denoise: {}",
        cli.clients,
        codec,
        cli.vad.unwrap_or(false),
        cli.denoise.unwrap_or(false)
    );
    let dump_state_loop = async move {
        let mut last_rx_bytes = 0u64;
        let mut last_tx_bytes = 0u64;

        loop {
            let current_rx = state.rx_bytes.load(Ordering::Relaxed);
            let current_tx = state.tx_bytes.load(Ordering::Relaxed);

            let rx_rate = current_rx.saturating_sub(last_rx_bytes);
            let tx_rate = current_tx.saturating_sub(last_tx_bytes);

            println!(
                "alive: {}, rx_bytes: {:.2} KB/s, tx_bytes: {:.2} KB/s",
                state.alive.load(Ordering::Relaxed),
                rx_rate as f64 / 1024.0,
                tx_rate as f64 / 1024.0
            );
            time::sleep(Duration::from_secs(1)).await;
            last_rx_bytes = current_rx;
            last_tx_bytes = current_tx;
        }
    };
    select! {
        _ = async {
            for handle in handles {
                handle.await.expect("Failed to join client");
            }
        } => {}
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down...");
        }
        _ = dump_state_loop => {
            info!("Dump state loop completed");
        }
    }

    Ok(())
}
