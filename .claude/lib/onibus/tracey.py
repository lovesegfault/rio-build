"""Tracey spec-coverage constants and marker extraction.

Authoritative domain set derived from docs/src/**/*.md standalone r[domain.*]
paragraphs. test_tracey_domains_matches_spec gates via checks.onibus-pytest —
adding a domain to docs/src/components/ without touching TRACEY_DOMAINS below
fails CI. (Hardcoding the alternation at 8 sites previously missed `common`;
120bab69 renamed worker→builder+fetcher without touching this set and the drift
went unnoticed until the test was wired into .#ci.)
"""

from __future__ import annotations

import re
from pathlib import Path

TRACEY_DOMAINS: frozenset[str] = frozenset({
    "builder", "common", "ctrl", "dash", "fetcher", "gw", "obs", "proto", "sched", "sec", "store"
})
TRACEY_DOMAIN_ALT = "|".join(sorted(TRACEY_DOMAINS))
# Capture the full marker ID (domain.area.detail), not just the domain —
# findall() returns capture groups. Non-capturing (?:...) for the domain alt.
TRACEY_MARKER_RE = re.compile(rf"r\[((?:{TRACEY_DOMAIN_ALT})\.[a-z][a-z0-9.-]+)\]")


def tracey_markers(path: Path) -> list[str]:
    """Extract r[domain.*] markers from a file. Sorted unique."""
    return sorted(set(TRACEY_MARKER_RE.findall(path.read_text())))
