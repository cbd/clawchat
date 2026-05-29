//! Exercises the `wait` liveness escape hatches end-to-end by running the real
//! `clawchat` binary against an in-process server and asserting its exit code:
//!   2 = idle-timeout expired, 3 = peer ended the conversation.
//! These are the contract a reply-then-wait loop relies on to avoid hanging.

use clawchat_client::ClawChatClient;
use clawchat_server::{ClawChatServer, ServerConfig};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::sleep;

/// Start a test server on a random TCP port; return (handle, tcp_addr, api_key, tmp).
async fn start_test_server() -> (
    tokio::task::JoinHandle<()>,
    String,
    String,
    tempfile::TempDir,
) {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp_addr = listener.local_addr().unwrap().to_string();
    drop(listener);

    let config = ServerConfig {
        socket_path: tmp_dir.path().join("test.sock"),
        tcp_addr: Some(tcp_addr.clone()),
        http_addr: None,
        db_path: tmp_dir.path().join("test.db"),
        auth_key_path: tmp_dir.path().join("auth.key"),
        no_auth: false,
    };
    let server = ClawChatServer::new(config).unwrap();
    let api_key = server.api_key().to_string();
    let handle = tokio::spawn(async move {
        let _ = server.run().await;
    });
    sleep(Duration::from_millis(100)).await;
    (handle, tcp_addr, api_key, tmp_dir)
}

/// A `wait --loop` on a silent room exits 2 once the idle deadline passes,
/// instead of blocking forever.
#[tokio::test]
async fn wait_idle_timeout_exits_2() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    let status = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new(env!("CARGO_BIN_EXE_clawchat"))
            .args([
                "--tcp", &addr, "--key", &key, "--name", "waiter", "wait", "lobby",
                "--loop", "--since-seq", "tip", "--idle-timeout", "1", "--heartbeat-secs", "0",
            ])
            .status(),
    )
    .await
    .expect("wait should exit on idle, not hang")
    .expect("spawning the clawchat binary should succeed");

    assert_eq!(status.code(), Some(2), "idle-timeout must exit with code 2");
}

/// A peer's `conversation_end` message is surfaced to a waiter, which then exits
/// 3 so its loop terminates rather than waiting for another turn.
#[tokio::test]
async fn wait_conversation_end_exits_3() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    // Waiter: blocking loop, no idle deadline — only a conversation_end (or a
    // real message) should end it. A long idle bound guards against a hang.
    let mut child = Command::new(env!("CARGO_BIN_EXE_clawchat"))
        .args([
            "--tcp", &addr, "--key", &key, "--name", "waiter", "wait", "lobby",
            "--loop", "--since-seq", "tip", "--idle-timeout", "12", "--heartbeat-secs", "0",
        ])
        .spawn()
        .expect("spawning the clawchat binary should succeed");

    // Let the waiter subscribe and resolve tip before the end message is sent.
    sleep(Duration::from_millis(600)).await;

    let speaker = ClawChatClient::connect_tcp(&addr, &key, "speaker", None, vec![])
        .await
        .unwrap();
    speaker.join_room("lobby").await.unwrap();
    speaker
        .send_message_with_metadata(
            "lobby",
            "that's a wrap",
            None,
            vec![],
            serde_json::json!({ "kind": "conversation_end" }),
        )
        .await
        .unwrap();

    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("waiter should exit after the end message, not hang")
        .expect("waiting on the child should succeed");

    assert_eq!(
        status.code(),
        Some(3),
        "a conversation_end message must make wait exit with code 3"
    );
}
