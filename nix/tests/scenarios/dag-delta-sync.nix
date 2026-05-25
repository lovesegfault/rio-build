# Directory-DAG delta-sync substituter (ADR-022 §8, P0574).
#
# Two-store fixture: store-A (the `control` node, fronted by the
# gateway under test) holds closure v1; store-B (the `storeb` node)
# holds closure v2 — the same deep/wide directory tree with exactly ONE
# file changed five levels down. The client then asks gateway-A to
# admit v2 with `nix copy --to ssh-ng://control --substitute-on-
# destination`: instead of the client pushing the whole NAR, the
# gateway delta-syncs v2 from store-B, pruning every subtree store-A
# already holds from v1 and fetching only the one changed file's bytes.
#
# What this proves (the P0574 exit criterion — O(changed-subtrees)
# discovery):
#   * rio_gateway_dagsync_subtrees_pruned_total > 0.9 × total_dirs —
#     the BFS never descended into the unchanged 90+% of the tree.
#   * rio_gateway_dagsync_blobs_fetched_total == 1 — exactly the one
#     changed file crossed the store-B → gateway-A wire.
#   * The reassembled NAR is byte-identical to the original: store-A's
#     PutPath re-hashes it against store-B's declared nar_hash, and the
#     test round-trips the path back out of store-A and diffs it
#     against the client's locally-built copy.
#
# Tree shape (v1 vs v2 differ ONLY in changed.txt's content):
#
#   $out/                          ── changed (root digest differs)
#     bin/tool*                    ── unchanged  ─┐
#     default -> bin/tool          ── (symlink)   │ 76 unchanged
#     sib1..sib15/data             ── unchanged  ─┤ subtrees, each
#     d1/                          ── changed     │ pruned at its root
#       sib1..sib15/data           ── unchanged  ─┤ in O(1)
#       d2/ … d3/ … d4/            ── changed     │
#         sib1..sib15/data each    ── unchanged  ─┘
#         d5/
#           changed.txt            ── THE one changed file
#
# 82 distinct directories (1 root + bin + 5 chain + 75 siblings), all
# with unique content so no two collapse to one dir_digest. The BFS
# walks 6 (root + d1..d5), prunes 76, and fetches 1 blob.
#
# Tenancy plumbing: the castore RPC surface is tenant-scoped
# (r[store.castore.tenant-scope]) and `nix copy` populates no
# path_tenants rows (only build-completion does), so the test seeds
# the junction rows directly in both databases under one shared tenant
# UUID — the same state the scheduler's upsert_path_tenants would have
# produced had the closures been built through rio.
#
# r[verify gw.substitute.dag-delta-sync] markers live at the
# default.nix wiring point per the P0341 convention.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) gatewayHost;
  drvs = import ../lib/derivations.nix { inherit pkgs; };

  # The deep/wide tree builder. `marker` is the ONLY difference between
  # v1 and v2 — it becomes the content of the one file five levels deep.
  # Every sibling directory's file embeds its own level+index so no two
  # directories share a body (a shared dir_digest would collapse the
  # BFS frontier and undercount the prune metric).
  mkTree = marker: ''
    bb="''${busybox}/bin/busybox"
    "$bb" mkdir -p "$out/d1/d2/d3/d4/d5" "$out/bin"
    "$bb" printf '#!/bin/sh\necho rio-dag-tool\n' > "$out/bin/tool"
    "$bb" chmod +x "$out/bin/tool"
    "$bb" ln -s bin/tool "$out/default"
    "$bb" echo "${marker}" > "$out/d1/d2/d3/d4/d5/changed.txt"
    for lvl in "" /d1 /d1/d2 /d1/d2/d3 /d1/d2/d3/d4; do
      i=1
      while [ "$i" -le 15 ]; do
        "$bb" mkdir -p "$out$lvl/sib$i"
        "$bb" echo "stable$lvl-$i" > "$out$lvl/sib$i/data"
        i=$((i+1))
      done
    done
  '';

  # Same derivation NAME for both versions — a realistic incremental
  # rebuild produces a different output hash for the same package name.
  treeV1 = drvs.mkCustom {
    name = "rio-dag-tree";
    script = mkTree "rio-dag-payload-v1";
  };
  treeV2 = drvs.mkCustom {
    name = "rio-dag-tree";
    script = mkTree "rio-dag-payload-v2";
  };
  bbArg = "--arg busybox '(builtins.storePath ${common.busybox})'";

  # 1 root + bin + d1..d5 + 5 levels x 15 siblings.
  totalDirs = 82;
  # Everything except root + d1..d5 is an unchanged subtree pruned at
  # its root: bin + 75 siblings.
  expectedPruned = 76;
in
pkgs.testers.runNixOSTest {
  name = "rio-dag-delta-sync";
  skipTypeCheck = true;

  # Two control-plane boots (~60s each, parallel) + two client-side
  # builds + two `nix copy` pushes + indexer wait + the sync itself.
  # No workers, no k3s.
  globalTimeout = 600 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture;
      withSsh = false;
    }}

    # storeb is outside fixture.waitReady (it only covers `control`).
    storeb.wait_for_unit("postgresql.service")
    storeb.wait_for_unit("rio-store.service")
    storeb.wait_for_open_port(9002)
    storeb.wait_for_unit("rio-scheduler.service")
    storeb.wait_for_open_port(9001)

    # ══════════════════════════════════════════════════════════════════
    # SSH key setup for BOTH gateways. The comment names the tenant —
    # gateway-A resolves it via its scheduler and mints the session JWT
    # that the tenant-scoped castore RPCs require on both stores.
    # ══════════════════════════════════════════════════════════════════
    client.succeed(
        "mkdir -p /root/.ssh && "
        "ssh-keygen -t ed25519 -N ''' -C 'dag-tenant' -f /root/.ssh/id_ed25519"
    )
    pubkey = client.succeed("cat /root/.ssh/id_ed25519.pub").strip()
    for node in [${gatewayHost}, storeb]:
        node.succeed(f"echo '{pubkey}' > /var/lib/rio/gateway/authorized_keys")
        node.succeed("systemctl restart rio-gateway.service")
        node.wait_for_unit("rio-gateway.service")
        node.wait_for_open_port(2222)

    # One tenant UUID shared by both databases. Gateway-A's scheduler
    # resolves 'dag-tenant' from PG-A at SSH auth; the path_tenants
    # rows seeded below must carry the same UUID in PG-B for the
    # castore tenant join to match the JWT's `sub`.
    tid = psql(${gatewayHost},
        "INSERT INTO tenants (tenant_name) VALUES ('dag-tenant') "
        "RETURNING tenant_id")
    psql(storeb,
        f"INSERT INTO tenants (tenant_id, tenant_name) "
        f"VALUES ('{tid}', 'dag-tenant')")
    print(f"dag-delta-sync: tenant {tid}")

    # ══════════════════════════════════════════════════════════════════
    # Build v1 and v2 on the client; push v1 → store-A, v2 → store-B.
    # ══════════════════════════════════════════════════════════════════
    v1 = client.succeed(
        "nix build --impure --no-link --print-out-paths ${bbArg} -f ${treeV1}"
    ).strip()
    v2 = client.succeed(
        "nix build --impure --no-link --print-out-paths ${bbArg} -f ${treeV2}"
    ).strip()
    assert v1 != v2 and v1.startswith("/nix/store/"), f"v1={v1!r} v2={v2!r}"
    print(f"dag-delta-sync: v1={v1} v2={v2}")

    client.succeed(f"nix copy --no-check-sigs --to 'ssh-ng://${gatewayHost}' {v1}")
    client.succeed(f"nix copy --no-check-sigs --to 'ssh-ng://storeb' {v2}")

    # The castore tables (directories/directory_paths/file_blobs) are
    # populated by the eager NAR indexer after PutPath commits; the
    # delta-sync needs them on BOTH sides (root_digest on B, the prune
    # oracle on A). Wait for both work queues to drain.
    for node in [${gatewayHost}, storeb]:
        node.wait_until_succeeds(
            "sudo -u postgres psql rio -qtAc "
            "\"SELECT count(*) FROM manifests "
            "WHERE status = 'complete' AND NOT nar_indexed\" | grep -qx 0",
            timeout=120,
        )

    # Seed the tenant junction for every path each store holds (what
    # the scheduler's build-completion upsert would have done).
    for node in [${gatewayHost}, storeb]:
        psql(node,
            "INSERT INTO path_tenants (store_path_hash, tenant_id) "
            f"SELECT store_path_hash, '{tid}' FROM narinfo "
            "ON CONFLICT DO NOTHING")

    # ══════════════════════════════════════════════════════════════════
    # Preconditions — each one keeps a later assertion from passing
    # vacuously.
    # ══════════════════════════════════════════════════════════════════
    with subtest("preconditions: stores are split, trees are indexed"):
        # Store-A must NOT have v2 yet, or the sync below is a no-op.
        a_has_v2 = psql(${gatewayHost},
            f"SELECT count(*) FROM narinfo WHERE store_path = '{v2}'")
        assert a_has_v2 == "0", (
            f"precondition FAIL: store-A already has {v2} — the delta-sync "
            "would be vacuous"
        )
        # Store-B's v2 tree has the expected number of DISTINCT
        # directories. If two sibling dirs collapsed to one digest the
        # prune-fraction assertion below would be measuring a different
        # tree than the comment describes.
        b_dirs = psql(storeb,
            "SELECT count(*) FROM directory_paths dp "
            "JOIN narinfo n USING (store_path_hash) "
            f"WHERE n.store_path = '{v2}'")
        assert b_dirs == "${toString totalDirs}", (
            f"precondition FAIL: v2 has {b_dirs} distinct directories, "
            f"expected ${toString totalDirs} — the tree fixture drifted; "
            "update totalDirs/expectedPruned and the prune math"
        )
        # v2's NAR root is a directory (root_digest non-empty) — the
        # capability probe requires it.
        b_root = psql(storeb,
            "SELECT octet_length(root_node) FROM nar_index ni "
            "JOIN narinfo n USING (store_path_hash) "
            f"WHERE n.store_path = '{v2}'")
        assert b_root != "" and int(b_root) > 0, (
            f"precondition FAIL: v2 has no nar_index.root_node row ({b_root!r})"
        )
        # The gateway is configured with the peer (the whole feature is
        # off without it).
        ${gatewayHost}.succeed(
            "systemctl show rio-gateway -p Environment | "
            "grep -q RIO_SUBSTITUTE_STORE_ADDR=storeb:9002"
        )

    # ══════════════════════════════════════════════════════════════════
    # THE TEST: ask gateway-A to admit v2, substituting on destination.
    # ══════════════════════════════════════════════════════════════════
    with subtest("dag-delta-sync: one changed file crosses the wire"):
        before = scrape_metrics(${gatewayHost}, 9090)
        client.succeed(
            f"nix copy --no-check-sigs --substitute-on-destination "
            f"--to 'ssh-ng://${gatewayHost}' {v2}"
        )

        # Store-A now has v2, complete. PutPath re-hashed the
        # reassembled NAR against store-B's declared nar_hash, so a
        # complete manifest IS the byte-integrity proof.
        status = psql(${gatewayHost},
            "SELECT m.status::text FROM manifests m "
            "JOIN narinfo n USING (store_path_hash) "
            f"WHERE n.store_path = '{v2}'")
        assert status == "complete", (
            f"v2 manifest on store-A is {status!r}, expected complete — "
            "did the delta-sync fall back to the client push and fail?"
        )
        a_hash = psql(${gatewayHost},
            f"SELECT encode(nar_hash, 'hex') FROM narinfo WHERE store_path = '{v2}'")
        b_hash = psql(storeb,
            f"SELECT encode(nar_hash, 'hex') FROM narinfo WHERE store_path = '{v2}'")
        assert a_hash == b_hash and a_hash != "", (
            f"nar_hash mismatch after sync: store-A {a_hash!r} vs store-B {b_hash!r}"
        )

        # O(changed-subtrees) discovery: >90% of the tree was pruned
        # without being enumerated against the remote, and exactly one
        # file's content was fetched.
        after = scrape_metrics(${gatewayHost}, 9090)
        floor = int(${toString totalDirs} * 0.9) + 1
        assert ${toString expectedPruned} >= floor, (
            "fixture self-check: expectedPruned must clear the 90% floor"
        )
        assert_metric_delta(
            before, after,
            "rio_gateway_dagsync_subtrees_pruned_total", ${toString expectedPruned},
        )
        assert_metric_delta(
            before, after, "rio_gateway_dagsync_blobs_fetched_total", 1,
        )
        # The 6 changed directories (root + d1..d5) are the only ones
        # fetched from the remote.
        assert_metric_delta(
            before, after, "rio_gateway_dagsync_dirs_fetched_total", 6,
        )
        pruned = metric_value(after, "rio_gateway_dagsync_subtrees_pruned_total")
        print(
            f"dag-delta-sync: pruned={pruned} of ${toString totalDirs} dirs, "
            f"fetched 1 blob"
        )

    # ══════════════════════════════════════════════════════════════════
    # Byte-level round-trip: what store-A serves back for v2 is
    # identical to what the client built locally.
    # ══════════════════════════════════════════════════════════════════
    with subtest("dag-delta-sync: reassembled path round-trips byte-identically"):
        client.succeed("mkdir -p /root/verify")
        client.succeed(
            f"nix copy --no-check-sigs --from 'ssh-ng://${gatewayHost}' "
            f"--to /root/verify {v2}"
        )
        # -r recurse, --no-dereference compares symlinks as symlinks.
        # diff exits 1 on any difference -> succeed() raises.
        client.succeed(f"diff -r --no-dereference {v2} /root/verify{v2}")
        # The executable bit survived the NarIndex -> NarNode round trip.
        client.succeed(f"test -x /root/verify{v2}/bin/tool")
        client.succeed(f"grep -q rio-dag-payload-v2 /root/verify{v2}/d1/d2/d3/d4/d5/changed.txt")

    ${common.collectCoverage "${fixture.pyNodeVars}, storeb"}
  '';
}
