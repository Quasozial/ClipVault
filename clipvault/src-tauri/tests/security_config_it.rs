//! The webview's security settings in `tauri.conf.json` and the capability
//! files. A mistake here (CSP turned off, a plugin granted to JS) does not
//! break anything visibly, so it is pinned by a test.

use serde_json::Value;
use std::path::Path;

fn json(rel: &str) -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
}

/// `directive` of a CSP string, e.g. `["'self'", "asset:"]` for `img-src`.
fn sources<'a>(csp: &'a str, directive: &str) -> Vec<&'a str> {
    csp.split(';')
        .map(str::trim)
        .find_map(|d| d.strip_prefix(directive).filter(|rest| rest.starts_with(' ')))
        .map(|rest| rest.split_whitespace().collect())
        .unwrap_or_default()
}

fn csp(key: &str) -> String {
    json("tauri.conf.json")["app"]["security"][key]
        .as_str()
        .unwrap_or_else(|| panic!("app.security.{key} must be set"))
        .to_string()
}

#[test]
fn csp_allows_only_the_app_itself_to_run_code() {
    for key in ["csp", "devCsp"] {
        let csp = csp(key);
        assert_eq!(sources(&csp, "default-src"), ["'self'"], "{key}");
        let script = sources(&csp, "script-src");
        assert!(!script.contains(&"'unsafe-eval'") && !script.iter().any(|s| s.contains("http")), "{key}: {script:?}");
        assert_eq!(sources(&csp, "object-src"), ["'none'"], "{key}");
        assert_eq!(sources(&csp, "base-uri"), ["'none'"], "{key}");
        for d in ["img-src", "media-src"] {
            let s = sources(&csp, d);
            assert!(s.contains(&"http://asset.localhost"), "{key} {d} must serve clips and thumbnails: {s:?}");
            assert!(!s.iter().any(|x| x.starts_with("https:") || *x == "*"), "{key} {d}: no remote sources: {s:?}");
        }
        let connect = sources(&csp, "connect-src");
        assert!(connect.contains(&"ipc:") && connect.contains(&"http://ipc.localhost"), "{key}: {connect:?}");
    }
    assert!(!csp("csp").contains("ws:"), "the dev server socket stays out of the release CSP");
    // Release: only the bundled script files. (Dev needs inline script for
    // Vite's React refresh preamble; that never ships.)
    assert_eq!(sources(&csp("csp"), "script-src"), ["'self'"]);
    assert_eq!(sources(&csp("devCsp"), "script-src"), ["'self'", "'unsafe-inline'"]);
}

#[test]
fn js_gets_no_plugin_permissions_it_does_not_use() {
    let caps = json("capabilities/default.json");
    let perms: Vec<&str> = caps["permissions"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert_eq!(perms, ["core:default"], "Explorer is opened from Rust after a path check, not by the webview");
}
