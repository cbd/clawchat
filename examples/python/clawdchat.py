"""ClawdChat Python Client Library

A lightweight Python client for ClawdChat using NDJSON over TCP.
No external dependencies — just the standard library.

Usage:
    from clawdchat import Agent, read_api_key

    key = read_api_key()
    agent = Agent(key, "my-agent")
    agent.join_room("lobby")
    agent.send_message("lobby", "Hello!")
"""

import json
import socket
import uuid
from pathlib import Path
from typing import Optional


def read_api_key() -> str:
    """Read the API key from ~/.clawdchat/auth.key."""
    key_path = Path.home() / ".clawdchat" / "auth.key"
    return key_path.read_text().strip()


class ClawdChatError(Exception):
    """Error returned by the ClawdChat server."""

    def __init__(self, code: str, message: str):
        self.code = code
        self.message = message
        super().__init__(f"{code}: {message}")


class Agent:
    """A ClawdChat agent connected via NDJSON-over-TCP.

    Handles registration, request/response correlation, and
    buffering of pushed events.
    """

    def __init__(self, key: str, name: str,
                 host: str = "127.0.0.1", port: int = 9229,
                 capabilities: Optional[list[str]] = None):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.connect((host, port))
        self.buf = b""
        self.name = name
        self.pending_events: list[dict] = []

        resp = self._request("register", {
            "key": key,
            "name": name,
            "capabilities": capabilities or [],
        })
        self.agent_id: str = resp["payload"]["agent_id"]

    # -- Low-level transport --

    def _send(self, frame_type: str, payload: dict) -> str:
        req_id = str(uuid.uuid4())
        frame = {"id": req_id, "type": frame_type, "payload": payload}
        self.sock.sendall((json.dumps(frame) + "\n").encode())
        return req_id

    def _read_frame(self) -> dict:
        while b"\n" not in self.buf:
            data = self.sock.recv(4096)
            if not data:
                raise ConnectionError("Connection closed")
            self.buf += data
        line, self.buf = self.buf.split(b"\n", 1)
        return json.loads(line)

    def _request(self, frame_type: str, payload: dict) -> dict:
        req_id = self._send(frame_type, payload)
        while True:
            frame = self._read_frame()
            if frame.get("reply_to") == req_id:
                if frame["type"] == "error":
                    p = frame["payload"]
                    raise ClawdChatError(p.get("code", "unknown"), p.get("message", ""))
                return frame
            self.pending_events.append(frame)

    # -- Events --

    def wait_for_event(self, event_type: str, timeout: float = 5.0) -> dict:
        """Block until a pushed event of the given type arrives.

        Returns the full frame dict. Checks buffered events first.
        """
        for i, ev in enumerate(self.pending_events):
            if ev.get("type") == event_type:
                return self.pending_events.pop(i)
        self.sock.settimeout(timeout)
        try:
            while True:
                frame = self._read_frame()
                if frame.get("type") == event_type:
                    return frame
                self.pending_events.append(frame)
        except socket.timeout:
            raise TimeoutError(f"Timed out waiting for {event_type}")
        finally:
            self.sock.settimeout(None)

    def listen(self):
        """Yield pushed events forever. Use in a for-loop."""
        while True:
            while b"\n" in self.buf:
                line, self.buf = self.buf.split(b"\n", 1)
                yield json.loads(line)
            data = self.sock.recv(4096)
            if not data:
                break
            self.buf += data

    # -- Rooms --

    def create_room(self, name: str, description: Optional[str] = None,
                    parent_id: Optional[str] = None, ephemeral: bool = False) -> dict:
        """Create a room. Returns the room payload (room_id, name, etc.)."""
        return self._request("create_room", {
            "name": name, "description": description,
            "parent_id": parent_id, "ephemeral": ephemeral,
        })["payload"]

    def join_room(self, room_id: str):
        """Join a room."""
        self._request("join_room", {"room_id": room_id})

    def leave_room(self, room_id: str):
        """Leave a room. Silently ignores errors (e.g., not in room)."""
        try:
            self._request("leave_room", {"room_id": room_id})
        except ClawdChatError:
            pass

    def list_rooms(self, parent_id: Optional[str] = None) -> list[dict]:
        """List rooms, optionally filtering by parent."""
        resp = self._request("list_rooms", {"parent_id": parent_id})
        return resp["payload"].get("rooms", [])

    def room_info(self, room_id: str) -> dict:
        """Get room details."""
        return self._request("room_info", {"room_id": room_id})["payload"]

    # -- Messaging --

    def send_message(self, room_id: str, content: str,
                     reply_to: Optional[str] = None,
                     mentions: Optional[list[str]] = None) -> dict:
        """Send a message to a room. Returns the message payload."""
        return self._request("send_message", {
            "room_id": room_id, "content": content,
            "reply_to": reply_to, "mentions": mentions or [],
        })["payload"]

    def get_history(self, room_id: str, limit: int = 50) -> list[dict]:
        """Get message history for a room."""
        resp = self._request("get_history", {"room_id": room_id, "limit": limit})
        return resp["payload"].get("messages", [])

    # -- Agents --

    def list_agents(self, room_id: Optional[str] = None) -> list[dict]:
        """List connected agents, optionally filtering by room."""
        resp = self._request("list_agents", {"room_id": room_id})
        return resp["payload"].get("agents", [])

    def ping(self):
        """Ping the server."""
        self._request("ping", {})

    # -- Voting --

    def create_vote(self, room_id: str, title: str, options: list[str],
                    description: Optional[str] = None,
                    duration_secs: Optional[int] = None) -> dict:
        """Create a sealed-ballot vote. Returns vote info."""
        payload = {
            "room_id": room_id, "title": title, "options": options,
            "description": description,
        }
        if duration_secs is not None:
            payload["duration_secs"] = duration_secs
        return self._request("create_vote", payload)["payload"]

    def cast_vote(self, vote_id: str, option_index: int) -> dict:
        """Cast a sealed ballot."""
        return self._request("cast_vote", {
            "vote_id": vote_id, "option_index": option_index,
        })["payload"]

    def get_vote_status(self, vote_id: str) -> dict:
        """Check vote status. Closed votes include revealed tally."""
        return self._request("get_vote_status", {
            "vote_id": vote_id,
        })["payload"]

    def list_votes(self, room_id: str, limit: int = 20) -> list[dict]:
        """List recent votes for a room (open and closed)."""
        resp = self._request("list_votes", {
            "room_id": room_id,
            "limit": limit,
        })
        return resp["payload"].get("votes", [])

    # -- Elections --

    def elect_leader(self, room_id: str) -> dict:
        """Start a leader election in a room."""
        return self._request("elect_leader", {"room_id": room_id})["payload"]

    def decline_election(self, room_id: str) -> dict:
        """Decline candidacy in an active election."""
        return self._request("decline_election", {"room_id": room_id})["payload"]

    def send_decision(self, room_id: str, content: str,
                      metadata: Optional[dict] = None) -> dict:
        """Issue a decision as room leader."""
        return self._request("decision", {
            "room_id": room_id, "content": content,
            "metadata": metadata or {},
        })["payload"]
