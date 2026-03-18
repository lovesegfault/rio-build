# Plan 0242: VM Section I security fragments — cache auth + mTLS

phase4c.md:46 — two fragments in [`nix/tests/scenarios/security.nix`](../../nix/tests/scenarios/security.nix) (403 lines, no other 4c touch).

**(i) Cache auth:** unauth `curl /{hash}.narinfo` → 401; with Bearer from tenants seed → 200. **Bonus (weak soft-dep on P0225):** unauth `/nix-cache-info` → 200 (verifies P0225's route move). If P0225 not merged, skip the bonus assert.

**(ii) mTLS extension:** the existing `mtls-reject` subtest at [`security.nix:89`](../../nix/tests/scenarios/security.nix) tests plaintext-on-TLS-port rejection. EXTEND it (don't duplicate) with a gRPC `QueryPathInfo` no-client-cert → handshake fails assertion.

## Tasks

### T1 — `test(vm):` cache-auth fragment

MODIFY [`nix/tests/scenarios/security.nix`](../../nix/tests/scenarios/security.nix) — TAIL-append fragment key `cache-auth`:

```nix
cache-auth = ''
  with subtest("cache-auth: unauth narinfo → 401; with Bearer → 200"):
      store_addr = "localhost:8080"  # or whatever the store's HTTP port is
      # Pick any known hash from the seeded store
      test_hash = server.succeed("ls /nix/store | head -1 | cut -d- -f1").strip()

      # Unauth → 401
      code = server.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' http://{store_addr}/{test_hash}.narinfo").strip()
      assert code == "401", f"expected 401 for unauth narinfo, got {code}"

      # With Bearer from tenants seed → 200
      token = server.succeed("psql -U rio -d rio -t -c \"SELECT cache_token FROM tenants LIMIT 1\"").strip()
      code = server.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' -H 'Authorization: Bearer {token}' http://{store_addr}/{test_hash}.narinfo").strip()
      assert code == "200", f"expected 200 with Bearer, got {code}"

      print(f"cache-auth PASS: 401 unauth, 200 with Bearer")

  # BONUS (weak soft-dep P0225): /nix-cache-info public
  with subtest("cache-auth-nixcacheinfo: /nix-cache-info public"):
      code = server.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' http://{store_addr}/nix-cache-info").strip()
      # If P0225 not merged, this may 401 — soft assertion
      if code == "200":
          print("cache-auth-nixcacheinfo PASS: /nix-cache-info 200 unauth (P0225 merged)")
      else:
          print(f"cache-auth-nixcacheinfo SKIP: got {code} — P0225 not merged yet, skipping bonus assert")
'';
```

### T2 — `test(vm):` extend mtls-reject with gRPC no-cert

MODIFY the existing `mtls-reject` subtest block at [`security.nix:89-111`](../../nix/tests/scenarios/security.nix) — EXTEND (don't duplicate):

```nix
# EXTEND the existing mtls-reject subtest (around :111 after the plaintext-reject check):
    with subtest("mtls-reject-grpc: gRPC QueryPathInfo no client cert → handshake fail"):
        # grpcurl without -cert/-key → TLS handshake fails
        rc = server.execute(
            "grpcurl -insecure -d '{\"store_path\":\"/nix/store/fake\"}' "
            "localhost:9001 rio.store.StoreService/QueryPathInfo"
        )[0]
        assert rc != 0, "grpcurl no-cert succeeded — mTLS NOT enforcing"
        print("mtls-reject-grpc PASS: no-client-cert handshake rejected")
```

Goes inside or adjacent to the existing `mtls-reject` subtest. Check at dispatch whether the existing subtest is structured to accept an extension or if a separate subtest block is cleaner.

## Exit criteria

- `/nbr .#ci` green — including `vm-security-*` tests
- `cache-auth` subtest: 401 unauth narinfo, 200 with Bearer
- `mtls-reject-grpc` subtest: grpcurl no-cert handshake fails

## Tracey

No markers — tests existing features (`r[store.cache.auth-bearer]` at [`store.md:156`](../../docs/src/components/store.md) and mTLS enforcement are already covered by impl markers; this adds VM-level verification but the existing `r[verify]` comments in the unit tests suffice for the tracey graph).

## Files

```json files
[
  {"path": "nix/tests/scenarios/security.nix", "action": "MODIFY", "note": "T1+T2: cache-auth fragment (TAIL-append) + mtls-reject gRPC extension near :89-111"}
]
```

```
nix/tests/scenarios/security.nix   # T1+T2: cache-auth + mtls-reject extension (403L, no other 4c touch)
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [225], "note": "Wave-1 frontier. security.nix 403L, no other 4c touch. Weak soft-dep on P0225 (bonus /nix-cache-info assert — skippable if unmerged)."}
```

**Depends on:** none. Tests existing features.
**Conflicts with:** none — `security.nix` has no other 4c touch.

**P0225 soft-dep:** the `/nix-cache-info` bonus assertion is conditional (`if code == "200"`) — gracefully skips if P0225 not merged.
