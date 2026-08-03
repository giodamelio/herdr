//! Frontend assets embedded in the binary.
//!
//! Vendored rather than loaded from a CDN so the browser client works offline
//! and on airgapped hosts. See `assets/vendor/xterm.vendor.json` for upstream
//! versions and licensing.

pub(crate) const INDEX_HTML: &str = include_str!("assets/index.html");

const APP_JS: &str = include_str!("assets/app.js");
const APP_CSS: &str = include_str!("assets/app.css");
const XTERM_JS: &str = include_str!("assets/vendor/xterm.js");
const XTERM_CSS: &str = include_str!("assets/vendor/xterm.css");
const XTERM_ADDON_FIT_JS: &str = include_str!("assets/vendor/xterm-addon-fit.js");

const JAVASCRIPT: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";

/// Resolves a request path segment to an embedded asset and its content type.
pub(crate) fn lookup(file: &str) -> Option<(&'static [u8], &'static str)> {
    let asset = match file {
        "app.js" => (APP_JS, JAVASCRIPT),
        "app.css" => (APP_CSS, CSS),
        "xterm.js" => (XTERM_JS, JAVASCRIPT),
        "xterm.css" => (XTERM_CSS, CSS),
        "xterm-addon-fit.js" => (XTERM_ADDON_FIT_JS, JAVASCRIPT),
        _ => return None,
    };
    Some((asset.0.as_bytes(), asset.1))
}

/// Renders a standalone message page, used when there is no valid session and
/// therefore no client to load.
pub(crate) fn error_page(message: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <meta name="color-scheme" content="dark" />
    <title>herdr</title>
    <style>
      body {{
        margin: 0;
        min-height: 100vh;
        display: grid;
        place-items: center;
        background: #101014;
        color: #e6e6e6;
        font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
      }}
      main {{ max-width: 32rem; padding: 2rem; text-align: center; }}
      code {{ padding: 0.2rem 0.45rem; border-radius: 0.3rem; background: #26262e; }}
    </style>
  </head>
  <body>
    <main>
      <p>{}</p>
    </main>
  </body>
</html>
"#,
        escape_html(message)
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_the_page_requests_is_embedded() {
        for file in [
            "app.js",
            "app.css",
            "xterm.js",
            "xterm.css",
            "xterm-addon-fit.js",
        ] {
            let (bytes, _) = lookup(file).unwrap_or_else(|| panic!("{file} is not embedded"));
            assert!(!bytes.is_empty(), "{file} is empty");
            assert!(
                INDEX_HTML.contains(&format!("/assets/{file}")),
                "{file} is embedded but the page never asks for it"
            );
        }
    }

    #[test]
    fn the_client_puts_the_terminal_into_mouse_tracking_when_herdr_takes_the_mouse() {
        // A terminal only stops running its own selection once the application
        // asks for mouse events. Herdr signals that out of band, so the client
        // has to tell xterm itself — without this it selects across the whole
        // grid on top of the selection Herdr already paints.
        assert!(
            APP_JS.contains(r"\x1b[?1000h\x1b[?1002h\x1b[?1006h"),
            "the client no longer enables mouse tracking"
        );
        assert!(
            APP_JS.contains(r"\x1b[?1000l\x1b[?1002l\x1b[?1006l"),
            "the client no longer disables mouse tracking"
        );
    }

    #[test]
    fn unknown_assets_are_not_served() {
        assert!(lookup("../../etc/passwd").is_none());
        assert!(lookup("app.js.map").is_none());
    }

    #[test]
    fn error_pages_escape_their_message() {
        let page = error_page("<script>alert(1)</script>");

        assert!(page.contains("&lt;script&gt;"));
        assert!(!page.contains("<script>alert"));
    }
}
