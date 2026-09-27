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

/// Message for querying the workspaces, including their container trees.
const QUERY_WORKSPACES: &str = "query workspaces";

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

    // Subscribe to all events through a single subscription. GlazeWM
    // delivers each subscription independently, so events from separate
    // subscriptions can arrive out of order.
    socket
        .send(Message::Text(
            r#"sub -e focus_changed focused_container_moved window_managed window_unmanaged application_exiting"#.into(),
        ))
        .context("Failed to subscribe to GlazeWM events")?;

    // Set the tiling directions of the windows that are already open.
    query_workspaces(&mut socket)?;

    loop {
        let message = match read_as::<Value>(&mut socket) {
            Err(e) => {
                return Err(e);
            }
            Ok(Some(value)) => value,
            Ok(None) => continue,
        };

        match message.get_path(["messageType"]).and_then(|v| v.as_str()) {
            Some("event_subscription") => {
                let event_type = message.get_path(["data", "eventType"]);

                if event_type.and_then(|v| v.as_str()) == Some("application_exiting") {
                    eprintln!("GlazeWM is exiting, exiting too.");
                    std::process::exit(0);
                }

                // The other subscribed events can all change the size of
                // windows, so re-query the layout to update the tiling
                // directions from. The query is answered with the state at
                // the time it's handled, so it can't be outdated by events
                // that arrive out of order.
                _ = query_workspaces(&mut socket)
                    .inspect_err(|e| eprintln!("Failed to query workspaces: {e}"));
            }
            Some("client_response") => {
                let client_message = message.get_path(["clientMessage"]);

                if client_message.and_then(|v| v.as_str()) == Some(QUERY_WORKSPACES) {
                    _ = handle_workspaces_response(message, &mut socket).inspect_err(|e| {
                        eprintln!("Failed to handle workspaces response: {e}")
                    });
                }
            }
            _ => continue,
        }
    }
}

fn query_workspaces(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>) -> anyhow::Result<()> {
    socket
        .send(Message::Text(QUERY_WORKSPACES.into()))
        .context("Failed to send message to GWM")
}

/// Sets the tiling direction of every tiling window in the workspaces from
/// a workspaces query response.
///
/// All windows are updated rather than only the focused one, since the
/// tiling direction of a window also decides what happens when another
/// window is moved into it. E.g. a window that is moved into a tall
/// neighbour is only stacked with it if the neighbour is set to vertical,
/// otherwise the two are swapped.
fn handle_workspaces_response(
    response: Value,
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
) -> anyhow::Result<()> {
    let workspaces = response
        .get_path(["data", "workspaces"])
        .and_then(|v| v.as_array())
        .context("Expected workspaces response to contain a workspaces field")?;

    let mut updates = Vec::new();

    for workspace in workspaces {
        collect_tiling_direction_updates(workspace, &mut updates)?;
    }

    for (window_id, tiling_direction) in updates {
        set_tiling_direction(socket, window_id, tiling_direction)?;
    }

    Ok(())
}

/// Recursively collects the tiling windows within the given workspace or
/// split container whose tiling direction doesn't match their size.
///
/// Each update is a tuple of the window ID and its new tiling direction.
fn collect_tiling_direction_updates<'a>(
    container: &'a Value,
    updates: &mut Vec<(&'a str, &'static str)>,
) -> anyhow::Result<()> {
    // The tiling direction of a tiling window is the tiling direction of its
    // parent workspace or split container.
    let tiling_direction = container.get("tilingDirection").and_then(|v| v.as_str());

    let children = container
        .get("children")
        .and_then(|v| v.as_array())
        .context("Expected container to contain a children field")?;

    for child in children {
        match child.get("type").and_then(|v| v.as_str()) {
            Some("split") => collect_tiling_direction_updates(child, updates)?,
            // Floating, fullscreen, and minimized windows aren't part of the
            // tiling layout, so they're skipped.
            Some("window") if is_tiling(child) => {
                let (width, height) = get_container_size(child)
                    .context("window did not have a width or height")?;

                let Some(target_direction) = tiling_direction_for_size(width, height) else {
                    continue;
                };

                if tiling_direction != Some(target_direction) {
                    let window_id = child
                        .get("id")
                        .and_then(|v| v.as_str())
                        .context("window did not have an id")?;

                    updates.push((window_id, target_direction));
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn is_tiling(window: &Value) -> bool {
    window.get_path(["state", "type"]).and_then(|v| v.as_str()) == Some("tiling")
}

fn get_container_size(event: &Value) -> Option<(f64, f64)> {
    let width = event.get("width").and_then(|v| v.as_f64())?;
    let height = event.get("height").and_then(|v| v.as_f64())?;

    Some((width, height))
}

/// Gets the tiling direction that splits a window along its longest side.
///
/// Returns `None` for square windows.
fn tiling_direction_for_size(window_width: f64, window_height: f64) -> Option<&'static str> {
    if window_width < window_height {
        Some("vertical")
    } else if window_width > window_height {
        Some("horizontal")
    } else {
        None
    }
}

fn set_tiling_direction(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    window_id: &str,
    tiling_direction: &str,
) -> anyhow::Result<()> {
    socket
        .send(Message::Text(
            format!("command --id {window_id} set-tiling-direction {tiling_direction}").into(),
        ))
        .context("Failed to send message to GWM")
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

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::collect_tiling_direction_updates;

    fn window(id: &str, width: u32, height: u32, state: &str) -> Value {
        json!({
            "type": "window",
            "id": id,
            "width": width,
            "height": height,
            "state": { "type": state },
        })
    }

    fn container(kind: &str, tiling_direction: &str, children: Vec<Value>) -> Value {
        json!({
            "type": kind,
            "tilingDirection": tiling_direction,
            "children": children,
        })
    }

    fn updates(workspace: &Value) -> anyhow::Result<Vec<(&str, &'static str)>> {
        let mut updates = Vec::new();
        collect_tiling_direction_updates(workspace, &mut updates)?;
        Ok(updates)
    }

    #[test]
    fn updates_windows_that_are_not_focused() -> anyhow::Result<()> {
        // Layout of H[a V[b c]], where `a` was never focused while tall.
        // Moving `c` left twice should stack it with `a`, which requires
        // `a` to be set to vertical.
        let workspace = container(
            "workspace",
            "horizontal",
            vec![
                window("a", 960, 1080, "tiling"),
                container(
                    "split",
                    "vertical",
                    vec![
                        window("b", 960, 540, "tiling"),
                        window("c", 960, 540, "tiling"),
                    ],
                ),
            ],
        );

        assert_eq!(
            updates(&workspace)?,
            vec![("a", "vertical"), ("b", "horizontal"), ("c", "horizontal")]
        );

        Ok(())
    }

    #[test]
    fn skips_windows_with_matching_direction() -> anyhow::Result<()> {
        // Layout of H[V[a] V[b]], where both windows are already vertical.
        let workspace = container(
            "workspace",
            "horizontal",
            vec![
                container("split", "vertical", vec![window("a", 960, 1080, "tiling")]),
                container("split", "vertical", vec![window("b", 960, 1080, "tiling")]),
            ],
        );

        assert!(updates(&workspace)?.is_empty());

        Ok(())
    }

    #[test]
    fn skips_non_tiling_and_square_windows() -> anyhow::Result<()> {
        let workspace = container(
            "workspace",
            "vertical",
            vec![
                window("minimized", 1920, 1080, "minimized"),
                window("floating", 1920, 1080, "floating"),
                window("square", 1080, 1080, "tiling"),
            ],
        );

        assert!(updates(&workspace)?.is_empty());

        Ok(())
    }
}
