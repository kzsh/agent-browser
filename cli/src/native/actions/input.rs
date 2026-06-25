use serde_json::{json, Value};

use crate::native::cdp::types::DispatchMouseEventParams;

use super::{browser_ctx, req_str};
use super::{DaemonState, MouseState};

// ---------------------------------------------------------------------------
// iOS handlers (stub)
// ---------------------------------------------------------------------------

pub(crate) async fn handle_swipe(cmd: &Value, state: &mut DaemonState) -> Result<Value, String> {
    // Route through Appium for iOS/WebDriver
    if let Some(ref appium) = state.appium {
        if state.browser.is_none() {
            let start_x = cmd.get("startX").and_then(|v| v.as_f64()).unwrap_or(200.0);
            let start_y = cmd.get("startY").and_then(|v| v.as_f64()).unwrap_or(400.0);
            let end_x = cmd.get("endX").and_then(|v| v.as_f64()).unwrap_or(200.0);
            let end_y = cmd.get("endY").and_then(|v| v.as_f64()).unwrap_or(100.0);

            if let Some(direction) = cmd.get("direction").and_then(|v| v.as_str()) {
                let distance = cmd
                    .get("distance")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(300.0);
                let (dx, dy) = match direction {
                    "up" => (0.0, -distance),
                    "down" => (0.0, distance),
                    "left" => (-distance, 0.0),
                    "right" => (distance, 0.0),
                    _ => (0.0, -distance),
                };
                let actual_end_x = start_x + dx;
                let actual_end_y = start_y + dy;
                let duration = cmd.get("duration").and_then(|v| v.as_u64()).unwrap_or(800);
                appium
                    .swipe(start_x, start_y, actual_end_x, actual_end_y, duration)
                    .await?;
                return Ok(json!({ "swiped": direction }));
            }

            let duration = cmd.get("duration").and_then(|v| v.as_u64()).unwrap_or(800);
            appium
                .swipe(start_x, start_y, end_x, end_y, duration)
                .await?;
            return Ok(json!({ "swiped": true, "from": [start_x, start_y], "to": [end_x, end_y] }));
        }
    }

    let (mgr, session_id) = browser_ctx(state)?;

    let start_x = cmd.get("startX").and_then(|v| v.as_f64()).unwrap_or(200.0);
    let start_y = cmd.get("startY").and_then(|v| v.as_f64()).unwrap_or(400.0);
    let end_x = cmd.get("endX").and_then(|v| v.as_f64()).unwrap_or(200.0);
    let end_y = cmd.get("endY").and_then(|v| v.as_f64()).unwrap_or(100.0);

    if let Some(direction) = cmd.get("direction").and_then(|v| v.as_str()) {
        let distance = cmd
            .get("distance")
            .and_then(|v| v.as_f64())
            .unwrap_or(300.0);
        let (dx, dy) = match direction {
            "up" => (0.0, -distance),
            "down" => (0.0, distance),
            "left" => (-distance, 0.0),
            "right" => (distance, 0.0),
            _ => (0.0, -distance),
        };
        let cx = start_x;
        let cy = start_y;

        mgr.client
            .send_command(
                "Input.dispatchTouchEvent",
                Some(json!({ "type": "touchStart", "touchPoints": [{ "x": cx, "y": cy }] })),
                Some(&session_id),
            )
            .await?;

        let steps = 10;
        for i in 1..=steps {
            let x = cx + dx * (i as f64) / (steps as f64);
            let y = cy + dy * (i as f64) / (steps as f64);
            mgr.client
                .send_command(
                    "Input.dispatchTouchEvent",
                    Some(json!({ "type": "touchMove", "touchPoints": [{ "x": x, "y": y }] })),
                    Some(&session_id),
                )
                .await?;
            tokio::time::sleep(tokio::time::Duration::from_millis(16)).await;
        }

        mgr.client
            .send_command(
                "Input.dispatchTouchEvent",
                Some(json!({ "type": "touchEnd", "touchPoints": [] })),
                Some(&session_id),
            )
            .await?;

        return Ok(json!({ "swiped": direction }));
    }

    // Manual coordinates
    mgr.client
        .send_command(
            "Input.dispatchTouchEvent",
            Some(json!({ "type": "touchStart", "touchPoints": [{ "x": start_x, "y": start_y }] })),
            Some(&session_id),
        )
        .await?;

    let steps = 10;
    for i in 1..=steps {
        let x = start_x + (end_x - start_x) * (i as f64) / (steps as f64);
        let y = start_y + (end_y - start_y) * (i as f64) / (steps as f64);
        mgr.client
            .send_command(
                "Input.dispatchTouchEvent",
                Some(json!({ "type": "touchMove", "touchPoints": [{ "x": x, "y": y }] })),
                Some(&session_id),
            )
            .await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(16)).await;
    }

    mgr.client
        .send_command(
            "Input.dispatchTouchEvent",
            Some(json!({ "type": "touchEnd", "touchPoints": [] })),
            Some(&session_id),
        )
        .await?;

    Ok(json!({ "swiped": true, "from": [start_x, start_y], "to": [end_x, end_y] }))
}

pub(crate) async fn handle_device_list() -> Result<Value, String> {
    #[cfg(target_os = "macos")]
    {
        use crate::native::webdriver::ios;
        let devices = ios::list_all_devices()?;
        Ok(ios::to_device_json(&devices))
    }

    #[cfg(not(target_os = "macos"))]
    {
        Err("device_list is only available on macOS with Xcode".to_string())
    }
}

// ---------------------------------------------------------------------------
// Input event handlers
// ---------------------------------------------------------------------------

fn mouse_button_mask(button: &str) -> i32 {
    match button {
        "left" => 1,
        "right" => 2,
        "middle" => 4,
        "back" => 8,
        "forward" => 16,
        _ => 0,
    }
}

fn primary_button_from_mask(buttons: i32) -> &'static str {
    if buttons & 1 != 0 {
        "left"
    } else if buttons & 2 != 0 {
        "right"
    } else if buttons & 4 != 0 {
        "middle"
    } else if buttons & 8 != 0 {
        "back"
    } else if buttons & 16 != 0 {
        "forward"
    } else {
        "none"
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_mouse_event_params(
    mouse_state: &mut MouseState,
    event_type: &str,
    x: Option<f64>,
    y: Option<f64>,
    button: Option<&str>,
    buttons: Option<i32>,
    click_count: Option<i32>,
    delta_x: Option<f64>,
    delta_y: Option<f64>,
    modifiers: Option<i32>,
) -> DispatchMouseEventParams {
    let x = x.unwrap_or(mouse_state.x);
    let y = y.unwrap_or(mouse_state.y);
    mouse_state.x = x;
    mouse_state.y = y;

    let mut next_buttons = buttons.unwrap_or(mouse_state.buttons);
    if buttons.is_none() {
        match event_type {
            "mousePressed" => {
                next_buttons |= mouse_button_mask(button.unwrap_or("left"));
            }
            "mouseReleased" => {
                next_buttons &= !mouse_button_mask(button.unwrap_or("left"));
            }
            _ => {}
        }
    }
    mouse_state.buttons = next_buttons;

    DispatchMouseEventParams {
        event_type: event_type.to_string(),
        x,
        y,
        button: Some(
            button
                .unwrap_or(primary_button_from_mask(next_buttons))
                .to_string(),
        ),
        buttons: Some(next_buttons),
        click_count,
        delta_x,
        delta_y,
        modifiers,
    }
}

pub(crate) async fn handle_input_mouse(
    cmd: &Value,
    state: &mut DaemonState,
) -> Result<Value, String> {
    let mgr = state.browser.as_ref().ok_or("Browser not launched")?;
    let session_id = mgr.active_session_id()?.to_string();
    let event_type = cmd
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("mouseMoved");
    let params = build_mouse_event_params(
        &mut state.mouse_state,
        event_type,
        cmd.get("x").and_then(|v| v.as_f64()),
        cmd.get("y").and_then(|v| v.as_f64()),
        cmd.get("button").and_then(|v| v.as_str()),
        cmd.get("buttons")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32),
        cmd.get("clickCount")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32),
        cmd.get("deltaX").and_then(|v| v.as_f64()),
        cmd.get("deltaY").and_then(|v| v.as_f64()),
        cmd.get("modifiers")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32),
    );

    mgr.client
        .send_command_typed::<_, Value>("Input.dispatchMouseEvent", &params, Some(&session_id))
        .await?;
    Ok(json!({ "dispatched": event_type }))
}

pub(crate) async fn handle_input_keyboard(
    cmd: &Value,
    state: &DaemonState,
) -> Result<Value, String> {
    let (mgr, session_id) = browser_ctx(state)?;
    let event_type = cmd
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("keyDown");

    let mut params = json!({ "type": event_type });
    for key in &["key", "code", "text"] {
        if let Some(v) = cmd.get(*key) {
            params[*key] = v.clone();
        }
    }

    mgr.client
        .send_command("Input.dispatchKeyEvent", Some(params), Some(&session_id))
        .await?;
    Ok(json!({ "dispatched": event_type }))
}

pub(crate) async fn handle_input_touch(cmd: &Value, state: &DaemonState) -> Result<Value, String> {
    let (mgr, session_id) = browser_ctx(state)?;
    let event_type = cmd
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("touchStart");

    mgr.client
        .send_command(
            "Input.dispatchTouchEvent",
            Some(json!({
                "type": event_type,
                "touchPoints": cmd.get("touchPoints").unwrap_or(&json!([])),
            })),
            Some(&session_id),
        )
        .await?;
    Ok(json!({ "dispatched": event_type }))
}

pub(crate) async fn handle_keydown(cmd: &Value, state: &DaemonState) -> Result<Value, String> {
    let (mgr, session_id) = browser_ctx(state)?;
    let key = req_str(cmd, "key")?;

    mgr.client
        .send_command(
            "Input.dispatchKeyEvent",
            Some(json!({ "type": "keyDown", "key": key })),
            Some(&session_id),
        )
        .await?;
    Ok(json!({ "keydown": key }))
}

pub(crate) async fn handle_keyup(cmd: &Value, state: &DaemonState) -> Result<Value, String> {
    let (mgr, session_id) = browser_ctx(state)?;
    let key = req_str(cmd, "key")?;

    mgr.client
        .send_command(
            "Input.dispatchKeyEvent",
            Some(json!({ "type": "keyUp", "key": key })),
            Some(&session_id),
        )
        .await?;
    Ok(json!({ "keyup": key }))
}

pub(crate) async fn handle_inserttext(cmd: &Value, state: &DaemonState) -> Result<Value, String> {
    let (mgr, session_id) = browser_ctx(state)?;
    let text = req_str(cmd, "text")?;

    mgr.client
        .send_command(
            "Input.insertText",
            Some(json!({ "text": text })),
            Some(&session_id),
        )
        .await?;
    Ok(json!({ "inserted": true }))
}

pub(crate) async fn handle_mousemove(
    cmd: &Value,
    state: &mut DaemonState,
) -> Result<Value, String> {
    let mgr = state.browser.as_ref().ok_or("Browser not launched")?;
    let session_id = mgr.active_session_id()?.to_string();
    let x = cmd.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let y = cmd.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let params = build_mouse_event_params(
        &mut state.mouse_state,
        "mouseMoved",
        Some(x),
        Some(y),
        None,
        None,
        None,
        None,
        None,
        None,
    );

    mgr.client
        .send_command_typed::<_, Value>("Input.dispatchMouseEvent", &params, Some(&session_id))
        .await?;
    Ok(json!({ "moved": true }))
}

pub(crate) async fn handle_mousedown(
    cmd: &Value,
    state: &mut DaemonState,
) -> Result<Value, String> {
    let mgr = state.browser.as_ref().ok_or("Browser not launched")?;
    let session_id = mgr.active_session_id()?.to_string();
    let button = cmd.get("button").and_then(|v| v.as_str()).unwrap_or("left");
    let params = build_mouse_event_params(
        &mut state.mouse_state,
        "mousePressed",
        None,
        None,
        Some(button),
        None,
        Some(1),
        None,
        None,
        None,
    );

    mgr.client
        .send_command_typed::<_, Value>("Input.dispatchMouseEvent", &params, Some(&session_id))
        .await?;
    Ok(json!({ "pressed": true }))
}

pub(crate) async fn handle_mouseup(cmd: &Value, state: &mut DaemonState) -> Result<Value, String> {
    let mgr = state.browser.as_ref().ok_or("Browser not launched")?;
    let session_id = mgr.active_session_id()?.to_string();
    let button = cmd.get("button").and_then(|v| v.as_str()).unwrap_or("left");
    let params = build_mouse_event_params(
        &mut state.mouse_state,
        "mouseReleased",
        None,
        None,
        Some(button),
        None,
        Some(1),
        None,
        None,
        None,
    );

    mgr.client
        .send_command_typed::<_, Value>("Input.dispatchMouseEvent", &params, Some(&session_id))
        .await?;
    Ok(json!({ "released": true }))
}
