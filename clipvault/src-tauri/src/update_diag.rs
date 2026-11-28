//! Explains a failed update check. tauri-plugin-updater reports any non-2xx
//! answer as "Could not fetch a valid release JSON from the remote", without
//! the status or who sent it, so a check that fails on a user's machine (while
//! the same URL opens fine in their browser) can't be told apart: antivirus or
//! firewall, a proxy wanting a login, GitHub rate limits. This repeats the
//! plugin's request to every configured endpoint, with the same HTTP stack and
//! user agent, and reports what each one answered; the result goes to the log
//! and the frontend's toast.
use std::time::Duration;
use tauri::{AppHandle, Runtime};

/// The plugin's own user agent (`tauri-plugin-updater/<version>`): some
/// filters treat requests by user agent, so the probe must look the same.
const USER_AGENT: &str = "tauri-plugin-updater/2.14.1";
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, serde::Serialize)]
pub struct EndpointProbe {
    /// Host of the configured endpoint (what the user would recognise).
    pub host: String,
    pub ok: bool,
    /// One line: "HTTP 403 from <host>: <page title>", the connection error, or "OK (0.2.2)".
    pub result: String,
}

/// Endpoints from `plugins.updater.endpoints` in tauri.conf.json.
fn endpoints<R: Runtime>(app: &AppHandle<R>) -> Vec<String> {
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|u| u.get("endpoints"))
        .and_then(|e| e.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

pub async fn diagnose<R: Runtime>(app: &AppHandle<R>) -> Vec<EndpointProbe> {
    probe_all(endpoints(app)).await
}

pub async fn probe_all(urls: Vec<String>) -> Vec<EndpointProbe> {
    // The plugin installs ring as rustls' provider before its first request;
    // do the same in case this runs first.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    let client = match reqwest::Client::builder().user_agent(USER_AGENT).timeout(TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return vec![EndpointProbe { host: "-".into(), ok: false, result: format!("no HTTP client: {}", chain(&e)) }];
        }
    };
    let mut out = Vec::new();
    for url in urls {
        let host = reqwest::Url::parse(&url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_else(|| url.clone());
        let probe = match client.get(&url).header(reqwest::header::ACCEPT, "application/json").send().await {
            Ok(res) => {
                let status = res.status();
                let final_host = res.url().host_str().unwrap_or("?").to_owned();
                let body = res.text().await.unwrap_or_default();
                if status.is_success() {
                    match serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| v.get("version").and_then(|v| v.as_str()).map(str::to_owned))
                    {
                        Some(v) => EndpointProbe { host, ok: true, result: format!("OK ({v})") },
                        None => EndpointProbe {
                            host,
                            ok: false,
                            result: format!("HTTP {status} from {final_host} but not a manifest: {}", gist(&body)),
                        },
                    }
                } else {
                    let what = gist(&body);
                    let sep = if what.is_empty() { "" } else { ": " };
                    EndpointProbe { host, ok: false, result: format!("HTTP {status} from {final_host}{sep}{what}") }
                }
            }
            Err(e) => EndpointProbe { host, ok: false, result: chain(&e) },
        };
        if probe.ok {
            tracing::info!("update endpoint {url}: {}", probe.result);
        } else {
            tracing::warn!("update endpoint {url}: {}", probe.result);
        }
        out.push(probe);
    }
    out
}

/// The error and its causes ("error sending request: … : invalid peer certificate: UnknownIssuer"),
/// skipping a cause that only repeats the previous line.
fn chain(e: &dyn std::error::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut cur = e.source();
    while let Some(c) = cur {
        let s = c.to_string();
        if !parts.last().is_some_and(|p| p.contains(&s)) {
            parts.push(s);
        }
        cur = c.source();
    }
    parts.join(": ")
}

/// A short hint at what answered: the HTML `<title>` of a block page, else the
/// start of the body, whitespace collapsed.
fn gist(body: &str) -> String {
    let lower = body.to_ascii_lowercase();
    let text = match (lower.find("<title>"), lower.find("</title>")) {
        (Some(a), Some(b)) if b > a + 7 => &body[a + 7..b],
        _ => body,
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(120) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::{gist, probe_all};

    /// Hits the network: `cargo test update_diag -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn probes_real_endpoints() {
        let urls = [
            "https://github.com/Quasozial/ClipVault/releases/latest/download/latest.json",
            "https://github.com/Quasozial/ClipVault/releases/download/v0.0.0/missing.json",
            "https://no-such-host.invalid/latest.json",
        ];
        let probes = probe_all(urls.iter().map(|s| s.to_string()).collect()).await;
        for p in &probes {
            eprintln!("{}: {}", p.host, p.result);
        }
        assert!(probes[0].ok, "{}", probes[0].result);
        assert!(probes[1].result.starts_with("HTTP 404"), "{}", probes[1].result);
        assert!(!probes[2].ok);
    }

    #[test]
    fn gist_prefers_title() {
        assert_eq!(gist("<html><head><TITLE> Blocked by\n Firewall </title></head></html>"), "Blocked by Firewall");
    }

    #[test]
    fn gist_trims_long_bodies() {
        let g = gist(&"x ".repeat(200));
        assert!(g.ends_with('…') && g.chars().count() == 121);
        assert_eq!(gist(""), "");
    }
}
