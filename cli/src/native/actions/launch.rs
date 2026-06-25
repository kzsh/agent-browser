use serde_json::{json, Value};
use std::env;
use std::fs;

use crate::native::browser::BrowserManager;
use crate::native::cdp::chrome::LaunchOptions;
use crate::native::network::{self, DomainFilter};
use crate::native::providers;
use crate::native::react;
use crate::native::state;
use crate::native::webdriver::appium::AppiumManager;
use crate::native::webdriver::backend::WebDriverBackend;
use crate::native::webdriver::ios;
use crate::native::webdriver::safari;

use super::{
    close_current_browser, launch_hash, plugins_from_command_or_env,
    provider_plugin_launch_options_from_command, remember_active_provider_session,
    write_engine_file, write_extensions_file, write_extensions_file_from_paths,
    write_provider_file, BackendType, DaemonState,
};

// ---------------------------------------------------------------------------
// Auto-launch
// ---------------------------------------------------------------------------

/// Connect to a running Chrome via auto-discovery and open a fresh tab so
/// subsequent navigations don't hijack the user's existing tabs.
pub(crate) async fn connect_auto_with_fresh_tab() -> Result<BrowserManager, String> {
    let mut mgr = BrowserManager::connect_auto().await?;
    mgr.tab_new(None, None).await?;
    let session_id = mgr.active_session_id()?.to_string();
    let _ = mgr
        .client
        .send_command("Page.bringToFront", None, Some(&session_id))
        .await;
    Ok(mgr)
}

pub(crate) async fn auto_launch(
    state: &mut DaemonState,
    plugins: Vec<crate::plugins::PluginConfig>,
) -> Result<(), String> {
    let mut options = launch_options_from_env();
    state.plugin_init_scripts.clear();

    // Use the stream server's viewport dimensions for --window-size so the
    // content area matches the desired viewport from the start.
    if let Some(ref server) = state.stream_server {
        options.viewport_size = Some(server.viewport().await);
    }
    let engine = env::var("AGENT_BROWSER_ENGINE").ok();

    // Extract storage_state before options is moved into BrowserManager::launch.
    let storage_state_path = options.storage_state.clone();

    // Store proxy credentials for Fetch.authRequired handling
    let has_proxy_auth = options.proxy_username.is_some();
    if has_proxy_auth {
        let mut creds = state.proxy_credentials.write().await;
        *creds = Some((
            options.proxy_username.clone().unwrap_or_default(),
            options.proxy_password.clone().unwrap_or_default(),
        ));
    }

    state.engine = engine.as_deref().unwrap_or("chrome").to_string();
    write_engine_file(&state.session_id, &state.engine);
    write_extensions_file(&state.session_id);

    if let Ok(cdp) = env::var("AGENT_BROWSER_CDP") {
        let mgr = BrowserManager::connect_cdp(&cdp).await?;
        state.reset_input_state();
        state.browser = Some(mgr);
        state.subscribe_to_browser_events();
        state.start_fetch_handler();
        state.start_dialog_handler();
        state.update_stream_client().await;
        apply_launch_init_scripts(state).await;
        try_auto_restore_state(state).await;
        try_load_storage_state(state, &storage_state_path).await;
        return Ok(());
    }

    if env::var("AGENT_BROWSER_AUTO_CONNECT").is_ok() {
        state.reset_input_state();
        state.browser = Some(connect_auto_with_fresh_tab().await?);
        state.subscribe_to_browser_events();
        state.start_fetch_handler();
        state.start_dialog_handler();
        state.update_stream_client().await;
        apply_launch_init_scripts(state).await;
        try_auto_restore_state(state).await;
        try_load_storage_state(state, &storage_state_path).await;
        return Ok(());
    }

    // Cloud provider: when AGENT_BROWSER_PROVIDER is set, connect via the
    // provider API instead of launching a local Chrome instance.  This mirrors
    // the logic in handle_launch() so that auto_launch (triggered by any
    // command arriving before an explicit "launch") honours the provider env.
    if let Ok(provider) = env::var("AGENT_BROWSER_PROVIDER") {
        let p = provider.to_lowercase();
        // ios/safari are device providers handled via explicit launch command
        if !p.is_empty() && p != "ios" && p != "safari" {
            let conn = providers::connect_provider_with_plugins(&p, &plugins).await?;
            let ws_headers = if p == "agentcore" {
                providers::take_agentcore_ws_headers()
            } else {
                None
            };
            let connect_result = if conn.direct_page {
                BrowserManager::connect_cdp_direct(&conn.ws_url).await
            } else if ws_headers.is_some() {
                BrowserManager::connect_cdp_with_headers(&conn.ws_url, ws_headers).await
            } else {
                BrowserManager::connect_cdp(&conn.ws_url).await
            };
            match connect_result {
                Ok(mgr) => {
                    state.reset_input_state();
                    state.browser = Some(mgr);
                    remember_active_provider_session(state, conn.session.clone(), &plugins);
                    state.subscribe_to_browser_events();
                    state.start_fetch_handler();
                    state.start_dialog_handler();
                    state.update_stream_client().await;
                    write_provider_file(&state.session_id, &p);
                    apply_launch_init_scripts(state).await;
                    try_auto_restore_state(state).await;
                    try_load_storage_state(state, &storage_state_path).await;
                    return Ok(());
                }
                Err(e) => {
                    if let Some(ref ps) = conn.session {
                        providers::close_provider_session_with_plugins(ps, &plugins).await;
                    }
                    return Err(format!("Provider '{}' connection failed: {}", p, e));
                }
            }
        }
    }

    apply_launch_mutator_plugins(state, &mut options, plugins).await?;
    write_extensions_file_from_paths(&state.session_id, options.extensions.as_deref());
    let hash = launch_hash(&options, &state.plugin_init_scripts);
    let mgr = BrowserManager::launch(options, engine.as_deref()).await?;
    state.reset_input_state();
    state.browser = Some(mgr);
    state.launch_hash = Some(hash);
    state.subscribe_to_browser_events();
    state.start_fetch_handler();
    state.start_dialog_handler();
    state.update_stream_client().await;

    // Enable Fetch with handleAuthRequests for proxy authentication
    if has_proxy_auth {
        if let Some(ref mgr) = state.browser {
            if let Ok(session_id) = mgr.active_session_id() {
                let _ = network::install_domain_filter_fetch(&mgr.client, session_id, true).await;
            }
        }
    }

    apply_launch_init_scripts(state).await;
    try_auto_restore_state(state).await;
    try_load_storage_state(state, &storage_state_path).await;
    Ok(())
}

/// Apply AGENT_BROWSER_ENABLE (built-in init scripts like `react-devtools`)
/// and AGENT_BROWSER_INIT_SCRIPTS (user-provided files) to the browser so the
/// scripts are registered before any page JS runs on the next navigation.
/// Also evaluates each script on the current page (if any) so the effect is
/// immediate for already-loaded pages.
async fn apply_launch_init_scripts(state: &DaemonState) {
    let Some(mgr) = state.browser.as_ref() else {
        return;
    };

    // Built-in features via --enable / AGENT_BROWSER_ENABLE.
    if let Ok(raw) = env::var("AGENT_BROWSER_ENABLE") {
        for feature in raw
            .split([',', '\n'])
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            match feature {
                "react-devtools" | "react" => {
                    let _ = mgr.add_script_to_evaluate(react::INSTALL_HOOK_JS).await;
                }
                other => {
                    eprintln!("warning: unknown --enable feature '{}'", other);
                }
            }
        }
    }

    // User init scripts via --init-script / AGENT_BROWSER_INIT_SCRIPTS.
    if let Ok(raw) = env::var("AGENT_BROWSER_INIT_SCRIPTS") {
        for path in raw
            .split([',', '\n'])
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            match fs::read_to_string(path) {
                Ok(source) => {
                    let _ = mgr.add_script_to_evaluate(&source).await;
                }
                Err(e) => {
                    eprintln!("warning: failed to read --init-script '{}': {}", path, e);
                }
            }
        }
    }

    for source in &state.plugin_init_scripts {
        let _ = mgr.add_script_to_evaluate(source).await;
    }
}

async fn apply_launch_mutator_plugins(
    state: &mut DaemonState,
    options: &mut LaunchOptions,
    plugins: Vec<crate::plugins::PluginConfig>,
) -> Result<(), String> {
    state.plugin_init_scripts.clear();
    if plugins.is_empty() {
        return Ok(());
    }

    let request = json!({
        "session": state.session_id,
        "launchOptions": {
            "headless": options.headless,
            "engine": env::var("AGENT_BROWSER_ENGINE").unwrap_or_else(|_| "chrome".to_string()),
            "args": options.args.clone(),
            "extensions": options.extensions.clone(),
            "userAgent": options.user_agent.clone(),
            "colorScheme": options.color_scheme.clone(),
            "downloadPath": options.download_path.clone(),
            "hideScrollbars": options.hide_scrollbars,
            "allowFileAccess": options.allow_file_access,
        }
    });

    for mutation in crate::plugins::launch_mutations_from_plugins(&plugins, request).await? {
        options.args.extend(mutation.args);
        if !mutation.extensions.is_empty() {
            options
                .extensions
                .get_or_insert_with(Vec::new)
                .extend(mutation.extensions);
        }
        if let Some(user_agent) = mutation.user_agent {
            options.user_agent = Some(user_agent);
        }
        state.plugin_init_scripts.extend(mutation.init_scripts);
    }

    Ok(())
}

pub(crate) fn launch_options_from_env() -> LaunchOptions {
    let headed = env::var("AGENT_BROWSER_HEADED")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    let extensions: Option<Vec<String>> = env::var("AGENT_BROWSER_EXTENSIONS").ok().map(|v| {
        v.split([',', '\n'])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });

    LaunchOptions {
        headless: !headed,
        executable_path: env::var("AGENT_BROWSER_EXECUTABLE_PATH").ok(),
        proxy: env::var("AGENT_BROWSER_PROXY").ok(),
        proxy_bypass: env::var("AGENT_BROWSER_PROXY_BYPASS").ok(),
        proxy_username: env::var("AGENT_BROWSER_PROXY_USERNAME").ok(),
        proxy_password: env::var("AGENT_BROWSER_PROXY_PASSWORD").ok(),
        profile: env::var("AGENT_BROWSER_PROFILE").ok(),
        allow_file_access: env::var("AGENT_BROWSER_ALLOW_FILE_ACCESS")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false),
        args: env::var("AGENT_BROWSER_ARGS")
            .map(|v| {
                v.split([',', '\n'])
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        extensions,
        storage_state: env::var("AGENT_BROWSER_STATE").ok(),
        user_agent: env::var("AGENT_BROWSER_USER_AGENT").ok(),
        ignore_https_errors: env::var("AGENT_BROWSER_IGNORE_HTTPS_ERRORS")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false),
        color_scheme: env::var("AGENT_BROWSER_COLOR_SCHEME").ok(),
        download_path: env::var("AGENT_BROWSER_DOWNLOAD_PATH").ok(),
        hide_scrollbars: hide_scrollbars_from_env(),
        viewport_size: None,
        use_real_keychain: false,
    }
}

fn hide_scrollbars_from_env() -> bool {
    env::var("AGENT_BROWSER_HIDE_SCROLLBARS")
        .map(|v| !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no" | ""))
        .unwrap_or(true)
}

pub(crate) fn hide_scrollbars_from_launch_cmd(cmd: &Value) -> bool {
    cmd.get("hideScrollbars")
        .and_then(|v| v.as_bool())
        .unwrap_or_else(hide_scrollbars_from_env)
}

async fn try_auto_restore_state(state: &mut DaemonState) {
    let session_name = match state.session_name.as_deref() {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => return,
    };
    if let Some(path) = state::find_auto_state_file(&session_name) {
        if let Some(ref mgr) = state.browser {
            if let Ok(session_id) = mgr.active_session_id() {
                let _ = state::load_state(&mgr.client, session_id, &path).await;
            }
        }
    }
}

/// Load storage state if a path is configured.
///
/// Explicit launch should surface this error. Best-effort callers can ignore
/// the returned `Result` and keep their previous behavior.
async fn load_storage_state(state: &DaemonState, path: &Option<String>) -> Result<(), String> {
    if let Some(ref path) = path {
        if let Some(ref mgr) = state.browser {
            if let Ok(session_id) = mgr.active_session_id() {
                state::load_state(&mgr.client, session_id, path).await?;
            }
        }
    }

    Ok(())
}

async fn rollback_failed_launch(state: &mut DaemonState) -> Result<(), String> {
    let close_result = close_current_browser(state).await;
    state.ref_map.clear();
    close_result
}

async fn load_storage_state_or_rollback(
    state: &mut DaemonState,
    path: &Option<String>,
) -> Result<(), String> {
    if let Err(err) = load_storage_state(state, path).await {
        if let Err(close_err) = rollback_failed_launch(state).await {
            return Err(format!(
                "{} (also failed to roll back browser after launch: {})",
                err, close_err
            ));
        }
        return Err(err);
    }

    Ok(())
}

/// Load storage state from AGENT_BROWSER_STATE if set.
async fn try_load_storage_state(state: &DaemonState, path: &Option<String>) {
    let _ = load_storage_state(state, path).await;
}

// ---------------------------------------------------------------------------
// Phase 1 handlers
// ---------------------------------------------------------------------------

pub(crate) async fn handle_launch(cmd: &Value, state: &mut DaemonState) -> Result<Value, String> {
    let headless = cmd
        .get("headless")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let cdp_url = cmd.get("cdpUrl").and_then(|v| v.as_str());
    let cdp_port = cmd.get("cdpPort").and_then(|v| v.as_u64());
    let auto_connect = cmd
        .get("autoConnect")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let provider_name = cmd.get("provider").and_then(|v| v.as_str());

    let extensions: Option<Vec<String>> =
        cmd.get("extensions").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });
    let storage_state = cmd.get("storageState").and_then(|v| v.as_str());
    let storage_state_owned = storage_state.map(|s| s.to_string());

    let mut launch_options = LaunchOptions {
        headless,
        executable_path: cmd
            .get("executablePath")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| env::var("AGENT_BROWSER_EXECUTABLE_PATH").ok()),
        proxy: cmd.get("proxy").and_then(|v| {
            v.as_str().map(|s| s.to_string()).or_else(|| {
                v.get("server")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
            })
        }),
        proxy_bypass: cmd
            .get("proxy")
            .and_then(|v| v.get("bypass"))
            .and_then(|v| v.as_str())
            .map(String::from),
        proxy_username: cmd
            .get("proxy")
            .and_then(|v| v.get("username"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| env::var("AGENT_BROWSER_PROXY_USERNAME").ok()),
        proxy_password: cmd
            .get("proxy")
            .and_then(|v| v.get("password"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| env::var("AGENT_BROWSER_PROXY_PASSWORD").ok()),
        profile: cmd
            .get("profile")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        allow_file_access: cmd
            .get("allowFileAccess")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        args: cmd
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        extensions,
        storage_state: storage_state.map(String::from),
        user_agent: cmd
            .get("userAgent")
            .and_then(|v| v.as_str())
            .map(String::from),
        ignore_https_errors: cmd
            .get("ignoreHTTPSErrors")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        color_scheme: cmd
            .get("colorScheme")
            .and_then(|v| v.as_str())
            .map(String::from),
        download_path: cmd
            .get("downloadPath")
            .and_then(|v| v.as_str())
            .map(String::from),
        hide_scrollbars: hide_scrollbars_from_launch_cmd(cmd),
        viewport_size: None,
        use_real_keychain: false,
    };

    state.plugin_init_scripts.clear();
    let local_launch =
        cdp_url.is_none() && cdp_port.is_none() && !auto_connect && provider_name.is_none();
    if local_launch {
        apply_launch_mutator_plugins(state, &mut launch_options, plugins_from_command_or_env(cmd))
            .await?;
    }

    let new_hash = launch_hash(&launch_options, &state.plugin_init_scripts);

    // Hash comparison and fast process-exit check are evaluated before the
    // async is_connection_alive to skip the expensive CDP liveness probe
    // when a relaunch is already certain.
    let needs_relaunch = if let Some(ref mut mgr) = state.browser {
        let is_external = cdp_url.is_some() || cdp_port.is_some() || auto_connect;
        let was_external = mgr.is_cdp_connection();
        let hash_changed = !is_external && state.launch_hash != Some(new_hash);
        let storage_state_requires_clean_launch = storage_state_owned.is_some() && !is_external;
        is_external != was_external
            || hash_changed
            || storage_state_requires_clean_launch
            || mgr.has_process_exited()
            || !mgr.is_connection_alive().await
    } else {
        true
    };

    if needs_relaunch {
        if state.browser.is_some() || state.active_provider_session.is_some() {
            close_current_browser(state).await?;
        }
    } else {
        load_storage_state(state, &storage_state_owned).await?;
        return Ok(json!({ "launched": true, "reused": true }));
    }
    state.ref_map.clear();

    let has_cdp = cdp_url.is_some() || cdp_port.is_some();
    crate::native::browser::validate_launch_options(
        launch_options.extensions.as_deref(),
        has_cdp,
        launch_options.profile.as_deref(),
        storage_state,
        launch_options.allow_file_access,
        launch_options.executable_path.as_deref(),
    )?;

    if let Some(url) = cdp_url {
        state.reset_input_state();
        state.browser = Some(BrowserManager::connect_cdp(url).await?);
        state.subscribe_to_browser_events();
        state.start_fetch_handler();
        state.start_dialog_handler();
        state.update_stream_client().await;
        load_storage_state_or_rollback(state, &storage_state_owned).await?;
        apply_launch_init_scripts(state).await;
        return Ok(json!({ "launched": true }));
    }

    if let Some(port) = cdp_port {
        state.reset_input_state();
        state.browser = Some(BrowserManager::connect_cdp(&port.to_string()).await?);
        state.subscribe_to_browser_events();
        state.start_fetch_handler();
        state.start_dialog_handler();
        state.update_stream_client().await;
        load_storage_state_or_rollback(state, &storage_state_owned).await?;
        apply_launch_init_scripts(state).await;
        return Ok(json!({ "launched": true }));
    }

    if auto_connect {
        state.reset_input_state();
        state.browser = Some(connect_auto_with_fresh_tab().await?);
        state.subscribe_to_browser_events();
        state.start_fetch_handler();
        state.start_dialog_handler();
        state.update_stream_client().await;
        load_storage_state_or_rollback(state, &storage_state_owned).await?;
        apply_launch_init_scripts(state).await;
        return Ok(json!({ "launched": true }));
    }

    if let Some(provider) = provider_name {
        match provider.to_lowercase().as_str() {
            "ios" => {
                return launch_ios(cmd, state).await;
            }
            "safari" => {
                return launch_safari(cmd, state).await;
            }
            _ => {
                let command_plugins = plugins_from_command_or_env(cmd);
                let conn = providers::connect_provider_with_plugins_and_options(
                    provider,
                    &command_plugins,
                    Some(provider_plugin_launch_options_from_command(cmd)),
                )
                .await?;
                let provider_metadata = conn.metadata.clone();

                let ws_headers = if provider.eq_ignore_ascii_case("agentcore") {
                    providers::take_agentcore_ws_headers()
                } else {
                    None
                };

                let connect_result = if conn.direct_page {
                    BrowserManager::connect_cdp_direct(&conn.ws_url).await
                } else if ws_headers.is_some() {
                    BrowserManager::connect_cdp_with_headers(&conn.ws_url, ws_headers).await
                } else {
                    BrowserManager::connect_cdp(&conn.ws_url).await
                };
                match connect_result {
                    Ok(mgr) => {
                        state.reset_input_state();
                        state.browser = Some(mgr);
                        remember_active_provider_session(
                            state,
                            conn.session.clone(),
                            &command_plugins,
                        );
                        state.subscribe_to_browser_events();
                        state.start_fetch_handler();
                        state.start_dialog_handler();
                        state.update_stream_client().await;
                        write_provider_file(&state.session_id, provider);
                        load_storage_state_or_rollback(state, &storage_state_owned).await?;
                        apply_launch_init_scripts(state).await;

                        if let Some(info) = providers::get_agentcore_info() {
                            return Ok(json!({
                                "launched": true,
                                "provider": provider,
                                "agentCoreSessionId": info.session_id,
                                "agentCoreLiveViewUrl": info.live_view_url
                            }));
                        }

                        if let Some(metadata) = provider_metadata {
                            return Ok(json!({
                                "launched": true,
                                "provider": provider,
                                "providerMetadata": metadata
                            }));
                        }

                        return Ok(json!({ "launched": true, "provider": provider }));
                    }
                    Err(e) => {
                        if let Some(ref ps) = conn.session {
                            providers::close_provider_session_with_plugins(ps, &command_plugins)
                                .await;
                        }
                        return Err(e);
                    }
                }
            }
        }
    }

    let engine = cmd
        .get("engine")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| env::var("AGENT_BROWSER_ENGINE").ok());

    // Store proxy credentials for Fetch.authRequired handling
    let has_proxy_auth = launch_options.proxy_username.is_some();
    if has_proxy_auth {
        let mut creds = state.proxy_credentials.write().await;
        *creds = Some((
            launch_options.proxy_username.clone().unwrap_or_default(),
            launch_options.proxy_password.clone().unwrap_or_default(),
        ));
    }

    if let Some(ref domains) = cmd
        .get("allowedDomains")
        .and_then(|v| v.as_str())
        .map(String::from)
    {
        let mut df = state.domain_filter.write().await;
        *df = Some(DomainFilter::new(domains));
    }

    state.engine = engine.as_deref().unwrap_or("chrome").to_string();
    write_engine_file(&state.session_id, &state.engine);
    write_extensions_file_from_paths(&state.session_id, launch_options.extensions.as_deref());
    state.reset_input_state();
    state.browser = Some(BrowserManager::launch(launch_options, engine.as_deref()).await?);
    state.launch_hash = Some(new_hash);
    state.subscribe_to_browser_events();
    state.start_fetch_handler();
    state.start_dialog_handler();
    state.update_stream_client().await;

    // Enable Fetch interception (domain filtering and/or proxy auth).
    // Only call Fetch.enable once to avoid overwriting handleAuthRequests.
    {
        let df = state.domain_filter.read().await;
        let has_domain_filter = df.is_some();

        if has_domain_filter || has_proxy_auth {
            if let Some(ref mgr) = state.browser {
                if let Ok(session_id) = mgr.active_session_id() {
                    if let Some(ref filter) = *df {
                        let _ = network::install_domain_filter(
                            &mgr.client,
                            session_id,
                            &filter.allowed_domains,
                            has_proxy_auth,
                        )
                        .await;
                        network::sanitize_existing_pages(&mgr.client, &mgr.pages_list(), filter)
                            .await;
                    } else {
                        // No domain filter, but proxy auth needs Fetch.enable
                        let _ = network::install_domain_filter_fetch(
                            &mgr.client,
                            session_id,
                            has_proxy_auth,
                        )
                        .await;
                    }
                }
            }
        }
    }

    // Load storage state only after Fetch interception is active so replayed
    // origin navigations go through the same domain and proxy handling as
    // normal browser traffic.
    load_storage_state_or_rollback(state, &storage_state_owned).await?;

    apply_launch_init_scripts(state).await;

    Ok(json!({ "launched": true }))
}

pub(crate) async fn launch_ios(cmd: &Value, state: &mut DaemonState) -> Result<Value, String> {
    let device_name = cmd.get("deviceName").and_then(|v| v.as_str());
    let device_udid = cmd.get("udid").and_then(|v| v.as_str());
    let platform_version = cmd.get("platformVersion").and_then(|v| v.as_str());

    // Select device (or use default)
    let device = ios::select_device(device_name, device_udid)?;

    // Boot simulator if it's not real and not already booted
    if !device.is_real && device.state != "Booted" {
        ios::boot_simulator(&device.udid)?;
    }

    // Start Appium
    let mut appium = AppiumManager::connect_or_launch(Some(&device.udid)).await?;

    // Create iOS Safari session
    appium
        .create_ios_session(Some(&device.name), platform_version)
        .await?;

    // Create a WebDriverBackend from the Appium session for common commands
    if let Some(sid) = appium.client.session_id_pub().map(String::from) {
        let wd_client =
            crate::native::webdriver::client::WebDriverClient::new_with_session(4723, sid);
        state.webdriver_backend = Some(WebDriverBackend::new(wd_client));
    }

    state.appium = Some(appium);
    state.backend_type = BackendType::WebDriver;
    state.engine = "safari".to_string();
    write_engine_file(&state.session_id, &state.engine);
    write_provider_file(&state.session_id, "ios");
    write_extensions_file(&state.session_id);
    state.reset_input_state();

    Ok(json!({
        "launched": true,
        "provider": "ios",
        "device": device.name,
        "udid": device.udid,
        "backend": "webdriver",
    }))
}

pub(crate) async fn launch_safari(cmd: &Value, state: &mut DaemonState) -> Result<Value, String> {
    let port: u16 = cmd
        .get("port")
        .and_then(|v| v.as_u64())
        .map(|p| p as u16)
        .unwrap_or(0);
    let driver_port = if port > 0 { port } else { 0 };

    // Find a free port if none specified
    let actual_port = if driver_port > 0 {
        driver_port
    } else {
        // Use any available high port
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("Failed to find free port: {}", e))?;
        listener
            .local_addr()
            .map_err(|e| format!("Failed to get local address: {}", e))?
            .port()
    };

    let driver = safari::launch_safaridriver(actual_port)?;
    let mut client = crate::native::webdriver::client::WebDriverClient::new(actual_port);

    client
        .create_session(serde_json::json!({
            "browserName": "safari",
        }))
        .await?;

    state.safari_driver = Some(driver);
    state.webdriver_backend = Some(WebDriverBackend::new(client));
    state.backend_type = BackendType::WebDriver;
    state.engine = "safari".to_string();
    write_engine_file(&state.session_id, &state.engine);
    write_provider_file(&state.session_id, "safari");
    write_extensions_file(&state.session_id);
    state.reset_input_state();

    Ok(json!({
        "launched": true,
        "provider": "safari",
        "port": actual_port,
        "backend": "webdriver",
    }))
}
