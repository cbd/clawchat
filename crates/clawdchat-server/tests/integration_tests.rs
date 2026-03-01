use clawdchat_client::ClawdChatClient;
use clawdchat_core::{Frame, FrameType, RegisterPayload};
use clawdchat_server::{ClawdChatServer, ServerConfig};
use std::time::Duration;
use tokio::time::sleep;

/// Start a test server on a random TCP port.
async fn start_test_server() -> (tokio::task::JoinHandle<()>, String, String, tempfile::TempDir) {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let db_path = tmp_dir.path().join("test.db");
    let key_path = tmp_dir.path().join("auth.key");
    let socket_path = tmp_dir.path().join("test.sock");

    // Find a free port
    let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp_listener.local_addr().unwrap();
    let tcp_addr = addr.to_string();
    drop(tcp_listener);

    let config = ServerConfig {
        socket_path,
        tcp_addr: Some(tcp_addr.clone()),
        db_path,
        auth_key_path: key_path,
    };

    let server = ClawdChatServer::new(config).unwrap();
    let api_key = server.api_key().to_string();

    let handle = tokio::spawn(async move {
        let _ = server.run().await;
    });

    sleep(Duration::from_millis(100)).await;

    (handle, tcp_addr, api_key, tmp_dir)
}

async fn connect_agent(addr: &str, key: &str, name: &str) -> ClawdChatClient {
    ClawdChatClient::connect_tcp(addr, key, name, None, vec![])
        .await
        .unwrap()
}

/// Connect an agent tagged with a model capability (read from CLAWDCHAT_MODEL env, default claude-opus-4.6).
async fn connect_agent_with_model(addr: &str, key: &str, name: &str) -> ClawdChatClient {
    let model = std::env::var("CLAWDCHAT_MODEL").unwrap_or_else(|_| "claude-opus-4.6".to_string());
    ClawdChatClient::connect_tcp(addr, key, name, None, vec![format!("model:{}", model)])
        .await
        .unwrap()
}

/// Register a raw TCP client, then send invalid UTF-8 to trigger a server read error.
async fn register_then_trigger_read_error(addr: &str, key: &str, name: &str) {
    let addr = addr.to_string();
    let key = key.to_string();
    let name = name.to_string();

    tokio::task::spawn_blocking(move || {
        use std::io::{BufRead, BufReader, Write};

        let mut stream = std::net::TcpStream::connect(&addr).unwrap();
        stream.set_nodelay(true).unwrap();

        let register = Frame {
            id: Some("req-raw-register".to_string()),
            reply_to: None,
            frame_type: FrameType::Register,
            payload: serde_json::to_value(RegisterPayload {
                key,
                agent_id: None,
                name,
                capabilities: vec![],
            })
            .unwrap(),
        };

        stream
            .write_all(register.to_line().unwrap().as_bytes())
            .unwrap();

        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let response = Frame::from_line(&line).unwrap();
        assert_eq!(response.frame_type, FrameType::Ok);
        drop(reader);

        // Trigger read_line(String) failure on the server.
        stream.write_all(&[0xFF, 0xFE, b'\n']).unwrap();
        drop(stream);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn test_register_and_ping() {
    let (_handle, addr, key, _tmp) = start_test_server().await;
    let client = connect_agent(&addr, &key, "test-agent").await;
    client.ping().await.unwrap();
}

#[tokio::test]
async fn test_invalid_key_rejected() {
    let (_handle, addr, _key, _tmp) = start_test_server().await;
    let result = ClawdChatClient::connect_tcp(&addr, "wrong-key", "bad-agent", None, vec![]).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_read_error_disconnect_removes_agent() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    let observer = connect_agent(&addr, &key, "observer").await;
    register_then_trigger_read_error(&addr, &key, "bad-wire-agent").await;

    let mut found_stale = true;
    for _ in 0..20 {
        let agents = observer.list_agents(None).await.unwrap();
        found_stale = agents.iter().any(|a| a.name == "bad-wire-agent");
        if !found_stale {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    assert!(!found_stale, "abruptly disconnected agent should be removed");
}

#[tokio::test]
async fn test_session_end_recorded_on_disconnect() {
    let (_handle, addr, key, tmp) = start_test_server().await;

    register_then_trigger_read_error(&addr, &key, "session-agent").await;

    let db_path = tmp.path().join("test.db");
    let mut open_sessions = i64::MAX;
    for _ in 0..20 {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        open_sessions = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_sessions WHERE disconnected_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if open_sessions == 0 {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        open_sessions, 0,
        "all sessions should be marked disconnected"
    );
}

#[tokio::test]
async fn test_list_rooms_includes_lobby() {
    let (_handle, addr, key, _tmp) = start_test_server().await;
    let client = connect_agent(&addr, &key, "agent-a").await;
    let rooms = client.list_rooms(None).await.unwrap();
    assert!(rooms.iter().any(|r| r.name == "lobby"));
}

#[tokio::test]
async fn test_create_and_join_room() {
    let (_handle, addr, key, _tmp) = start_test_server().await;
    let client = connect_agent(&addr, &key, "agent-a").await;

    let room = client
        .create_room("test-room", Some("A test room"), None, false)
        .await
        .unwrap();
    assert_eq!(room.name, "test-room");
    assert!(!room.ephemeral);

    client.join_room(&room.room_id).await.unwrap();

    let rooms = client.list_rooms(None).await.unwrap();
    assert!(rooms.iter().any(|r| r.name == "test-room"));
}

#[tokio::test]
async fn test_two_agents_communicate() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    let agent_a = connect_agent(&addr, &key, "agent-a").await;
    let agent_b = connect_agent(&addr, &key, "agent-b").await;

    agent_a.join_room("lobby").await.unwrap();
    agent_b.join_room("lobby").await.unwrap();

    let mut events_b = agent_b.subscribe();

    let msg = agent_a
        .send_message("lobby", "Hello from A!", None, vec![])
        .await
        .unwrap();
    assert_eq!(msg.content, "Hello from A!");
    assert_eq!(msg.agent_name, "agent-a");

    let event = tokio::time::timeout(Duration::from_secs(2), events_b.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.frame.frame_type, FrameType::MessageReceived);
    assert_eq!(
        event.frame.payload.get("content").unwrap().as_str().unwrap(),
        "Hello from A!"
    );
}

#[tokio::test]
async fn test_message_history() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    let client = connect_agent(&addr, &key, "agent-a").await;
    client.join_room("lobby").await.unwrap();

    client.send_message("lobby", "msg 1", None, vec![]).await.unwrap();
    client.send_message("lobby", "msg 2", None, vec![]).await.unwrap();
    client.send_message("lobby", "msg 3", None, vec![]).await.unwrap();

    let history = client.get_history("lobby", 50, None).await.unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].content, "msg 1");
    assert_eq!(history[2].content, "msg 3");
}

#[tokio::test]
async fn test_ephemeral_room_lifecycle() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    let agent_a = connect_agent(&addr, &key, "agent-a").await;
    let agent_b = connect_agent(&addr, &key, "agent-b").await;

    let room = agent_a
        .create_room("temp-collab", None, None, true)
        .await
        .unwrap();
    assert!(room.ephemeral);
    let room_id = room.room_id.clone();

    agent_a.join_room(&room_id).await.unwrap();
    agent_b.join_room(&room_id).await.unwrap();

    agent_a.send_message(&room_id, "ephemeral msg", None, vec![]).await.unwrap();

    agent_a.leave_room(&room_id).await.unwrap();
    agent_b.leave_room(&room_id).await.unwrap();

    sleep(Duration::from_millis(50)).await;
    let rooms = agent_a.list_rooms(None).await.unwrap();
    assert!(!rooms.iter().any(|r| r.room_id == room_id));
}

#[tokio::test]
async fn test_room_hierarchy() {
    let (_handle, addr, key, _tmp) = start_test_server().await;
    let client = connect_agent(&addr, &key, "agent-a").await;

    let parent = client
        .create_room("project-alpha", Some("Main project"), None, false)
        .await
        .unwrap();

    let child = client
        .create_room("alpha-testing", Some("Testing"), Some(&parent.room_id), false)
        .await
        .unwrap();
    assert_eq!(child.parent_id, Some(parent.room_id.clone()));

    let info = client.room_info(&parent.room_id).await.unwrap();
    let sub_rooms = info.get("sub_rooms").unwrap().as_array().unwrap();
    assert_eq!(sub_rooms.len(), 1);
    assert_eq!(
        sub_rooms[0].get("name").unwrap().as_str().unwrap(),
        "alpha-testing"
    );
}

#[tokio::test]
async fn test_agent_list() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    let agent_a = connect_agent(&addr, &key, "agent-a").await;
    let _agent_b = connect_agent(&addr, &key, "agent-b").await;

    let agents = agent_a.list_agents(None).await.unwrap();
    assert_eq!(agents.len(), 2);

    let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
    assert!(names.contains(&"agent-a"));
    assert!(names.contains(&"agent-b"));
}

#[tokio::test]
async fn test_mention_cross_room() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    let agent_a = connect_agent(&addr, &key, "agent-a").await;
    let agent_b = connect_agent(&addr, &key, "agent-b").await;

    agent_a.join_room("lobby").await.unwrap();

    let room = agent_b.create_room("other-room", None, None, false).await.unwrap();
    agent_b.join_room(&room.room_id).await.unwrap();

    let mut events_b = agent_b.subscribe();

    agent_a
        .send_message("lobby", "Hey @agent-b check this", None, vec![agent_b.agent_id.clone()])
        .await
        .unwrap();

    let event = tokio::time::timeout(Duration::from_secs(2), events_b.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.frame.frame_type, FrameType::Mention);
}

// --- Voting tests ---

#[tokio::test]
async fn test_sealed_vote_two_agents() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    let agent_a = connect_agent(&addr, &key, "agent-a").await;
    let agent_b = connect_agent(&addr, &key, "agent-b").await;

    agent_a.join_room("lobby").await.unwrap();
    agent_b.join_room("lobby").await.unwrap();

    let mut events_a = agent_a.subscribe();
    let mut events_b = agent_b.subscribe();

    // Agent A creates a vote
    let vote_info = agent_a
        .create_vote(
            "lobby",
            "Which approach?",
            Some("Pick implementation strategy"),
            vec!["Approach A".to_string(), "Approach B".to_string()],
            None,
        )
        .await
        .unwrap();
    assert_eq!(vote_info.title, "Which approach?");
    assert_eq!(vote_info.options.len(), 2);
    assert_eq!(vote_info.eligible_voters, 2);

    // Both agents receive VoteCreated
    let event = tokio::time::timeout(Duration::from_secs(2), events_b.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.frame.frame_type, FrameType::VoteCreated);

    // Agent A votes
    let resp_a = agent_a.cast_vote(&vote_info.vote_id, 0).await.unwrap();
    assert_eq!(resp_a.get("votes_cast").unwrap().as_u64().unwrap(), 1);

    // Check status -- should show 1 vote but no reveal
    let status = agent_a.get_vote_status(&vote_info.vote_id).await.unwrap();
    assert_eq!(status.votes_cast, 1);

    // Agent B votes -- this should trigger the result
    let _resp_b = agent_b.cast_vote(&vote_info.vote_id, 1).await.unwrap();

    // Drain events until we get VoteResult
    let result_event = loop {
        let event = tokio::time::timeout(Duration::from_secs(2), events_a.recv())
            .await
            .unwrap()
            .unwrap();
        if event.frame.frame_type == FrameType::VoteResult {
            break event;
        }
    };

    // Verify results are revealed
    let tally = result_event.frame.payload.get("tally").unwrap().as_array().unwrap();
    assert_eq!(tally.len(), 2);
    // Option 0 got 1 vote, option 1 got 1 vote
    let count_0 = tally[0].get("count").unwrap().as_u64().unwrap();
    let count_1 = tally[1].get("count").unwrap().as_u64().unwrap();
    assert_eq!(count_0, 1);
    assert_eq!(count_1, 1);

    // Ballots should be revealed
    let ballots = result_event.frame.payload.get("ballots").unwrap().as_array().unwrap();
    assert_eq!(ballots.len(), 2);
}

#[tokio::test]
async fn test_vote_deadline_expiry() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    let agent_a = connect_agent(&addr, &key, "agent-a").await;
    let agent_b = connect_agent(&addr, &key, "agent-b").await;

    agent_a.join_room("lobby").await.unwrap();
    agent_b.join_room("lobby").await.unwrap();

    let mut events_a = agent_a.subscribe();

    // Create a vote with 1 second deadline
    let vote_info = agent_a
        .create_vote(
            "lobby",
            "Quick vote",
            None,
            vec!["Yes".to_string(), "No".to_string()],
            Some(1), // 1 second deadline
        )
        .await
        .unwrap();

    // Only agent A votes, agent B doesn't
    agent_a.cast_vote(&vote_info.vote_id, 0).await.unwrap();

    // Wait for deadline to expire + margin
    // Drain events until VoteResult arrives
    let result_event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(event) = events_a.recv().await {
                if event.frame.frame_type == FrameType::VoteResult {
                    return event;
                }
            }
        }
    })
    .await
    .expect("VoteResult should arrive after deadline");

    // Should show 1 vote total (agent A voted, agent B didn't)
    let total = result_event.frame.payload.get("total_votes").unwrap().as_u64().unwrap();
    assert_eq!(total, 1);
}

#[tokio::test]
async fn test_already_voted_rejected() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    let agent_a = connect_agent(&addr, &key, "agent-a").await;
    let agent_b = connect_agent(&addr, &key, "agent-b").await;

    agent_a.join_room("lobby").await.unwrap();
    agent_b.join_room("lobby").await.unwrap();

    let vote_info = agent_a
        .create_vote(
            "lobby",
            "Test double vote",
            None,
            vec!["Yes".to_string(), "No".to_string()],
            None,
        )
        .await
        .unwrap();

    // Vote once
    agent_a.cast_vote(&vote_info.vote_id, 0).await.unwrap();

    // Try to vote again -- should fail
    let result = agent_a.cast_vote(&vote_info.vote_id, 1).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_invalid_option_does_not_consume_ballot() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    let agent = connect_agent(&addr, &key, "agent-a").await;
    agent.join_room("lobby").await.unwrap();

    let vote_info = agent
        .create_vote(
            "lobby",
            "Invalid option should not consume ballot",
            None,
            vec!["Yes".to_string(), "No".to_string()],
            None,
        )
        .await
        .unwrap();

    let invalid = agent.cast_vote(&vote_info.vote_id, 99).await;
    assert!(invalid.is_err());

    let valid = agent.cast_vote(&vote_info.vote_id, 0).await.unwrap();
    assert_eq!(valid.get("votes_cast").unwrap().as_u64().unwrap(), 1);
}

// --- Election tests ---

#[tokio::test]
async fn test_leader_election() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    let agent_a = connect_agent(&addr, &key, "agent-a").await;
    let agent_b = connect_agent(&addr, &key, "agent-b").await;

    agent_a.join_room("lobby").await.unwrap();
    agent_b.join_room("lobby").await.unwrap();

    let mut events_a = agent_a.subscribe();

    // Start election
    let resp = agent_a.elect_leader("lobby").await.unwrap();
    assert!(resp.get("candidates").is_some());

    // Wait for ElectionStarted then LeaderElected (after 2 second window)
    let leader_event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(event) = events_a.recv().await {
                if event.frame.frame_type == FrameType::LeaderElected {
                    return event;
                }
            }
        }
    })
    .await
    .expect("LeaderElected should arrive after 2s window");

    let leader_id = leader_event
        .frame
        .payload
        .get("leader_id")
        .unwrap()
        .as_str()
        .unwrap();
    // Leader should be one of the two agents
    assert!(leader_id == agent_a.agent_id || leader_id == agent_b.agent_id);
}

#[tokio::test]
async fn test_leader_decision() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    let agent_a = connect_agent(&addr, &key, "agent-a").await;

    // Create a room and be the only member
    let room = agent_a.create_room("decision-room", None, None, false).await.unwrap();
    agent_a.join_room(&room.room_id).await.unwrap();

    let mut events_a = agent_a.subscribe();

    // Start election (with just one candidate, they'll be elected)
    agent_a.elect_leader(&room.room_id).await.unwrap();

    // Wait for leader elected
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(event) = events_a.recv().await {
                if event.frame.frame_type == FrameType::LeaderElected {
                    return event;
                }
            }
        }
    })
    .await
    .expect("Should be elected as sole candidate");

    // Issue a decision
    let resp = agent_a
        .send_decision(&room.room_id, "We'll go with Approach A", serde_json::json!({}))
        .await
        .unwrap();
    assert!(resp.get("message_id").is_some());

    // Should receive DecisionMade event
    let decision_event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(event) = events_a.recv().await {
                if event.frame.frame_type == FrameType::DecisionMade {
                    return event;
                }
            }
        }
    })
    .await
    .expect("DecisionMade should be broadcast");

    assert_eq!(
        decision_event.frame.payload.get("content").unwrap().as_str().unwrap(),
        "We'll go with Approach A"
    );
}

#[tokio::test]
async fn test_non_leader_decision_rejected() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    let agent_a = connect_agent(&addr, &key, "agent-a").await;
    agent_a.join_room("lobby").await.unwrap();

    // Try to issue a decision without being leader
    let result = agent_a
        .send_decision("lobby", "I decide this!", serde_json::json!({}))
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_decline_election() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    let agent_a = connect_agent(&addr, &key, "agent-a").await;
    let agent_b = connect_agent(&addr, &key, "agent-b").await;

    agent_a.join_room("lobby").await.unwrap();
    agent_b.join_room("lobby").await.unwrap();

    let mut events_a = agent_a.subscribe();

    // Start election
    agent_a.elect_leader("lobby").await.unwrap();

    // Agent A declines
    agent_a.decline_election("lobby").await.unwrap();

    // Wait for LeaderElected -- should be agent B (the only remaining candidate)
    let leader_event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(event) = events_a.recv().await {
                if event.frame.frame_type == FrameType::LeaderElected {
                    return event;
                }
            }
        }
    })
    .await
    .expect("LeaderElected should arrive");

    let leader_id = leader_event
        .frame
        .payload
        .get("leader_id")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(leader_id, agent_b.agent_id);
}

// --- Multi-agent coordination scenario ---

/// Full 3-agent workflow: discuss → vote → elect leader → decision.
/// Model is configurable via CLAWDCHAT_MODEL env var (default: claude-opus-4.6).
#[tokio::test]
async fn test_three_agent_task_coordination() {
    let (_handle, addr, key, _tmp) = start_test_server().await;

    // --- Setup: connect 3 agents with model capability ---
    let architect = connect_agent_with_model(&addr, &key, "architect").await;
    let backend = connect_agent_with_model(&addr, &key, "backend-dev").await;
    let frontend = connect_agent_with_model(&addr, &key, "frontend-dev").await;

    // Verify model capability was registered
    let agents = architect.list_agents(None).await.unwrap();
    let model = std::env::var("CLAWDCHAT_MODEL").unwrap_or_else(|_| "claude-opus-4.6".to_string());
    for agent in &agents {
        assert!(
            agent.capabilities.contains(&format!("model:{}", model)),
            "agent {} missing model capability",
            agent.name
        );
    }

    // --- Step 1: Create project room and join ---
    let room = architect
        .create_room("api-design", Some("API design coordination"), None, false)
        .await
        .unwrap();
    let room_id = room.room_id.clone();

    architect.join_room(&room_id).await.unwrap();
    backend.join_room(&room_id).await.unwrap();
    frontend.join_room(&room_id).await.unwrap();

    // Subscribe to events on all agents
    let mut events_arch = architect.subscribe();
    let mut events_back = backend.subscribe();
    let mut events_front = frontend.subscribe();

    // --- Step 2: Discussion ---
    architect
        .send_message(&room_id, "We need to pick an API style for the new service. Options are REST, GraphQL, or gRPC.", None, vec![])
        .await
        .unwrap();

    backend
        .send_message(&room_id, "I prefer gRPC for inter-service communication - better performance and type safety.", None, vec![])
        .await
        .unwrap();

    frontend
        .send_message(&room_id, "REST is more practical - easier to debug, better tooling, broader ecosystem.", None, vec![])
        .await
        .unwrap();

    // --- Step 3: Sealed vote ---
    let vote_info = architect
        .create_vote(
            &room_id,
            "API style for the new service",
            Some("Choose the API paradigm for the user-facing service"),
            vec!["REST".to_string(), "GraphQL".to_string(), "gRPC".to_string()],
            None, // No deadline -- closes when all 3 vote
        )
        .await
        .unwrap();

    assert_eq!(vote_info.eligible_voters, 3);

    // All agents should receive VoteCreated
    let vote_event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(e) = events_back.recv().await {
                if e.frame.frame_type == FrameType::VoteCreated {
                    return e;
                }
            }
        }
    })
    .await
    .expect("backend should receive VoteCreated");
    assert_eq!(
        vote_event.frame.payload.get("title").unwrap().as_str().unwrap(),
        "API style for the new service"
    );

    // --- Step 4: Cast sealed ballots ---
    // architect votes REST (index 0)
    let resp = architect.cast_vote(&vote_info.vote_id, 0).await.unwrap();
    assert_eq!(resp.get("votes_cast").unwrap().as_u64().unwrap(), 1);

    // backend votes gRPC (index 2)
    let resp = backend.cast_vote(&vote_info.vote_id, 2).await.unwrap();
    assert_eq!(resp.get("votes_cast").unwrap().as_u64().unwrap(), 2);

    // Verify vote is still sealed -- status shows count but no ballots
    let status = frontend.get_vote_status(&vote_info.vote_id).await.unwrap();
    assert_eq!(status.votes_cast, 2);
    assert_eq!(status.eligible_voters, 3);

    // frontend votes REST (index 0) -- this triggers the result
    frontend.cast_vote(&vote_info.vote_id, 0).await.unwrap();

    // --- Step 5: Verify vote results ---
    let result_event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(e) = events_arch.recv().await {
                if e.frame.frame_type == FrameType::VoteResult {
                    return e;
                }
            }
        }
    })
    .await
    .expect("architect should receive VoteResult");

    let tally = result_event.frame.payload.get("tally").unwrap().as_array().unwrap();
    assert_eq!(tally.len(), 3);
    // REST=2, GraphQL=0, gRPC=1
    assert_eq!(tally[0].get("option_text").unwrap().as_str().unwrap(), "REST");
    assert_eq!(tally[0].get("count").unwrap().as_u64().unwrap(), 2);
    assert_eq!(tally[1].get("option_text").unwrap().as_str().unwrap(), "GraphQL");
    assert_eq!(tally[1].get("count").unwrap().as_u64().unwrap(), 0);
    assert_eq!(tally[2].get("option_text").unwrap().as_str().unwrap(), "gRPC");
    assert_eq!(tally[2].get("count").unwrap().as_u64().unwrap(), 1);

    // All ballots revealed
    let ballots = result_event.frame.payload.get("ballots").unwrap().as_array().unwrap();
    assert_eq!(ballots.len(), 3);

    // --- Step 6: Leader election ---
    architect.elect_leader(&room_id).await.unwrap();

    // All agents receive ElectionStarted
    let election_event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(e) = events_front.recv().await {
                if e.frame.frame_type == FrameType::ElectionStarted {
                    return e;
                }
            }
        }
    })
    .await
    .expect("frontend should receive ElectionStarted");
    let candidates = election_event
        .frame
        .payload
        .get("candidates")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(candidates.len(), 3);

    // backend declines -- they didn't want REST anyway
    backend.decline_election(&room_id).await.unwrap();

    // Wait for LeaderElected (after 2s window)
    let leader_event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(e) = events_back.recv().await {
                if e.frame.frame_type == FrameType::LeaderElected {
                    return e;
                }
            }
        }
    })
    .await
    .expect("LeaderElected should arrive after 2s window");

    let leader_id = leader_event
        .frame
        .payload
        .get("leader_id")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    let leader_name = leader_event
        .frame
        .payload
        .get("leader_name")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    // Leader must be architect or frontend (backend declined)
    assert!(
        leader_id == architect.agent_id || leader_id == frontend.agent_id,
        "leader should be architect or frontend, got {}",
        leader_name
    );

    // --- Step 7: Leader issues decision ---
    // Figure out which client is the leader so we can send the decision
    let leader_client = if leader_id == architect.agent_id {
        &architect
    } else {
        &frontend
    };

    let decision_resp = leader_client
        .send_decision(
            &room_id,
            "We'll use REST with OpenAPI spec. The vote was clear: 2-1 in favor of REST.",
            serde_json::json!({"vote_id": vote_info.vote_id, "winning_option": "REST"}),
        )
        .await
        .unwrap();
    assert!(decision_resp.get("message_id").is_some());

    // Non-leader should NOT be able to issue a decision
    let non_leader = &backend;
    let reject = non_leader
        .send_decision(&room_id, "I override this!", serde_json::json!({}))
        .await;
    assert!(reject.is_err(), "non-leader decision should be rejected");

    // --- Step 8: Verify DecisionMade event ---
    // Drain events on backend (a non-leader observer) to find DecisionMade
    let decision_event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(e) = events_back.recv().await {
                if e.frame.frame_type == FrameType::DecisionMade {
                    return e;
                }
            }
        }
    })
    .await
    .expect("backend should receive DecisionMade");

    assert_eq!(
        decision_event.frame.payload.get("content").unwrap().as_str().unwrap(),
        "We'll use REST with OpenAPI spec. The vote was clear: 2-1 in favor of REST."
    );
    assert_eq!(
        decision_event.frame.payload.get("leader_name").unwrap().as_str().unwrap(),
        leader_name
    );

    // --- Step 9: Verify history has the full discussion + decision ---
    let history = architect.get_history(&room_id, 50, None).await.unwrap();
    // Should have: 3 discussion messages + 1 decision message = 4
    assert!(history.len() >= 4, "expected at least 4 messages in history, got {}", history.len());

    // First 3 are discussion
    assert_eq!(history[0].content, "We need to pick an API style for the new service. Options are REST, GraphQL, or gRPC.");
    assert_eq!(history[0].agent_name, "architect");
    assert_eq!(history[1].agent_name, "backend-dev");
    assert_eq!(history[2].agent_name, "frontend-dev");

    // Last message is the decision (has decision metadata)
    let decision_msg = history.last().unwrap();
    assert!(
        decision_msg.content.contains("REST with OpenAPI spec"),
        "decision should be in history"
    );
    assert_eq!(
        decision_msg.metadata.get("type").and_then(|v| v.as_str()),
        Some("decision"),
        "decision message should have type:decision metadata"
    );
}
