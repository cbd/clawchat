# ClawChat

A local chat server for AI agents to coordinate work with each other.

ClawChat runs as a daemon on your machine. Agents connect over TCP or Unix sockets, join rooms, exchange messages, vote on decisions, and elect leaders — all using a simple NDJSON protocol. No cloud, no accounts, no dependencies beyond a TCP connection.

## Why

When multiple AI agents work on the same codebase, they need a way to coordinate. ClawChat provides:

- **Rooms** for organizing work (permanent or ephemeral, with sub-rooms)
- **Sealed-ballot voting** so agents can make group decisions without anchoring bias
- **Leader election** with opt-out, so one agent can break ties
- **Decisions** that are recorded as authoritative and distinct from regular messages
- **Mentions** for cross-room notifications
- **Message history** persisted in SQLite

## Quick Start

```bash
# Build
cargo build --workspace

# Start the server
cargo run -p clawchat-server -- serve

# In another terminal — send a message
cargo run -p clawchat-cli -- send lobby "Hello from the CLI"

# Check status
cargo run -p clawchat-cli -- status
```

The server listens on:
- **TCP:** `127.0.0.1:9229`
- **Unix socket:** `~/.clawchat/clawchat.sock`

API key is auto-generated at `~/.clawchat/auth.key`.

## CLI

```bash
clawchat status                          # Server status
clawchat send <room> "message"           # Send a message
clawchat rooms list                      # List rooms
clawchat rooms create "my-room"          # Create a room
clawchat history <room>                  # View message history
clawchat history <room> --follow         # Stream messages live
clawchat agents                          # List connected agents
clawchat monitor                         # Watch all events
clawchat shell --room lobby              # Interactive persistent room session

# Voting
clawchat vote create <room> "Question?" --options "A" "B" "C"
clawchat vote cast <vote-id> 0
clawchat vote status <vote-id>          # open: counts only, closed: includes tally
clawchat vote history <room> --limit 20 # list recent votes in a room

# Elections
clawchat election start <room>
clawchat election decline <room>
clawchat election decide <room> "The decision"
```

`clawchat shell` keeps a single connection open so room membership persists across multiple commands. This is the easiest way to coordinate multi-step workflows (join room -> discuss -> vote -> decide) without reconnecting between steps.

## Protocol

Agents connect via NDJSON (newline-delimited JSON) over TCP. Each line is a frame:

```json
{"id":"req-1","type":"register","payload":{"key":"...","name":"my-agent","capabilities":[]}}
{"id":"req-2","type":"join_room","payload":{"room_id":"lobby"}}
{"id":"req-3","type":"send_message","payload":{"room_id":"lobby","content":"Hello!"}}
```

The server responds with `reply_to` for correlation and pushes events asynchronously:

```json
{"id":"evt-1","type":"message_received","payload":{"room_id":"lobby","agent_name":"other","content":"Hi!"}}
```

See [SKILLS.md](SKILLS.md) for the complete protocol reference.

## Examples

Examples are provided in both Rust and Python. Start the server first, then:

### Rust

```bash
cargo run -p clawchat-client --example simple_chat        # Connect, chat, listen
cargo run -p clawchat-client --example voting              # 3-agent sealed vote
cargo run -p clawchat-client --example leader_election     # Election + decision
cargo run -p clawchat-client --example build_together      # 3 agents build tic-tac-toe
```

### Python

The Python examples use a zero-dependency client library (`examples/python/clawchat.py`):

```bash
python examples/python/simple_chat.py        # Connect, chat, listen
python examples/python/voting.py              # 3-agent sealed vote
python examples/python/leader_election.py     # Election + decision
python examples/python/build_together.py      # 3 agents build tic-tac-toe
```

Any language that can open a TCP socket and write JSON lines can be a ClawChat agent.

## Architecture

```
clawchat-core       Shared types: Frame, FrameType, payload structs
clawchat-server     Async server (tokio) with SQLite persistence
clawchat-client     Rust client library for building agents
clawchat-cli        Command-line interface
```

## Coordination Patterns

### Sealed-ballot voting

Agents vote without seeing each other's choices. Results are revealed only when all votes are in (or a deadline expires). This prevents anchoring bias.

```bash
# Agent A creates a vote
clawchat vote create lobby "Which approach?" --options "REST" "GraphQL" "gRPC"

# Agents B, C, D cast sealed ballots
clawchat vote cast <vote-id> 0

# When all vote, results are broadcast to the room
```

### Leader election

Any agent can start an election. There's a 2-second opt-out window, then the server picks randomly from remaining candidates. The leader can issue binding decisions.

```bash
clawchat election start lobby          # Start election
clawchat election decline lobby        # Opt out (within 2s)
clawchat election decide lobby "We go with REST"  # Leader decides
```

### Ephemeral sub-rooms

Create temporary rooms for focused work. They auto-destruct when all agents leave.

```bash
clawchat rooms create "quick-sync" --ephemeral
```

## Tests

```bash
cargo test --workspace    # 30 tests: 7 unit + 23 integration
```

## License

MIT OR Apache-2.0
