#!/usr/bin/env python3
"""Vendor AG Grid Community into crates/console/assets/vendor/ag-grid (hgl-ai#104).

    python3 -I update-ag-grid.py VERSION SHA512 WORKDIR

Downloads ag-grid-community-VERSION.tgz from the npm registry into WORKDIR, checks it
against SHA512 (the registry's `dist.integrity`, `sha512-...`), and copies the grid
script without its embedded styles, the structural stylesheet and the Quartz theme.
Every `data:` URL in the stylesheets becomes a file beside them, so the console's
Content-Security-Policy can stay self-only. Then update AG_GRID_VERSION in
crates/console/src/lib.rs and the table in VENDOR.md.
"""

import base64
import hashlib
import io
import re
import sys
import tarfile
import urllib.parse
import urllib.request
from pathlib import Path

OUT = Path(__file__).resolve().parent / "ag-grid"
COPY = {
    "package/dist/ag-grid-community.min.noStyle.js": "ag-grid-community.min.noStyle.js",
    "package/styles/ag-grid.min.css": "ag-grid.min.css",
    "package/styles/ag-theme-quartz.min.css": "ag-theme-quartz.min.css",
    "package/LICENSE.txt": "LICENSE.txt",
}
EXT = {"image/svg+xml": "svg", "font/woff2": "woff2"}
DATA_URL = re.compile(r"""url\((['"]?)data:([^;,]+)((?:;[^;,]+)*),(.*?)\1\)""")


def externalise(css: str, files: dict) -> str:
    def one(m):
        mime, params, payload = m.group(2), m.group(3), m.group(4)
        if ";base64" in params:
            raw = base64.b64decode(payload)
        else:
            raw = urllib.parse.unquote(payload).encode()
        name = "data-%s.%s" % (hashlib.sha256(raw).hexdigest()[:12], EXT[mime])
        files[name] = raw
        return "url(%s)" % name

    return DATA_URL.sub(one, css)


def main():
    version, integrity, work = sys.argv[1], sys.argv[2], Path(sys.argv[3])
    url = "https://registry.npmjs.org/ag-grid-community/-/ag-grid-community-%s.tgz" % version
    tgz = work / ("ag-grid-community-%s.tgz" % version)
    if not tgz.exists():
        work.mkdir(parents=True, exist_ok=True)
        with urllib.request.urlopen(url) as r:
            tgz.write_bytes(r.read())
    data = tgz.read_bytes()
    got = "sha512-" + base64.b64encode(hashlib.sha512(data).digest()).decode()
    if got != integrity:
        sys.exit("integrity mismatch: %s" % got)
    files = {}
    with tarfile.open(fileobj=io.BytesIO(data)) as t:
        for src, dst in COPY.items():
            body = t.extractfile(src).read()
            if dst.endswith(".css"):
                body = externalise(body.decode(), files).encode()
                if b"data:" in body:
                    sys.exit("%s still has a data: URL" % dst)
            files[dst] = body
    OUT.mkdir(exist_ok=True)
    for old in OUT.iterdir():
        old.unlink()
    for name, body in sorted(files.items()):
        (OUT / name).write_bytes(body)
        print(name, len(body))


if __name__ == "__main__":
    main()
