use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::native::auth;
use crate::native::cdp::client::CdpClient;

use crate::native::interaction;
use crate::native::network::{self, DomainFilter};
use crate::native::recording;

use super::{browser_ctx, req_str};
use super::{
    execute_command, wait_for_selector, DaemonState, FetchPausedRequest, HarEntry, RouteEntry,
    RouteResponse, TrackedRequest, AUTH_LOGIN_PREFERRED_SELECTOR_WINDOW_MS,
    AUTH_LOGIN_SELECTOR_POLL_INTERVAL_MS, AUTH_LOGIN_WAIT_UNTIL,
};

// ---------------------------------------------------------------------------
// Video and HAR handlers
// ---------------------------------------------------------------------------

pub(crate) async fn handle_video_start(
    cmd: &Value,
    state: &mut DaemonState,
) -> Result<Value, String> {
    let path = req_str(cmd, "path")?;

    if state.recording_state.active {
        return Err("A recording is already in progress".to_string());
    }

    let mgr = state.browser.as_ref().ok_or("Browser not launched")?;
    let session_id = mgr.active_session_id()?.to_string();

    recording::recording_start(&mut state.recording_state, path)?;
    state
        .start_recording_task(mgr.client.clone(), session_id)
        .await?;

    Ok(json!({
        "started": true,
        "note": "Video recording started. Use video_stop to save the recording."
    }))
}

pub(crate) async fn handle_video_stop(state: &mut DaemonState) -> Result<Value, String> {
    if !state.recording_state.active {
        return Ok(json!({
            "stopped": false,
            "note": "No video recording was started. Use recording_stop if you used recording_start."
        }));
    }

    state.stop_recording_task().await?;
    recording::recording_stop(&mut state.recording_state)
}

/// Begin capturing network traffic for a later HAR export.
pub(crate) async fn handle_har_start(state: &mut DaemonState) -> Result<Value, String> {
    let (mgr, session_id) = browser_ctx(state)?;
    mgr.client
        .send_command_no_params("Network.enable", Some(&session_id))
        .await?;
    // Also enable Network on cross-origin iframe sessions so their
    // requests are captured in the HAR output.
    for iframe_sid in state.iframe_sessions.values() {
        let _ = mgr
            .client
            .send_command_no_params("Network.enable", Some(iframe_sid.as_str()))
            .await;
    }
    state.har_recording = true;
    state.har_entries.clear();
    Ok(json!({ "started": true }))
}

/// Stop HAR recording and write the captured requests to disk.
pub(crate) async fn handle_har_stop(cmd: &Value, state: &mut DaemonState) -> Result<Value, String> {
    let path = har_output_path(cmd.get("path").and_then(|v| v.as_str()));

    state.har_recording = false;

    let entries: Vec<Value> = state.har_entries.drain(..).map(har_entry_to_json).collect();
    let request_count = entries.len();
    let browser = har_browser_metadata(state).await;

    let mut log = json!({
        "version": "1.2",
        "creator": {
            "name": "agent-browser",
            "version": env!("CARGO_PKG_VERSION")
        },
        "entries": entries
    });
    if let Some(browser) = browser {
        log["browser"] = browser;
    }
    let har = json!({ "log": log });

    let har_str = serde_json::to_string_pretty(&har)
        .map_err(|e| format!("Failed to serialize HAR: {}", e))?;
    std::fs::write(&path, har_str).map_err(|e| format!("Failed to write HAR: {}", e))?;

    Ok(json!({ "path": path, "requestCount": request_count }))
}

// ---------------------------------------------------------------------------
// HAR serialization helpers
// ---------------------------------------------------------------------------

/// Convert a `HarEntry` (collected from CDP events) into a HAR 1.2 entry object.
pub(crate) fn har_entry_to_json(e: HarEntry) -> Value {
    let started_date_time = har_wall_time_to_rfc3339(e.wall_time);

    let request_cookies = e
        .request_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
        .map(|(_, v)| har_parse_request_cookies(v))
        .unwrap_or_default();

    let query_string = har_parse_query_string(&e.url);

    let req_headers: Vec<Value> = e
        .request_headers
        .iter()
        .map(|(k, v)| json!({ "name": k, "value": v }))
        .collect();

    let resp_cookies: Vec<Value> = e
        .response_headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
        .map(|(_, v)| {
            // Split on ';' first to discard attributes (Path, HttpOnly, etc.),
            // then split on '=' once to separate name from value.
            let name_value = v.split(';').next().unwrap_or("");
            let (name, value) = name_value.split_once('=').unwrap_or((name_value, ""));
            json!({ "name": name.trim(), "value": value.trim() })
        })
        .collect();

    let resp_headers: Vec<Value> = e
        .response_headers
        .iter()
        .map(|(k, v)| json!({ "name": k, "value": v }))
        .collect();

    let (timings, total_time) =
        har_compute_timings(e.cdp_timing.as_ref(), e.loading_finished_timestamp);

    let mime_type = if e.mime_type.is_empty() {
        "application/octet-stream".to_string()
    } else {
        e.mime_type
    };

    let post_content_type = e
        .request_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("text/plain")
        .to_string();

    let mut request = json!({
        "method": e.method,
        "url": e.url,
        "httpVersion": e.http_version,
        "cookies": request_cookies,
        "headers": req_headers,
        "queryString": query_string,
        "headersSize": -1,
        "bodySize": e.request_body_size,
    });
    if let Some(body) = e.post_data {
        request["postData"] = json!({ "mimeType": post_content_type, "text": body });
    }

    json!({
        "startedDateTime": started_date_time,
        "time": total_time,
        "request": request,
        "response": {
            "status": e.status.unwrap_or(0),
            "statusText": e.status_text,
            "httpVersion": e.http_version,
            "cookies": resp_cookies,
            "headers": resp_headers,
            "content": {
                "size": e.response_body_size,
                "mimeType": mime_type,
            },
            "redirectURL": e.redirect_url,
            "headersSize": -1,
            "bodySize": e.response_body_size,
        },
        "cache": {},
        "timings": timings,
        "_resourceType": e.resource_type,
    })
}

/// Convert a CDP headers object (`{ "Name": "value", ... }`) into a flat
/// `Vec<(name, value)>` preserving insertion order.
pub(crate) fn har_extract_headers(headers_val: Option<&Value>) -> Vec<(String, String)> {
    headers_val
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Map a CDP `response.protocol` value to an HTTP-version string as required
/// by the HAR spec (e.g. `"h2"` → `"HTTP/2.0"`).
pub(crate) fn har_cdp_protocol_to_http_version(protocol: &str) -> String {
    match protocol.to_ascii_lowercase().as_str() {
        "h2" => "HTTP/2.0".to_string(),
        "h3" => "HTTP/3.0".to_string(),
        "http/1.0" => "HTTP/1.0".to_string(),
        _ => "HTTP/1.1".to_string(),
    }
}

/// Parse query-string parameters from a URL into a HAR `queryString` array.
pub(crate) fn har_parse_query_string(url_str: &str) -> Vec<Value> {
    url::Url::parse(url_str)
        .map(|u| {
            u.query_pairs()
                .map(|(k, v)| json!({ "name": k.as_ref(), "value": v.as_ref() }))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a `Cookie: name1=val1; name2=val2` header value into HAR cookie objects.
pub(crate) fn har_parse_request_cookies(cookie_header: &str) -> Vec<Value> {
    cookie_header
        .split(';')
        .filter_map(|pair| {
            let pair = pair.trim();
            if pair.is_empty() {
                return None;
            }
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some(json!({ "name": name.trim(), "value": value.trim() }))
        })
        .collect()
}

/// Compute HAR `timings` and total `time` (ms) from a CDP `ResourceTiming`
/// object and the optional `Network.loadingFinished` monotonic timestamp.
///
/// CDP timing values are milliseconds relative to `requestTime` (seconds since
/// browser start). A value of `-1` means the phase did not occur.
pub(crate) fn har_compute_timings(
    cdp_timing: Option<&Value>,
    loading_finished_ts: Option<f64>,
) -> (Value, f64) {
    let Some(t) = cdp_timing else {
        return (json!({ "send": 0, "wait": 0, "receive": 0 }), 0.0);
    };

    let get = |key: &str| t.get(key).and_then(|v| v.as_f64()).unwrap_or(-1.0);

    let request_time = get("requestTime");
    let dns_start = get("dnsStart");
    let dns_end = get("dnsEnd");
    let connect_start = get("connectStart");
    let connect_end = get("connectEnd");
    let ssl_start = get("sslStart");
    let ssl_end = get("sslEnd");
    let send_start = get("sendStart");
    let send_end = get("sendEnd");
    let recv_headers_start = get("receiveHeadersStart");
    let recv_headers_end = get("receiveHeadersEnd");

    let dns = if dns_start >= 0.0 && dns_end >= 0.0 {
        dns_end - dns_start
    } else {
        -1.0
    };
    let connect = if connect_start >= 0.0 && connect_end >= 0.0 {
        connect_end - connect_start
    } else {
        -1.0
    };
    let ssl = if ssl_start >= 0.0 && ssl_end >= 0.0 {
        ssl_end - ssl_start
    } else {
        -1.0
    };
    let send = (send_end - send_start).max(0.0);

    // wait: end of sending → first byte of response headers.
    let wait_end = if recv_headers_start >= 0.0 {
        recv_headers_start
    } else {
        recv_headers_end
    };
    let wait = if send_end >= 0.0 && wait_end >= send_end {
        wait_end - send_end
    } else {
        0.0
    };

    // receive: first response byte → loading complete.
    // requestTime (seconds) + recv_headers_end (ms) / 1000 = absolute headers-end timestamp.
    let receive = loading_finished_ts
        .filter(|_| request_time >= 0.0 && recv_headers_end >= 0.0)
        .map(|lf_ts| {
            let recv_start_abs = request_time + recv_headers_end / 1000.0;
            ((lf_ts - recv_start_abs) * 1000.0).max(0.0)
        })
        .unwrap_or(0.0);

    let blocked = if dns_start > 0.0 {
        dns_start
    } else if connect_start > 0.0 {
        connect_start
    } else if send_start > 0.0 {
        send_start
    } else {
        -1.0
    };

    let total: f64 = [
        if blocked > 0.0 { blocked } else { 0.0 },
        if dns >= 0.0 { dns } else { 0.0 },
        if connect >= 0.0 { connect } else { 0.0 },
        send,
        wait,
        receive,
    ]
    .iter()
    .sum();

    let mut timings = json!({ "send": send, "wait": wait, "receive": receive });
    if blocked > 0.0 {
        timings["blocked"] = json!(blocked);
    }
    if dns >= 0.0 {
        timings["dns"] = json!(dns);
    }
    if connect >= 0.0 {
        timings["connect"] = json!(connect);
    }
    if ssl >= 0.0 {
        timings["ssl"] = json!(ssl);
    }

    (timings, total)
}

/// Format a Unix epoch timestamp (seconds, fractional) as RFC 3339 using the
/// `time` crate, e.g. `"2024-03-17T10:30:00.456Z"`.
pub(crate) fn har_wall_time_to_rfc3339(wall_time: f64) -> String {
    if wall_time > 0.0 {
        let nanos = (wall_time * 1_000_000_000.0).round() as i128;
        if let Ok(dt) = OffsetDateTime::from_unix_timestamp_nanos(nanos) {
            if let Ok(s) = dt.format(&Rfc3339) {
                return s;
            }
        }
    }
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub(crate) fn har_output_path(explicit_path: Option<&str>) -> String {
    match explicit_path {
        Some(path) => path.to_string(),
        None => {
            let dir = get_har_dir();
            let _ = std::fs::create_dir_all(&dir);
            dir.join(format!("har-{}.har", unix_timestamp_millis()))
                .to_string_lossy()
                .to_string()
        }
    }
}

pub(crate) fn get_har_dir() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        home.join(".agent-browser").join("tmp").join("har")
    } else {
        std::env::temp_dir().join("agent-browser").join("har")
    }
}

pub(crate) fn unix_timestamp_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub(crate) async fn har_browser_metadata(state: &DaemonState) -> Option<Value> {
    let mgr = state.browser.as_ref()?;
    if !mgr.is_connection_alive().await {
        return None;
    }

    let version = mgr
        .client
        .send_command_no_params("Browser.getVersion", None)
        .await
        .ok()?;
    browser_metadata_from_version(&version)
}

pub(crate) fn browser_metadata_from_version(version: &Value) -> Option<Value> {
    let product = version.get("product").and_then(|v| v.as_str())?;
    let (name, browser_version) = product.split_once('/').unwrap_or((product, ""));
    Some(json!({
        "name": name,
        "version": browser_version,
    }))
}

// ---------------------------------------------------------------------------
// Fetch interception resolver (domain filter + routes + origin headers)
// ---------------------------------------------------------------------------

pub(crate) fn collapse_wildcards(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut last_was_star = false;
    for ch in pattern.chars() {
        if ch == '*' {
            if !last_was_star {
                out.push(ch);
            }
            last_was_star = true;
        } else {
            out.push(ch);
            last_was_star = false;
        }
    }
    out
}

pub(crate) fn route_url_matches(pattern: &str, url: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return url.contains(pattern);
    }

    let pattern = collapse_wildcards(pattern);
    let parts: Vec<&str> = pattern.split('*').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return true;
    }

    let anchored_start = !pattern.starts_with('*');
    let anchored_end = !pattern.ends_with('*');
    let mut pos = 0usize;
    let mut idx = 0usize;

    if anchored_start {
        let first = parts[0];
        if !url.starts_with(first) {
            return false;
        }
        pos = first.len();
        idx = 1;
    }

    while idx < parts.len() {
        let part = parts[idx];
        let Some(found) = url[pos..].find(part) else {
            return false;
        };
        pos += found + part.len();
        idx += 1;
    }

    if anchored_end {
        if let Some(last) = parts.last() {
            return url.ends_with(last);
        }
    }

    true
}

pub(crate) async fn resolve_fetch_paused(
    client: &CdpClient,
    domain_filter: Option<&DomainFilter>,
    routes: &[RouteEntry],
    origin_headers: &HashMap<String, HashMap<String, String>>,
    paused: &FetchPausedRequest,
) {
    let session_id = &paused.session_id;

    // Domain filter check (takes priority over routes and origin headers)
    if let Some(filter) = domain_filter {
        if let Ok(parsed) = url::Url::parse(&paused.url) {
            let scheme = parsed.scheme();
            if scheme != "http" && scheme != "https" {
                if paused.resource_type.eq_ignore_ascii_case("document") {
                    let _ = client
                        .send_command(
                            "Fetch.failRequest",
                            Some(json!({
                                "requestId": paused.request_id,
                                "errorReason": "BlockedByClient"
                            })),
                            Some(session_id),
                        )
                        .await;
                } else {
                    let _ = client
                        .send_command(
                            "Fetch.continueRequest",
                            Some(json!({ "requestId": paused.request_id })),
                            Some(session_id),
                        )
                        .await;
                }
                return;
            }

            if let Some(hostname) = parsed.host_str() {
                if !filter.is_allowed(hostname) {
                    if paused.resource_type.eq_ignore_ascii_case("document") {
                        let error_body = format!(
                            "<html><body><h1>Blocked</h1><p>Navigation to {} is not allowed by domain filter.</p></body></html>",
                            hostname
                        );
                        let encoded = base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            error_body.as_bytes(),
                        );
                        let _ = client
                            .send_command(
                                "Fetch.fulfillRequest",
                                Some(json!({
                                    "requestId": paused.request_id,
                                    "responseCode": 403,
                                    "responseHeaders": [
                                        { "name": "Content-Type", "value": "text/html" },
                                    ],
                                    "body": encoded,
                                })),
                                Some(session_id),
                            )
                            .await;
                    } else {
                        let _ = client
                            .send_command(
                                "Fetch.failRequest",
                                Some(json!({
                                    "requestId": paused.request_id,
                                    "errorReason": "BlockedByClient"
                                })),
                                Some(session_id),
                            )
                            .await;
                    }
                    return;
                }
            }
        }
    }

    // Route matching
    for route in routes {
        let url_matches = route_url_matches(&route.url_pattern, &paused.url);

        let resource_type_matches = route.resource_types.is_empty()
            || route
                .resource_types
                .iter()
                .any(|rt| rt.eq_ignore_ascii_case(&paused.resource_type));

        let matches = url_matches && resource_type_matches;

        if matches {
            if route.abort {
                let _ = client
                    .send_command(
                        "Fetch.failRequest",
                        Some(json!({
                            "requestId": paused.request_id,
                            "errorReason": "Failed"
                        })),
                        Some(session_id),
                    )
                    .await;
                return;
            }

            if let Some(ref resp) = route.response {
                let status = resp.status.unwrap_or(200);
                let body_str = resp.body.as_deref().unwrap_or("");
                let encoded = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    body_str.as_bytes(),
                );
                let mut headers = vec![];
                if let Some(ct) = &resp.content_type {
                    headers.push(json!({ "name": "Content-Type", "value": ct }));
                }
                if let Some(h) = &resp.headers {
                    for (k, v) in h {
                        headers.push(json!({ "name": k, "value": v }));
                    }
                }

                let _ = client
                    .send_command(
                        "Fetch.fulfillRequest",
                        Some(json!({
                            "requestId": paused.request_id,
                            "responseCode": status,
                            "responseHeaders": headers,
                            "body": encoded,
                        })),
                        Some(session_id),
                    )
                    .await;
                return;
            }
        }
    }

    // No matching route — continue, injecting origin-scoped headers if applicable.
    let extra = url::Url::parse(&paused.url)
        .ok()
        .map(|u| u.origin().ascii_serialization())
        .and_then(|o| origin_headers.get(&o));

    if let Some(extra_headers) = extra {
        // Merge original request headers with extra headers.
        // Fetch.continueRequest replaces (not merges), so include originals.
        let mut combined: Vec<Value> = Vec::new();
        if let Some(ref orig) = paused.request_headers {
            for (k, v) in orig {
                if !extra_headers.keys().any(|ek| ek.eq_ignore_ascii_case(k)) {
                    if let Some(s) = v.as_str() {
                        combined.push(json!({ "name": k, "value": s }));
                    }
                }
            }
        }
        for (k, v) in extra_headers {
            combined.push(json!({ "name": k, "value": v }));
        }
        let _ = client
            .send_command(
                "Fetch.continueRequest",
                Some(json!({ "requestId": paused.request_id, "headers": combined })),
                Some(session_id),
            )
            .await;
    } else {
        let _ = client
            .send_command(
                "Fetch.continueRequest",
                Some(json!({ "requestId": paused.request_id })),
                Some(session_id),
            )
            .await;
    }
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

/// Build the Fetch.enable patterns list from current routes, domain filter,
/// and origin headers state.  When domain filtering or origin-scoped headers
/// are active a wildcard pattern is included so all requests are intercepted.
pub(crate) async fn build_fetch_patterns(state: &DaemonState) -> Vec<Value> {
    let routes = state.routes.read().await;
    let mut patterns: Vec<Value> = routes
        .iter()
        .map(|r| json!({ "urlPattern": collapse_wildcards(&r.url_pattern) }))
        .collect();
    let has_domain_filter = state.domain_filter.read().await.is_some();
    let has_origin_headers = !state.origin_headers.read().await.is_empty();
    let has_proxy_creds = state.proxy_credentials.read().await.is_some();
    if (has_domain_filter || has_origin_headers || has_proxy_creds)
        && !patterns.iter().any(|p| p["urlPattern"] == "*")
    {
        patterns.push(json!({ "urlPattern": "*" }));
    }
    patterns
}

/// Build the full Fetch.enable params object, including `handleAuthRequests`
/// when proxy credentials are configured.
pub(crate) async fn build_fetch_enable_params(state: &DaemonState, patterns: Vec<Value>) -> Value {
    let has_proxy_creds = state.proxy_credentials.read().await.is_some();
    if has_proxy_creds {
        json!({ "patterns": patterns, "handleAuthRequests": true })
    } else {
        json!({ "patterns": patterns })
    }
}

pub(crate) fn parse_route_response(cmd: &Value) -> Option<RouteResponse> {
    cmd.get("response")
        .and_then(|v| {
            if v.is_null() {
                return None;
            }
            Some(RouteResponse {
                status: v.get("status").and_then(|s| s.as_u64()).map(|s| s as u16),
                body: v.get("body").and_then(|s| s.as_str()).map(String::from),
                content_type: v
                    .get("contentType")
                    .and_then(|s| s.as_str())
                    .map(String::from),
                headers: v.get("headers").and_then(|h| {
                    h.as_object().map(|m| {
                        m.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                }),
            })
        })
        .or_else(|| {
            cmd.get("body")
                .and_then(|v| v.as_str())
                .map(|body| RouteResponse {
                    status: None,
                    body: Some(body.to_string()),
                    content_type: None,
                    headers: None,
                })
        })
}

pub(crate) async fn handle_route(cmd: &Value, state: &mut DaemonState) -> Result<Value, String> {
    let (mgr, session_id) = browser_ctx(state)?;
    let url_pattern = cmd
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'url' parameter")?
        .to_string();
    let abort = cmd.get("abort").and_then(|v| v.as_bool()).unwrap_or(false);

    let resource_types: Vec<String> = cmd
        .get("resourceType")
        .or_else(|| cmd.get("resourceTypes"))
        .and_then(|v| {
            if let Some(s) = v.as_str() {
                Some(
                    s.split(',')
                        .map(|p| p.trim().to_string())
                        .filter(|p| !p.is_empty())
                        .collect(),
                )
            } else {
                v.as_array().map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .filter(|s| !s.is_empty())
                        .collect()
                })
            }
        })
        .unwrap_or_default();

    let response = parse_route_response(cmd);

    {
        let mut routes = state.routes.write().await;
        routes.push(RouteEntry {
            url_pattern: url_pattern.clone(),
            response,
            abort,
            resource_types,
        });
    }

    let patterns = build_fetch_patterns(state).await;
    let params = build_fetch_enable_params(state, patterns).await;
    mgr.client
        .send_command("Fetch.enable", Some(params), Some(&session_id))
        .await?;

    Ok(json!({ "routed": url_pattern }))
}

pub(crate) async fn handle_unroute(cmd: &Value, state: &mut DaemonState) -> Result<Value, String> {
    let (mgr, session_id) = browser_ctx(state)?;

    let url = cmd.get("url").and_then(|v| v.as_str());

    {
        let mut routes = state.routes.write().await;
        match url {
            Some(pattern) => {
                routes.retain(|r| r.url_pattern != pattern);
            }
            None => {
                routes.clear();
            }
        }
    }

    let patterns = build_fetch_patterns(state).await;
    if patterns.is_empty() {
        mgr.client
            .send_command("Fetch.disable", None, Some(&session_id))
            .await?;
    } else {
        let params = build_fetch_enable_params(state, patterns).await;
        mgr.client
            .send_command("Fetch.enable", Some(params), Some(&session_id))
            .await?;
    }

    let label = url.unwrap_or("all");
    Ok(json!({ "unrouted": label }))
}

pub(crate) fn matches_status_filter(status: Option<i64>, filter: &str) -> bool {
    let Some(code) = status else { return false };
    let f = filter.to_lowercase();
    if let Ok(exact) = f.parse::<i64>() {
        return code == exact;
    }
    if f.len() == 3 && f.ends_with("xx") {
        if let Ok(prefix) = f[..1].parse::<i64>() {
            return code / 100 == prefix;
        }
    }
    if let Some((lo, hi)) = f.split_once('-') {
        if let (Ok(lo), Ok(hi)) = (lo.parse::<i64>(), hi.parse::<i64>()) {
            return code >= lo && code <= hi;
        }
    }
    false
}

pub(crate) async fn handle_requests(cmd: &Value, state: &mut DaemonState) -> Result<Value, String> {
    if cmd.get("clear").and_then(|v| v.as_bool()).unwrap_or(false) {
        state.tracked_requests.clear();
        return Ok(json!({ "cleared": true }));
    }

    if !state.request_tracking {
        state.request_tracking = true;
        if let Some(ref mgr) = state.browser {
            if let Ok(session_id) = mgr.active_session_id() {
                let _ = mgr
                    .client
                    .send_command_no_params("Network.enable", Some(session_id))
                    .await;
            }
        }
    }

    let filter = cmd.get("filter").and_then(|v| v.as_str());
    let type_filter = cmd.get("type").and_then(|v| v.as_str());
    let method_filter = cmd.get("method").and_then(|v| v.as_str());
    let status_filter = cmd.get("status").and_then(|v| v.as_str());

    let type_list: Vec<String> = type_filter
        .map(|t| t.split(',').map(|s| s.trim().to_lowercase()).collect())
        .unwrap_or_default();

    let requests: Vec<&TrackedRequest> = state
        .tracked_requests
        .iter()
        .filter(|r| {
            if let Some(f) = filter {
                if !r.url.contains(f) {
                    return false;
                }
            }
            if !type_list.is_empty() && !type_list.contains(&r.resource_type.to_lowercase()) {
                return false;
            }
            if let Some(m) = method_filter {
                if !r.method.eq_ignore_ascii_case(m) {
                    return false;
                }
            }
            if let Some(s) = status_filter {
                if !matches_status_filter(r.status, s) {
                    return false;
                }
            }
            true
        })
        .collect();

    Ok(json!({ "requests": requests }))
}

pub(crate) async fn handle_request_detail(
    cmd: &Value,
    state: &mut DaemonState,
) -> Result<Value, String> {
    let request_id = req_str(cmd, "requestId")?;

    let entry = state
        .tracked_requests
        .iter()
        .find(|r| r.request_id == request_id)
        .ok_or("Request not found")?;

    let mut result = serde_json::to_value(entry).unwrap_or(json!({}));

    if let Some(ref mgr) = state.browser {
        if let Ok(session_id) = mgr.active_session_id() {
            if let Ok(body_result) = mgr
                .client
                .send_command(
                    "Network.getResponseBody",
                    Some(json!({ "requestId": request_id })),
                    Some(session_id),
                )
                .await
            {
                let base64_encoded = body_result
                    .get("base64Encoded")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let body = body_result
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if base64_encoded {
                    result["responseBody"] = json!(format!("[base64, {} chars]", body.len()));
                } else {
                    result["responseBody"] = json!(body);
                }
            }
        }
    }

    Ok(result)
}

pub(crate) async fn handle_http_credentials(
    cmd: &Value,
    state: &DaemonState,
) -> Result<Value, String> {
    let (mgr, session_id) = browser_ctx(state)?;
    let username = req_str(cmd, "username")?;
    let password = req_str(cmd, "password")?;

    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("{}:{}", username, password),
    );

    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), format!("Basic {}", encoded));
    network::set_extra_headers(&mgr.client, &session_id, &headers).await?;

    Ok(json!({ "set": true }))
}

// ---------------------------------------------------------------------------
// Auth handlers
// ---------------------------------------------------------------------------

/// Wait for any selector in `selectors` to appear and return the first match.
///
/// This is used by `auth_login` auto-detection so SPA login forms can render
/// after initial navigation without requiring global network-idle.
pub(crate) async fn wait_for_any_selector(
    client: &crate::native::cdp::client::CdpClient,
    session_id: &str,
    selectors: &[&str],
    timeout_ms: u64,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);

    loop {
        for selector in selectors {
            let expression = format!(
                r#"(() => {{
                    const el = document.querySelector({sel});
                    if (!el) return false;

                    const r = el.getBoundingClientRect();
                    const s = window.getComputedStyle(el);
                    const opacity = parseFloat(s.opacity || '1');
                    const isVisible =
                        r.width > 0 &&
                        r.height > 0 &&
                        s.visibility !== 'hidden' &&
                        s.display !== 'none' &&
                        (!Number.isFinite(opacity) || opacity > 0);

                    if (!isVisible) return false;
                    if (el.matches(':disabled')) return false;

                    if (el instanceof HTMLInputElement && el.type === 'hidden') return false;
                    if ((el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement) && el.readOnly) return false;

                    return true;
                }})()"#,
                sel = serde_json::to_string(selector).unwrap_or_default()
            );

            let result: crate::native::cdp::types::EvaluateResult = client
                .send_command_typed(
                    "Runtime.evaluate",
                    &crate::native::cdp::types::EvaluateParams {
                        expression,
                        return_by_value: Some(true),
                        await_promise: Some(true),
                    },
                    Some(session_id),
                )
                .await?;

            if result
                .result
                .value
                .as_ref()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return Ok((*selector).to_string());
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(format!("Wait timed out after {}ms", timeout_ms));
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(
            AUTH_LOGIN_SELECTOR_POLL_INTERVAL_MS,
        ))
        .await;
    }
}

pub(crate) async fn handle_auth_save(cmd: &Value) -> Result<Value, String> {
    let name = cmd
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'name'")?;
    let url = cmd
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'url'")?;
    let username = cmd
        .get("username")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'username'")?;
    let password = cmd
        .get("password")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'password'")?;
    let username_selector = cmd.get("usernameSelector").and_then(|v| v.as_str());
    let password_selector = cmd.get("passwordSelector").and_then(|v| v.as_str());
    let submit_selector = cmd.get("submitSelector").and_then(|v| v.as_str());
    auth::auth_save(
        name,
        url,
        username,
        password,
        username_selector,
        password_selector,
        submit_selector,
    )
}

pub(crate) async fn handle_auth_login(
    cmd: &Value,
    state: &mut DaemonState,
) -> Result<Value, String> {
    let name = cmd
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'name'")?;
    if state.browser.is_none() {
        return Err("Browser not launched".to_string());
    }
    let url_override = cmd.get("url").and_then(|v| v.as_str());
    let cred = if let Some(provider) = cmd.get("credentialProvider").and_then(|v| v.as_str()) {
        let command_plugins = cmd
            .get("plugins")
            .and_then(|v| {
                serde_json::from_value::<Vec<crate::plugins::PluginConfig>>(v.clone()).ok()
            })
            .unwrap_or_else(crate::plugins::plugins_from_env);
        let resolved = crate::plugins::resolve_credential_with_plugins(
            provider,
            &command_plugins,
            crate::plugins::CredentialResolveRequest {
                profile_name: name,
                item_ref: cmd.get("credentialItem").and_then(|v| v.as_str()),
                url: url_override,
            },
        )
        .await?;
        auth::AuthProfile {
            name: name.to_string(),
            url: url_override
                .map(String::from)
                .or(resolved.url)
                .unwrap_or_default(),
            username: resolved.username,
            password: resolved.password,
            username_selector: resolved.username_selector,
            password_selector: resolved.password_selector,
            submit_selector: resolved.submit_selector,
            created_at: None,
            last_login_at: None,
        }
    } else {
        let mut profile = auth::credentials_get_full(name)?;
        if let Some(url) = url_override {
            profile.url = url.to_string();
        }
        profile
    };
    if cred.url.is_empty() {
        return Err("Credential has no URL".to_string());
    }
    let auth::AuthProfile {
        url,
        username,
        password,
        username_selector: stored_username_selector,
        password_selector: stored_password_selector,
        submit_selector: stored_submit_selector,
        ..
    } = cred;

    let mgr = state.browser.as_mut().ok_or("Browser not launched")?;
    mgr.navigate(&url, AUTH_LOGIN_WAIT_UNTIL).await?;

    let session_id = mgr.active_session_id()?.to_string();
    let auth_timeout_ms = mgr.default_timeout_ms();

    let preferred_user_selectors = [
        "input[type=email]",
        "input[name=email]",
        "input[id=email]",
        "input[autocomplete=email]",
        "input[autocomplete=username]",
        "input[name=username]",
        "input[name*=email i]",
        "input[name*=user i]",
        "input[id*=email i]",
        "input[id*=user i]",
        "input[type=text][name*=email i]",
        "input[type=text][name*=user i]",
        "input[type=text][id*=email i]",
        "input[type=text][id*=user i]",
        "input[type=text][autocomplete=email]",
        "input[type=text][autocomplete=username]",
    ];
    let fallback_user_selectors = ["input[type=text]", "input:not([type])"];
    let auto_submit_selectors = [
        "button[type=submit]",
        "input[type=submit]",
        "button:not([type])",
    ];

    let username_sel = cmd
        .get("usernameSelector")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or(stored_username_selector);
    let password_sel = cmd
        .get("passwordSelector")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or(stored_password_selector);
    let submit_sel = cmd
        .get("submitSelector")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or(stored_submit_selector);

    // Find and fill username
    let user_sel = if let Some(s) = username_sel {
        wait_for_selector(&mgr.client, &session_id, &s, "visible", auth_timeout_ms)
            .await
            .map_err(|_| format!("Timed out waiting for username selector '{}'", s))?;
        s
    } else {
        let preferred_window_ms = auth_timeout_ms.min(AUTH_LOGIN_PREFERRED_SELECTOR_WINDOW_MS);
        let fallback_window_ms = auth_timeout_ms.saturating_sub(preferred_window_ms);

        match wait_for_any_selector(
            &mgr.client,
            &session_id,
            &preferred_user_selectors,
            preferred_window_ms,
        )
        .await
        {
            Ok(selector) => selector,
            Err(_) => {
                if fallback_window_ms == 0 {
                    return Err(format!(
                        "Timed out waiting for username field (preferred selectors for {}ms: {})",
                        preferred_window_ms,
                        preferred_user_selectors.join(", ")
                    ));
                }

                wait_for_any_selector(
                    &mgr.client,
                    &session_id,
                    &fallback_user_selectors,
                    fallback_window_ms,
                )
                .await
                .map_err(|_| {
                    format!(
                        "Timed out waiting for username field (preferred selectors for {}ms: {}; fallback selectors for {}ms: {})",
                        preferred_window_ms,
                        preferred_user_selectors.join(", "),
                        fallback_window_ms,
                        fallback_user_selectors.join(", ")
                    )
                })?
            }
        }
    };
    interaction::fill(
        &mgr.client,
        &session_id,
        &state.ref_map,
        &user_sel,
        &username,
        &state.iframe_sessions,
    )
    .await?;

    // Find and fill password
    let pass_sel = password_sel.unwrap_or_else(|| "input[type=password]".to_string());
    wait_for_selector(
        &mgr.client,
        &session_id,
        &pass_sel,
        "visible",
        auth_timeout_ms,
    )
    .await
    .map_err(|_| format!("Timed out waiting for password selector '{}'", pass_sel))?;
    interaction::fill(
        &mgr.client,
        &session_id,
        &state.ref_map,
        &pass_sel,
        &password,
        &state.iframe_sessions,
    )
    .await?;

    // Find and click submit
    let sub_sel = if let Some(s) = submit_sel {
        wait_for_selector(&mgr.client, &session_id, &s, "visible", auth_timeout_ms)
            .await
            .map_err(|_| format!("Timed out waiting for submit selector '{}'", s))?;
        s
    } else {
        wait_for_any_selector(
            &mgr.client,
            &session_id,
            &auto_submit_selectors,
            auth_timeout_ms,
        )
        .await
        .map_err(|_| {
            format!(
                "Timed out waiting for submit button (tried selectors: {})",
                auto_submit_selectors.join(", ")
            )
        })?
    };
    interaction::click(
        &mgr.client,
        &session_id,
        &state.ref_map,
        &sub_sel,
        "left",
        1,
        &state.iframe_sessions,
    )
    .await?;

    // Wait for navigation after submit (with fallback timeout)
    let mut rx = mgr.client.subscribe();
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
    let mut navigated = false;

    loop {
        let result = tokio::time::timeout_at(deadline, rx.recv()).await;
        match result {
            Ok(Ok(event)) => {
                if event.session_id.as_deref() == Some(&session_id) {
                    match event.method.as_str() {
                        "Page.frameNavigated" | "Page.loadEventFired" => {
                            navigated = true;
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    if !navigated {
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    }

    Ok(json!({ "loggedIn": true, "name": name }))
}

// ---------------------------------------------------------------------------
// Confirmation handlers (stub)
// ---------------------------------------------------------------------------

pub(crate) async fn handle_confirm(_cmd: &Value, state: &mut DaemonState) -> Result<Value, String> {
    let pending = state
        .pending_confirmation
        .take()
        .ok_or("No pending confirmation")?;

    let mut approved_actions = pending.approved_actions.clone();
    if !approved_actions.iter().any(|a| a == &pending.action) {
        approved_actions.push(pending.action.clone());
    }
    let previous_confirmed = std::mem::replace(
        &mut state.confirmed_policy_actions,
        approved_actions.into_iter().collect(),
    );
    let result = Box::pin(execute_command(&pending.cmd, state)).await;
    state.confirmed_policy_actions = previous_confirmed;

    Ok(json!({ "confirmed": true, "action": pending.action, "result": result }))
}

pub(crate) async fn handle_deny(_cmd: &Value, state: &mut DaemonState) -> Result<Value, String> {
    let pending = state
        .pending_confirmation
        .take()
        .ok_or("No pending confirmation")?;

    Ok(json!({ "denied": true, "action": pending.action }))
}
