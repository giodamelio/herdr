//! Integration tests for the browser client: the login redirect, the session
//! cookie, terminal frame delivery over the WebSocket, and revocation.

pub mod support;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::Value;
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid,
};
use tungstenite::client::IntoClientRequest;

const WS_READ_TIMEOUT: Duration = Duration::from_secs(10);

fn unique_test_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/herdr-web-test-{name}-{}-{nanos}",
        std::process::id()
    ))
}

struct SpawnedHerdr {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Drop for SpawnedHerdr {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        unregister_spawned_herdr_pid(pid);
    }
}

fn write_web_config(app_config_dir: &std::path::Path, web_config: &str) {
    fs::write(
        app_config_dir.join("config.toml"),
        format!("onboarding = false\n\n[web]\n{web_config}"),
    )
    .unwrap();
}

/// A running server with the browser client enabled on an ephemeral port.
struct WebFixture {
    base: PathBuf,
    config_home: PathBuf,
    runtime_dir: PathBuf,
    api_socket_path: PathBuf,
    server: Option<SpawnedHerdr>,
}

impl WebFixture {
    fn start(name: &str, web_config: &str) -> Self {
        let base = unique_test_dir(name);
        let config_home = base.join("config");
        let runtime_dir = base.join("run");
        let api_socket_path = runtime_dir.join("herdr.sock");

        // Debug builds read from `herdr-dev` rather than `herdr`, and these
        // tests depend on the config actually taking effect.
        let app_config_dir = config_home.join(if cfg!(debug_assertions) {
            "herdr-dev"
        } else {
            "herdr"
        });
        fs::create_dir_all(&app_config_dir).unwrap();
        fs::create_dir_all(&runtime_dir).unwrap();
        register_runtime_dir(&runtime_dir);
        write_web_config(&app_config_dir, web_config);

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();

        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
        cmd.arg("server");
        cmd.env("XDG_CONFIG_HOME", &config_home);
        cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
        cmd.env("HERDR_SOCKET_PATH", &api_socket_path);
        cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
        cmd.env_remove("HERDR_ENV");
        cmd.env("SHELL", "/bin/sh");

        let child = pair.slave.spawn_command(cmd).unwrap();
        register_spawned_herdr_pid(child.process_id());
        drop(pair.slave);

        let fixture = Self {
            base,
            config_home,
            runtime_dir,
            api_socket_path,
            server: Some(SpawnedHerdr {
                _master: pair.master,
                child,
            }),
        };
        fixture.wait_for_api_socket();
        fixture
    }

    fn wait_for_api_socket(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.api_socket_path.exists()
                && std::os::unix::net::UnixStream::connect(&self.api_socket_path).is_ok()
            {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("server api socket never appeared");
    }

    fn cli(&self, args: &[&str]) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command
            .args(args)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("HERDR_SOCKET_PATH", &self.api_socket_path)
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_ENV");
        command.output().unwrap()
    }

    /// Runs a CLI command and parses its JSON envelope. Successes are printed
    /// to stdout and errors to stderr, so both are candidates.
    fn cli_json(&self, args: &[&str]) -> Value {
        let output = self.cli(args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        serde_json::from_str(stdout.trim())
            .or_else(|_| serde_json::from_str(stderr.trim()))
            .unwrap_or_else(|err| {
                panic!("expected json from {args:?}: {err}\nstdout: {stdout}\nstderr: {stderr}")
            })
    }

    /// Mints a connect link and returns its bound address and invite token.
    fn connect_link(&self) -> (String, String) {
        let response = self.cli_json(&["web", "connect", "--json"]);
        let result = &response["result"];
        assert!(result["url"].is_string(), "web connect failed: {response}");
        let url = result["url"].as_str().unwrap().to_string();
        let bind = result["bind"].as_str().unwrap().to_string();
        let invite = url.rsplit('/').next().unwrap().to_string();
        (bind, invite)
    }

    fn app_config_dir(&self) -> PathBuf {
        self.config_home.join(if cfg!(debug_assertions) {
            "herdr-dev"
        } else {
            "herdr"
        })
    }

    /// Rewrites the `[web]` config and makes the running server pick it up.
    fn rewrite_web_config(&self, web_config: &str) {
        write_web_config(&self.app_config_dir(), web_config);
        let output = self.cli(&["server", "reload-config"]);
        assert!(
            output.status.success(),
            "reload-config failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn finish(mut self) {
        drop(self.server.take());
        cleanup_test_base(&self.base);
    }
}

/// A raw HTTP/1.1 response, parsed just far enough for these assertions.
struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn http_get(bind: &str, path: &str, cookie: Option<&str>) -> HttpResponse {
    let mut stream = TcpStream::connect(bind).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {bind}\r\nConnection: close\r\n");
    if let Some(cookie) = cookie {
        request.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();

    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();

    HttpResponse {
        status,
        headers,
        body: body.to_string(),
    }
}

/// Follows a connect link and returns the session cookie it sets.
fn log_in(bind: &str, invite: &str) -> String {
    let response = http_get(bind, &format!("/connect/{invite}"), None);
    assert_eq!(response.status, 302, "connect should redirect");
    assert_eq!(response.header("location"), Some("/"));

    let set_cookie = response
        .header("set-cookie")
        .expect("connect should set a session cookie");
    assert!(set_cookie.contains("HttpOnly"), "cookie: {set_cookie}");
    assert!(
        set_cookie.contains("SameSite=Strict"),
        "cookie: {set_cookie}"
    );
    set_cookie.split(';').next().unwrap().to_string()
}

type Socket = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

fn open_socket(bind: &str, cookie: &str) -> Result<Socket, tungstenite::Error> {
    let mut request = format!("ws://{bind}/ws?cols=80&rows=24")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Cookie", cookie.parse().unwrap());

    let (socket, _) = tungstenite::connect(request)?;
    if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_ref() {
        stream.set_read_timeout(Some(WS_READ_TIMEOUT)).unwrap();
    }
    Ok(socket)
}

/// Reads until a binary frame arrives, which is how terminal output travels.
fn read_terminal_bytes(socket: &mut Socket) -> Vec<u8> {
    let deadline = Instant::now() + WS_READ_TIMEOUT;
    while Instant::now() < deadline {
        match socket.read() {
            Ok(tungstenite::Message::Binary(bytes)) => return bytes.to_vec(),
            Ok(_) => continue,
            Err(err) => panic!("websocket read failed before any terminal output: {err}"),
        }
    }
    panic!("no terminal output arrived within {WS_READ_TIMEOUT:?}");
}

fn sessions(fixture: &WebFixture) -> Vec<Value> {
    fixture.cli_json(&["web", "sessions", "--json"])["result"]["sessions"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn wait_for_connections(fixture: &WebFixture, expected: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let total: u64 = sessions(fixture)
            .iter()
            .filter_map(|session| session["connections"].as_u64())
            .sum();
        if total == expected {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn web_commands_are_refused_until_the_feature_is_enabled() {
    let fixture = WebFixture::start("disabled", "enabled = false\n");

    let response = fixture.cli_json(&["web", "connect", "--json"]);

    assert_eq!(response["error"]["code"], "web_disabled");
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("[web]"),
        "the error should say how to enable it: {response}"
    );
    fixture.finish();
}

#[test]
fn a_connect_link_logs_a_browser_in_and_streams_the_terminal() {
    let fixture = WebFixture::start("connect", "enabled = true\nbind = \"127.0.0.1:0\"\n");
    let (bind, invite) = fixture.connect_link();

    let cookie = log_in(&bind, &invite);
    let page = http_get(&bind, "/", Some(&cookie));
    assert_eq!(page.status, 200);
    assert!(
        page.body.contains("/assets/xterm.js"),
        "body: {}",
        page.body
    );

    let mut socket = open_socket(&bind, &cookie).expect("the cookie should authorize the upgrade");
    let bytes = read_terminal_bytes(&mut socket);

    // The first frame is a full repaint, which always begins by entering
    // synchronized output and hiding the cursor.
    assert!(
        bytes.starts_with(b"\x1b[?2026h"),
        "expected a full repaint, got {:?}",
        String::from_utf8_lossy(&bytes[..bytes.len().min(40)])
    );

    let _ = socket.close(None);
    fixture.finish();
}

#[test]
fn an_invite_works_once_and_only_once() {
    let fixture = WebFixture::start("single-use", "enabled = true\nbind = \"127.0.0.1:0\"\n");
    let (bind, invite) = fixture.connect_link();

    let first = http_get(&bind, &format!("/connect/{invite}"), None);
    let second = http_get(&bind, &format!("/connect/{invite}"), None);

    assert_eq!(first.status, 302);
    assert_eq!(second.status, 401);
    assert!(
        second.body.contains("herdr web connect"),
        "the reader should be told how to recover: {}",
        second.body
    );
    fixture.finish();
}

#[test]
fn unknown_invites_are_rejected() {
    let fixture = WebFixture::start("bad-invite", "enabled = true\nbind = \"127.0.0.1:0\"\n");
    let (bind, _) = fixture.connect_link();

    let response = http_get(&bind, "/connect/not-a-real-invite", None);

    assert_eq!(response.status, 401);
    fixture.finish();
}

#[test]
fn the_page_and_the_socket_both_require_a_session() {
    let fixture = WebFixture::start("no-cookie", "enabled = true\nbind = \"127.0.0.1:0\"\n");
    let (bind, _) = fixture.connect_link();

    let page = http_get(&bind, "/", None);
    let asset = http_get(&bind, "/assets/app.js", None);
    let socket = open_socket(&bind, "herdr_web=made-up");

    assert_eq!(page.status, 401);
    assert_eq!(asset.status, 401);
    match socket {
        Err(tungstenite::Error::Http(response)) => assert_eq!(response.status(), 401),
        Err(other) => panic!("expected an http 401, got {other}"),
        Ok(_) => panic!("an unknown cookie should not open a socket"),
    }
    fixture.finish();
}

#[test]
fn a_session_survives_reconnecting_with_the_same_cookie() {
    let fixture = WebFixture::start("reconnect", "enabled = true\nbind = \"127.0.0.1:0\"\n");
    let (bind, invite) = fixture.connect_link();
    let cookie = log_in(&bind, &invite);

    let mut first = open_socket(&bind, &cookie).unwrap();
    read_terminal_bytes(&mut first);
    let _ = first.close(None);
    drop(first);

    let mut second = open_socket(&bind, &cookie).expect("the cookie should still be valid");
    let bytes = read_terminal_bytes(&mut second);

    assert!(!bytes.is_empty());
    let _ = second.close(None);
    fixture.finish();
}

#[test]
fn sessions_are_listed_with_their_live_connection_count() {
    let fixture = WebFixture::start("sessions", "enabled = true\nbind = \"127.0.0.1:0\"\n");
    let (bind, invite) = fixture.connect_link();
    let cookie = log_in(&bind, &invite);

    assert_eq!(sessions(&fixture).len(), 1);
    assert_eq!(sessions(&fixture)[0]["connections"], 0);

    let mut socket = open_socket(&bind, &cookie).unwrap();
    read_terminal_bytes(&mut socket);

    assert!(
        wait_for_connections(&fixture, 1),
        "the live socket should be counted: {:?}",
        sessions(&fixture)
    );

    let _ = socket.close(None);
    drop(socket);

    assert!(
        wait_for_connections(&fixture, 0),
        "closing the socket should drop the count: {:?}",
        sessions(&fixture)
    );
    fixture.finish();
}

#[test]
fn last_seen_advances_while_the_browser_is_being_used() {
    let fixture = WebFixture::start("last-seen", "enabled = true\nbind = \"127.0.0.1:0\"\n");
    let (bind, invite) = fixture.connect_link();
    let cookie = log_in(&bind, &invite);
    let mut socket = open_socket(&bind, &cookie).unwrap();
    read_terminal_bytes(&mut socket);
    let after_handshake = sessions(&fixture)[0]["last_seen_unix"].as_u64().unwrap();

    // Only HTTP requests pass through the token store, and an open socket makes
    // none, so input has to be what keeps the session looking alive.
    thread::sleep(Duration::from_millis(1100));
    socket
        .send(tungstenite::Message::Text(
            r#"{"type":"input","events":[{"kind":"key","key":"a"}]}"#.into(),
        ))
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut latest = after_handshake;
    while Instant::now() < deadline && latest <= after_handshake {
        latest = sessions(&fixture)[0]["last_seen_unix"].as_u64().unwrap();
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        latest > after_handshake,
        "last seen stayed at {after_handshake} while the browser was sending input"
    );
    let _ = socket.close(None);
    fixture.finish();
}

#[test]
fn disconnecting_revokes_the_cookie_and_closes_live_sockets() {
    let fixture = WebFixture::start("disconnect", "enabled = true\nbind = \"127.0.0.1:0\"\n");
    let (bind, invite) = fixture.connect_link();
    let cookie = log_in(&bind, &invite);
    let mut socket = open_socket(&bind, &cookie).unwrap();
    read_terminal_bytes(&mut socket);
    assert!(wait_for_connections(&fixture, 1));

    let id = sessions(&fixture)[0]["id"].as_str().unwrap().to_string();
    let output = fixture.cli(&["web", "disconnect", &id]);
    assert!(
        output.status.success(),
        "disconnect failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        read_until_closed(&mut socket),
        "the socket should be closed"
    );
    assert!(sessions(&fixture).is_empty());
    assert_eq!(http_get(&bind, "/", Some(&cookie)).status, 401);
    fixture.finish();
}

#[test]
fn reporting_commands_do_not_bind_a_listener() {
    let fixture = WebFixture::start(
        "no-side-effect",
        "enabled = true\nbind = \"127.0.0.1:7791\"\n",
    );

    // `sessions` before any `connect` must answer without opening a socket —
    // a read command should never start serving.
    let sessions = fixture.cli_json(&["web", "sessions", "--json"]);
    assert_eq!(sessions["result"]["sessions"].as_array().unwrap().len(), 0);
    assert!(
        TcpStream::connect("127.0.0.1:7791").is_err(),
        "web sessions bound the listener"
    );

    let disconnect = fixture.cli(&["web", "disconnect", "deadbeefcafe"]);
    assert!(!disconnect.status.success());
    assert!(
        TcpStream::connect("127.0.0.1:7791").is_err(),
        "web disconnect bound the listener"
    );

    // `connect` is the one command that may start it.
    fixture.connect_link();
    assert!(TcpStream::connect("127.0.0.1:7791").is_ok());
    fixture.finish();
}

#[test]
fn disconnecting_an_unknown_session_reports_not_found() {
    let fixture = WebFixture::start("unknown", "enabled = true\nbind = \"127.0.0.1:0\"\n");
    fixture.connect_link();

    let response = fixture.cli(&["web", "disconnect", "deadbeefcafe"]);

    assert!(!response.status.success());
    assert!(
        String::from_utf8_lossy(&response.stderr).contains("not_found"),
        "stderr: {}",
        String::from_utf8_lossy(&response.stderr)
    );
    fixture.finish();
}

#[test]
fn every_connect_call_mints_a_new_invite_on_the_same_listener() {
    let fixture = WebFixture::start("reissue", "enabled = true\nbind = \"127.0.0.1:0\"\n");

    let (first_bind, first_invite) = fixture.connect_link();
    let (second_bind, second_invite) = fixture.connect_link();

    assert_eq!(first_bind, second_bind, "the listener should stay bound");
    assert_ne!(first_invite, second_invite);
    assert_eq!(
        http_get(&first_bind, &format!("/connect/{first_invite}"), None).status,
        302
    );
    assert_eq!(
        http_get(&first_bind, &format!("/connect/{second_invite}"), None).status,
        302
    );
    fixture.finish();
}

#[test]
fn disabling_the_feature_and_reloading_stops_the_listener() {
    let fixture = WebFixture::start("disable-reload", "enabled = true\nbind = \"127.0.0.1:0\"\n");
    let (bind, invite) = fixture.connect_link();
    let cookie = log_in(&bind, &invite);
    assert_eq!(http_get(&bind, "/", Some(&cookie)).status, 200);

    fixture.rewrite_web_config("enabled = false\nbind = \"127.0.0.1:0\"\n");

    // The socket must stop answering entirely; refusing new links while still
    // serving browsers that already hold a cookie would not be "disabled".
    assert!(
        wait_until_refused(&bind),
        "the listener kept accepting connections after being disabled"
    );
    assert_eq!(
        fixture.cli_json(&["web", "connect", "--json"])["error"]["code"],
        "web_disabled"
    );
    fixture.finish();
}

/// Waits for a listener to stop accepting connections.
fn wait_until_refused(bind: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect(bind).is_err() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn enabling_the_feature_and_reloading_starts_serving() {
    let fixture = WebFixture::start("enable-reload", "enabled = false\n");
    assert_eq!(
        fixture.cli_json(&["web", "connect", "--json"])["error"]["code"],
        "web_disabled"
    );

    fixture.rewrite_web_config("enabled = true\nbind = \"127.0.0.1:0\"\n");

    let (bind, invite) = fixture.connect_link();
    let cookie = log_in(&bind, &invite);
    assert_eq!(http_get(&bind, "/", Some(&cookie)).status, 200);
    fixture.finish();
}

#[test]
fn a_configured_public_url_is_used_for_the_link() {
    let fixture = WebFixture::start(
        "public-url",
        "enabled = true\nbind = \"127.0.0.1:0\"\npublic_url = \"https://box.example.ts.net\"\n",
    );

    let response = fixture.cli_json(&["web", "connect", "--json"]);
    let url = response["result"]["url"].as_str().unwrap();

    assert!(
        url.starts_with("https://box.example.ts.net/connect/"),
        "url: {url}"
    );
    fixture.finish();
}

#[test]
fn the_url_flag_overrides_the_configured_origin() {
    let fixture = WebFixture::start("url-flag", "enabled = true\nbind = \"127.0.0.1:0\"\n");

    let response = fixture.cli_json(&[
        "web",
        "connect",
        "--url",
        "https://other.example.ts.net/",
        "--json",
    ]);
    let url = response["result"]["url"].as_str().unwrap();

    assert!(
        url.starts_with("https://other.example.ts.net/connect/"),
        "url: {url}"
    );
    fixture.finish();
}

#[test]
fn autostart_binds_the_listener_before_any_connect() {
    let fixture = WebFixture::start(
        "autostart",
        "enabled = true\nbind = \"127.0.0.1:0\"\nautostart = true\n",
    );

    // A bound listener answers before any invite exists; an unbound one would
    // refuse the connection outright.
    let (bind, _) = fixture.connect_link();
    assert_eq!(http_get(&bind, "/", None).status, 401);
    fixture.finish();
}

fn read_until_closed(socket: &mut Socket) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match socket.read() {
            Ok(tungstenite::Message::Close(_)) => return true,
            Ok(_) => continue,
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return true
            }
            Err(tungstenite::Error::Io(err))
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                return true
            }
            // A read timeout means the socket is still open, so keep waiting
            // rather than reporting a close that never happened.
            Err(tungstenite::Error::Io(err))
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(err) => panic!("unexpected websocket error while waiting for close: {err}"),
        }
    }
    false
}
