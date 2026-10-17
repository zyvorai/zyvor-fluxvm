# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
"""Agent sessions: one handle for a sandbox, its workspace, policy, checkpoints and ledger.

``FluxVM.create_session()`` returns a :class:`Session`. Fork works only on the in-tree KVM
engine; check ``session.capabilities["fork"]`` instead of calling :meth:`Session.fork` and
catching :class:`~fluxvm.errors.Conflict`.
"""

import http.client
import json
import time
import urllib.parse
from typing import Any, Dict, Iterable, Iterator, List, Mapping, Optional, Sequence, Tuple

from .errors import ConnectionFailed, RequestTimeout
from .models import ExecResult


def parse_sse(lines: Iterable[bytes]) -> Iterator[Tuple[str, Optional[str], str]]:
    """Split Server-Sent Events lines into ``(event, id, data)`` tuples."""
    event, ident, data, has = "message", None, [], False
    for raw in lines:
        line = raw.decode("utf-8", errors="replace").rstrip("\r\n")
        if line == "":
            if has:
                yield event, ident, "\n".join(data)
            event, ident, data, has = "message", None, [], False
        elif not line.startswith(":"):
            name, _, value = line.partition(":")
            value = value[1:] if value.startswith(" ") else value
            if name == "event":
                event = value
            elif name == "id":
                ident = value
            elif name == "data":
                data.append(value)
            else:
                continue
            has = True


class Session:
    """A handle to one agent session. Use as a context manager to delete on exit."""

    def __init__(self, client: Any, info: Dict[str, Any]):
        self._client = client
        self.info = info

    # ------------------------------------------------------------ properties

    @property
    def id(self) -> str:
        return self.info["id"]

    @property
    def backend(self) -> str:
        return self.info.get("backend", "")

    @property
    def capabilities(self) -> Dict[str, bool]:
        return dict(self.info.get("capabilities") or {})

    @property
    def workspace(self) -> Optional[Dict[str, Any]]:
        return self.info.get("workspace")

    def __repr__(self) -> str:
        return "Session(id={!r}, agent={!r}, backend={!r}, state={!r})".format(
            self.id, self.info.get("agent"), self.backend, self.info.get("state"))

    def __enter__(self) -> "Session":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.delete()

    def _path(self, suffix: str = "") -> str:
        return "/v1/agents/sessions/{}{}".format(urllib.parse.quote(self.id, safe=""), suffix)

    # ---------------------------------------------------------------- calls

    def refresh(self) -> Dict[str, Any]:
        """Re-read the record (checkpoints, children, state)."""
        self.info = self._client._json("GET", self._path())
        return self.info

    def exec(self, command: str, timeout: Optional[int] = None) -> ExecResult:
        """Run a shell string in the session. A non-zero exit is not an error; the call
        is recorded in the session's ledger."""
        payload: Dict[str, Any] = {"command": command}
        request_timeout = None
        if timeout is not None:
            payload["timeout_seconds"] = int(timeout)
            request_timeout = float(timeout) + 15.0
        out = self._client._json("POST", self._path("/exec"), payload, timeout=request_timeout)
        return ExecResult(
            exit_code=int(out.get("exit_code", 0)),
            stdout=out.get("stdout", "") or "",
            stderr=out.get("stderr", "") or "",
        )

    def checkpoint(self, tag: Optional[str] = None, memory: bool = True) -> Dict[str, Any]:
        """Snapshot under ``tag`` (server-chosen when None). ``memory=False`` is disk-only:
        much smaller, but the restore cold-boots."""
        payload: Dict[str, Any] = {}
        if tag:
            payload["tag"] = tag
        if not memory:
            payload["memory"] = False
        return self._client._json("POST", self._path("/checkpoint"), payload, timeout=300.0)

    def restore(self, tag: str) -> Dict[str, Any]:
        """Return to a checkpoint, discarding everything since."""
        return self._client._json(
            "POST", self._path("/restore"), {"tag": tag}, timeout=300.0)

    def fork(self, count: int = 1) -> List["Session"]:
        """Create ``count`` child sessions from the current state. Raises
        :class:`~fluxvm.errors.Conflict` (and creates nothing) on a backend that cannot fork."""
        out = self._client._json(
            "POST", self._path("/fork"), {"count": int(count)}, timeout=300.0)
        return [Session(self._client, item) for item in out.get("items", [])]

    def events(self, since: Optional[str] = None) -> Dict[str, Any]:
        """The ledger (``ledger``) and the sandbox's VM events (``vm_events``). ``since`` is
        an RFC 3339 time."""
        path = self._path("/events")
        if since:
            path += "?since=" + urllib.parse.quote(since, safe="")
        return self._client._json("GET", path)

    def watch_events(
        self,
        since: Optional[str] = None,
        last_event_id: Optional[str] = None,
        max_reconnects: int = 5,
        reconnect_delay: float = 1.0,
    ) -> Iterator[Dict[str, Any]]:
        """Follow ``GET .../events/stream``: replays the history, then yields new events as
        ``{"type": "ledger" | "vm" | "error", "data": {...}, "id": "..."}``.

        Reconnects with ``Last-Event-ID`` when the connection drops, so no ledger entry is
        repeated or missed (a VM event with the same timestamp as the last one may repeat).
        A final ``error`` event (the session is gone) is yielded and then the iterator ends.
        Gives up with the last error after ``max_reconnects`` failed attempts in a row. Break
        out of the loop to stop.
        """
        failures = 0
        while True:
            headers = {"Accept": "text/event-stream"}
            path = self._path("/events/stream")
            if last_event_id is not None:
                headers["Last-Event-ID"] = last_event_id
            elif since:
                path += "?since=" + urllib.parse.quote(since, safe="")
            try:
                resp = self._client._open_stream(path, headers)
            except (ConnectionFailed, RequestTimeout):
                failures += 1
                if failures > max_reconnects:
                    raise
                time.sleep(reconnect_delay)
                continue
            failures = 0
            try:
                try:
                    for event, ident, data in parse_sse(resp):
                        if ident is not None:
                            last_event_id = ident
                        try:
                            payload = json.loads(data)
                        except ValueError:
                            payload = {}
                        if event == "error":
                            # The hour limit asks for a reconnect; anything else is final.
                            if str(payload.get("error", "")).startswith("stream time limit"):
                                break
                            yield {"type": "error", "data": payload, "id": ident}
                            return
                        if event in ("ledger", "vm"):
                            yield {"type": event, "data": payload, "id": ident}
                except (OSError, http.client.HTTPException):
                    pass  # dropped or idle connection: reconnect from the last id
            finally:
                resp.close()
            time.sleep(reconnect_delay)

    def artifacts(self) -> Dict[str, Any]:
        """Workspace changes since the baseline (``changes`` is None, with a ``note``, when
        there is none), plus checkpoints, children and counters."""
        return self._client._json("GET", self._path("/artifacts"))

    def delete(self) -> None:
        """End the session and delete its sandbox. Safe to call twice."""
        self._client._json("DELETE", self._path(), expect=(204,))


def create_session(
    client: Any,
    agent: str = "custom",
    repo: Optional[str] = None,
    workspace_path: Optional[str] = None,
    policy: Optional[str] = None,
    allow: Optional[Sequence[str]] = None,
    image: Optional[str] = None,
    cpus: Optional[int] = None,
    memory_mb: Optional[int] = None,
    ttl_seconds: Optional[int] = None,
    extra_policy: Optional[Mapping[str, Any]] = None,
) -> Session:
    """``POST /v1/agents/sessions``. Returns once the sandbox runs commands and, when
    ``repo`` was given, the clone has been tried: check ``session.workspace["cloned"]``."""
    body: Dict[str, Any] = {"agent": agent}
    workspace: Dict[str, Any] = {}
    if repo:
        workspace["git"] = repo
    if workspace_path:
        workspace["path"] = workspace_path
    if workspace:
        body["workspace"] = workspace
    runtime: Dict[str, Any] = {}
    if image:
        runtime["image"] = image
    if cpus:
        runtime["cpus"] = int(cpus)
    if memory_mb:
        runtime["memory_mb"] = int(memory_mb)
    if runtime:
        body["runtime"] = runtime
    pol: Dict[str, Any] = dict(extra_policy or {})
    if policy:
        pol["preset"] = policy
    if allow:
        pol["network"] = {"default": "deny", "allow": list(allow)}
    if pol:
        body["policy"] = pol
    if ttl_seconds:
        body["ttl_seconds"] = int(ttl_seconds)
    info = client._json("POST", "/v1/agents/sessions", body, timeout=300.0)
    return Session(client, info)
