# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
"""Client for FluxVM central's fleet API (``/fleet/*``): the Mac cloud.

Standard library only. Answers are the API's JSON as plain ``dict``/``list``
values (see ``schemas/fleet-openapi.yaml``), so new server fields show up
without a client release.

Idempotent calls (GET, PUT, DELETE) are retried on 429 and 5xx with
exponential backoff, honouring ``Retry-After``; POSTs are never retried.
"""

import json
import random
import ssl
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Callable, Dict, List, Mapping, Optional, Sequence

from .client import _error_from_response, _is_timeout, _NoRedirect
from .errors import ApiError, Conflict, ConnectionFailed, NotFound, RequestTimeout, ServiceUnavailable

ME = "me"
POWER_ACTIONS = ("start", "stop", "restart", "pause", "resume")
_IDEMPOTENT = ("GET", "HEAD", "PUT", "DELETE")


def _seg(value: str) -> str:
    return urllib.parse.quote(str(value), safe="")


def _query(**params: Any) -> str:
    kept = {k: v for k, v in params.items() if v is not None and v != ""}
    if not kept:
        return ""
    return "?" + urllib.parse.urlencode(
        {k: ("true" if v is True else "false" if v is False else v) for k, v in kept.items()})


def _fleet_error(status: int, raw: bytes, headers: Mapping[str, str]) -> ApiError:
    err = _error_from_response(status, raw, headers)
    if type(err) is ApiError and status == 409:
        return Conflict(err.status, err.body, err.message)
    if type(err) is ApiError and status == 503:
        return ServiceUnavailable(err.status, err.body, err.message)
    return err


class FleetClient:
    """Client for one central (``fluxvm-agent central``).

    :param base_url: e.g. ``"https://fleet.example.com"`` (default port 7799).
    :param token: the fleet (operator) token, a tenant's primary token, or a named API key.
    :param timeout: per-attempt socket timeout in seconds.
    :param retries: retries of an idempotent call after a 429, a 5xx or a connection failure.
    :param retry_delay: first backoff delay in seconds (doubles each retry, capped at ``retry_max``).
    """

    def __init__(
        self,
        base_url: str,
        token: Optional[str] = None,
        timeout: float = 60.0,
        retries: int = 3,
        retry_delay: float = 0.5,
        retry_max: float = 10.0,
        ssl_context: Optional[ssl.SSLContext] = None,
        user_agent: str = "fluxvm-fleet-python/0.1.0",
        sleep: Callable[[float], None] = time.sleep,
    ):
        if not base_url:
            raise ValueError("base_url is required")
        self.base_url = base_url.rstrip("/")
        self.token = token
        self.timeout = timeout
        self.retries = max(0, retries)
        self.retry_delay = retry_delay
        self.retry_max = retry_max
        self.user_agent = user_agent
        self._sleep = sleep
        handlers: List[Any] = [_NoRedirect()]
        if ssl_context is not None:
            handlers.append(urllib.request.HTTPSHandler(context=ssl_context))
        self._opener = urllib.request.build_opener(*handlers)

    # ------------------------------------------------------------------ core

    def _once(self, method: str, path: str, body: Optional[bytes], accept: str):
        req = urllib.request.Request(self.base_url + path, data=body, method=method)
        req.add_header("Accept", accept)
        req.add_header("User-Agent", self.user_agent)
        if body is not None:
            req.add_header("Content-Type", "application/json")
        if self.token:
            req.add_header("Authorization", "Bearer " + self.token)
        try:
            with self._opener.open(req, timeout=self.timeout) as resp:
                return resp.status, dict(resp.headers.items()), resp.read()
        except urllib.error.HTTPError as err:
            try:
                return err.code, dict(err.headers.items()), err.read()
            finally:
                err.close()
        except (urllib.error.URLError, OSError) as err:
            if _is_timeout(err):
                raise RequestTimeout("{} {} timed out".format(method, path)) from err
            raise ConnectionFailed("{} {}: {}".format(method, self.base_url + path, err)) from err

    def _backoff(self, attempt: int, retry_after: Optional[float]) -> float:
        delay = min(self.retry_delay * (2 ** (attempt - 1)), self.retry_max)
        delay += random.uniform(0, delay / 4)
        if retry_after is not None and retry_after > delay:
            delay = min(retry_after, self.retry_max)
        return delay

    def _request(self, method: str, path: str, payload: Any = None, accept: str = "application/json"):
        body = None if payload is None else json.dumps(payload).encode("utf-8")
        attempts = 1 + (self.retries if method in _IDEMPOTENT else 0)
        last: Optional[BaseException] = None
        retry_after: Optional[float] = None
        for attempt in range(attempts):
            if attempt:
                self._sleep(self._backoff(attempt, retry_after))
            try:
                status, headers, raw = self._once(method, path, body, accept)
            except (ConnectionFailed, RequestTimeout) as err:
                last, retry_after = err, None
                if attempt + 1 < attempts:
                    continue
                raise
            if (status == 429 or status >= 500) and attempt + 1 < attempts:
                value = headers.get("Retry-After")
                try:
                    retry_after = float(value) if value is not None else None
                except ValueError:
                    retry_after = None
                continue
            return status, headers, raw
        assert last is not None
        raise last

    def _json(self, method: str, path: str, payload: Any = None) -> Any:
        status, headers, raw = self._request(method, path, payload)
        if not 200 <= status < 300:
            raise _fleet_error(status, raw, headers)
        if not raw:
            return None
        try:
            return json.loads(raw.decode("utf-8"))
        except ValueError as err:
            raise ApiError(status, raw.decode("utf-8", errors="replace"), "response was not JSON") from err

    def _text(self, path: str, accept: str) -> str:
        status, headers, raw = self._request("GET", path, None, accept)
        if not 200 <= status < 300:
            raise _fleet_error(status, raw, headers)
        return raw.decode("utf-8", errors="replace")

    def request(self, method: str, path: str, payload: Any = None) -> Any:
        """Any fleet route this client does not wrap yet; returns the decoded JSON (or None)."""
        return self._json(method.upper(), path, payload)

    def health(self) -> bool:
        """``GET /healthz``."""
        return bool((self._json("GET", "/healthz") or {}).get("ok"))

    # --------------------------------------------------------------- tenants

    def list_tenants(self) -> List[Dict[str, Any]]:
        """``GET /fleet/tenants`` (operator)."""
        return self._json("GET", "/fleet/tenants")["items"]

    def create_tenant(self, id: str, display_name: Optional[str] = None,
                      quota: Optional[Mapping[str, int]] = None) -> Dict[str, Any]:
        """``POST /fleet/tenants`` (operator) -> ``{"tenant", "token", "note"}``; the token is shown once."""
        body: Dict[str, Any] = {"id": id, "quota": dict(quota or {})}
        if display_name is not None:
            body["display_name"] = display_name
        return self._json("POST", "/fleet/tenants", body)

    def get_tenant(self, id: str) -> Dict[str, Any]:
        return self._json("GET", "/fleet/tenants/" + _seg(id))

    def me(self) -> Dict[str, Any]:
        """``GET /fleet/tenants/me`` (a tenant token or key)."""
        return self._json("GET", "/fleet/tenants/me")

    def update_tenant(self, id: str, quota: Optional[Mapping[str, int]] = None, disabled: Optional[bool] = None,
                      display_name: Optional[str] = None) -> Dict[str, Any]:
        body = {k: v for k, v in (("quota", quota), ("disabled", disabled), ("display_name", display_name))
                if v is not None}
        return self._json("PATCH", "/fleet/tenants/" + _seg(id), body)

    def delete_tenant(self, id: str) -> None:
        self._json("DELETE", "/fleet/tenants/" + _seg(id))

    def rotate_tenant_token(self, id: str) -> Dict[str, Any]:
        return self._json("POST", "/fleet/tenants/{}/rotate-token".format(_seg(id)))

    def set_budget(self, monthly: Optional[float], tenant: str = ME) -> Dict[str, Any]:
        """``PUT /fleet/tenants/{id|me}/budget``; None or 0 removes it."""
        return self._json("PUT", "/fleet/tenants/{}/budget".format(_seg(tenant)), {"monthly": monthly})

    # -------------------------------------------------------------- API keys

    def list_keys(self, tenant: str = ME) -> List[Dict[str, Any]]:
        return self._json("GET", "/fleet/tenants/{}/keys".format(_seg(tenant)))["items"]

    def create_key(self, name: str, read_only: bool = False, expires_in_days: Optional[int] = None,
                   tenant: str = ME) -> Dict[str, Any]:
        """``POST /fleet/tenants/{id|me}/keys`` -> ``{"key", "token", "note"}``; the token is shown once."""
        body: Dict[str, Any] = {"name": name, "read_only": read_only}
        if expires_in_days is not None:
            body["expires_in_days"] = expires_in_days
        return self._json("POST", "/fleet/tenants/{}/keys".format(_seg(tenant)), body)

    def revoke_key(self, key: str, tenant: str = ME) -> Dict[str, Any]:
        """``DELETE /fleet/tenants/{id|me}/keys/{id or name}`` -> the revoked key."""
        return self._json("DELETE", "/fleet/tenants/{}/keys/{}".format(_seg(tenant), _seg(key)))["revoked"]

    # ------------------------------------------------------- nodes, capacity

    def list_nodes(self) -> List[Dict[str, Any]]:
        """``GET /fleet/nodes`` (operator)."""
        return self._json("GET", "/fleet/nodes")["items"]

    def get_node(self, name: str) -> Dict[str, Any]:
        """``GET /fleet/nodes/{name}``: a tenant sees only a Mac it leases."""
        return self._json("GET", "/fleet/nodes/" + _seg(name))

    def capacity(self) -> Dict[str, Any]:
        return self._json("GET", "/fleet/capacity")

    # ---------------------------------------------------------------- leases

    def create_lease(self, hours: Optional[int] = None, node: Optional[str] = None,
                     node_selector: Optional[Mapping[str, str]] = None, min_vcpus: Optional[int] = None,
                     min_memory_mib: Optional[int] = None, nested_virtualization: bool = False,
                     tenant: Optional[str] = None) -> Dict[str, Any]:
        """``POST /fleet/leases``: a whole Mac for at least 24 hours -> ``{"lease", "node"}``."""
        body: Dict[str, Any] = {}
        for key, value in (("hours", hours), ("node", node), ("nodeSelector", node_selector),
                           ("min_vcpus", min_vcpus), ("min_memory_mib", min_memory_mib), ("tenant", tenant)):
            if value is not None:
                body[key] = dict(value) if isinstance(value, Mapping) else value
        if nested_virtualization:
            body["nested_virtualization"] = True
        return self._json("POST", "/fleet/leases", body)

    def list_leases(self, state: Optional[str] = None, tenant: Optional[str] = None) -> List[Dict[str, Any]]:
        return self._json("GET", "/fleet/leases" + _query(state=state, tenant=tenant))["items"]

    def get_lease(self, id: str) -> Dict[str, Any]:
        return self._json("GET", "/fleet/leases/" + _seg(id))

    def release_lease(self, id: str, force: bool = False) -> Dict[str, Any]:
        """``POST /fleet/leases/{id}/release``; refused (Conflict) before the 24-hour minimum unless forced."""
        return self._json("POST", "/fleet/leases/{}/release{}".format(_seg(id), "?force=true" if force else ""))

    # ------------------------------------------------------------------- VMs

    def launch_vm(self, name: str, image: str, guest_os: str = "macos", vcpus: Optional[int] = None,
                  memory_mib: Optional[int] = None, node: Optional[str] = None,
                  networks: Optional[Sequence[str]] = None, volumes: Optional[Sequence[str]] = None,
                  ttl_seconds: Optional[int] = None, disk_size_gib: Optional[int] = None,
                  node_selector: Optional[Mapping[str, str]] = None, reschedulable: bool = False,
                  tenant: Optional[str] = None, extra: Optional[Mapping[str, Any]] = None) -> Dict[str, Any]:
        """``POST /fleet/vms`` -> ``{"node", "vm"}``. ``image`` is the disk path on the Mac
        (see :meth:`launch_from_catalog`); ``extra`` adds further create-request fields."""
        body: Dict[str, Any] = dict(extra or {})
        apple: Dict[str, Any] = dict(body.get("apple") or {})
        apple["guest_os"] = guest_os
        if networks:
            apple["networks"] = [{"name": n} for n in networks]
        body.update({"name": name, "backend": body.get("backend", "vz"), "image": image, "apple": apple})
        for key, value in (("vcpus", vcpus), ("memory_mib", memory_mib), ("node", node),
                           ("ttl_seconds", ttl_seconds), ("disk_size_gib", disk_size_gib), ("tenant", tenant)):
            if value is not None:
                body[key] = value
        if volumes:
            body["volumes"] = list(volumes)
        if node_selector:
            body["nodeSelector"] = dict(node_selector)
        if reschedulable:
            body["reschedulable"] = True
        return self._json("POST", "/fleet/vms", body)

    def launch_from_catalog(self, name: str, image: str, **kwargs: Any) -> Dict[str, Any]:
        """Launch from the image catalog entry ``image``, with its size as the default."""
        entry = self.get_image(image)
        kwargs.setdefault("vcpus", entry.get("vcpus"))
        kwargs.setdefault("memory_mib", entry.get("memory_mib"))
        if entry.get("disk_size_gib") is not None:
            kwargs.setdefault("disk_size_gib", entry["disk_size_gib"])
        kwargs.setdefault("guest_os", entry["guest_os"])
        return self.launch_vm(name, entry["image"], **kwargs)

    def list_vms(self) -> Dict[str, Any]:
        """``GET /fleet/vms`` -> ``{"items", "unreachable_nodes"}``."""
        return self._json("GET", "/fleet/vms")

    def get_vm(self, node: str, id: str) -> Dict[str, Any]:
        """One VM from ``GET /fleet/vms`` (there is no single-VM route). NotFound when it is not listed;
        ServiceUnavailable when its Mac did not answer."""
        listing = self.list_vms()
        for vm in listing.get("items", []):
            if vm.get("node") == node and vm.get("id") == id:
                return vm
        for down in listing.get("unreachable_nodes", []):
            if down.get("node") == node:
                raise ServiceUnavailable(503, down, "node '{}' did not answer: {}".format(node, down.get("reason")))
        raise NotFound(404, None, "no VM '{}' on node '{}'".format(id, node))

    def find_vm(self, name: str) -> Dict[str, Any]:
        matches = [vm for vm in self.list_vms().get("items", []) if vm.get("name") == name]
        if not matches:
            raise NotFound(404, None, "no VM named '{}'".format(name))
        if len(matches) > 1:
            raise Conflict(409, None, "more than one VM is named '{}'".format(name))
        return matches[0]

    def delete_vm(self, node: str, id: str) -> None:
        self._json("DELETE", "/fleet/vms/{}/{}".format(_seg(node), _seg(id)))

    def power_vm(self, node: str, id: str, action: str) -> Dict[str, Any]:
        """``POST /fleet/vms/{node}/{id}/{start|stop|restart|pause|resume}``."""
        if action not in POWER_ACTIONS:
            raise ValueError("action must be one of " + ", ".join(POWER_ACTIONS))
        return self._json("POST", "/fleet/vms/{}/{}/{}".format(_seg(node), _seg(id), action))

    def exec_vm(self, node: str, id: str, command: str, timeout_seconds: Optional[int] = None) -> Dict[str, Any]:
        """``POST /fleet/vms/{node}/{id}/exec`` -> the guest agent's ``{"exit_code", "stdout", "stderr"}``."""
        body: Dict[str, Any] = {"command": command}
        if timeout_seconds is not None:
            body["timeout_seconds"] = timeout_seconds
        return self._json("POST", "/fleet/vms/{}/{}/exec".format(_seg(node), _seg(id)), body)

    def vm_logs(self, node: str, id: str, lines: Optional[int] = None) -> str:
        return self._text("/fleet/vms/{}/{}/logs{}".format(_seg(node), _seg(id), _query(lines=lines)), "text/plain")

    # -------------------------------------------------------------- networks

    def list_networks(self) -> List[Dict[str, Any]]:
        return self._json("GET", "/fleet/overlays")["items"]

    def create_network(self, name: str, nodes: Optional[Sequence[str]] = None, hosts_per_node: Optional[int] = None,
                       mtu: Optional[int] = None, tenant: Optional[str] = None) -> Dict[str, Any]:
        body: Dict[str, Any] = {"name": name}
        for key, value in (("nodes", list(nodes) if nodes is not None else None), ("hosts_per_node", hosts_per_node),
                           ("mtu", mtu), ("tenant", tenant)):
            if value is not None:
                body[key] = value
        return self._json("POST", "/fleet/overlays", body)

    def get_network(self, name: str) -> Dict[str, Any]:
        return self._json("GET", "/fleet/overlays/" + _seg(name))

    def delete_network(self, name: str) -> None:
        self._json("DELETE", "/fleet/overlays/" + _seg(name))

    def add_network_member(self, name: str, node: str) -> Dict[str, Any]:
        return self._json("POST", "/fleet/overlays/{}/members".format(_seg(name)), {"node": node})

    def remove_network_member(self, name: str, node: str) -> Dict[str, Any]:
        return self._json("DELETE", "/fleet/overlays/{}/members/{}".format(_seg(name), _seg(node)))

    def rotate_network_key(self, name: str) -> Dict[str, Any]:
        return self._json("POST", "/fleet/overlays/{}/rotate".format(_seg(name)))

    # ---------------------------------------------------------------- images

    def list_images(self) -> List[Dict[str, Any]]:
        return self._json("GET", "/fleet/images")["items"]

    def get_image(self, name: str) -> Dict[str, Any]:
        for entry in self.list_images():
            if entry.get("name") == name:
                return entry
        raise NotFound(404, None, "no image '{}' in the catalog".format(name))

    def put_image(self, name: str, image: str, guest_os: str, **fields: Any) -> Dict[str, Any]:
        """``PUT /fleet/images/{name}`` (operator); ``fields``: display_name, description, vcpus, memory_mib,
        disk_size_gib, source."""
        body = dict(fields, image=image, guest_os=guest_os)
        return self._json("PUT", "/fleet/images/" + _seg(name), body)

    def delete_image(self, name: str) -> None:
        self._json("DELETE", "/fleet/images/" + _seg(name))

    # -------------------------------------------------- webhooks, notifications

    def list_webhooks(self) -> Dict[str, Any]:
        """``GET /fleet/webhooks`` -> ``{"items", "kinds"}`` (never the secrets)."""
        return self._json("GET", "/fleet/webhooks")

    def create_webhook(self, url: str, events: Optional[Sequence[str]] = None) -> Dict[str, Any]:
        """``POST /fleet/webhooks`` -> ``{"webhook", "secret", "note"}``; the secret is shown once."""
        body: Dict[str, Any] = {"url": url}
        if events:
            body["events"] = list(events)
        return self._json("POST", "/fleet/webhooks", body)

    def delete_webhook(self, id: str) -> None:
        self._json("DELETE", "/fleet/webhooks/" + _seg(id))

    def test_webhook(self, id: str) -> Dict[str, Any]:
        return self._json("POST", "/fleet/webhooks/{}/test".format(_seg(id)))["delivery"]

    def notifications(self, limit: Optional[int] = None) -> List[Dict[str, Any]]:
        return self._json("GET", "/fleet/notifications" + _query(limit=limit))["items"]

    # ------------------------------------------------- prices, billing, usage

    def prices(self) -> Dict[str, Any]:
        return self._json("GET", "/fleet/prices")

    def set_prices(self, prices: Mapping[str, Any]) -> Dict[str, Any]:
        """``PUT /fleet/prices`` (operator)."""
        return self._json("PUT", "/fleet/prices", dict(prices))

    def billing(self, month: Optional[str] = None, tenant: Optional[str] = None) -> Dict[str, Any]:
        """``GET /fleet/billing`` -> ``{"from", "to", "currency", "prices_set", "statements"}``."""
        return self._json("GET", "/fleet/billing" + _query(month=month, tenant=tenant))

    def billing_csv(self, month: Optional[str] = None, tenant: Optional[str] = None) -> str:
        return self._text("/fleet/billing" + _query(month=month, tenant=tenant, format="csv"), "text/csv")

    def usage(self, start: Optional[str] = None, end: Optional[str] = None,
              tenant: Optional[str] = None) -> Dict[str, Any]:
        """``GET /fleet/usage`` (``start``/``end`` are RFC 3339)."""
        return self._json("GET", "/fleet/usage" + _query(**{"from": start, "to": end, "tenant": tenant}))

    def events(self, limit: Optional[int] = None, tenant: Optional[str] = None) -> List[Dict[str, Any]]:
        """``GET /fleet/events``: the activity log, newest first."""
        return self._json("GET", "/fleet/events" + _query(limit=limit, tenant=tenant))["items"]

    # --------------------------------------------- volumes (central with volumes)

    def list_volumes(self) -> List[Dict[str, Any]]:
        return self._json("GET", "/fleet/volumes")["items"]

    def create_volume(self, name: str, size_gib: int, node: Optional[str] = None) -> Dict[str, Any]:
        body: Dict[str, Any] = {"name": name, "size_gib": size_gib}
        if node is not None:
            body["node"] = node
        return self._json("POST", "/fleet/volumes", body)

    def get_volume(self, name: str) -> Dict[str, Any]:
        return self._json("GET", "/fleet/volumes/" + _seg(name))

    def resize_volume(self, name: str, size_gib: int) -> Dict[str, Any]:
        return self._json("POST", "/fleet/volumes/{}/resize".format(_seg(name)), {"size_gib": size_gib})

    def delete_volume(self, name: str) -> None:
        self._json("DELETE", "/fleet/volumes/" + _seg(name))

    # ------------------------------------------------ pools (central with pools)

    def list_pools(self) -> List[Dict[str, Any]]:
        return self._json("GET", "/fleet/pools")["items"]

    def create_pool(self, name: str, image: str, min_ready: int, vcpus: Optional[int] = None,
                    memory_mib: Optional[int] = None, max_total: Optional[int] = None,
                    nodes: Optional[Sequence[str]] = None) -> Dict[str, Any]:
        body: Dict[str, Any] = {"name": name, "image": image, "min_ready": min_ready}
        for key, value in (("vcpus", vcpus), ("memory_mib", memory_mib), ("max_total", max_total),
                           ("nodes", list(nodes) if nodes is not None else None)):
            if value is not None:
                body[key] = value
        return self._json("POST", "/fleet/pools", body)

    def get_pool(self, name: str) -> Dict[str, Any]:
        return self._json("GET", "/fleet/pools/" + _seg(name))

    def update_pool(self, name: str, **fields: Any) -> Dict[str, Any]:
        """``PATCH /fleet/pools/{name}`` with any of image, vcpus, memory_mib, min_ready, max_total, nodes."""
        return self._json("PATCH", "/fleet/pools/" + _seg(name), {k: v for k, v in fields.items() if v is not None})

    def delete_pool(self, name: str) -> None:
        self._json("DELETE", "/fleet/pools/" + _seg(name))
