"""A small app with a real login, a session cookie, a JSON API and a page that
renders itself with fetch(). Used to verify that Scout can sign in as a user it
is authorised to be, and then act as that user."""

import json
import re
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

SESSIONS = {}
ITEMS = [
    {"id": 1, "name": "Alpha", "qty": 3},
    {"id": 2, "name": "Beta", "qty": 5},
]
NEXT_ID = [3]

LOGIN_PAGE = """<!doctype html>
<html><head><title>Sign in &middot; Fixture</title></head>
<body>
  <h1>Sign in</h1>
  <form id="login" method="post" action="/session">
    <input type="hidden" name="csrf_token" value="abc123secret">
    <label>Email <input type="email" name="email" id="email"></label>
    <label>Password <input type="password" name="password" id="password"></label>
    <label><input type="checkbox" name="remember" value="1" checked> Remember me</label>
    <button type="submit">Sign in</button>
  </form>
  <a href="/forgot">Forgot password?</a>
</body></html>
"""

DASHBOARD = """<!doctype html>
<html><head><title>Dashboard &middot; Fixture</title></head>
<body>
  <h1>Welcome, {user}</h1>
  <p class="plan">Plan: {plan}</p>
  <ul id="items"></ul>
  <p id="count"></p>
  <script>
  (async function () {{
    var r = await fetch('/api/items', {{ headers: {{ 'accept': 'application/json' }} }});
    var items = await r.json();
    var ul = document.getElementById('items');
    items.forEach(function (it) {{
      var li = document.createElement('li');
      li.className = 'item';
      li.textContent = it.name + ' x' + it.qty;
      ul.appendChild(li);
    }});
    document.getElementById('count').textContent = 'You have ' + items.length + ' items.';
  }})();
  </script>
</body></html>
"""

FAILED = """<!doctype html>
<html><head><title>Sign in &middot; Fixture</title></head>
<body>
  <h1>Sign in</h1>
  <p class="error">Invalid email or password.</p>
  <form id="login" method="post" action="/session">
    <input type="hidden" name="csrf_token" value="abc123secret">
    <label>Email <input type="email" name="email" id="email"></label>
    <label>Password <input type="password" name="password" id="password"></label>
    <button type="submit">Sign in</button>
  </form>
</body></html>
"""


def session_of(handler):
    raw = handler.headers.get("Cookie", "")
    for part in raw.split(";"):
        part = part.strip()
        if part.startswith("sid="):
            sid = part[4:]
            return sid, SESSIONS.get(sid)
    return None, None


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _send(self, status, body, ctype="text/html; charset=utf-8", headers=None):
        if isinstance(body, str):
            body = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def _json(self, status, obj):
        self._send(status, json.dumps(obj), "application/json")

    def _body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    def do_GET(self):
        path = urlparse(self.path).path
        _, user = session_of(self)
        if path == "/":
            if user:
                return self._send(302, b"", headers={"Location": "/dashboard"})
            return self._send(200, LOGIN_PAGE)
        if path == "/login":
            return self._send(200, LOGIN_PAGE)
        if path in ("/dashboard", "/dashboard/", "/account"):
            if not user:
                return self._send(302, b"", headers={"Location": "/login"})
            return self._send(200, DASHBOARD.format(user=user["email"], plan=user["plan"]))
        if path == "/api/items":
            if not user:
                return self._json(401, {"error": "sign in first"})
            return self._json(200, ITEMS)
        if path == "/api/me":
            if not user:
                return self._json(401, {"error": "sign in first"})
            return self._json(200, {"user": user["email"], "plan": user["plan"], "note": user["note"]})
        if path == "/favicon.ico":
            return self._send(404, b"")
        self._send(404, "not found")

    def do_POST(self):
        path = urlparse(self.path).path
        body = self._body()
        ctype = self.headers.get("Content-Type", "")
        if path == "/session":
            fields = parse_qs(body.decode(), keep_blank_values=True)
            email = (fields.get("email") or [""])[0]
            pw = (fields.get("password") or [""])[0]
            token = (fields.get("csrf_token") or [""])[0]
            if email == "tester@example.com" and pw == "hunter2" and token == "abc123secret":
                sid = uuid.uuid4().hex
                SESSIONS[sid] = {
                    "email": email,
                    "plan": "pro",
                    "note": "signed in at /session",
                }
                return self._send(
                    303,
                    b"",
                    headers={
                        "Location": "/dashboard",
                        "Set-Cookie": f"sid={sid}; Path=/; HttpOnly; SameSite=Lax",
                    },
                )
            # Wrong credentials: the form comes back, exactly as a real site does.
            return self._send(401, FAILED)
        if path == "/api/items":
            _, user = session_of(self)
            if not user:
                return self._json(401, {"error": "sign in first"})
            try:
                obj = json.loads(body)
            except Exception:
                obj = dict(parse_qs(body.decode()))
            obj = {k: v[0] if isinstance(v, list) else v for k, v in obj.items()}
            item = {
                "id": NEXT_ID[0],
                "name": str(obj.get("name", "unnamed")),
                "qty": int(obj.get("qty", 1)),
            }
            NEXT_ID[0] += 1
            ITEMS.append(item)
            return self._json(201, item)
        self._send(404, "not found")

    def do_DELETE(self):
        path = urlparse(self.path).path
        _, user = session_of(self)
        m = re.match(r"^/api/items/(\d+)$", path)
        if m and user:
            ident = int(m.group(1))
            for i, it in enumerate(ITEMS):
                if it["id"] == ident:
                    ITEMS.pop(i)
                    return self._json(200, {"deleted": ident, "remaining": len(ITEMS)})
            return self._json(404, {"error": "no such item"})
        self._send(404, "not found")

    def do_PUT(self):
        path = urlparse(self.path).path
        _, user = session_of(self)
        m = re.match(r"^/api/items/(\d+)$", path)
        if m and user:
            ident = int(m.group(1))
            obj = json.loads(self._body() or b"{}")
            for it in ITEMS:
                if it["id"] == ident:
                    if "name" in obj:
                        it["name"] = str(obj["name"])
                    if "qty" in obj:
                        it["qty"] = int(obj["qty"])
                    return self._json(200, it)
            return self._json(404, {"error": "no such item"})
        self._send(404, "not found")


if __name__ == "__main__":
    port = int(__import__("sys").argv[1]) if len(__import__("sys").argv) > 1 else 8899
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
