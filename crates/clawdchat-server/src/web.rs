use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Path, State},
    http::{header, StatusCode, Uri},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use clawdchat_core::Room;
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use rust_embed::Embed;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::broker::Broker;
use crate::rate_limit::RateLimiter;
use crate::server::connection_loop;
use crate::store::Store;
use crate::voting::VoteManager;

#[derive(Embed)]
#[folder = "web/"]
struct WebAssets;

/// Shared state passed to all axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub broker: Arc<Broker>,
    pub store: Arc<Store>,
    pub ephemeral_rooms: Arc<DashMap<String, Room>>,
    pub vote_mgr: Arc<VoteManager>,
    pub rate_limiter: Arc<RateLimiter>,
    pub no_auth: bool,
    pub api_key: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/api/keys", post(create_api_key))
        .route("/api/status", get(api_status))
        .route("/api/rooms", get(api_list_rooms))
        .route("/api/agents", get(api_list_agents))
        .route("/api/rooms/{room_id}/history", get(api_room_history))
        .fallback(static_handler)
        .with_state(state)
        .layer(tower_http::cors::CorsLayer::permissive())
}

// --- WebSocket handler ---

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_connection(socket, state))
}

async fn handle_ws_connection(ws: WebSocket, state: AppState) {
    let (mut ws_sender, mut ws_receiver) = ws.split();

    // Create an in-memory duplex stream (bidirectional pipe)
    let (server_stream, client_stream) = tokio::io::duplex(65536);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let (client_read, mut client_write) = tokio::io::split(client_stream);

    // Task 1: WebSocket receiver → pipe writer
    // Reads text messages from WS, writes them as NDJSON lines to the pipe
    let ws_to_pipe = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            match msg {
                Message::Text(text) => {
                    let text_bytes = text.as_bytes();
                    if client_write.write_all(text_bytes).await.is_err() {
                        break;
                    }
                    if !text_bytes.ends_with(b"\n") {
                        if client_write.write_all(b"\n").await.is_err() {
                            break;
                        }
                    }
                }
                Message::Close(_) => break,
                _ => {} // Ignore binary, ping/pong (axum handles pong automatically)
            }
        }
        // Drop client_write to signal EOF to server_read
    });

    // Task 2: Pipe reader → WebSocket sender
    // Reads NDJSON lines from the pipe, sends them as WS text messages
    let pipe_to_ws = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(client_read);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = line.trim_end().to_string();
                    if ws_sender.send(Message::Text(trimmed.into())).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Task 3: Run the existing connection_loop on the server side of the duplex
    let broker = state.broker;
    let store = state.store;
    let ephemeral_rooms = state.ephemeral_rooms;
    let vote_mgr = state.vote_mgr;
    let rate_limiter = state.rate_limiter;
    let api_key = state.api_key;
    let no_auth = state.no_auth;

    let connection_task = tokio::spawn(async move {
        let _ = connection_loop(
            server_read,
            server_write,
            broker,
            store,
            ephemeral_rooms,
            vote_mgr,
            api_key,
            no_auth,
            rate_limiter,
        )
        .await;
    });

    // Wait for any task to complete (connection closing)
    tokio::select! {
        _ = ws_to_pipe => {},
        _ = pipe_to_ws => {},
        _ = connection_task => {},
    }
}

// --- REST API endpoints ---

async fn create_api_key(
    State(state): State<AppState>,
    body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    let label = body
        .and_then(|b| b.get("label").and_then(|v| v.as_str()).map(String::from));

    let key = crate::auth::generate_key();

    if let Err(e) = state.store.create_api_key(&key, label.as_deref()) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        );
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "api_key": key,
            "tier": "free",
        })),
    )
}

async fn api_status(State(state): State<AppState>) -> impl IntoResponse {
    let agent_count = state.broker.agents.len();
    let rooms = state.store.list_rooms(None).unwrap_or_default();
    let ephemeral_count = state.ephemeral_rooms.len();

    Json(serde_json::json!({
        "status": "ok",
        "agents_connected": agent_count,
        "rooms": rooms.len() + ephemeral_count,
    }))
}

async fn api_list_rooms(State(state): State<AppState>) -> impl IntoResponse {
    // Public API: only show public rooms
    let mut rooms = state
        .store
        .list_rooms_for_key(None, None)
        .unwrap_or_default();

    // Include public ephemeral rooms
    for entry in state.ephemeral_rooms.iter() {
        let room = entry.value();
        if room.visibility == "public" {
            rooms.push(room.clone());
        }
    }

    Json(serde_json::json!({"rooms": rooms}))
}

async fn api_list_agents(State(state): State<AppState>) -> impl IntoResponse {
    let agents: Vec<serde_json::Value> = state
        .broker
        .agents
        .iter()
        .map(|a| {
            serde_json::json!({
                "agent_id": a.info.agent_id,
                "name": a.info.name,
                "capabilities": a.info.capabilities,
            })
        })
        .collect();

    Json(serde_json::json!({"agents": agents}))
}

async fn api_room_history(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
) -> impl IntoResponse {
    // Only allow history for public rooms via REST API
    let room = state.store.get_room(&room_id).ok().flatten();
    match room {
        Some(r) if r.visibility == "public" => {
            let messages = state.store.get_history(&room_id, 100, None).unwrap_or_default();
            (StatusCode::OK, Json(serde_json::json!({"messages": messages}))).into_response()
        }
        Some(_) => (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Room is private"})),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Room not found"})),
        )
            .into_response(),
    }
}

// --- Static file serving ---

async fn static_handler(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match WebAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime.as_ref().to_string())],
                content.data.into_owned(),
            )
                .into_response()
        }
        None => {
            // SPA fallback: serve index.html for unmatched routes
            match WebAssets::get("index.html") {
                Some(content) => (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/html".to_string())],
                    content.data.into_owned(),
                )
                    .into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
    }
}
