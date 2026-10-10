#!/usr/bin/env python3
"""Accept uncompressed OTLP JSON on loopback and preserve each request body.

With --forward, also relay each request (body + Authorization) to the real upstream and return its response
unchanged, saving the upstream status/body next to the request as `<ts>.response`. Used by the live check.
"""
import argparse
import json
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        if self.path != "/api/v2/traces":
            self.send_error(404)
            return
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        stamp = time.time_ns()
        with (self.server.output / f"{stamp}.json").open("xb") as file:
            file.write(body)
        status, reply = 200, b"{}"
        if self.server.forward:
            request = urllib.request.Request(self.server.forward + self.path, data=body, method="POST", headers={
                "Content-Type": "application/json", "Authorization": self.headers.get("Authorization", "")})
            try:
                with urllib.request.urlopen(request, timeout=60) as response:
                    status, reply = response.status, response.read()
            except urllib.error.HTTPError as error:
                status, reply = error.code, error.read()
            (self.server.output / f"{stamp}.response").write_text(json.dumps({"status": status, "body": reply.decode(errors="replace")}))
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--port-file", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--forward", help="real upstream base URL, e.g. https://api.respan.ai")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    server.output = args.output
    server.forward = args.forward.rstrip("/") if args.forward else None
    args.port_file.write_text(str(server.server_port))
    server.serve_forever()


if __name__ == "__main__":
    main()
