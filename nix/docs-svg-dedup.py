#!/usr/bin/env python3
"""
Hoist all <symbol id="gHEX">…</symbol> glyph defs into one
document-level <svg style="display:none"><defs>, dedup by id.
typst content-hashes glyph IDs so identical glyphs share an id;
<use href="#gXXX"> resolves document-globally (SVG-sprite pattern).

Also rewrites the equation single-render sentinel
fill="#000000" → fill="currentColor" so
`.inline-equation svg { color: var(--fg) }` (rio-css) themes them
without dual-rendering.
"""

import pathlib
import re
import sys

SYM = re.compile(rb'<symbol id="(g[0-9A-F]+)"[^>]*>.*?</symbol>', re.S)
# Body open-tag may carry attributes; match through to its closing '>'.
BODY_OPEN = re.compile(rb"<body\b[^>]*>")

for f in pathlib.Path(sys.argv[1]).rglob("*.html"):
    src = f.read_bytes()
    seen: set[bytes] = set()
    defs: list[bytes] = []

    def strip(m: re.Match[bytes]) -> bytes:
        i = m.group(1)
        if i not in seen:
            seen.add(i)
            defs.append(m.group(0))
        return b""

    out = SYM.sub(strip, src)
    if defs:
        sprite = (
            b'<svg xmlns="http://www.w3.org/2000/svg" style="display:none">'
            b"<defs>" + b"".join(defs) + b"</defs></svg>"
        )
        # Sprite must go INSIDE <body> (HTML validity); insert
        # immediately after the opening tag.
        out = BODY_OPEN.sub(lambda m: m.group(0) + sprite, out, count=1)
    # currentColor rewrite (lib/rio.typ's sentinel — see the
    # math.equation show-rule comment). Size-only: the CSS
    # [fill="#000000"]/[stroke="#000000"] attribute selectors are what
    # make rendering correct (and `shiroa serve` parity, which has no
    # post-process). stroke covers fraction bars/radicals/arrows.
    out = out.replace(b'fill="#000000"', b'fill="currentColor"')
    out = out.replace(b'stroke="#000000"', b'stroke="currentColor"')
    # typst emits <defs id="glyph"> / <defs id="clip-path"> per
    # html.frame(); the wrapper ids are unused (refs go to the inner
    # <symbol id="gHEX"> / <clipPath id="cHEX">, never #glyph or
    # #clip-path) and duplicate across frames. Strip them.
    out = out.replace(b'<defs id="glyph">', b"<defs>")
    out = out.replace(b'<defs id="clip-path">', b"<defs>")
    # Dyn-paged renderer plumbing — useless in --mode static-html (no
    # .typst-doc elements; html.frame() emits final SVG). shiroa.js
    # polls /heartbeat (404 spam under miniserve); svg_utils.js
    # console.log()s on load then no-ops; the wasm-init inline script
    # (matched by its base64 prefix = `window.typstRerender `) fetches
    # a 1MB wasm and waits on shiroa-js load. Strip the two file refs;
    # reduce wasm-init to just the no-op stubs index.js calls
    # (typstRerender on theme switch / sidebar resize).
    out = re.sub(
        rb'<script src="[^"]*/internal/shiroa\.js"[^>]*></script>\s*', b"", out
    )
    out = re.sub(
        rb'<script src="[^"]*/internal/svg_utils\.js"[^>]*>\s*</script>\s*', b"", out
    )
    out = re.sub(
        rb'<script src="data:application/javascript;base64,'
        rb'd2luZG93LnR5cHN0UmVyZW5kZXIg[^"]*">\s*</script>',
        b"<script>window.typstRerender=()=>{};"
        b"window.typstChangeTheme=()=>{};</script>",
        out,
    )
    if out != src:
        f.write_bytes(out)
        total = len(seen) + len(SYM.findall(src)) - len(seen)  # = original count
        print(
            f"{f.relative_to(sys.argv[1])}: {len(src) // 1024}K → "
            f"{len(out) // 1024}K ({len(seen)} unique / {total} symbols)"
        )
