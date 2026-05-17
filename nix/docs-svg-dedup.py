#!/usr/bin/env python3
"""
Hoist all <symbol id="gHEX">…</symbol> glyph defs into one
document-level <svg style="display:none"><defs>, dedup by id.
typst content-hashes glyph IDs so identical glyphs share an id;
<use href="#gXXX"> resolves document-globally (SVG-sprite pattern).

Also strips the dyn-paged renderer script tags (shiroa.js heartbeat
poll, svg_utils.js, wasm-init) — useless in --mode static-html.
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
    # NO currentColor rewrite. The rio-css `[fill="#000000"]` attribute
    # selectors handle equation theming in both serve and build; a
    # page-wide replace would also hit .rio-frame diagram SVGs (which
    # use `filter: invert()` for dark themes — currentColor → light
    # --fg → inverted → dark-on-dark). And `currentColor` is longer
    # than `#000000`, so it isn't even a size win.
    #
    # typst emits <defs id="glyph"> / <defs id="clip-path"> per
    # html.frame(); the wrapper ids are unused (refs go to the inner
    # <symbol id="gHEX"> / <clipPath id="cHEX">, never #glyph or
    # #clip-path) and duplicate across frames. Strip them.
    out = out.replace(b'<defs id="glyph">', b"<defs>")
    out = out.replace(b'<defs id="clip-path">', b"<defs>")
    # html.frame() emits both width="Npt" AND an inline
    # style="width: Mem; height: Mem" computed at typst's font size
    # (~10.5pt/em). The browser honours the inline style at the page's
    # 16px/em → ~14% overshoot. The .rio-frame > svg { max-width: 100% }
    # rule clamps width but can't override inline height (specificity),
    # leaving letterbox bands. Strip the inline style; the width=/height=
    # attrs + viewBox are sufficient.
    out = re.sub(
        rb'(<svg class="typst-doc"[^>]*?) style="[^"]*"', rb"\1", out
    )
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
