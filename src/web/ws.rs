//! The single multiplexed WebSocket.
//!
//! One socket serves the dashboard and every detail view: one reconnect path,
//! one schema. Every envelope carries an `agent_id`, and a client subscribes to
//! the agents whose transcripts it wants replayed (§7).

use std::collections::HashSet;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use crate::agent::supervisor::ServerMsg;

use super::routes::{AppState, REPLAY_WINDOW, decision_from};

/// Messages a browser sends us.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Start streaming an agent, replaying from `after_seq`. Omit `after_seq`
    /// for a fresh load, which gets the last [`REPLAY_WINDOW`] events.
    Subscribe {
        agent_id: String,
        #[serde(default)]
        after_seq: Option<i64>,
    },
    Unsubscribe {
        agent_id: String,
    },
    SendMessage {
        agent_id: String,
        text: String,
    },
    PermissionDecision {
        agent_id: String,
        request_id: String,
        behavior: String,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        updated_input: Option<Value>,
    },
    Interrupt {
        agent_id: String,
    },
    Ping,
}

/// Whether a broadcast is for everyone or only for subscribers of one agent.
///
/// Dashboard-level news (status, arrivals, departures, notices, clone progress)
/// always goes out; transcript traffic is filtered.
pub fn is_broadcast_wide(msg: &ServerMsg) -> bool {
    !matches!(
        msg,
        ServerMsg::Event { .. } | ServerMsg::Partial { .. } | ServerMsg::Commands { .. }
    )
}

/// The agent a message concerns, if any.
pub fn msg_agent_id(msg: &ServerMsg) -> Option<&str> {
    match msg {
        ServerMsg::Event { agent_id, .. }
        | ServerMsg::Status { agent_id, .. }
        | ServerMsg::PermissionRequest { agent_id, .. }
        | ServerMsg::PermissionResolved { agent_id, .. }
        | ServerMsg::PermissionModeChanged { agent_id, .. }
        | ServerMsg::Partial { agent_id, .. }
        | ServerMsg::Commands { agent_id, .. }
        | ServerMsg::Queued { agent_id, .. }
        | ServerMsg::AgentRemoved { agent_id } => Some(agent_id),
        ServerMsg::AgentAdded { agent } => Some(&agent.id),
        ServerMsg::Notice { agent_id, .. } => agent_id.as_deref(),
        ServerMsg::CloneProgress { .. }
        | ServerMsg::CloneDone { .. }
        | ServerMsg::RateLimit { .. } => None,
    }
}

/// Should this socket forward `msg`, given what it is subscribed to?
pub fn should_forward(msg: &ServerMsg, subscriptions: &HashSet<String>) -> bool {
    if is_broadcast_wide(msg) {
        return true;
    }
    msg_agent_id(msg).is_some_and(|id| subscriptions.contains(id))
}

pub async fn handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| run(socket, state))
}

async fn run(mut socket: WebSocket, state: AppState) {
    let mut rx = state.sup.subscribe();
    let mut subscriptions: HashSet<String> = HashSet::new();

    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if let Some(reply) = handle_client(&state, &text, &mut subscriptions).await
                        && send(&mut socket, &reply).await.is_err() {
                            return;
                        }
                }
                Some(Ok(Message::Close(_))) | None => return,
                Some(Ok(_)) => {}
                Some(Err(err)) => {
                    tracing::debug!(?err, "websocket closed");
                    return;
                }
            },
            broadcast = rx.recv() => match broadcast {
                Ok(msg) => {
                    if should_forward(&msg, &subscriptions)
                        && send_server_msg(&mut socket, &msg).await.is_err() {
                            return;
                        }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "websocket fell behind the event bus");
                    let notice = json!({
                        "type": "notice",
                        "agent_id": Value::Null,
                        "level": "warn",
                        "text": format!("This browser fell behind and missed {n} live updates. Reload to resync."),
                    });
                    if send(&mut socket, &notice).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Closed) => return,
            },
        }
    }
}

async fn send(socket: &mut WebSocket, value: &Value) -> Result<(), axum::Error> {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    socket.send(Message::Text(text.into())).await
}

async fn send_server_msg(socket: &mut WebSocket, msg: &ServerMsg) -> Result<(), axum::Error> {
    match serde_json::to_value(msg) {
        Ok(value) => send(socket, &value).await,
        Err(err) => {
            tracing::error!(?err, "could not serialise a server message");
            Ok(())
        }
    }
}

/// Handle one client message. Returns an immediate reply, if there is one.
async fn handle_client(
    state: &AppState,
    text: &str,
    subscriptions: &mut HashSet<String>,
) -> Option<Value> {
    let msg: ClientMsg = match serde_json::from_str(text) {
        Ok(msg) => msg,
        Err(err) => {
            return Some(json!({
                "type": "notice",
                "agent_id": Value::Null,
                "level": "error",
                "text": format!("Malformed client message: {err}"),
            }));
        }
    };

    match msg {
        ClientMsg::Ping => Some(json!({"type": "pong"})),
        ClientMsg::Subscribe {
            agent_id,
            after_seq,
        } => {
            subscriptions.insert(agent_id.clone());
            Some(replay(state, &agent_id, after_seq).await)
        }
        ClientMsg::Unsubscribe { agent_id } => {
            subscriptions.remove(&agent_id);
            None
        }
        ClientMsg::SendMessage { agent_id, text } => {
            match state.sup.send_message(&agent_id, &text).await {
                Ok(()) => None,
                Err(err) => Some(error_notice(&agent_id, err)),
            }
        }
        ClientMsg::Interrupt { agent_id } => match state.sup.interrupt(&agent_id).await {
            Ok(()) => None,
            Err(err) => Some(error_notice(&agent_id, err)),
        },
        ClientMsg::PermissionDecision {
            agent_id,
            request_id,
            behavior,
            message,
            updated_input,
        } => {
            let Some(decision) = decision_from(&behavior, message, updated_input) else {
                return Some(json!({
                    "type": "notice",
                    "agent_id": agent_id,
                    "level": "error",
                    "text": format!("Unknown permission behavior: {behavior}"),
                }));
            };
            match state.sup.decide(&agent_id, &request_id, decision).await {
                Ok(()) => None,
                Err(err) => Some(error_notice(&agent_id, err)),
            }
        }
    }
}

fn error_notice(agent_id: &str, err: anyhow::Error) -> Value {
    json!({
        "type": "notice",
        "agent_id": agent_id,
        "level": "error",
        "text": format!("{err:#}"),
    })
}

/// The cursor-based replay: `WHERE seq > ?`, same query for a fresh load and a
/// reconnect.
async fn replay(state: &AppState, agent_id: &str, after_seq: Option<i64>) -> Value {
    let db = state.sup.db().clone();
    let id = agent_id.to_string();
    let after = match after_seq {
        Some(seq) => seq,
        None => {
            let id = id.clone();
            db.run(move |db| db.tail_cursor(&id, REPLAY_WINDOW))
                .await
                .unwrap_or(0)
        }
    };
    let id_for_query = id.clone();
    let events = db
        .run(move |db| db.events_after(&id_for_query, after, REPLAY_WINDOW))
        .await
        .unwrap_or_default();
    // Only a live process can be answered, so a stopped agent replays no
    // pending approvals.
    let pending = if state.sup.is_running(agent_id).await {
        let id = id.clone();
        db.run(move |db| db.pending_permissions(&id))
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let cursor = events.last().map(|e| e.seq).unwrap_or(after);
    let id_for_head = id.clone();
    let head = db
        .run(move |db| db.max_seq(&id_for_head))
        .await
        .unwrap_or(cursor);
    json!({
        "type": "replay",
        "agent_id": agent_id,
        "after": after,
        "cursor": cursor,
        // The page is capped, so a client reconnecting from an old cursor has
        // to be told to come back for the rest — otherwise the events between
        // the page and the live stream are lost for good (§7).
        "has_more": cursor < head,
        "events": events,
        "pending_permissions": pending,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::state::Status;

    fn subs(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn status_msg(agent_id: &str) -> ServerMsg {
        ServerMsg::Status {
            agent_id: agent_id.to_string(),
            status: Status::Idle,
            status_detail: None,
            exit_code: None,
            last_stderr: None,
            cost_usd: 0.0,
        }
    }

    fn event_msg(agent_id: &str) -> ServerMsg {
        ServerMsg::Event {
            agent_id: agent_id.to_string(),
            seq: 1,
            ts: 0,
            kind: "assistant".to_string(),
            payload: json!({}),
        }
    }

    #[test]
    fn status_reaches_every_socket_so_the_dashboard_stays_live() {
        assert!(should_forward(&status_msg("a"), &HashSet::new()));
        assert!(should_forward(
            &ServerMsg::AgentRemoved {
                agent_id: "a".into()
            },
            &HashSet::new()
        ));
        assert!(should_forward(
            &ServerMsg::CloneProgress {
                clone_id: "c".into(),
                line: "x".into()
            },
            &HashSet::new()
        ));
    }

    #[test]
    fn transcript_traffic_only_reaches_subscribers() {
        assert!(!should_forward(&event_msg("a"), &HashSet::new()));
        assert!(should_forward(&event_msg("a"), &subs(&["a"])));
        assert!(!should_forward(&event_msg("b"), &subs(&["a"])));

        let partial = ServerMsg::Partial {
            agent_id: "a".into(),
            payload: json!({}),
        };
        assert!(!should_forward(&partial, &subs(&["b"])));
        assert!(should_forward(&partial, &subs(&["a"])));
    }

    #[test]
    fn client_messages_round_trip() {
        let cases = vec![
            ClientMsg::Subscribe {
                agent_id: "a".into(),
                after_seq: Some(12),
            },
            ClientMsg::Unsubscribe {
                agent_id: "a".into(),
            },
            ClientMsg::SendMessage {
                agent_id: "a".into(),
                text: "hello".into(),
            },
            ClientMsg::PermissionDecision {
                agent_id: "a".into(),
                request_id: "7".into(),
                behavior: "allow".into(),
                message: None,
                updated_input: None,
            },
            ClientMsg::Interrupt {
                agent_id: "a".into(),
            },
            ClientMsg::Ping,
        ];
        for case in cases {
            let text = serde_json::to_string(&case).expect("serialise");
            let back: ClientMsg = serde_json::from_str(&text).expect("parse");
            assert_eq!(
                serde_json::to_value(&case).expect("value"),
                serde_json::to_value(&back).expect("value")
            );
        }
    }

    #[test]
    fn subscribe_without_a_cursor_is_accepted() {
        let msg: ClientMsg =
            serde_json::from_str(r#"{"type":"subscribe","agent_id":"a"}"#).expect("parse");
        match msg {
            ClientMsg::Subscribe { after_seq, .. } => assert_eq!(after_seq, None),
            other => panic!("expected subscribe, got {other:?}"),
        }
    }

    #[test]
    fn server_messages_are_tagged_by_type() {
        let value = serde_json::to_value(status_msg("a")).expect("serialise");
        assert_eq!(value["type"], json!("status"));
        assert_eq!(value["agent_id"], json!("a"));
        assert_eq!(value["status"], json!("idle"));

        let value = serde_json::to_value(event_msg("a")).expect("serialise");
        assert_eq!(value["type"], json!("event"));
        assert_eq!(value["seq"], json!(1));
    }

    #[test]
    fn agent_ids_are_found_on_every_variant_that_has_one() {
        assert_eq!(msg_agent_id(&event_msg("a")), Some("a"));
        assert_eq!(
            msg_agent_id(&ServerMsg::Notice {
                agent_id: None,
                level: "warn".into(),
                text: "x".into()
            }),
            None
        );
        // Account-wide news belongs to no agent.
        assert_eq!(
            msg_agent_id(&ServerMsg::RateLimit {
                info: Box::new(crate::agent::protocol::RateLimitInfo {
                    status: "allowed".into(),
                    resets_at: None,
                    rate_limit_type: None,
                    utilization: None,
                    is_using_overage: None,
                    unified_windows: Default::default(),
                    extra: Default::default(),
                })
            }),
            None
        );
        assert_eq!(
            msg_agent_id(&ServerMsg::CloneDone {
                clone_id: "c".into(),
                path: None,
                error: None
            }),
            None
        );
    }
}
