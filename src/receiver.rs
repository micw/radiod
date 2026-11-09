//! Trusted-LAN WebSocket transport. Playback policy and all output queues live in Core.
use crate::core::Core;
use anyhow::{Context, Result, bail};
use std::{
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tungstenite::{
    Message, accept_hdr_with_config,
    handshake::server::{ErrorResponse, Request, Response},
    http::{HeaderValue, StatusCode},
    protocol::WebSocketConfig,
};

const PROTOCOL: &str = "radiod.receiver.v1";
const MAX_CONNECTIONS: usize = 8;
struct Permit(Arc<AtomicUsize>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub fn start(core: Arc<Mutex<Core>>) -> Result<()> {
    let config = core
        .lock()
        .map_err(|_| anyhow::anyhow!("core lock poisoned"))?
        .config
        .bluetooth
        .clone();
    if !config.enabled {
        return Ok(());
    }
    let listener = TcpListener::bind(config.listen).context("bind Bluetooth control listener")?;
    eprintln!("Bluetooth control listening on {}", listener.local_addr()?);
    let count = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("bluetooth-listener".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    continue;
                };
                let Ok(peer) = stream.peer_addr() else {
                    continue;
                };
                if !config
                    .allowed_networks
                    .iter()
                    .any(|n| n.contains(&peer.ip()))
                {
                    continue;
                }
                if count
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                        (n < MAX_CONNECTIONS).then_some(n + 1)
                    })
                    .is_err()
                {
                    continue;
                }
                let permit = Permit(count.clone());
                let core = core.clone();
                if let Err(error) = std::thread::Builder::new()
                    .name("bluetooth-control".into())
                    .spawn(move || {
                        let _permit = permit;
                        if let Err(_error) = serve_client(stream, core) {
                            // Neither arbitrary wire data nor tokenized URLs enter logs.
                            eprintln!("Bluetooth control connection ended or rejected");
                        }
                    })
                {
                    eprintln!("Cannot start Bluetooth control worker: {error}");
                }
            }
        })?;
    Ok(())
}

// tungstenite's callback trait requires this concrete error response type.
#[allow(clippy::result_large_err)]
fn handshake(
    request: &Request,
    mut response: Response,
) -> std::result::Result<Response, ErrorResponse> {
    let protocol = request
        .headers()
        .get("Sec-WebSocket-Protocol")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|s| s.split(',').any(|p| p.trim() == PROTOCOL));
    if request.uri().path() != "/v1/receivers" || request.uri().query().is_some() || !protocol {
        return Err(tungstenite::http::Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Some("receiver v1 endpoint/subprotocol required".into()))
            .unwrap());
    }
    response
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", HeaderValue::from_static(PROTOCOL));
    Ok(response)
}

fn serve_client(stream: TcpStream, core: Arc<Mutex<Core>>) -> Result<()> {
    let peer = stream.peer_addr()?.ip();
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let limits = WebSocketConfig::default()
        .read_buffer_size(4096)
        .write_buffer_size(4096)
        .max_write_buffer_size(65536)
        .max_message_size(Some(16384))
        .max_frame_size(Some(16384));
    let mut socket = accept_hdr_with_config(stream, handshake, Some(limits))
        .map_err(|_| anyhow::anyhow!("invalid control handshake"))?;
    socket
        .get_mut()
        .set_read_timeout(Some(Duration::from_millis(100)))?;
    let connection = core
        .lock()
        .map_err(|_| anyhow::anyhow!("core lock poisoned"))?
        .bluetooth_open(peer)?;
    let result = (|| -> Result<()> {
        let mut window = std::time::Instant::now();
        let mut messages = 0u32;
        loop {
            match socket.read() {
                Ok(Message::Text(text)) => {
                    if window.elapsed() >= Duration::from_secs(1) {
                        window = std::time::Instant::now();
                        messages = 0;
                    }
                    messages += 1;
                    if messages > 100 {
                        bail!("control rate limit");
                    }
                    let value =
                        serde_json::from_str(text.as_str()).context("invalid control JSON")?;
                    let applied = core
                        .lock()
                        .map_err(|_| anyhow::anyhow!("core lock poisoned"))?
                        .bluetooth_message(connection, value);
                    if applied.is_err() {
                        socket.send(Message::Text(serde_json::json!({"v":1,"type":"error","reason":"invalid or stale receiver message"}).to_string().into()))?;
                        break;
                    }
                }
                Ok(Message::Close(_)) => break,
                Ok(Message::Binary(_)) => bail!("audio frames must use negotiated RTP"),
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    break;
                }
                Err(_) => bail!("control read failed"),
            }
            let (replies, alive) = {
                let mut state = core
                    .lock()
                    .map_err(|_| anyhow::anyhow!("core lock poisoned"))?;
                let replies = state.bluetooth_drain(connection);
                (replies, state.bluetooth_alive(connection))
            };
            for reply in replies {
                socket.send(Message::Text(serde_json::to_string(&reply)?.into()))?;
            }
            socket.flush()?;
            if !alive {
                break;
            }
        }
        Ok(())
    })();
    if let Ok(mut state) = core.lock() {
        state.bluetooth_closed(connection);
    }
    let _ = socket.close(None);
    result
}
