#![cfg_attr(feature = "no_console", windows_subsystem = "windows")]
#![allow(unused_labels)]

use anyhow::{Context};
use serde::de::DeserializeOwned;
use serde_json::value::Index;
use serde_json::Value;
use std::net::TcpStream;
use tray_item::{IconSource, TrayItem};
use tungstenite::http::Uri;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message, WebSocket};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut tray: TrayItem = TrayItem::new(
        "GAT - GlazeWM Alternating Tiler",
        IconSource::Resource("main-icon"),
    )?;
    tray.add_label("GAT - GlazeWM Alternating Tiler")?;
    tray.add_menu_item("Quit GAT", || std::process::exit(0))?;

    let (mut socket, _) = connect(
        "ws://localhost:6123"
            .parse::<Uri>()
            .context("Failed to parse GWM WS URL")?,
    )
    .context("Failed to connect to GWM WS")?;

    socket
        .send(Message::Text(r#"sub -e focus_changed"#.into()))
        .context("Failed to subscribe to focus_changed event")?;

    socket
        .send(Message::Text(r#"sub -e focused_container_moved"#.into()))
        .context("Failed to subscribe to container moved")?;
    
    socket
        .send(Message::Text(r#"sub -e application_exiting"#.into()))
        .context("Failed to subscribe to container moved")?;
    
    loop {
        let event = match read_as::<Value>(&mut socket) {
            Err(e) => {
                return Err(e);
            }
            Ok(Some(value)) => value,
            Ok(None) => continue,
        };

        let event_type = event.get_path(["data", "eventType"]);
        
        match event_type.and_then(|v| v.as_str()) {
            Some("focused_container_moved") => {
                _ = handle_focused_container_moved(event, &mut socket).inspect_err(|e| {
                    eprintln!("Failed to handle focused container moved event: {e}")
                });
            }
            Some("focus_changed") => {
                _ = handle_focus_changed(event, &mut socket)
                    .inspect_err(|e| eprintln!("Failed to handle focus changed event: {e}"))
            }
            Some("application_exiting") => {
                eprintln!("GlazeWM is exiting, exiting too.");
                std::process::exit(0);
            }
            _ => continue,
        }
    }
}

fn handle_focused_container_moved(
    event: Value,
    web_socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
) -> anyhow::Result<()> {
    let root_container = event
        .get_path(["data", "focusedContainer"])
        .context("Expected focused container event to contain a focused container field")?;

    fn find_focused_window(container: &Value) -> Option<&Value> {
        let container_type = container.get_path(["type"]).and_then(|v| v.as_str())?;
        let has_focus = container.get_path(["hasFocus"]).and_then(|v| v.as_bool())?;

        // Termination case: focused window found
        if container_type == "window" && has_focus {
            return Some(container);
        }

        let children = container.get("children").and_then(|v| v.as_array())?;

        // Recursive case: search through children
        for child in children {
            if let Some(focused_container) = find_focused_window(child) {
                return Some(focused_container);
            }
        }

        // Termination case: No window with focus
        None
    }

    if let Some(focused_window) = find_focused_window(root_container) {
        // Only react to genuinely tiled, shown windows. A minimized/floating/
        // fullscreen window (or one mid-transition) reports stale dimensions
        // that don't represent the tile it occupies, which would set the wrong
        // tiling direction.
        if is_tiling_shown(focused_window) {
            let (width, height) = get_container_size(&focused_window)
                .context("focused container did not have a width or height")?;
            change_tiling_direction(web_socket, width, height)?;
        }
    }

    Ok(())
}

fn handle_focus_changed(
    event: Value,
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
) -> anyhow::Result<()> {
    let focused_container = event
        .get_path(["data", "focusedContainer"])
        .context("Expected focus changed event to contain a focused container field")?;

    // Skip windows that aren't tiled and fully shown. Minimized windows in
    // particular keep stale width/height from before they were minimized, so
    // reading them here would set the wrong tiling direction and cause newly
    // restored/opened windows to split the wrong way (e.g. side-by-side
    // instead of stacked).
    if !is_tiling_shown(focused_container) {
        return Ok(());
    }

    let (width, height) = get_container_size(focused_container)
        .context("focused container did not have a width or height")?;

    change_tiling_direction(socket, width, height)?;

    Ok(())
}

/// Returns true only for windows that are actively part of the tiling layout
/// and fully shown. Their reported width/height reflect the tile they occupy,
/// which is what the tiling-direction heuristic relies on.
fn is_tiling_shown(container: &Value) -> bool {
    let state_type = container
        .get_path(["state", "type"])
        .and_then(|v| v.as_str());
    let display_state = container
        .get_path(["displayState"])
        .and_then(|v| v.as_str());

    state_type == Some("tiling") && display_state == Some("shown")
}

fn get_container_size(event: &Value) -> Option<(f64, f64)> {
    let width = event.get("width").and_then(|v| v.as_f64())?;
    let height = event.get("height").and_then(|v| v.as_f64())?;

    Some((width, height))
}

fn change_tiling_direction(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    window_width: f64,
    window_height: f64,
) -> anyhow::Result<()> {
    if window_width < window_height {
        socket
            .send(Message::Text(
                "command set-tiling-direction vertical".into(),
            ))
            .context("Failed to send message to GWM")?;
    }
    if window_width > window_height {
        socket
            .send(Message::Text(
                "command set-tiling-direction horizontal".into(),
            ))
            .context("Failed to send message to GWM")?;
    };

    Ok(())
}

fn read_as<T: DeserializeOwned>(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>) -> anyhow::Result<Option<T>> {
    let msg = match socket.read() {
        Ok(msg) => msg,
        Err(err) => {
            return Err(err).context("Failed to read message from GWM socket");
        }
    };

    let text = match msg.to_text() {
        Ok(text) => text,
        Err(err) => {
            eprintln!("Error while converting message to text: {err}");
            return Ok(None);
        }
    };

    let json_msg = match serde_json::from_str(text) {
        Ok(msg) => msg,
        Err(err) => {
            eprintln!("Error while parsing message as json: {err}");
            return Ok(None);
        }
    };

    Ok(Some(json_msg))
}

trait JsonValueExt {
    /// Retrieves a nested value based on the provided path of keys.
    ///
    /// # Arguments
    /// * `path` - An iterable of string keys specifying the nested path.
    ///
    /// # Returns
    /// * `Option<&Value>` - The nested value if found, otherwise `None`.
    fn get_path<T: IntoIterator<Item = I>, I: Index>(&self, path: T) -> Option<&Value>;
}

impl JsonValueExt for Value {
    fn get_path<T: IntoIterator<Item = I>, I: Index>(&self, path: T) -> Option<&Value> {
        path.into_iter()
            .fold(Some(self), |acc, key| acc.and_then(|v| v.get(key)))
    }
}
