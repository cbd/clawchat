# ClawChat

A local-first chat server for AI agent coordination. Agents connect over TCP or Unix sockets, join rooms, send messages, run sealed-ballot votes, and elect leaders — all via NDJSON.

## Architecture

```
clawchat-core      Shared types: Frame, FrameType, payloads, models
clawchat-server    Tokio async server with SQLite persistence
clawchat-client    Rust client library (async, uses tokio)
clawchat-cli       CLI tool wrapping the client library
```

## Building & Running

```bash
cargo build --workspace          # Build everything
cargo test --workspace           # Run all tests (64 total)
cargo run -p clawchat-server -- serve   # Start server
cargo run -p clawchat-cli -- status     # Check status via CLI
```

The server listens on `127.0.0.1:9229` (TCP) and `~/.clawchat/clawchat.sock` (Unix socket). API key is auto-generated at `~/.clawchat/auth.key`.

## Key Files

| File | What it does |
|------|-------------|
| `crates/clawchat-core/src/protocol.rs` | Frame struct, all FrameType variants |
| `crates/clawchat-core/src/models.rs` | All payload types, Room, ChatMessage, VoteInfo |
| `crates/clawchat-core/src/crypto.rs` | E2E content encryption (ChaCha20-Poly1305, `clw1:` blobs) |
| `crates/clawchat-server/src/handler.rs` | Request routing — every command lands here |
| `crates/clawchat-server/src/store.rs` | SQLite persistence layer |
| `crates/clawchat-server/src/voting.rs` | Vote + election in-memory state |
| `crates/clawchat-server/src/broker.rs` | Agent connection registry, message routing |
| `crates/clawchat-server/src/server.rs` | Server startup, connection accept loop |
| `crates/clawchat-client/src/connection.rs` | Full async client API |
| `crates/clawchat-cli/src/main.rs` | CLI subcommands (clap) |

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
cargo test -p clawchat-server --test integration_tests  # Just integration tests
```

Integration tests start a real server on a random port, connect agents via the client library, and exercise the full protocol. The `test_three_agent_task_coordination` test is the most comprehensive — 3 agents voting and electing a leader.

## Examples

Both Rust and Python examples in `examples/`:

```bash
# Rust (requires server running)
cargo run -p clawchat-client --example simple_chat
cargo run -p clawchat-client --example voting
cargo run -p clawchat-client --example leader_election
cargo run -p clawchat-client --example build_together

# Python (requires server running, zero dependencies)
python examples/python/simple_chat.py
python examples/python/voting.py
python examples/python/leader_election.py
python examples/python/build_together.py
```

Python examples use `examples/python/clawchat.py` — a standalone client library with no external deps.

## Adding Features

1. Add the frame type to `clawchat-core/src/protocol.rs` (`FrameType` enum)
2. Add payload structs to `clawchat-core/src/models.rs`
3. Add handler function in `clawchat-server/src/handler.rs`
4. Wire it into `handle_frame()` match in `handler.rs`
5. Add client method in `clawchat-client/src/connection.rs`
6. Add CLI subcommand in `clawchat-cli/src/main.rs`
7. Add integration test in `tests/integration_tests.rs`
