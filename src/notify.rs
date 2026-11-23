use crate::config::Settings;
use crate::{log_info, log_warn};
use serde_json::json;
use std::time::Duration;
use ureq::tls::TlsConfig;
use ureq::Agent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookFormat {
    Discord,
    Slack,
    Ntfy,
    Generic,
}

impl WebhookFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            WebhookFormat::Discord => "discord",
            WebhookFormat::Slack => "slack",
            WebhookFormat::Ntfy => "ntfy",
            WebhookFormat::Generic => "generic",
        }
    }
}

pub fn detect_webhook_format(url: &str, configured: &str) -> WebhookFormat {
    match configured.trim().to_ascii_lowercase().as_str() {
        "discord" => return WebhookFormat::Discord,
        "slack" => return WebhookFormat::Slack,
        "ntfy" => return WebhookFormat::Ntfy,
        "generic" => return WebhookFormat::Generic,
        _ => {}
    }

    let lower = url.to_ascii_lowercase();
    if lower.contains("discord.com/api/webhooks/") || lower.contains("discordapp.com/api/webhooks/")
    {
        WebhookFormat::Discord
    } else if lower.contains("hooks.slack.com/") {
        WebhookFormat::Slack
    } else if lower.contains("://ntfy.sh/") || lower.contains("://ntfy.") || lower.contains("/ntfy/")
    {
        WebhookFormat::Ntfy
    } else {
        WebhookFormat::Generic
    }
}

pub fn send_webhook(settings: &Settings, title: &str, message: &str) -> Result<WebhookFormat, String> {
    let url = settings
        .notify_webhook_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| "NOTIFY_WEBHOOK_URL is not configured".to_string())?;

    let format = detect_webhook_format(url, &settings.notify_webhook_format);
    let tls = TlsConfig::builder()
        .disable_verification(!settings.verify_ssl)
        .build();
    let config = Agent::config_builder()
        .tls_config(tls)
        .max_idle_connections(0)
        .timeout_global(Some(Duration::from_secs(15)))
        .build();
    let agent: Agent = config.into();

    let mut req = agent.post(url);
    if let Some(token) = settings
        .notify_webhook_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        req = req.header("Authorization", format!("Bearer {token}"));
    }

    let resp = match format {
        WebhookFormat::Discord => {
            let payload = json!({
                "content": format!("**{title}**\n{message}"),
            });
            req.send_json(&payload)
        }
        WebhookFormat::Slack => {
            let payload = json!({
                "text": format!("*{title}*\n{message}"),
            });
            req.send_json(&payload)
        }
        WebhookFormat::Ntfy => req
            .header("Title", title)
            .header("Priority", "high")
            .header("Tags", "warning")
            .header("Content-Type", "text/plain; charset=utf-8")
            .send(message.as_bytes()),
        WebhookFormat::Generic => {
            let payload = json!({
                "title": title,
                "message": message,
                "text": format!("{title}: {message}"),
                "content": format!("**{title}**\n{message}"),
            });
            req.send_json(&payload)
        }
    };

    match resp {
        Ok(_) => Ok(format),
        Err(err) => Err(format!("Webhook delivery failed ({}): {err}", format.as_str())),
    }
}

pub fn notify_auth_failure(settings: &Settings, reason: &str, requires_otp: bool) {
    if settings
        .notify_webhook_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .is_none()
    {
        return;
    }

    let title = "synology-photos-thumbgen: DSM login failed";
    let recovery = if requires_otp {
        "2FA/OTP verification is required. Run:\ndocker compose run --rm thumbgen auth && docker compose restart"
    } else {
        "Check DSM_USERNAME / DSM_PASSWORD in .env, then run:\ndocker compose restart"
    };
    let message = format!(
        "Paused thumbnail watcher to prevent DSM Auto Block.\nReason: {reason}\n\nRecovery:\n{recovery}"
    );

    match send_webhook(settings, title, &message) {
        Ok(fmt) => log_info!("Sent auth-failure notification via {} webhook", fmt.as_str()),
        Err(err) => log_warn!("Failed sending auth-failure notification: {}", err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn read_http_request(
        stream: &mut std::net::TcpStream,
    ) -> (String, HashMap<String, String>, Vec<u8>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();

        let mut headers = HashMap::new();
        let mut content_len = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                let key = k.trim().to_ascii_lowercase();
                let val = v.trim().to_string();
                if key == "content-length" {
                    content_len = val.parse().unwrap_or(0);
                }
                headers.insert(key, val);
            }
        }
        let mut body = vec![0u8; content_len];
        if content_len > 0 {
            reader.read_exact(&mut body).unwrap();
        }
        (request_line, headers, body)
    }

    #[test]
    fn test_detect_webhook_format_auto_and_explicit() {
        assert_eq!(
            detect_webhook_format("https://discord.com/api/webhooks/123/abc", "auto"),
            WebhookFormat::Discord
        );
        assert_eq!(
            detect_webhook_format("https://discordapp.com/api/webhooks/123/abc", "auto"),
            WebhookFormat::Discord
        );
        assert_eq!(
            detect_webhook_format("https://hooks.slack.com/services/T/B/X", "auto"),
            WebhookFormat::Slack
        );
        assert_eq!(
            detect_webhook_format("https://ntfy.sh/my-topic", "auto"),
            WebhookFormat::Ntfy
        );
        assert_eq!(
            detect_webhook_format("https://ntfy.example.com/alerts", "auto"),
            WebhookFormat::Ntfy
        );
        assert_eq!(
            detect_webhook_format("https://example.com/hook", "auto"),
            WebhookFormat::Generic
        );
        assert_eq!(
            detect_webhook_format("https://example.com/hook", "discord"),
            WebhookFormat::Discord
        );
        assert_eq!(
            detect_webhook_format("https://discord.com/api/webhooks/123/abc", "generic"),
            WebhookFormat::Generic
        );
    }

    #[test]
    fn test_send_webhook_discord_slack_ntfy_and_generic() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let mut captured = Vec::new();
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let (req_line, headers, body) = read_http_request(&mut stream);
                captured.push((req_line, headers, String::from_utf8_lossy(&body).into_owned()));
                let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
                stream.write_all(resp.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
            captured
        });

        let cases = [
            ("discord", Some("tok-1")),
            ("slack", None),
            ("ntfy", Some("tok-ntfy")),
            ("generic", None),
        ];

        for (fmt, token) in cases {
            let settings = Settings {
                notify_webhook_url: Some(format!("http://{addr}/hook/{fmt}")),
                notify_webhook_format: fmt.to_string(),
                notify_webhook_token: token.map(ToString::to_string),
                ..Settings::default()
            };
            let sent_fmt = send_webhook(&settings, "Alert Title", "Body text").unwrap();
            assert_eq!(sent_fmt.as_str(), fmt);
        }

        let captured = server.join().unwrap();
        assert_eq!(captured.len(), 4);

        // Discord
        assert!(captured[0].0.starts_with("POST /hook/discord"));
        assert_eq!(
            captured[0].1.get("authorization").map(String::as_str),
            Some("Bearer tok-1")
        );
        let d_json: serde_json::Value = serde_json::from_str(&captured[0].2).unwrap();
        assert_eq!(d_json["content"], "**Alert Title**\nBody text");

        // Slack
        assert!(captured[1].0.starts_with("POST /hook/slack"));
        let s_json: serde_json::Value = serde_json::from_str(&captured[1].2).unwrap();
        assert_eq!(s_json["text"], "*Alert Title*\nBody text");

        // ntfy
        assert!(captured[2].0.starts_with("POST /hook/ntfy"));
        assert_eq!(
            captured[2].1.get("authorization").map(String::as_str),
            Some("Bearer tok-ntfy")
        );
        assert_eq!(
            captured[2].1.get("title").map(String::as_str),
            Some("Alert Title")
        );
        assert_eq!(
            captured[2].1.get("priority").map(String::as_str),
            Some("high")
        );
        assert_eq!(captured[2].2, "Body text");

        // Generic
        assert!(captured[3].0.starts_with("POST /hook/generic"));
        let g_json: serde_json::Value = serde_json::from_str(&captured[3].2).unwrap();
        assert_eq!(g_json["title"], "Alert Title");
        assert_eq!(g_json["message"], "Body text");
        assert_eq!(g_json["text"], "Alert Title: Body text");
    }
}
