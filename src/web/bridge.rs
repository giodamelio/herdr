//! Translates one browser WebSocket into one herdr client connection.
//!
//! The bridge is a client like any other: it connects to the client protocol
//! socket, performs the `Hello` handshake, and speaks bincode on that side
//! while speaking WebSocket frames on the other. Its lifetime is the socket's
//! lifetime, so closing the browser tab reaps the `ClientConnection` too.

use std::io;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use interprocess::local_socket::traits::Stream as _;
use interprocess::TryClone as _;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::ipc::{self, LocalStream};
use crate::protocol::{
    self, ClientMessage, RenderEncoding, ServerMessage, MAX_FRAME_SIZE, PROTOCOL_VERSION,
};
use crate::web::auth::SessionGuard;
use crate::web::wire::{
    translate_server_message, BridgeOutput, BrowserMessage, ControlMessage, InputEncoding,
};

/// Outbound queue depth toward the browser. Render frames are already diffed,
/// so a browser this far behind is not going to catch up.
const OUTBOUND_QUEUE: usize = 256;

/// Queue depth toward the herdr server. Input is small and bursty.
const INBOUND_QUEUE: usize = 256;

/// How long to wait for the server's `Welcome` before giving up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Terminal size the connection starts at, replaced by the browser's first
/// resize once the fit addon has measured the viewport.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

type BrowserSink = SplitSink<WebSocket, Message>;

#[derive(Debug, Clone, Copy)]
pub(crate) struct BridgeOptions {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

impl Default for BridgeOptions {
    fn default() -> Self {
        Self {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
        }
    }
}

impl BridgeOptions {
    /// Clamps browser-reported dimensions to something a terminal can hold.
    pub(crate) fn from_requested(cols: Option<u16>, rows: Option<u16>) -> Self {
        let default = Self::default();
        Self {
            cols: cols.filter(|cols| *cols > 0).unwrap_or(default.cols),
            rows: rows.filter(|rows| *rows > 0).unwrap_or(default.rows),
        }
    }
}

/// Runs a browser session until the socket, the server, or a revocation ends it.
pub(crate) async fn run(socket: WebSocket, mut guard: SessionGuard, options: BridgeOptions) {
    let session = guard.id.clone();
    let stream = match connect_and_handshake(options).await {
        Ok(stream) => stream,
        Err(err) => {
            warn!(session = %session, err = %err, "browser bridge could not attach to the server");
            close_with(
                socket,
                &format!("could not attach to the herdr server: {err}"),
            )
            .await;
            return;
        }
    };

    let write_stream = match stream.try_clone() {
        Ok(write_stream) => write_stream,
        Err(err) => {
            warn!(session = %session, err = %err, "could not split the client socket");
            close_with(socket, "could not attach to the herdr server").await;
            return;
        }
    };

    let (mut sink, mut browser_rx) = socket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<BridgeOutput>(OUTBOUND_QUEUE);
    let (inbound_tx, inbound_rx) = mpsc::channel::<ClientMessage>(INBOUND_QUEUE);

    // The reader learns the negotiated keyboard protocol from the server; the
    // browser frames it has to encode arrive on this task, so the flags are
    // shared rather than passed along either path.
    let keyboard_flags = Arc::new(AtomicU16::new(0));
    let reader_flags = Arc::clone(&keyboard_flags);

    tokio::task::spawn_blocking(move || read_server_messages(stream, outbound_tx, reader_flags));
    tokio::task::spawn_blocking(move || write_client_messages(write_stream, inbound_rx));

    loop {
        tokio::select! {
            // Revocation wins over in-flight traffic so `web disconnect` is immediate.
            _ = guard.revoked.recv() => {
                let _ = sink.send(closed_frame("this browser session was disconnected")).await;
                break;
            }
            output = outbound_rx.recv() => {
                let Some(output) = output else { break };
                if send_output(&mut sink, output).await.is_err() {
                    break;
                }
            }
            frame = browser_rx.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        // Anything the browser sends is the user doing something,
                        // which is what `herdr web sessions` reports as last seen.
                        guard.touch();
                        let encoding =
                            InputEncoding::from_kitty_flags(keyboard_flags.load(Ordering::Relaxed));
                        if !forward_browser_frame(&text, encoding, &inbound_tx, &mut sink).await {
                            break;
                        }
                    }
                    // Binary, ping, pong, and close frames carry nothing the bridge needs.
                    Some(Ok(_)) => {}
                    Some(Err(err)) => {
                        debug!(session = %session, err = %err, "browser socket failed");
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    // Blocking tasks cannot be aborted once running, so they are wound down by
    // hand: dropping the sender ends the writer, which sends `Detach`; the
    // server then closes the connection, which ends the reader's blocking read.
    drop(inbound_tx);
    let _ = sink.close().await;
    debug!(session = %session, "browser bridge closed");
}

/// Forwards one browser frame, returning false when the bridge should stop.
///
/// Frames herdr cannot parse or use are logged and skipped rather than closing
/// the session, so one stray event from a browser quirk is not fatal.
async fn forward_browser_frame(
    text: &str,
    encoding: InputEncoding,
    inbound_tx: &mpsc::Sender<ClientMessage>,
    sink: &mut BrowserSink,
) -> bool {
    let messages = match serde_json::from_str::<BrowserMessage>(text)
        .map_err(|err| err.to_string())
        .and_then(|message| message.into_client_messages(encoding))
    {
        Ok(messages) => messages,
        Err(err) => {
            debug!(err = %err, "ignoring unusable browser frame");
            return true;
        }
    };

    for message in messages {
        if inbound_tx.send(message).await.is_err() {
            let _ = sink
                .send(closed_frame("the herdr server closed the connection"))
                .await;
            return false;
        }
    }
    true
}

async fn send_output(sink: &mut BrowserSink, output: BridgeOutput) -> Result<(), axum::Error> {
    match output {
        BridgeOutput::Terminal(bytes) => sink.send(Message::Binary(bytes.into())).await,
        BridgeOutput::Control(control) => sink.send(control_frame(&control)).await,
        BridgeOutput::Ignored => Ok(()),
    }
}

fn control_frame(control: &ControlMessage) -> Message {
    // Serializing a fixed enum cannot fail, and an empty frame the browser
    // skips beats unwrapping in the middle of a live session.
    Message::Text(serde_json::to_string(control).unwrap_or_default().into())
}

fn closed_frame(reason: &str) -> Message {
    control_frame(&ControlMessage::Closed {
        reason: reason.to_string(),
    })
}

async fn close_with(socket: WebSocket, reason: &str) {
    let (mut sink, _) = socket.split();
    let _ = sink.send(closed_frame(reason)).await;
    let _ = sink.close().await;
}

async fn connect_and_handshake(options: BridgeOptions) -> io::Result<LocalStream> {
    tokio::task::spawn_blocking(move || blocking_handshake(options))
        .await
        .map_err(|err| io::Error::other(format!("handshake task failed: {err}")))?
}

fn blocking_handshake(options: BridgeOptions) -> io::Result<LocalStream> {
    let socket_path = crate::server::socket_paths::client_socket_path();
    let mut stream = ipc::connect_local_stream(&socket_path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to connect to the herdr client socket {}: {err}",
                socket_path.display()
            ),
        )
    })?;

    // Cell pixel dimensions of zero tell the server this client cannot render
    // Kitty graphics, which keeps image bytes out of the ANSI stream, and
    // leaves no geometry for pixel mouse coordinates.
    let hello = ClientMessage::TerminalHello {
        version: PROTOCOL_VERSION,
        cols: options.cols,
        rows: options.rows,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
    };
    protocol::write_message(&mut stream, &hello)
        .map_err(|err| io::Error::other(format!("handshake write failed: {err}")))?;

    set_recv_timeout(&stream, Some(HANDSHAKE_TIMEOUT))?;
    let welcome: ServerMessage = protocol::read_message(&mut stream, MAX_FRAME_SIZE)
        .map_err(|err| io::Error::other(format!("handshake read failed: {err}")))?;
    set_recv_timeout(&stream, None)?;

    match welcome {
        ServerMessage::Welcome {
            error: Some(error), ..
        } => Err(io::Error::other(error)),
        ServerMessage::Welcome {
            encoding: RenderEncoding::TerminalAnsi,
            ..
        } => Ok(stream),
        ServerMessage::Welcome { encoding, .. } => Err(io::Error::other(format!(
            "server negotiated {encoding:?} but the browser client needs terminal ANSI"
        ))),
        other => Err(io::Error::other(format!(
            "expected a welcome from the server, got {other:?}"
        ))),
    }
}

/// Bounds the handshake read where the platform supports it.
///
/// Platforms without socket receive timeouts fall back to blocking until the
/// server replies or the socket closes, which is the same guarantee the native
/// client gets there.
fn set_recv_timeout(stream: &LocalStream, timeout: Option<Duration>) -> io::Result<()> {
    match stream.set_recv_timeout(timeout) {
        Err(err) if err.kind() == io::ErrorKind::Unsupported => {
            debug!(err = %err, "client socket receive timeout unavailable");
            Ok(())
        }
        result => result,
    }
}

fn read_server_messages(
    mut stream: LocalStream,
    outbound: mpsc::Sender<BridgeOutput>,
    keyboard_flags: Arc<AtomicU16>,
) {
    loop {
        let message: ServerMessage = match protocol::read_message(&mut stream, MAX_FRAME_SIZE) {
            Ok(message) => message,
            Err(err) => {
                debug!(err = %err, "client socket read ended");
                return;
            }
        };

        if let ServerMessage::DirectTerminalKeyboardProtocol { flags, .. } = &message {
            keyboard_flags.store(*flags, Ordering::Relaxed);
        }

        let output = translate_server_message(message);
        if matches!(output, BridgeOutput::Ignored) {
            continue;
        }
        if outbound.blocking_send(output).is_err() {
            return;
        }
    }
}

fn write_client_messages(mut stream: LocalStream, mut inbound: mpsc::Receiver<ClientMessage>) {
    while let Some(message) = inbound.blocking_recv() {
        if let Err(err) = protocol::write_message(&mut stream, &message) {
            debug!(err = %err, "client socket write ended");
            return;
        }
    }
    let _ = protocol::write_message(&mut stream, &ClientMessage::Detach);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_dimensions_fall_back_when_missing_or_zero() {
        let default = BridgeOptions::default();

        assert_eq!(
            (
                BridgeOptions::from_requested(None, None).cols,
                BridgeOptions::from_requested(Some(0), Some(0)).rows,
            ),
            (default.cols, default.rows)
        );
    }

    #[test]
    fn requested_dimensions_are_used_when_present() {
        let options = BridgeOptions::from_requested(Some(140), Some(52));

        assert_eq!((options.cols, options.rows), (140, 52));
    }
}
