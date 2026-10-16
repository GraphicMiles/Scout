#!/usr/bin/env python3
"""Local fixture site for benchmarks.

Serves two scenarios that exercise different bottlenecks:
  * /small  - a 3-item page, dominated by per-request overhead
  * /large  - an 8000-item ~0.8 MB page, dominated by parse cost

No network latency, no JavaScript and no hostile markup, so these numbers say
nothing about real-site compatibility. They measure the runtime itself.
"""
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SMALL = """<!doctype html><html><head><title>Small Fixture</title></head><body>
<ul>%s</ul></body></html>""" % "".join(
    f'<li class="item"><span class="name">Item {i}</span>'
    f'<span class="price">${i}.50</span><a href="/a/{i}">link</a></li>'
    for i in range(3)
)

LARGE = """<!doctype html><html><head><title>Large Fixture</title></head><body>
<ul>%s</ul></body></html>""" % "".join(
    f'<li class="item"><span class="name">Item {i}</span>'
    f'<span class="price">${i}.50</span><a href="/a/{i}">link</a></li>'
    for i in range(8000)
)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    # Python's BaseHTTPRequestHandler writes headers and body as separate
    # segments. Without this the fixture itself holds the body for the peer's
    # delayed ACK, adding a flat ~40 ms that has nothing to do with Scout.
    disable_nagle_algorithm = True

    def do_GET(self):  # noqa: N802
        if self.path.startswith("/large"):
            body = LARGE.encode()
        elif self.path.startswith("/small"):
            body = SMALL.encode()
        else:
            body = b"<html><body>ok</body></html>"
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8799
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
