#!/usr/bin/env python3

import io
import os
import sys
import json
import secrets
import urllib.request
import urllib.error

API_BASE = os.environ.get("API_BASE", "http://127.0.0.1:8080")
SET_NAME = os.environ.get("SET_NAME", "Sample Set")


def dev_token():
	player = os.environ.get("DEV_PLAYER", "a5331404-0e77-440e-8bef-24c071dac1ae")
	req = urllib.request.Request(
		f"{API_BASE}/v1/dashboard/dev-login",
		data=json.dumps({"player": player}).encode(),
		method="POST",
		headers={"Content-Type": "application/json"},
	)
	try:
		with urllib.request.urlopen(req) as response:
			return "Bearer " + json.load(response)["token"]
	except urllib.error.URLError as error:
		sys.exit(f"error: dev login failed (debug build running at {API_BASE}?): {error}")


AUTH = dev_token()


def request(method, path, *, content_type=None, body=None):
	req = urllib.request.Request(f"{API_BASE}{path}", data=body, method=method)
	req.add_header("Authorization", AUTH)
	if content_type:
		req.add_header("Content-Type", content_type)
	try:
		with urllib.request.urlopen(req) as response:
			return response.status, response.read().decode("utf-8", "replace")
	except urllib.error.HTTPError as error:
		return error.code, error.read().decode("utf-8", "replace")
	except urllib.error.URLError as error:
		sys.exit(f"error: cannot reach {API_BASE}: {error}")


def post_json(path, payload):
	return request(
		"POST",
		path,
		content_type="application/json",
		body=json.dumps(payload).encode("utf-8"),
	)


def encode_multipart(fields, files):
	boundary = "----polyset" + secrets.token_hex(16)
	body = io.BytesIO()

	def write(text):
		body.write(text.encode("utf-8"))

	for name, value in fields:
		write(f"--{boundary}\r\n")
		write(f'Content-Disposition: form-data; name="{name}"\r\n\r\n')
		write(f"{value}\r\n")
	for name, filename, data in files:
		write(f"--{boundary}\r\n")
		write(
			f'Content-Disposition: form-data; name="{name}"; filename="{filename}"\r\n'
		)
		write("Content-Type: application/zip\r\n\r\n")
		body.write(data)
		write("\r\n")
	write(f"--{boundary}--\r\n")

	return f"multipart/form-data; boundary={boundary}", body.getvalue()


def requirements():
	status, text = request("GET", "/cosmetics")
	if status != 200:
		sys.exit(f"error: GET /cosmetics failed: {status} {text}")

	picked = []
	for cosmetic in json.loads(text).get("cosmetics", []):
		variants = cosmetic.get("variants", [])
		if not variants:
			continue
		if len(variants) > 1:
			picked.append({"group_id": cosmetic["id"]})
		else:
			picked.append({"cosmetic_id": variants[0]["id"]})
		if len(picked) == 2:
			return picked

	sys.exit("error: need at least two cosmetics to make a set; seed cosmetics first")


def main():
	effect = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("EFFECT_ZIP")
	needed = requirements()

	status, text = post_json(
		"/cosmetics/manage/sets/create",
		{"name": SET_NAME, "priority": 0, "requirements": needed},
	)
	if status != 200:
		sys.exit(f"error: creating the set failed: {status} {text.strip()[:200]}")
	created = json.loads(text)
	print(f"created set {created['id']} ({created['name']}) needing {needed}")

	if not effect:
		print("no effect bundle given; pass a zip path to attach one")
		return

	with open(effect, "rb") as handle:
		data = handle.read()
	content_type, body = encode_multipart(
		[("set_id", str(created["id"]))],
		[("file", os.path.basename(effect), data)],
	)
	status, text = request(
		"POST", "/cosmetics/manage/sets/effect", content_type=content_type, body=body
	)
	if status != 200:
		sys.exit(f"error: uploading the effect failed: {status} {text.strip()[:200]}")
	print(f"attached effect {json.loads(text).get('effect_url')}")


if __name__ == "__main__":
	main()
