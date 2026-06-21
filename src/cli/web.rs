use crate::api::schema::{EmptyParams, Method, Request, WebConnectParams, WebDisconnectParams};

pub(super) fn run_web_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_web_help();
        return Ok(2);
    };

    match subcommand {
        "connect" => web_connect(&args[1..]),
        "sessions" => web_sessions(&args[1..]),
        "disconnect" => web_disconnect(&args[1..]),
        "help" | "--help" | "-h" => {
            print_web_help();
            Ok(0)
        }
        _ => {
            print_web_help();
            Ok(2)
        }
    }
}

fn web_connect(args: &[String]) -> std::io::Result<i32> {
    let mut public_url = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => json = true,
            "--url" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!("--url requires a value");
                    return Ok(2);
                };
                public_url = Some(value.clone());
            }
            other => {
                if let Some(value) = other.strip_prefix("--url=") {
                    public_url = Some(value.to_string());
                } else {
                    eprintln!("usage: herdr web connect [--url <origin>] [--json]");
                    return Ok(2);
                }
            }
        }
        index += 1;
    }

    let response = super::send_request(&Request {
        id: "cli:web:connect".into(),
        method: Method::WebConnect(WebConnectParams { public_url }),
    })?;

    if json || response.get("error").is_some() {
        return super::print_response(&response);
    }

    let result = &response["result"];
    let Some(url) = result["url"].as_str() else {
        return super::print_response(&response);
    };

    println!("{url}");
    if let Some(expires_in_secs) = result["expires_in_secs"].as_u64() {
        println!();
        println!("This link works once and expires in {expires_in_secs}s.");
    }
    if result["url"]
        .as_str()
        .is_some_and(|url| url.starts_with("http://"))
    {
        println!(
            "Serving over plain http: the browser will refuse to write to the clipboard, and a"
        );
        println!(
            "remote origin needs https. Put it behind `tailscale serve` and set [web] public_url."
        );
    }

    Ok(0)
}

fn web_sessions(args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => {
            eprintln!("usage: herdr web sessions [--json]");
            return Ok(2);
        }
    };

    let response = super::send_request(&Request {
        id: "cli:web:sessions".into(),
        method: Method::WebSessions(EmptyParams::default()),
    })?;

    if json || response.get("error").is_some() {
        return super::print_response(&response);
    }

    let Some(sessions) = response["result"]["sessions"].as_array() else {
        return super::print_response(&response);
    };

    if sessions.is_empty() {
        println!("no browser sessions");
        return Ok(0);
    }

    println!("{:<14} {:<12} LAST SEEN", "SESSION", "CONNECTIONS");
    for session in sessions {
        println!(
            "{:<14} {:<12} {}",
            session["id"].as_str().unwrap_or("?"),
            session["connections"].as_u64().unwrap_or(0),
            format_age(session["last_seen_unix"].as_u64()),
        );
    }

    Ok(0)
}

fn web_disconnect(args: &[String]) -> std::io::Result<i32> {
    let Some(session) = args.first() else {
        eprintln!("usage: herdr web disconnect <session>");
        return Ok(2);
    };
    if args.len() > 1 {
        eprintln!("usage: herdr web disconnect <session>");
        return Ok(2);
    }

    super::send_ok_request(Method::WebDisconnect(WebDisconnectParams {
        session: session.clone(),
    }))
}

fn format_age(last_seen_unix: Option<u64>) -> String {
    let Some(last_seen_unix) = last_seen_unix else {
        return "unknown".to_string();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let seconds = now.saturating_sub(last_seen_unix);
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

fn print_web_help() {
    println!("Open this herdr session in a browser.");
    println!();
    println!("Usage: herdr web <subcommand>");
    println!();
    println!("Subcommands:");
    println!(
        "  connect [--url <origin>] [--json]   Print a single-use link that logs a browser in"
    );
    println!("  sessions [--json]                   List browser sessions");
    println!("  disconnect <session>                Revoke a session and close its connections");
    println!();
    println!("Requires `enabled = true` under [web] in your herdr config.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_are_reported_in_the_largest_useful_unit() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        assert_eq!(format_age(Some(now)), "0s ago");
        assert_eq!(format_age(Some(now - 90)), "1m ago");
        assert_eq!(format_age(Some(now - 7200)), "2h ago");
        assert_eq!(format_age(Some(now - 172_800)), "2d ago");
    }

    #[test]
    fn a_missing_timestamp_is_reported_rather_than_guessed() {
        assert_eq!(format_age(None), "unknown");
    }

    #[test]
    fn future_timestamps_do_not_underflow() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        assert_eq!(format_age(Some(now + 600)), "0s ago");
    }
}
