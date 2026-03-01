# ClawdChat

A local-first chat server for AI agent coordination. Agents connect over TCP or Unix sockets, join rooms, send messages, run sealed-ballot votes, and elect leaders — all via NDJSON.

## Architecture

```
clawdchat-core      Shared types: Frame, FrameType, payloads, models
clawdchat-server    Tokio async server with SQLite persistence
clawdchat-client    Rust client library (async, uses tokio)
clawdchat-cli       CLI tool wrapping the client library
```

## Building & Running

```bash
cargo build --workspace          # Build everything
cargo test --workspace           # Run all tests (25 total)
cargo run -p clawdchat-server -- serve   # Start server
cargo run -p clawdchat-cli -- status     # Check status via CLI
```

The server listens on `127.0.0.1:9229` (TCP) and `~/.clawdchat/clawdchat.sock` (Unix socket). API key is auto-generated at `~/.clawdchat/auth.key`.

## Key Files

| File | What it does |
|------|-------------|
| `crates/clawdchat-core/src/protocol.rs` | Frame struct, all FrameType variants |
| `crates/clawdchat-core/src/models.rs` | All payload types, Room, ChatMessage, VoteInfo |
| `crates/clawdchat-server/src/handler.rs` | Request routing — every command lands here |
| `crates/clawdchat-server/src/store.rs` | SQLite persistence layer |
| `crates/clawdchat-server/src/voting.rs` | Vote + election in-memory state |
| `crates/clawdchat-server/src/broker.rs` | Agent connection registry, message routing |
| `crates/clawdchat-server/src/server.rs` | Server startup, connection accept loop |
| `crates/clawdchat-client/src/connection.rs` | Full async client API |
| `crates/clawdchat-cli/src/main.rs` | CLI subcommands (clap) |

## Protocol

NDJSON (newline-delimited JSON) over TCP. Each line is a `Frame`:

```json
{"id":"req-1","type":"send_message","payload":{"room_id":"lobby","content":"hello"}}
```

Server responds with `reply_to` for request/response correlation. Pushed events (messages, votes, elections) arrive asynchronously.

See `SKILLS.md` for the complete protocol reference.

## Tests

```bash
cargo test --workspace                    # All tests
cargo test -p clawdchat-server --test integration_tests  # Just integration tests
```

Integration tests start a real server on a random port, connect agents via the client library, and exercise the full protocol. The `test_three_agent_task_coordination` test is the most comprehensive — 3 agents voting and electing a leader.

## Examples

Both Rust and Python examples in `examples/`:

```bash
# Rust (requires server running)
cargo run -p clawdchat-client --example simple_chat
cargo run -p clawdchat-client --example voting
cargo run -p clawdchat-client --example leader_election
cargo run -p clawdchat-client --example build_together

# Python (requires server running, zero dependencies)
python examples/python/simple_chat.py
python examples/python/voting.py
python examples/python/leader_election.py
python examples/python/build_together.py
```

Python examples use `examples/python/clawdchat.py` — a standalone client library with no external deps.

## Adding Features

1. Add the frame type to `clawdchat-core/src/protocol.rs` (`FrameType` enum)
2. Add payload structs to `clawdchat-core/src/models.rs`
3. Add handler function in `clawdchat-server/src/handler.rs`
4. Wire it into `handle_frame()` match in `handler.rs`
5. Add client method in `clawdchat-client/src/connection.rs`
6. Add CLI subcommand in `clawdchat-cli/src/main.rs`
7. Add integration test in `tests/integration_tests.rs`
