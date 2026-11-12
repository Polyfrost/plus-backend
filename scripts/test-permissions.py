#!/usr/bin/env python3
"""Integration test for dashboard sign-in, roles and permissions.

Needs a *debug* backend (for /v1/dashboard/dev-login) on a DISPOSABLE
database: it creates players and roles and calls every permission-guarded
route, some of which change or delete things. Refuses non-local API_BASEs.

  API_BASE             default http://localhost:8080
  ADMIN_PLAYER         a player holding ADMINISTRATOR (default: the seeded Wyvest)
  DASHBOARD_ORIGIN     an origin in DASHBOARD_ORIGINS (default http://localhost:3000)
  SPECIAL_CHAT_TARGET  a uuid in SPECIAL_CHAT_TARGETS; chat checks skip without it

Only the Python standard library is used. Exits non-zero on any failure.
"""

import json
import os
import socket
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

API = os.environ.get("API_BASE", "http://localhost:8080").rstrip("/")
ADMIN = os.environ.get("ADMIN_PLAYER", "a5331404-0e77-440e-8bef-24c071dac1ae")
ORIGIN = os.environ.get("DASHBOARD_ORIGIN", "http://localhost:3000")
CHAT_TARGET = os.environ.get("SPECIAL_CHAT_TARGET")
COOKIE = "__Host-plus_session"

ADMINISTRATOR, MANAGE_ROLES, VIEW_ANALYTICS = 1 << 0, 1 << 1, 1 << 2
MANAGE_API_TOKENS, USE_SPECIAL_CHAT = 1 << 8, 1 << 9
VIEW_REVENUE, MANAGE_REFERRALS = 1 << 10, 1 << 13
KEY_FORBIDDEN = ADMINISTRATOR | MANAGE_ROLES | MANAGE_API_TOKENS
UNUSED_BIT = 1 << 40
U64 = (1 << 64) - 1

# Routes that must stay guarded; a missing one means a gate was dropped.
MUST_BE_GUARDED = {
	("get", "/v1/roles"), ("post", "/v1/roles"), ("put", "/v1/roles/{id}"),
	("delete", "/v1/roles/{id}"), ("put", "/v1/players/{player}/roles/{id}"),
	("delete", "/v1/players/{player}/roles/{id}"), ("get", "/v1/tokens"),
	("post", "/v1/tokens"), ("get", "/v1/analytics/health"),
	("post", "/v1/admin/special-chat/token"), ("get", "/v1/admin/special-chat/targets"),
	("post", "/cosmetics/grant"), ("get", "/links"), ("post", "/links"),
	("get", "/players/by-link/{provider}/{provider_user_id}"),
}

failures = []


def check(ok, label):
	print(("  ok   " if ok else "  FAIL ") + label)
	if not ok:
		failures.append(label)


def call(method, path, token=None, cookie=None, origin=None, body=None, headers=None, key=None):
	"""Waits out the global per-address limiter, which this many requests trip."""
	while True:
		status, text, response_headers = _call(method, path, token, cookie, origin, body, headers, key)
		retry = response_headers.get("Retry-After") if status == 429 else None
		if retry is None:
			return status, text, response_headers
		time.sleep(int(retry) + 1)


def _call(method, path, token, cookie, origin, body, headers, key):
	request = urllib.request.Request(API + path, method=method.upper())
	if key:
		request.add_header("X-Api-Token", key)
	if token:
		request.add_header("Authorization", f"Bearer {token}")
	if cookie:
		request.add_header("Cookie", cookie)
	if origin:
		request.add_header("Origin", origin)
	for name, value in (headers or {}).items():
		request.add_header(name, value)
	if body is not None:
		request.data = json.dumps(body).encode()
		request.add_header("Content-Type", "application/json")
	try:
		with urllib.request.urlopen(request) as response:
			return response.status, response.read().decode(), response.headers
	except urllib.error.HTTPError as error:
		return error.code, error.read().decode(), error.headers


def dev_login(player, game=False):
	"""A dashboard session token, or with `game` the kind `/login` gives the game."""
	status, text, _ = call("post", "/v1/dashboard/dev-login", body={"player": player, "game": game})
	assert status == 200, f"dev-login {player}: {status} {text}"
	return json.loads(text)["token"]


def create_role(admin, permissions, position=0, name=None):
	name = name or f"t-{uuid.uuid4().hex[:10]}"
	status, text, _ = call("post", "/v1/roles", admin, body={
		"name": name, "color": None, "position": position, "permissions": str(permissions),
	})
	assert status == 201, f"create role: {status} {text}"
	return json.loads(text)["id"]


def assign(admin, player, role):
	status, text, _ = call("put", f"/v1/players/{player}/roles/{role}", admin)
	assert status == 204, f"assign: {status} {text}"


def create_key(creator, permissions, prefixes=()):
	status, text, _ = call("post", "/v1/tokens", creator, body={
		"label": f"k-{uuid.uuid4().hex[:8]}", "exempt_prefixes": list(prefixes),
		"permissions": str(permissions),
	})
	assert status == 201, f"create key: {status} {text}"
	created = json.loads(text)
	return created["token"], created["id"]


def player_with(admin, permissions, position=0):
	"""A fresh player holding one role with exactly `permissions`."""
	player = str(uuid.uuid4())
	role = create_role(admin, permissions, position)
	assign(admin, player, role)
	return player, dev_login(player), role


def fill(path, operation):
	"""Fills path parameters with values that parse but match nothing."""
	for param in operation.get("parameters", []):
		if param.get("in") != "path":
			continue
		schema = param.get("schema", {})
		if schema.get("type") == "integer":
			value = "999999999"
		elif schema.get("format") == "uuid":
			value = str(uuid.uuid4())
		elif "enum" in schema:
			value = schema["enum"][0]
		else:
			value = "zz-test"
		path = path.replace("{" + param["name"] + "}", urllib.parse.quote(value))
	return path


def guarded_routes():
	routes = []
	for spec_path in ("/openapi.json", "/v1/openapi.json"):
		status, text, _ = call("get", spec_path)
		assert status == 200, f"{spec_path}: {status}"
		for path, methods in json.loads(text)["paths"].items():
			for method, operation in methods.items():
				if not isinstance(operation, dict):
					continue
				if "x-required-permission" in operation:
					routes.append((method, path, int(operation["x-required-permission"]), operation))
				elif "x-required-any-permission" in operation:
					# Any one flag of the mask opens it: test with the lowest.
					mask = int(operation["x-required-any-permission"])
					routes.append((method, path, mask & -mask, dict(operation, any_mask=mask)))
	return routes


def test_route_matrix(admin):
	print("Every guarded route: anonymous, wrong flags, all other flags, exact flag, admin")
	routes = guarded_routes()
	found = {(method, path) for method, path, _, _ in routes}
	missing = MUST_BE_GUARDED - found
	check(not missing, f"{len(routes)} guarded routes, none of the must-guard ones missing {sorted(missing) or ''}")

	tokens, keys = {}, {}
	_, unrelated, _ = player_with(admin, UNUSED_BIT)
	admin_game = dev_login(ADMIN, game=True)
	for method, path, required, operation in routes:
		cache = (required, operation.get("any_mask"))
		if cache not in tokens:
			_, exact, _ = player_with(admin, required)
			_, others, _ = player_with(admin, U64 & ~operation.get("any_mask", required) & ~ADMINISTRATOR)
			tokens[cache] = (exact, others)
		exact, others = tokens[cache]
		if cache not in keys:
			keyable = required & KEY_FORBIDDEN == 0
			keys[cache] = (
				create_key(admin, required)[0] if keyable else None,
				create_key(admin, U64 & ~KEY_FORBIDDEN & ~operation.get("any_mask", required))[0],
			)
		exact_key, other_key = keys[cache]
		url = fill(path, operation)
		label = f"{method.upper()} {path}"

		status, _, _ = call(method, url)
		check(status == 401, f"{label}: anonymous -> {status}")
		status, _, _ = call(method, url, unrelated)
		check(status == 403, f"{label}: unrelated flag -> {status}")
		status, _, _ = call(method, url, others)
		check(status == 403, f"{label}: every flag but its own -> {status}")
		status, _, _ = call(method, url, exact)
		check(status not in (401, 403), f"{label}: exactly its flag -> {status}")
		status, _, _ = call(method, url, admin)
		check(status not in (401, 403), f"{label}: administrator -> {status}")
		status, _, _ = call(method, url, admin_game)
		check(status == 403, f"{label}: administrator's game token -> {status}")
		status, _, _ = call(method, url, key=other_key)
		check(status == 403, f"{label}: key with every other flag -> {status}")
		if exact_key:
			status, _, _ = call(method, url, key=exact_key)
			check(status not in (401, 403), f"{label}: key with exactly its flag -> {status}")


def test_credentials(admin):
	print("Credentials: bearer, cookie, origin, the old password")
	good = f"{COOKIE}={admin}"
	check(call("get", "/v1/me", admin)[0] == 200, "bearer token")
	check(call("get", "/v1/me", cookie=good, origin=ORIGIN)[0] == 200, "cookie from the dashboard")
	check(call("get", "/v1/me", cookie=f"a=1; {good}; b=2", origin=ORIGIN)[0] == 200, "cookie among others")
	check(call("get", "/v1/me", cookie=good)[0] == 401, "cookie without Origin is refused")
	check(call("get", "/v1/me", cookie=good, origin="https://store.polyfrost.org")[0] == 401, "cookie from a sibling subdomain is refused")
	check(call("get", "/v1/me", cookie=good, origin="null")[0] == 401, "cookie from an opaque origin is refused")
	check(call("get", "/v1/me", cookie=f"plus_session={admin}", origin=ORIGIN)[0] == 401, "unprefixed cookie name is ignored")
	check(call("get", "/v1/me", cookie=f"{COOKIE}x={admin}", origin=ORIGIN)[0] == 401, "lookalike cookie name is ignored")
	check(call("get", "/v1/analytics/health", headers={"Authorization": "dev"})[0] == 401, "old admin password header is refused")
	tampered = admin[:-2] + ("AA" if not admin.endswith("AA") else "BB")
	check(call("get", "/v1/me", tampered)[0] == 401, "tampered token is refused")
	check(call("get", "/v1/me", "v4.local.garbage")[0] == 401, "garbage token is refused")
	check(call("get", "/v1/me", headers={"Authorization": f"Basic {admin}"})[0] == 401, "non-bearer scheme is refused")

	game = dev_login(ADMIN, game=True)
	check(call("get", "/v1/me", game)[0] == 200, "a game token still reads its own player")
	check(call("get", "/v1/roles", game)[0] == 403, "an administrator's game token carries no permissions")
	check(call("get", f"/cosmetics/player?player={ADMIN}", cookie=good, origin="https://store.polyfrost.org")[0] == 200,
		"a stray cookie from a sibling page leaves an optional-auth call anonymous")


def test_session_endpoints(admin):
	print("Session endpoints")
	status, _, headers = call("post", "/v1/dashboard/dev-login", body={"player": ADMIN})
	cookie = headers.get("Set-Cookie", "")
	for attribute in (f"{COOKIE}=", "HttpOnly", "Secure", "SameSite=Strict", "Path=/"):
		check(attribute in cookie, f"session cookie has {attribute}")
	check("Domain=" not in cookie, "session cookie is host-only")

	status, _, headers = call("post", "/v1/dashboard/logout", origin=ORIGIN)
	check(status == 204 and "Max-Age=0" in headers.get("Set-Cookie", ""), "logout clears the cookie")
	check(call("post", "/v1/dashboard/logout")[0] == 403, "logout needs the dashboard origin")
	check(call("post", "/v1/dashboard/login", body={"user_hash": "x", "xsts_token": "y"})[0] == 403, "login needs the dashboard origin")
	check(call("post", "/v1/dashboard/login", origin="https://store.polyfrost.org", body={"user_hash": "x", "xsts_token": "y"})[0] == 403, "login refuses a sibling subdomain")
	status, text, _ = call("post", "/v1/dashboard/login", origin=ORIGIN, body={"user_hash": "x", "xsts_token": "y"})
	check(status == 401, f"login with a junk XSTS token is refused by Minecraft -> {status} {text.strip()}")
	check(call("post", "/v1/dashboard/dev-login", body={"player": str(uuid.uuid4())})[0] == 403, "players without permissions cannot sign in")

	request = urllib.request.Request(API + "/v1/me", method="OPTIONS")
	for name, value in (("Origin", ORIGIN), ("Access-Control-Request-Method", "GET")):
		request.add_header(name, value)
	with urllib.request.urlopen(request) as response:
		check(response.headers.get("Access-Control-Allow-Credentials") == "true", "CORS allows credentials for the dashboard")
	request = urllib.request.Request(API + "/v1/me", method="OPTIONS")
	request.add_header("Origin", "https://evil.example")
	request.add_header("Access-Control-Request-Method", "GET")
	try:
		with urllib.request.urlopen(request) as response:
			allowed = response.headers.get("Access-Control-Allow-Origin")
	except urllib.error.HTTPError as error:
		allowed = error.headers.get("Access-Control-Allow-Origin")
	check(allowed is None, "CORS does not answer an unknown origin")


def test_transactions(admin):
	print("Other players' transactions")
	viewer, viewer_dashboard, _ = player_with(admin, 1 << 7)  # VIEW_PLAYERS
	viewer_game = dev_login(viewer, game=True)
	other = f"/transactions/player?player={ADMIN}"
	check(call("get", other, viewer_dashboard)[0] == 200, "VIEW_PLAYERS reads them from the dashboard")
	check(call("get", other, viewer_game)[0] == 403, "the same player's game token cannot")
	_, analyst, _ = player_with(admin, VIEW_ANALYTICS)
	check(call("get", other, analyst)[0] == 403, "a dashboard user without VIEW_PLAYERS cannot")
	check(call("get", "/transactions/player", viewer_game)[0] == 200, "a game token still reads its own")


def test_login_rate_limit():
	print("Sign-in rate limit")
	statuses = [
		call("post", "/v1/dashboard/login", origin=ORIGIN, body={"user_hash": "x", "xsts_token": "y"})[0]
		for _ in range(12)
	]
	check(429 in statuses and statuses.count(401) <= 10, f"stops after 10 attempts a minute -> {statuses}")


def test_api_keys(admin):
	print("API keys for external services")
	_, manager, _ = player_with(admin, MANAGE_API_TOKENS | VIEW_ANALYTICS)

	def make(creator, permissions, prefixes=()):
		return call("post", "/v1/tokens", creator, body={
			"label": "x", "exempt_prefixes": list(prefixes), "permissions": str(permissions),
		})[0]
	check(make(manager, VIEW_ANALYTICS) == 201, "a manager issues a key with a flag it holds")
	check(make(manager, 1 << 4) == 403, "but not with a flag it lacks")
	for name, flag in (("ADMINISTRATOR", ADMINISTRATOR), ("MANAGE_ROLES", MANAGE_ROLES), ("MANAGE_API_TOKENS", MANAGE_API_TOKENS)):
		check(make(admin, flag) == 400, f"no key may hold {name}, even from an administrator")
	check(make(admin, 0) == 400, "a key with neither prefixes nor permissions is refused")
	check(make(admin, 0, ["asset/"]) == 400, "relative prefixes are refused")
	check(call("post", "/v1/tokens", admin, body={"label": "x", "permissions": "-1"})[0] == 400, "negative permissions are refused")
	check(make(admin, 0, ["/asset/"]) == 201, "a pure rate-limit key still works")

	key, key_id = create_key(manager, VIEW_ANALYTICS)
	status, text, _ = call("get", "/v1/tokens", admin)
	listed = next(t for t in json.loads(text) if t["id"] == key_id)
	check(listed["permissions"] == str(VIEW_ANALYTICS) and key not in text, "listing shows its permissions, never the secret")
	check(call("get", "/v1/analytics/overview", key=key)[0] == 200, "the key reads the analytics aggregations")
	check(call("get", "/v1/roles", key=key)[0] == 403, "but nothing else it was not given")
	check(call("post", "/v1/tokens", key=key, body={"label": "x", "permissions": "4"})[0] == 403, "keys cannot mint keys")
	check(call("get", "/v1/me", key=key)[0] == 401, "a key is not a player")
	check(call("get", "/groups", key=key)[0] == 401, "nor reaches player endpoints")
	check(call("get", "/v1/analytics/overview", key=key + "x")[0] == 401, "a wrong key is refused")
	check(call("get", "/v1/analytics/overview", key="")[0] == 401, "an empty key is refused")
	_, nobody, _ = player_with(admin, UNUSED_BIT)
	check(call("get", "/v1/analytics/overview", nobody, key=key)[0] == 403, "with a player token too, the player is who counts")
	check(call("get", "/v1/analytics/overview", "garbage", key=key)[0] == 401, "and a bad player token is not rescued by the key")
	check(call("delete", f"/v1/tokens/{key_id}", admin)[0] == 200, "revoking")
	check(call("get", "/v1/analytics/overview", key=key)[0] == 401, "a revoked key stops working at once")


def test_revenue_split(admin):
	print("Revenue only with VIEW_REVENUE")
	_, analyst, _ = player_with(admin, VIEW_ANALYTICS)
	_, finance, _ = player_with(admin, VIEW_ANALYTICS | VIEW_REVENUE)
	analyst_key, _ = create_key(admin, VIEW_ANALYTICS)
	check(call("get", "/v1/analytics/monetization", analyst)[0] == 403, "monetization needs VIEW_REVENUE")
	check(call("get", "/v1/analytics/monetization", analyst_key)[0] == 401, "(sanity: a key is not a bearer token)")
	check(call("get", "/v1/analytics/monetization", key=analyst_key)[0] == 403, "nor does an analytics-only key get it")
	check(call("get", "/v1/analytics/monetization", finance)[0] == 200, "VIEW_REVENUE opens monetization")
	for who, token, shown in (("analyst", analyst, False), ("finance", finance, True)):
		status, text, _ = call("get", "/v1/analytics/daily", token)
		days = json.loads(text)["days"] if status == 200 else []
		check(status == 200 and days and all((d["transactions_completed"] is not None) == shown for d in days),
			f"daily sales counts {'shown' if shown else 'null'} for {who}")
		status, text, _ = call("get", "/v1/analytics/catalog", token)
		entries = json.loads(text)["cosmetics"] if status == 200 else None
		check(status == 200 and all((e["revenue_minor"] is not None) == shown and (e["refunded_minor"] is not None) == shown
			for e in entries), f"catalog money {'shown' if shown else 'null'} for {who} ({len(entries or [])} rows)")
	_, referrals_only, _ = player_with(admin, MANAGE_REFERRALS)
	code = "T" + uuid.uuid4().hex[:6].upper()
	status, text, _ = call("post", "/v1/admin/referrals", admin, body={"code": code, "name": "Test creator"})
	check(status == 200, f"admin creates a creator code -> {status} {text[:80]}")
	listed = [r for r in json.loads(call("get", "/v1/admin/referrals", referrals_only)[1]) if r["code"] == code]
	check(listed and listed[0]["revenue_minor"] is None, "creator-code revenue is null without VIEW_REVENUE")
	listed = [r for r in json.loads(call("get", "/v1/admin/referrals", admin)[1]) if r["code"] == code]
	check(listed and listed[0]["revenue_minor"] == 0, "and a number with it")
	_, cosmetics_only, _ = player_with(admin, 1 << 3)
	check(call("get", "/v1/admin/catalog", cosmetics_only)[0] == 200, "cosmetics managers can read the catalogue")
	check(call("get", "/v1/admin/orders", cosmetics_only)[0] == 403, "but not orders")


def test_role_validation(admin):
	print("Role validation")
	def make(**fields):
		body = {"name": f"v-{uuid.uuid4().hex[:8]}", "color": None, "position": 0, "permissions": "0"}
		body.update(fields)
		return call("post", "/v1/roles", admin, body=body)
	check(make(name="   ")[0] == 400, "blank name")
	check(make(name="x" * 65)[0] == 400, "name over 64 characters")
	check(make(color=-1)[0] == 400, "negative colour")
	check(make(color=0x1000000)[0] == 400, "colour over 0xFFFFFF")
	check(make(permissions="-1")[0] == 400, "negative permissions")
	check(make(permissions=str(U64 + 1))[0] == 400, "permissions over u64")
	check(make(permissions="abc")[0] == 400, "non-numeric permissions")
	status, text, _ = make(permissions=str(U64), color=0xFFFFFF)
	check(status == 201 and json.loads(text)["permissions"] == str(U64), "u64::MAX round-trips exactly")
	name = f"dup-{uuid.uuid4().hex[:8]}"
	make(name=name)
	check(make(name=name)[0] == 409, "duplicate name")


def test_hierarchy(admin):
	print("Hierarchy: a non-admin manager")
	lead_player, lead, lead_role = player_with(admin, MANAGE_ROLES | VIEW_ANALYTICS, position=50)
	below = create_role(admin, VIEW_ANALYTICS, position=10)
	peer = create_role(admin, 0, position=50)
	above = create_role(admin, 0, position=90)
	admin_role = create_role(admin, ADMINISTRATOR, position=1)

	def make(position, permissions):
		return call("post", "/v1/roles", lead, body={
			"name": f"h-{uuid.uuid4().hex[:8]}", "color": None,
			"position": position, "permissions": str(permissions),
		})[0]
	check(make(49, VIEW_ANALYTICS) == 201, "creates a role below with a flag it holds")
	check(make(49, ADMINISTRATOR) == 403, "cannot create an administrator role")
	check(make(49, USE_SPECIAL_CHAT) == 403, "cannot create a role with a flag it lacks")
	check(make(50, 0) == 403, "cannot create a role at its own position")
	check(make(51, 0) == 403, "cannot create a role above itself")

	def update(role, position, permissions):
		return call("put", f"/v1/roles/{role}", lead, body={
			"name": f"u-{uuid.uuid4().hex[:8]}", "color": 1, "position": position,
			"permissions": str(permissions),
		})[0]
	check(update(below, 20, VIEW_ANALYTICS) == 200, "edits a role below")
	check(update(below, 60, VIEW_ANALYTICS) == 403, "cannot lift a role above itself")
	check(update(below, 20, USE_SPECIAL_CHAT) == 403, "cannot add a flag it lacks")
	check(update(lead_role, 49, MANAGE_ROLES) == 403, "cannot edit its own top role")
	check(update(above, 10, 0) == 403, "cannot pull down a role above it")
	check(update(admin_role, 1, 0) == 403, "cannot strip an administrator role")
	check(call("delete", f"/v1/roles/{admin_role}", lead)[0] == 403, "cannot delete an administrator role")
	check(call("delete", f"/v1/roles/{peer}", lead)[0] == 403, "cannot delete a peer role")

	junior = str(uuid.uuid4())
	check(call("put", f"/v1/players/{junior}/roles/{below}", lead)[0] == 204, "gives a lower role to a new player")
	check(call("delete", f"/v1/players/{junior}/roles/{below}", lead)[0] == 204, "takes it back")
	check(call("put", f"/v1/players/{lead_player}/roles/{below}", lead)[0] == 204, "gives itself a lower role")
	check(call("put", f"/v1/players/{junior}/roles/{peer}", lead)[0] == 403, "cannot hand out a peer role")
	check(call("put", f"/v1/players/{lead_player}/roles/{admin_role}", lead)[0] == 403, "cannot make itself administrator")

	peer_player, _, _ = player_with(admin, UNUSED_BIT, position=50)
	senior_player, _, _ = player_with(admin, UNUSED_BIT, position=90)
	_, admin2, _ = player_with(admin, ADMINISTRATOR, position=0)
	for target, name in ((peer_player, "a peer"), (senior_player, "a superior")):
		check(call("put", f"/v1/players/{target}/roles/{below}", lead)[0] == 403, f"cannot give roles to {name}")
		check(call("delete", f"/v1/players/{target}/roles/{below}", lead)[0] == 403, f"cannot take roles from {name}")
	status, me, _ = call("get", "/v1/me", admin2)
	admin2_player = json.loads(me)["id"]
	check(call("put", f"/v1/players/{admin2_player}/roles/{below}", lead)[0] == 403, "cannot change an administrator's roles")

	check(call("get", "/v1/analytics/health", lead)[0] == 200, "holds VIEW_ANALYTICS while it has the role")
	call("delete", f"/v1/players/{lead_player}/roles/{lead_role}", admin)
	call("delete", f"/v1/players/{lead_player}/roles/{below}", admin)
	check(call("get", "/v1/analytics/health", lead)[0] == 403, "losing the role revokes access on the same token")


def websocket_handshake(token=None, cookie=None, origin=None, probe=None):
	"""Returns (status line, Sec-WebSocket-Protocol, reply) of a raw upgrade.

	With `probe`, sends it as a text frame once upgraded and returns whether
	the server answered anything within two seconds."""
	url = urllib.parse.urlparse(API)
	lines = [
		"GET /websocket HTTP/1.1", f"Host: {url.netloc}", "Upgrade: websocket",
		"Connection: Upgrade", "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==",
		"Sec-WebSocket-Version: 13",
	]
	if token:
		lines.append(f"Sec-WebSocket-Protocol: bearer, {token}")
	if cookie:
		lines.append(f"Cookie: {cookie}")
	if origin:
		lines.append(f"Origin: {origin}")
	with socket.create_connection((url.hostname, url.port or 80), timeout=10) as conn:
		conn.sendall(("\r\n".join(lines) + "\r\n\r\n").encode())
		head = conn.recv(4096).decode(errors="replace").split("\r\n")
		replied = None
		if probe is not None and "101" in head[0]:
			payload = json.dumps(probe).encode()
			assert len(payload) < 126
			mask = os.urandom(4)
			conn.sendall(bytes([0x81, 0x80 | len(payload)]) + mask
				+ bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))
			conn.settimeout(2)
			try:
				replied = len(conn.recv(4096)) > 0
			except socket.timeout:
				replied = False
	protocol = next((h.split(":", 1)[1].strip() for h in head if h.lower().startswith("sec-websocket-protocol:")), None)
	return head[0], protocol, replied


def test_special_chat(admin):
	print("Special Chat")
	_, staff, _ = player_with(admin, USE_SPECIAL_CHAT)
	_, outsider, _ = player_with(admin, VIEW_ANALYTICS)
	check(call("get", "/v1/admin/special-chat/targets", outsider)[0] == 403, "needs USE_SPECIAL_CHAT")
	if not CHAT_TARGET:
		print("  skip SPECIAL_CHAT_TARGET not set")
		return
	check(call("post", "/v1/admin/special-chat/token", staff, body={"uuid": ADMIN})[0] == 404, "cannot mint a token for a non-target")

	# Make the target an administrator: its scoped token must still not carry that.
	admin_role = create_role(admin, ADMINISTRATOR)
	assign(admin, CHAT_TARGET, admin_role)
	status, text, _ = call("post", "/v1/admin/special-chat/token", staff, body={"uuid": CHAT_TARGET})
	check(status == 200, "mints a token for a target")
	scoped = json.loads(text)["token"]
	check(call("get", "/groups", scoped)[0] == 200, "scoped token reads groups")
	check(call("post", "/players/resolve", scoped, body={"ids": []})[0] not in (401, 403), "scoped token resolves names")
	for method, path in (("get", "/v1/me"), ("get", "/v1/roles"), ("get", "/v1/tokens"),
			("get", "/v1/analytics/health"), ("post", "/oidc/authorize"), ("get", "/account/links"),
			("post", "/v1/admin/special-chat/token"), ("post", "/groups"),
			("post", "/groups/1/claim"), ("post", f"/groups/dm/{ADMIN}"),
			("post", f"/groups/1/members/{ADMIN}"), ("get", "/groups/delegations"),
			("get", "/transactions/player"), ("post", "/account/links/discord")):
		check(call(method, path, scoped, body={} if method == "post" else None)[0] == 403, f"scoped token refused at {path}")
	call("delete", f"/v1/players/{CHAT_TARGET}/roles/{admin_role}", admin)

	probe = {"type": "GetActiveCosmetics", "players": []}
	status, protocol, replied = websocket_handshake(token=scoped, probe=probe)
	check("101" in status and protocol == "bearer", f"websocket accepts the bearer subprotocol -> {status}")
	check(replied is False, "scoped websocket only listens: a packet gets no answer")
	_, _, replied = websocket_handshake(token=admin, probe=probe)
	check(replied is True, "a normal websocket answers the same packet (so the probe works)")
	check("401" in websocket_handshake()[0], "websocket refuses anonymous upgrades")
	check("401" in websocket_handshake(cookie=f"{COOKIE}={admin}", origin="https://evil.example")[0], "websocket refuses a cookie from a foreign origin")


def main():
	host = urllib.parse.urlparse(API).hostname
	if host not in ("localhost", "127.0.0.1", "::1"):
		sys.exit(f"refusing to run against {API}: this test changes data")
	admin = dev_login(ADMIN)

	test_credentials(admin)
	test_session_endpoints(admin)
	test_role_validation(admin)
	test_hierarchy(admin)
	test_special_chat(admin)
	test_transactions(admin)
	test_api_keys(admin)
	test_revenue_split(admin)
	test_route_matrix(admin)
	test_login_rate_limit()

	print(f"\n{len(failures)} failure(s)")
	for failure in failures:
		print(f"  - {failure}")
	sys.exit(1 if failures else 0)


if __name__ == "__main__":
	main()
