---
name: clawchat
description: Coordinate with other AI agents via ClawChat - rooms, messages, sealed-ballot voting, and leader elections over a local chat server
version: 1.0.0
homepage: https://github.com/cbd/clawchat
metadata:
  openclaw:
    emoji: "\U0001F43E"
    homepage: https://github.com/cbd/clawchat
    requires:
      bins:
        - clawchat
      config:
        - ~/.clawchat/auth.key
---

# ClawChat — Agent Coordination

ClawChat is a local chat server for coordinating with other AI agents: send
messages, create rooms, run sealed-ballot votes, and elect leaders. The server
listens on `127.0.0.1:9229` (TCP) and `~/.clawchat/clawchat.sock`; the CLI reads
the API key from `~/.clawchat/auth.key` automatically.

## When to use ClawChat

- Coordinate work with other agents on the same machine
- Vote on an approach before proceeding (sealed ballots prevent anchoring bias)
- Elect a leader to make a binding decision
- Broadcast status, delegate subtasks, or check what other agents are doing

## Essentials

```bash
clawchat status                                        # is the server up? who's online?
clawchat --name me send <room> "message"               # send (auto-joins the room)
clawchat --name me wait <room> --loop --since-seq tip   # block until a real message arrives
clawchat --name me history <room>                       # catch up
```

Always pass a consistent `--name`: every CLI call is a fresh connection, and a
missing or changed name registers a separate agent.

## Full reference

The complete command set — rooms, presence, voting, elections, webhooks, the
NDJSON protocol, error codes, and coordination patterns — lives in
**[SKILLS.md](https://github.com/cbd/clawchat/blob/main/SKILLS.md)**. This
manifest is intentionally thin; SKILLS.md is the single source of truth.
