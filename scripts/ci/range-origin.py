#!/usr/bin/env python3
"""Minimal HTTP origin with single-range support, used by the end-to-end test.

Python's built-in http.server ignores Range, so this stands in for Jellyfin's byte-range behaviour.
Usage: range-origin.py <port> <directory>
"""

import os
import re
import sys
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer

RANGE = re.compile(r"^bytes=(\d*)-(\d*)$")


class RangeHandler(SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def send_head(self):
        path = self.translate_path(self.path)
        if not os.path.isfile(path):
            self.send_error(404)
            return None
        size = os.path.getsize(path)
        start, end = 0, size - 1
        header = self.headers.get("Range")
        partial = False
        if header:
            match = RANGE.match(header.strip())
            if not match or (not match.group(1) and not match.group(2)):
                self.send_error(416)
                return None
            first, last = match.group(1), match.group(2)
            if first:
                start = int(first)
                end = min(int(last), size - 1) if last else size - 1
            else:
                start = max(size - int(last), 0)
            if start >= size or start > end:
                self.send_response(416)
                self.send_header("Content-Range", f"bytes */{size}")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return None
            partial = True
        handle = open(path, "rb")
        handle.seek(start)
        self.remaining = end - start + 1
        self.send_response(206 if partial else 200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(self.remaining))
        if partial:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()
        return handle

    def copyfile(self, source, outputfile):
        while self.remaining > 0:
            chunk = source.read(min(65536, self.remaining))
            if not chunk:
                break
            outputfile.write(chunk)
            self.remaining -= len(chunk)


if __name__ == "__main__":
    port, directory = int(sys.argv[1]), sys.argv[2]
    os.chdir(directory)
    ThreadingHTTPServer(("127.0.0.1", port), RangeHandler).serve_forever()
