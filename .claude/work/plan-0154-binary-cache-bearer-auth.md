# Plan 0154: Binary cache Bearer auth middleware

## Design

The binary cache server (`http://…/nix-cache-info`, narinfo, nar fetch) had no auth at all — any client on the network could read the full store. Phase 4a adds Bearer token authentication with a token→tenant mapping via the `tenants.cache_token` column (added in migration 009 Part A). The middleware queries `SELECT tenant_name FROM tenants WHERE cache_token = $1` on each request; valid token → authenticated as that tenant (attached as `AuthenticatedTenant` extension for future per-tenant scoping in 4b). Unknown or missing token → 401 + `WWW-Authenticate: Bearer`.

**Unauthenticated access is explicit opt-in** via `cache_allow_unauthenticated` config (default `false`). The important failure mode: when auth is required but no tenants have tokens configured, the middleware returns **503 with a descriptive message** (`binary cache auth not configured: set cache_allow_unauthenticated=true or configure tenant cache_token values`) — fail loud so operators notice misconfiguration in the Nix client's error output immediately, rather than silently serving 401s.

Rounds 3-6 fixed a **bypass class**: round 3 (`266592b`) found `"Bearer "` (trailing space) → `strip_prefix` yields `Some("")` → matched `WHERE cache_token = ''` if an operator mistakenly created such a tenant. Round 4 (`f105b53`) found the round-3 fix was incomplete — `"Bearer    "` (trailing whitespace) passed `!is_empty()` and matched a whitespace-only token. Fixed with `.map(str::trim)`. Round 5 (`767d6ea`) made the misconfiguration-check log DB errors in the fallback path. Round 6 (`4c57313`) made the `Bearer` scheme match case-insensitive per RFC 7235.

## Files

```json files
[
  {"path": "rio-store/src/cache_server/mod.rs", "action": "NEW", "note": "git mv from cache_server.rs; wire auth middleware into router"},
  {"path": "rio-store/src/cache_server/auth.rs", "action": "NEW", "note": "Bearer middleware; token→tenant query; 503 on misconfigured"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "tracey marker + Phase-deferral block removed"}
]
```

## Tracey

- `r[impl store.cache.auth-bearer]` — `d223185`
- `r[verify store.cache.auth-bearer]` — `d223185`

2 marker annotations.

## Entry

- Depends on P0153: tenants table (cache_token column)

## Exit

Merged as `d223185` (1 commit) + bypass fixes in rounds 3-6 (`266592b`, `f105b53`, `767d6ea`, `4c57313`, `b4a7918` TODO). `.#ci` green. 5 unit tests: allow_unauthenticated passthrough, 503 on misconfigured, valid token, unknown token, empty token reject.
