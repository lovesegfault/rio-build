"""Tracey spec-coverage constants and marker extraction.

Authoritative domain set derived from docs/src/**/*.md standalone r[domain.*]
paragraphs. test_tracey_domains_matches_spec() catches drift — hardcoding the
alternation at 8 sites previously missed `common`. `dash` seeded by P0245
(pulled forward from P0284 so 6 dashboard plans don't tracey-validate-fail
on merge).
"""

from __future__ import annotations

import re
from pathlib import Path

TRACEY_DOMAINS: frozenset[str] = frozenset({
    "common", "ctrl", "dash", "gw", "obs", "proto", "sched", "sec", "store", "worker"
})
TRACEY_DOMAIN_ALT = "|".join(sorted(TRACEY_DOMAINS))
# Capture the full marker ID (domain.area.detail), not just the domain —
# findall() returns capture groups. Non-capturing (?:...) for the domain alt.
TRACEY_MARKER_RE = re.compile(rf"r\[((?:{TRACEY_DOMAIN_ALT})\.[a-z][a-z0-9.-]+)\]")


def tracey_markers(path: Path) -> list[str]:
    """Extract r[domain.*] markers from a file. Sorted unique."""
    return sorted(set(TRACEY_MARKER_RE.findall(path.read_text())))
