//! JSON control vocabulary spoken between the browser and the bridge.
//!
//! The browser never sees the bincode client protocol. Terminal output travels
//! as binary WebSocket frames holding raw escape bytes; everything else is a
//! JSON text frame translated here into `ClientMessage` / from `ServerMessage`.
//!
//! Input runs the same way round. A terminal-ANSI client sends the bytes its
//! host terminal would have produced, so browser events are encoded here with
//! the pane's negotiated keyboard protocol rather than forwarded as semantic
//! events.

use crossterm::event::{KeyEvent, KeyEventState, KeyModifiers, MouseEventKind};
use serde::{Deserialize, Serialize};

use crate::input::{
    encode_key, encode_mouse_button, encode_mouse_scroll, KeyboardProtocol, MouseProtocolEncoding,
};
use crate::protocol::{
    ClientClipboardImageTarget, ClientKeyCode, ClientKeyKind, ClientMessage, ClientMouseButton,
    ClientMouseKind, NotifyKind, ServerMessage, MAX_CLIPBOARD_IMAGE_PAYLOAD,
};

/// Kitty "report event types" bit. Set means the focused pane wants key
/// releases, which is also what the browser needs in order to send them.
const KITTY_FLAG_REPORT_EVENT_TYPES: u16 = 0b0000_0010;

/// The server enables SGR mouse reporting on an attached terminal, and the
/// browser handshake reports no cell pixel size, so pixel-resolution mouse is
/// never negotiated for this client.
const MOUSE_ENCODING: MouseProtocolEncoding = MouseProtocolEncoding::Sgr;

/// Input modes the server has negotiated for this connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputEncoding {
    pub(crate) keyboard: KeyboardProtocol,
}

impl Default for InputEncoding {
    fn default() -> Self {
        Self {
            keyboard: KeyboardProtocol::Legacy,
        }
    }
}

impl InputEncoding {
    pub(crate) fn from_kitty_flags(flags: u16) -> Self {
        Self {
            keyboard: KeyboardProtocol::from_kitty_flags(flags),
        }
    }
}

// ---------------------------------------------------------------------------
// Browser → bridge
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum BrowserMessage {
    Input {
        events: Vec<BrowserInputEvent>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    ClipboardImage {
        extension: String,
        /// Standard base64 of the raw image bytes.
        data: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum BrowserInputEvent {
    Key {
        /// A single character, or a named key such as `enter` or `f5`.
        key: String,
        #[serde(default)]
        modifiers: u8,
        #[serde(default)]
        press: BrowserKeyPress,
    },
    /// Committed text from an IME or any other composed input.
    Text {
        text: String,
    },
    Mouse {
        action: BrowserMouseAction,
        #[serde(default)]
        button: BrowserMouseButton,
        column: u16,
        row: u16,
        #[serde(default)]
        modifiers: u8,
    },
    Paste {
        text: String,
    },
    Focus {
        gained: bool,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BrowserKeyPress {
    #[default]
    Down,
    Repeat,
    Up,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BrowserMouseAction {
    Down,
    Up,
    Drag,
    Move,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BrowserMouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

impl BrowserMessage {
    /// Translates one browser frame into client protocol messages.
    ///
    /// Unmappable input events are dropped rather than failing the frame, so a
    /// browser key herdr has no code for cannot stall the session.
    pub(crate) fn into_client_messages(
        self,
        encoding: InputEncoding,
    ) -> Result<Vec<ClientMessage>, String> {
        match self {
            Self::Input { events } => {
                // One frame of browser events becomes one run of bytes, which
                // keeps a burst of keystrokes in order and in a single write.
                let mut data = Vec::new();
                for event in events {
                    event.encode(encoding, &mut data);
                }
                if data.is_empty() {
                    return Ok(Vec::new());
                }
                Ok(vec![ClientMessage::Input { data }])
            }
            Self::Resize { cols, rows } => Ok(vec![ClientMessage::Resize {
                cols,
                rows,
                cell_width_px: 0,
                cell_height_px: 0,
                // Without a cell pixel size there is no coherent geometry to
                // resolve pixel mouse coordinates against.
                pixel_mouse: false,
            }]),
            Self::ClipboardImage { extension, data } => {
                let extension = sanitized_image_extension(&extension)
                    .ok_or_else(|| format!("unsupported clipboard image extension: {extension}"))?;
                let data = decode_clipboard_image(&data)?;
                Ok(vec![ClientMessage::ClipboardImage {
                    // The browser renders the whole server-drawn surface rather
                    // than one pane, which is the same target the native
                    // terminal-ANSI client reports.
                    target: ClientClipboardImageTarget::DirectTerminal,
                    extension,
                    data,
                }])
            }
        }
    }
}

impl BrowserInputEvent {
    /// Appends the terminal bytes this event would have produced.
    ///
    /// Keys herdr has no code for append nothing rather than failing the whole
    /// frame.
    fn encode(self, encoding: InputEncoding, out: &mut Vec<u8>) {
        match self {
            Self::Key {
                key,
                modifiers,
                press,
            } => {
                let Some((code, modifiers)) = parse_key(&key, modifiers) else {
                    return;
                };
                let key = KeyEvent {
                    code: code.to_crossterm(),
                    modifiers: KeyModifiers::from_bits_truncate(modifiers),
                    kind: ClientKeyKind::from(press).to_crossterm(),
                    state: KeyEventState::NONE,
                };
                out.extend_from_slice(&encode_key(key, encoding.keyboard));
            }
            // Composed text is already the characters the user meant, so it
            // goes out verbatim the way a terminal delivers IME commits.
            Self::Text { text } => out.extend_from_slice(text.as_bytes()),
            Self::Mouse {
                action,
                button,
                column,
                row,
                modifiers,
            } => {
                let kind = action.into_client_kind(button).to_crossterm();
                let modifiers = KeyModifiers::from_bits_truncate(modifiers);
                let bytes = match kind {
                    MouseEventKind::ScrollUp
                    | MouseEventKind::ScrollDown
                    | MouseEventKind::ScrollLeft
                    | MouseEventKind::ScrollRight => {
                        encode_mouse_scroll(kind, column, row, modifiers, MOUSE_ENCODING)
                    }
                    _ => encode_mouse_button(kind, column, row, modifiers, MOUSE_ENCODING),
                };
                if let Some(bytes) = bytes {
                    out.extend_from_slice(&bytes);
                }
            }
            // A host terminal brackets pastes so the receiving application can
            // tell them from typing; the server relies on the same markers.
            Self::Paste { text } => {
                out.extend_from_slice(b"\x1b[200~");
                out.extend_from_slice(text.as_bytes());
                out.extend_from_slice(b"\x1b[201~");
            }
            Self::Focus { gained: true } => out.extend_from_slice(b"\x1b[I"),
            Self::Focus { gained: false } => out.extend_from_slice(b"\x1b[O"),
        }
    }
}

impl From<BrowserKeyPress> for ClientKeyKind {
    fn from(press: BrowserKeyPress) -> Self {
        match press {
            BrowserKeyPress::Down => Self::Press,
            BrowserKeyPress::Repeat => Self::Repeat,
            BrowserKeyPress::Up => Self::Release,
        }
    }
}

impl From<BrowserMouseButton> for ClientMouseButton {
    fn from(button: BrowserMouseButton) -> Self {
        match button {
            BrowserMouseButton::Left => Self::Left,
            BrowserMouseButton::Right => Self::Right,
            BrowserMouseButton::Middle => Self::Middle,
        }
    }
}

impl BrowserMouseAction {
    fn into_client_kind(self, button: BrowserMouseButton) -> ClientMouseKind {
        match self {
            Self::Down => ClientMouseKind::Down(button.into()),
            Self::Up => ClientMouseKind::Up(button.into()),
            Self::Drag => ClientMouseKind::Drag(button.into()),
            Self::Move => ClientMouseKind::Moved,
            Self::ScrollUp => ClientMouseKind::ScrollUp,
            Self::ScrollDown => ClientMouseKind::ScrollDown,
            Self::ScrollLeft => ClientMouseKind::ScrollLeft,
            Self::ScrollRight => ClientMouseKind::ScrollRight,
        }
    }
}

const SHIFT: u8 = 0b0000_0001;

/// Resolves a browser key name into a protocol key code.
///
/// Single-character names map straight to `Char`, which is also what the
/// browser reports while Ctrl or Alt are held. Shift+Tab becomes `BackTab` to
/// match what `src/input/parse.rs` produces for `ESC [ Z`.
fn parse_key(key: &str, modifiers: u8) -> Option<(ClientKeyCode, u8)> {
    let mut chars = key.chars();
    if let (Some(only), None) = (chars.next(), chars.next()) {
        return Some((ClientKeyCode::Char(only), modifiers));
    }

    if let Some(number) = key.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        if (1..=24).contains(&number) {
            return Some((ClientKeyCode::F(number), modifiers));
        }
    }

    let code = match key {
        "backspace" => ClientKeyCode::Backspace,
        "enter" => ClientKeyCode::Enter,
        "left" => ClientKeyCode::Left,
        "right" => ClientKeyCode::Right,
        "up" => ClientKeyCode::Up,
        "down" => ClientKeyCode::Down,
        "home" => ClientKeyCode::Home,
        "end" => ClientKeyCode::End,
        "pageup" => ClientKeyCode::PageUp,
        "pagedown" => ClientKeyCode::PageDown,
        "tab" if modifiers & SHIFT != 0 => ClientKeyCode::BackTab,
        "tab" => ClientKeyCode::Tab,
        "delete" => ClientKeyCode::Delete,
        "insert" => ClientKeyCode::Insert,
        "escape" => ClientKeyCode::Esc,
        "space" => ClientKeyCode::Char(' '),
        _ => return None,
    };
    Some((code, modifiers))
}

fn sanitized_image_extension(extension: &str) -> Option<String> {
    let extension = extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp").then_some(extension)
}

fn decode_clipboard_image(data: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;

    let data = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|err| format!("clipboard image is not valid base64: {err}"))?;
    if data.len() > MAX_CLIPBOARD_IMAGE_PAYLOAD {
        return Err(format!(
            "clipboard image is {} bytes, over the {MAX_CLIPBOARD_IMAGE_PAYLOAD} byte limit",
            data.len()
        ));
    }
    Ok(data)
}

// ---------------------------------------------------------------------------
// Bridge → browser
// ---------------------------------------------------------------------------

/// Control frames sent to the browser as JSON text. Terminal output does not
/// appear here; it goes out as binary frames instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ControlMessage {
    /// Base64 clipboard payload a pane requested via OSC 52.
    Clipboard {
        data: String,
    },
    Title {
        title: Option<String>,
    },
    Notify {
        message: String,
        body: Option<String>,
    },
    MouseCapture {
        enabled: bool,
    },
    /// The focused pane wants every key transition, so start sending key
    /// releases as well as presses.
    ReportKeyReleases {
        enabled: bool,
    },
    /// The session is over and reconnecting will not help.
    Closed {
        reason: String,
    },
}

/// Splits a server message into what the browser should receive.
pub(crate) enum BridgeOutput {
    /// Raw terminal escape bytes for a binary frame.
    Terminal(Vec<u8>),
    Control(ControlMessage),
    /// Nothing the browser can use.
    Ignored,
}

pub(crate) fn translate_server_message(message: ServerMessage) -> BridgeOutput {
    match message {
        ServerMessage::Terminal(frame) => BridgeOutput::Terminal(frame.bytes),
        ServerMessage::Clipboard { data } => {
            BridgeOutput::Control(ControlMessage::Clipboard { data })
        }
        ServerMessage::WindowTitle { title } => {
            BridgeOutput::Control(ControlMessage::Title { title })
        }
        // Sound is a host-terminal concern; the browser only shows the text.
        ServerMessage::Notify {
            kind: NotifyKind::Toast | NotifyKind::SystemToast,
            message,
            body,
        } => BridgeOutput::Control(ControlMessage::Notify { message, body }),
        // The browser handshake reports no cell pixel size, so the server never
        // grants this client pixel mouse and `sgr_pixels` is always false here.
        ServerMessage::MouseCapture {
            enabled,
            sgr_pixels: _,
        } => BridgeOutput::Control(ControlMessage::MouseCapture { enabled }),
        ServerMessage::DirectTerminalKeyboardProtocol { flags, .. } => {
            BridgeOutput::Control(ControlMessage::ReportKeyReleases {
                enabled: flags & KITTY_FLAG_REPORT_EVENT_TYPES != 0,
            })
        }
        ServerMessage::ServerShutdown { reason } => BridgeOutput::Control(ControlMessage::Closed {
            reason: reason.unwrap_or_else(|| "the herdr server stopped".to_string()),
        }),
        // The render path strips BEL, so the browser terminal only rings when
        // the bells arrive as their own bytes.
        ServerMessage::TerminalBell { count } => {
            BridgeOutput::Terminal(vec![0x07; usize::from(count)])
        }
        // Sound-only notifications, kitty graphics, sound reloads, stray
        // handshakes, and everything belonging to the client-owned shell
        // protocol have no browser equivalent on the terminal-ANSI path.
        ServerMessage::Notify { .. }
        | ServerMessage::Graphics { .. }
        | ServerMessage::GraphicsFile { .. }
        | ServerMessage::GraphicsTransmissionRetired { .. }
        | ServerMessage::ReloadSoundConfig
        | ServerMessage::Welcome { .. }
        | ServerMessage::ClientShellSnapshot(_)
        | ServerMessage::PaneSurface(_)
        | ServerMessage::PaneSurfacePatch(_)
        | ServerMessage::SemanticNotification(_)
        | ServerMessage::ClientShellError { .. }
        | ServerMessage::ClientShellKeyboardReportAll { .. }
        | ServerMessage::ClientShellEndpointResponseChunk { .. }
        | ServerMessage::EndpointControl { .. } => BridgeOutput::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Vec<ClientMessage> {
        parse_with(json, InputEncoding::default())
    }

    fn parse_with(json: &str, encoding: InputEncoding) -> Vec<ClientMessage> {
        serde_json::from_str::<BrowserMessage>(json)
            .expect("browser message should parse")
            .into_client_messages(encoding)
            .expect("browser message should translate")
    }

    fn input_bytes(json: &str) -> Vec<u8> {
        input_bytes_with(json, InputEncoding::default())
    }

    fn input_bytes_with(json: &str, encoding: InputEncoding) -> Vec<u8> {
        match parse_with(json, encoding).pop() {
            Some(ClientMessage::Input { data }) => data,
            other => panic!("expected terminal input bytes, got {other:?}"),
        }
    }

    #[test]
    fn plain_characters_become_their_own_bytes() {
        assert_eq!(
            input_bytes(r#"{"type":"input","events":[{"kind":"key","key":"a"}]}"#),
            b"a"
        );
    }

    #[test]
    fn control_combinations_become_control_bytes() {
        assert_eq!(
            input_bytes(r#"{"type":"input","events":[{"kind":"key","key":"c","modifiers":2}]}"#),
            b"\x03"
        );
    }

    #[test]
    fn named_keys_and_function_keys_are_recognised() {
        assert_eq!(
            input_bytes(r#"{"type":"input","events":[{"kind":"key","key":"enter"}]}"#),
            b"\r"
        );
        assert_eq!(
            input_bytes(r#"{"type":"input","events":[{"kind":"key","key":"pageup"}]}"#),
            b"\x1b[5~"
        );
        assert_eq!(
            input_bytes(r#"{"type":"input","events":[{"kind":"key","key":"escape"}]}"#),
            b"\x1b"
        );
        assert_eq!(
            input_bytes(r#"{"type":"input","events":[{"kind":"key","key":"space"}]}"#),
            b" "
        );
    }

    #[test]
    fn shift_tab_becomes_back_tab() {
        assert_eq!(
            input_bytes(r#"{"type":"input","events":[{"kind":"key","key":"tab","modifiers":1}]}"#),
            b"\x1b[Z"
        );
    }

    #[test]
    fn one_frame_of_keys_becomes_one_ordered_run_of_bytes() {
        assert_eq!(
            input_bytes(
                r#"{"type":"input","events":[
                    {"kind":"key","key":"h"},
                    {"kind":"key","key":"i"},
                    {"kind":"key","key":"enter"}
                ]}"#
            ),
            b"hi\r"
        );
    }

    #[test]
    fn unknown_key_names_are_dropped_without_failing_the_frame() {
        assert_eq!(
            input_bytes(
                r#"{"type":"input","events":[
                    {"kind":"key","key":"contextmenu"},
                    {"kind":"key","key":"enter"}
                ]}"#
            ),
            b"\r"
        );
    }

    #[test]
    fn a_frame_of_only_unmappable_keys_sends_nothing() {
        let messages = parse(r#"{"type":"input","events":[{"kind":"key","key":"contextmenu"}]}"#);

        assert!(messages.is_empty());
    }

    #[test]
    fn key_releases_are_reported_only_when_the_pane_asked_for_them() {
        const RELEASE: &str =
            r#"{"type":"input","events":[{"kind":"key","key":"a","press":"up"}]}"#;

        // Legacy panes have no encoding for a release, so nothing is sent.
        assert!(parse(RELEASE).is_empty());

        let reporting = InputEncoding::from_kitty_flags(KITTY_FLAG_REPORT_EVENT_TYPES);
        assert!(!input_bytes_with(RELEASE, reporting).is_empty());
    }

    #[test]
    fn mouse_events_become_sgr_reports_with_one_based_cells() {
        assert_eq!(
            input_bytes(
                r#"{"type":"input","events":[
                    {"kind":"mouse","action":"down","button":"right","column":4,"row":9}
                ]}"#
            ),
            b"\x1b[<2;5;10M"
        );
        assert_eq!(
            input_bytes(
                r#"{"type":"input","events":[
                    {"kind":"mouse","action":"scroll_up","column":0,"row":0}
                ]}"#
            ),
            b"\x1b[<64;1;1M"
        );
    }

    #[test]
    fn paste_is_bracketed_the_way_a_host_terminal_delivers_it() {
        assert_eq!(
            input_bytes(r#"{"type":"input","events":[{"kind":"paste","text":"hello"}]}"#),
            b"\x1b[200~hello\x1b[201~"
        );
    }

    #[test]
    fn focus_transitions_become_focus_reports() {
        assert_eq!(
            input_bytes(
                r#"{"type":"input","events":[
                    {"kind":"focus","gained":true},
                    {"kind":"focus","gained":false}
                ]}"#
            ),
            b"\x1b[I\x1b[O"
        );
    }

    #[test]
    fn composed_text_commits_verbatim() {
        assert_eq!(
            input_bytes(r#"{"type":"input","events":[{"kind":"text","text":"日本"}]}"#),
            "日本".as_bytes()
        );
    }

    #[test]
    fn resize_reports_no_cell_pixels_so_kitty_graphics_stay_off() {
        let messages = parse(r#"{"type":"resize","cols":120,"rows":40}"#);

        assert_eq!(
            messages,
            vec![ClientMessage::Resize {
                cols: 120,
                rows: 40,
                cell_width_px: 0,
                cell_height_px: 0,
                pixel_mouse: false,
            }]
        );
    }

    #[test]
    fn clipboard_images_are_decoded_and_their_extension_normalised() {
        let messages = parse(r#"{"type":"clipboard_image","extension":".PNG","data":"aGVsbG8="}"#);

        assert_eq!(
            messages,
            vec![ClientMessage::ClipboardImage {
                target: ClientClipboardImageTarget::DirectTerminal,
                extension: "png".to_string(),
                data: b"hello".to_vec(),
            }]
        );
    }

    #[test]
    fn clipboard_images_reject_unknown_extensions_and_bad_base64() {
        let bad_extension = serde_json::from_str::<BrowserMessage>(
            r#"{"type":"clipboard_image","extension":"exe","data":"aGVsbG8="}"#,
        )
        .unwrap()
        .into_client_messages(InputEncoding::default());
        let bad_base64 = serde_json::from_str::<BrowserMessage>(
            r#"{"type":"clipboard_image","extension":"png","data":"not base64!"}"#,
        )
        .unwrap()
        .into_client_messages(InputEncoding::default());

        assert!(bad_extension.is_err());
        assert!(bad_base64.is_err());
    }

    #[test]
    fn terminal_frames_become_binary_output() {
        let output =
            translate_server_message(ServerMessage::Terminal(crate::protocol::TerminalFrame {
                seq: 1,
                width: 80,
                height: 24,
                full: true,
                bytes: b"\x1b[2J".to_vec(),
            }));

        match output {
            BridgeOutput::Terminal(bytes) => assert_eq!(bytes, b"\x1b[2J"),
            _ => panic!("expected terminal bytes"),
        }
    }

    #[test]
    fn keyboard_protocol_flags_drive_the_key_release_control_message() {
        let reporting = translate_server_message(ServerMessage::DirectTerminalKeyboardProtocol {
            flags: KITTY_FLAG_REPORT_EVENT_TYPES,
            modify_other_keys_level: 0,
        });
        let quiet = translate_server_message(ServerMessage::DirectTerminalKeyboardProtocol {
            flags: 0,
            modify_other_keys_level: 0,
        });

        assert!(matches!(
            reporting,
            BridgeOutput::Control(ControlMessage::ReportKeyReleases { enabled: true })
        ));
        assert!(matches!(
            quiet,
            BridgeOutput::Control(ControlMessage::ReportKeyReleases { enabled: false })
        ));
    }

    #[test]
    fn host_only_server_messages_are_ignored() {
        for message in [
            ServerMessage::Graphics { bytes: vec![1] },
            ServerMessage::ReloadSoundConfig,
            ServerMessage::ClientShellKeyboardReportAll { enabled: true },
            ServerMessage::Notify {
                kind: NotifyKind::Sound,
                message: "done".into(),
                body: None,
            },
        ] {
            assert!(
                matches!(translate_server_message(message), BridgeOutput::Ignored),
                "message should be ignored"
            );
        }
    }

    #[test]
    fn shutdown_becomes_a_terminal_close_reason() {
        let output = translate_server_message(ServerMessage::ServerShutdown { reason: None });

        match output {
            BridgeOutput::Control(ControlMessage::Closed { reason }) => {
                assert!(!reason.is_empty())
            }
            _ => panic!("expected a close control message"),
        }
    }
}
