# Tiered chunk-backend cache semantics (ADR-023, P0555): rio-store
# replicas running `chunk_backend.kind = "tiered"` against a real S3
# API (Garage), proving the Express-tier data flow end to end — puts
# land in the authoritative bucket only, the cache tier is filled
# solely by read-through, a fill made by one replica is served to the
# next, and `express_bucket` unset degrades to plain S3 behavior.
#
# ── Topology (and where it deviates from the plan sketch) ────────────
#
# The P0555 sketch said "two minio instances (one local Express
# stand-in, one shared remote)". Two adjustments:
#
#   - Garage instead of MinIO: the pinned nixpkgs marks the minio
#     server abandoned-with-unfixed-CVEs (knownVulnerabilities → eval
#     refuses without an explicit insecure allowance) and recommends
#     Garage as the migration target. Garage is packaged, has a NixOS
#     module, and implements the S3 surface the aws-sdk needs here
#     (PutObject/GetObject + the SDK's default x-amz-checksum-crc32
#     integrity headers). The unflagged `minio-client` (mc) is still
#     used as a generic S3 client for object-listing assertions.
#
#   - One server hosting TWO buckets, not two servers: both tiers'
#     S3 clients come from the same `rio_common::s3::default_client`
#     env chain, so a single `AWS_ENDPOINT_URL` override applies to
#     BOTH buckets — in prod the SDK routes per-bucket by NAME (the
#     `--x-s3` suffix → zonal endpoint), under an endpoint override
#     everything goes to that one endpoint. The tier distinction in
#     TieredChunkBackend is which BUCKET an operation addresses, never
#     which host, so the rio-store code paths exercised are identical.
#
# Two more stand-in-vs-AWS adaptations:
#   - The express stand-in bucket is deliberately NOT named `*--x-s3`:
#     the SDK detects the directory-bucket suffix and inserts an
#     `s3express:CreateSession` hop a non-AWS endpoint cannot answer
#     (same reason the unit tests in backend/tiered.rs avoid the
#     suffix). The config takes any bucket name, so a plain name keeps
#     the SDK on ordinary SigV4.
#   - `AWS_ENDPOINT_URL` uses the loopback IP, not a hostname: the
#     SDK's S3 endpoint rules use path-style addressing for IP
#     endpoints, which Garage speaks without any root_domain/wildcard
#     DNS setup (a hostname endpoint would make the SDK prepend the
#     bucket as a virtual-host subdomain).
#
# Replicas (all on the control VM, sharing its PostgreSQL and the two
# Garage buckets — the same shape as prod replicas sharing PG + S3 + a
# per-AZ Express bucket):
#   A — the fixture's `services.rio.store` (gateway-attached, :9002,
#       metrics :9092): tiered + express bucket. Receives every
#       PutPath from the gateway.
#   B — `rio-store-b` (extra unit, :9012, metrics :9094): tiered +
#       express bucket. Never written to, never restarted — its first
#       read proves cross-replica warmth.
#   C — `rio-store-c` (extra unit, :9013, metrics :9095): tiered with
#       NO express bucket → `local = None` pass-through.
#
# Every replica's [chunk_backend] is configured via env vars
# (RIO_CHUNK_BACKEND__*) rather than /etc/rio/store.toml: all three
# replicas are the same binary on the same host reading the same
# /etc/rio path, and the env layer can override a TOML key but never
# unset one — a store.toml carrying A's express_bucket would silently
# leak it into C. The fixture is passed extraConfig = "" so the file
# is never rendered.
#
# Reads are driven with grpcurl GetPath directly against each
# replica's gRPC port (the gateway only talks to A), and the tiered
# counters (`rio_store_tiered_local_{hits,misses,errors}_total`,
# `rio_store_tiered_writethrough_errors_total` — the chunk-cache-tier
# dashboard's source series) plus `mc ls` object listings carry the
# assertions. Metrics + key-set comparisons are structural; no
# wall-clock gates.
#
# Subtest contract (markers live at default.nix:subtests, NOT here):
#
# put-remote-only — after the seed PutPath through replica A: the
#   authoritative bucket holds the chunk objects, the local (express
#   stand-in) bucket is EMPTY, and A's tiered counters never moved —
#   writes go to the remote tier only, the local tier is read-through.
#
# cold-miss-fallback — restart A (drops its in-process moka cache so
#   GetPath actually reaches the TieredChunkBackend; the writing
#   replica is moka-hot for everything it just wrote), then GetPath
#   the seeded path on A: the read succeeds even though every chunk is
#   absent from the local bucket, local misses == chunk count, no
#   local/write-through errors, and the read-through filled the local
#   bucket (its chunks/ key set now equals the remote's).
#
# replica-warm-via-read-through — GetPath the same path on B, a
#   replica that has never served a read (moka cold) and never wrote:
#   every chunk is served from the local tier A's read-through filled
#   (hits == chunk count, zero misses), and the local bucket is
#   unchanged (no double-fill).
#
# local-none-passthrough — seed a second path via A, then GetPath both
#   paths on C (`express_bucket` unset → local = None): reads succeed,
#   C's tiered counters all stay zero, and the local bucket gains
#   nothing — the pass-through shape neither reads nor writes the
#   cache tier, exactly like `kind = "s3"`.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (pkgs) lib;
  inherit (fixture) gatewayHost;
  protoset = import ../lib/protoset.nix { inherit pkgs; };

  remoteBucket = "rio-chunks";
  localBucket = "rio-cache-local";
  s3Endpoint = "http://127.0.0.1:3900";

  # Fixed test credentials, imported into Garage in the prelude
  # (`garage key import`) so every store replica can carry them as
  # plain env vars from boot — Garage-generated keys would only be
  # known at runtime, after the services already started. Garage key
  # shape: "GK" + 24 hex chars, 64-hex secret. Deliberately repeated
  # low-entropy patterns (not secrets — ephemeral airgapped VM, same
  # spirit as lib/{hmac,jwt}-keys.nix) so ripsecrets has nothing to
  # flag.
  s3AccessKey = "GK0123456789abcdef01234567";
  s3SecretKey = "cafef00dcafef00dcafef00dcafef00dcafef00dcafef00dcafef00dcafef00d";
  # Garage node-to-node RPC secret (32-byte hex). Single-node cluster
  # inside an ephemeral VM — fine for it to live in the nix store.
  garageRpcSecret = "deadbeefcafebabedeadbeefcafebabedeadbeefcafebabedeadbeefcafebabe";

  # mc with a fixed config dir so alias state is independent of $HOME.
  mc = "mc --config-dir /tmp/mc";

  # aws-sdk env shared by every replica. Loopback-IP endpoint →
  # path-style (see header). Static credentials; metadata lookups
  # disabled so the SDK never probes a non-existent IMDS.
  awsEnv = {
    AWS_ENDPOINT_URL = s3Endpoint;
    AWS_ACCESS_KEY_ID = s3AccessKey;
    AWS_SECRET_ACCESS_KEY = s3SecretKey;
    AWS_REGION = "us-east-1";
    AWS_EC2_METADATA_DISABLED = "true";
  };

  # Tiered chunk-backend env for one replica. `express = false` omits
  # the EXPRESS_BUCKET var entirely (absent → express_bucket = None →
  # local = None), mirroring how the helm chart renders it.
  tieredEnv =
    express:
    awsEnv
    // {
      RIO_CHUNK_BACKEND__KIND = "tiered";
      RIO_CHUNK_BACKEND__BUCKET = remoteBucket;
      RIO_CHUNK_BACKEND__PREFIX = "";
    }
    // lib.optionalAttrs express {
      RIO_CHUNK_BACKEND__EXPRESS_BUCKET = localBucket;
    };

  # Same LLVM_PROFILE_FILE shape as common.nix covEnv (double-% is the
  # systemd specifier escape). The extra replicas are not stopped by
  # collectCoverage, so their profraws are best-effort only — replica
  # A (the fixture's store) carries the coverage signal.
  covEnv = lib.optionalAttrs common.coverage {
    LLVM_PROFILE_FILE = "/var/lib/rio/cov/rio-%%h-%%p-%%m.profraw";
  };

  # An extra rio-store replica as a plain systemd unit. The store
  # NixOS module is single-instance, so B and C mirror its
  # env/ExecStart shape directly (nix/modules/{store,_common}.nix).
  mkReplica =
    {
      name,
      port,
      metricsPort,
      express,
    }:
    {
      description = "rio-store replica ${name} (vm-store-tiered)";
      wantedBy = [ "multi-user.target" ];
      after = [
        "network-online.target"
        "postgresql.service"
        "rio-store.service"
      ];
      wants = [ "network-online.target" ];
      # Replica A applies the sqlx migrations on startup and only opens
      # its gRPC port afterwards — gate on that so B/C never race the
      # migration advisory lock on a fresh DB (same guard as the
      # fixture's scheduler preStart).
      preStart = ''
        for _ in $(seq 1 60); do
          ${pkgs.netcat}/bin/nc -z localhost 9002 && exit 0
          sleep 0.5
        done
        echo "rio-store (replica A) port 9002 not open after 30s" >&2
        exit 1
      '';
      environment = {
        RIO_LOG_FORMAT = "pretty";
        RIO_LISTEN_ADDR = "[::]:${toString port}";
        RIO_METRICS_ADDR = "[::]:${toString metricsPort}";
        RIO_DATABASE_URL = common.databaseUrl;
      }
      // tieredEnv express
      // covEnv;
      serviceConfig = {
        ExecStart = "${common.rio-workspace}/bin/rio-store";
        Restart = "on-failure";
        RestartSec = "5s";
        StateDirectory = "rio/store-${name}";
      };
    };

  # Control-node extension: Garage + replica A's tiered env + the two
  # extra replicas. Composes with the fixture's control node via
  # imports (NixOS module merge).
  controlExtension = {
    services.garage = {
      enable = true;
      # Explicit major pin (the module requires choosing one): the
      # layout/key/bucket CLI calls in the prelude are the 1.x syntax.
      package = pkgs.garage_1_x;
      settings = {
        replication_factor = 1;
        rpc_bind_addr = "127.0.0.1:3901";
        rpc_public_addr = "127.0.0.1:3901";
        rpc_secret = garageRpcSecret;
        s3_api = {
          api_bind_addr = "127.0.0.1:3900";
          # Must match the replicas' AWS_REGION — Garage rejects
          # requests whose SigV4 credential scope names another region.
          s3_region = "us-east-1";
          root_domain = ".s3.garage.localhost";
        };
      };
    };

    systemd.services = {
      # Replica A: the module-managed store the gateway talks to.
      rio-store.environment = tieredEnv true;

      rio-store-b = mkReplica {
        name = "b";
        port = 9012;
        metricsPort = 9094;
        express = true;
      };
      rio-store-c = mkReplica {
        name = "c";
        port = 9013;
        metricsPort = 9095;
        express = false;
      };
    };

    environment.systemPackages = [
      pkgs.minio-client
      pkgs.grpcurl
    ];

    # PG + scheduler + gateway + three store replicas + Garage on one
    # VM — the fixture's 1024 MiB default is too tight.
    virtualisation.memorySize = lib.mkForce (2048 + (if common.coverage then 256 else 0));
  };

  tieredFixture = fixture // {
    nodes = fixture.nodes // {
      control = {
        imports = [
          fixture.nodes.control
          controlExtension
        ];
      };
    };
  };

  prelude = ''
    ${common.mkBootstrap {
      fixture = tieredFixture;
      inherit gatewayHost;
    }}

    control.wait_for_unit("garage.service")
    control.wait_for_open_port(3900)
    # Single-node Garage bring-up: assign a storage role + apply the
    # layout, create both buckets, import the fixed key the replicas
    # already carry in their env, and grant it on both buckets. All of
    # this must precede the first PutPath (the seed below) — the store
    # builds its S3 clients lazily and would surface a missing bucket
    # or key as a failed upload.
    node_id = control.succeed("garage node id -q").strip().split("@")[0]
    control.succeed(f"garage layout assign -z dc1 -c 1G {node_id}")
    control.succeed("garage layout apply --version 1")
    control.succeed("garage bucket create ${remoteBucket}")
    control.succeed("garage bucket create ${localBucket}")
    control.succeed(
        "garage key import --yes -n vmtest ${s3AccessKey} ${s3SecretKey}"
    )
    control.succeed(
        "garage bucket allow --read --write --owner ${remoteBucket} --key vmtest"
    )
    control.succeed(
        "garage bucket allow --read --write --owner ${localBucket} --key vmtest"
    )
    # mc is only the listing client for the assertions below.
    control.succeed(
        "${mc} alias set garage ${s3Endpoint} ${s3AccessKey} ${s3SecretKey}"
    )

    control.wait_for_unit("rio-store-b.service")
    control.wait_for_open_port(9012)
    control.wait_for_unit("rio-store-c.service")
    control.wait_for_open_port(9013)

    BUSYBOX = "${common.busybox}"
    PROTOSET = "${protoset}/rio.protoset"


    def bucket_chunk_keys(bucket):
        """Sorted chunk-object keys (`chunks/aa/<hex>`) in one bucket.

        `mc ls --json` emits one JSON object per line with the object
        name in `key`. Filter to the chunks/ prefix so non-chunk
        objects (e.g. a future binary-cache-compat writer's narinfo
        blobs in the remote bucket) can never skew the comparison."""
        out = control.succeed(f"${mc} ls --recursive --json garage/{bucket}")
        keys = []
        for line in out.splitlines():
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                key = json.loads(line).get("key", "")
            except json.JSONDecodeError:
                continue
            if "chunks/" in key:
                keys.append(key[key.index("chunks/"):])
        return sorted(keys)


    def tiered_metrics(port):
        """The four TieredChunkBackend counters from one replica's
        exporter. A counter that has never been incremented is absent
        from the scrape — read that as 0."""
        scraped = scrape_metrics(control, port)

        def val(name):
            return scraped.get(name, {}).get("", 0.0)

        return {
            "hits": val("rio_store_tiered_local_hits_total"),
            "misses": val("rio_store_tiered_local_misses_total"),
            "errors": val("rio_store_tiered_local_errors_total"),
            "writethrough_errors": val("rio_store_tiered_writethrough_errors_total"),
        }


    def get_path(port, store_path, label):
        """GetPath via grpcurl against one replica and assert the
        stream completed: grpcurl exits non-zero on any gRPC error
        status, and the captured stream must carry both the PathInfo
        header and at least one NAR data frame. The store SHA-256-
        verifies the reassembled NAR against the recorded nar_hash, so
        a clean stream implies content integrity."""
        out_file = f"/tmp/getpath-{label}.json"
        req = json.dumps({"store_path": store_path})
        control.succeed(
            "grpcurl -plaintext -max-time 120 "
            f"-protoset {PROTOSET} "
            f"-d '{req}' "
            f"localhost:{port} rio.store.StoreService/GetPath > {out_file}"
        )
        control.succeed(f"grep -q '\"info\"' {out_file}")
        control.succeed(f"grep -q narChunk {out_file}")


    # Seed: the busybox closure via ssh-ng through the gateway →
    # replica A PutPath → chunks land in the authoritative bucket.
    # This is the PutPath that put-remote-only asserts on.
    ${common.seedBusybox gatewayHost}
  '';

  fragments = {
    "put-remote-only" = ''
      with subtest("put-remote-only: PutPath writes the authoritative bucket only"):
          remote_keys = bucket_chunk_keys("${remoteBucket}")
          assert len(remote_keys) >= 1, (
              "seed PutPath produced no chunk objects in the remote bucket - "
              "did the upload really go through the tiered backend?"
          )
          local_keys = bucket_chunk_keys("${localBucket}")
          assert local_keys == [], (
              f"local (express stand-in) bucket must be EMPTY after PutPath; "
              f"found {local_keys!r} - writes are leaking into the cache tier"
          )
          # The put path never consults the local tier either: replica A
          # has served no reads yet, so every tiered counter is still 0.
          a = tiered_metrics(9092)
          assert a == {"hits": 0.0, "misses": 0.0, "errors": 0.0,
                       "writethrough_errors": 0.0}, (
              f"replica A tiered counters moved during PutPath: {a!r}"
          )
    '';

    "cold-miss-fallback" = ''
      with subtest("cold-miss-fallback: local miss falls back to remote and fills"):
          n_chunks = len(bucket_chunk_keys("${remoteBucket}"))
          # Restart replica A to drop its in-process moka cache - the
          # writing replica is moka-hot for every chunk it just wrote,
          # so without the restart GetPath would never reach the
          # TieredChunkBackend at all.
          control.succeed("systemctl restart rio-store.service")
          control.wait_for_unit("rio-store.service")
          control.wait_for_open_port(9002)

          get_path(9002, BUSYBOX, "cold-a")

          a = tiered_metrics(9092)
          assert a["misses"] == n_chunks, (
              f"replica A local misses = {a['misses']}, expected {n_chunks} "
              f"(one per chunk of the seeded path); counters: {a!r}"
          )
          assert a["hits"] == 0.0 and a["errors"] == 0.0, (
              f"cold read must be all misses, no hits/errors: {a!r}"
          )
          assert a["writethrough_errors"] == 0.0, (
              f"read-through fill failed (writethrough_errors > 0): {a!r}"
          )
          # The read-through filled the cache tier: the local bucket now
          # holds exactly the chunk keys the authoritative bucket does.
          assert_set_eq(
              bucket_chunk_keys("${localBucket}"),
              bucket_chunk_keys("${remoteBucket}"),
              context="local bucket after read-through fill",
          )
    '';

    "replica-warm-via-read-through" = ''
      with subtest("replica-warm: another replica's first read hits the warmed local tier"):
          n_chunks = len(bucket_chunk_keys("${remoteBucket}"))
          local_before = bucket_chunk_keys("${localBucket}")
          assert len(local_before) == n_chunks, (
              f"local tier should already hold all {n_chunks} chunks from "
              f"cold-miss-fallback's read-through, found {len(local_before)}"
          )

          # Replica B has never served a read (its moka is cold) and
          # never wrote anything - its first GetPath goes straight to
          # the tiered backend and must be served from the local tier
          # that replica A's read-through filled.
          get_path(9012, BUSYBOX, "warm-b")

          b = tiered_metrics(9094)
          assert b["hits"] == n_chunks, (
              f"replica B local hits = {b['hits']}, expected {n_chunks}; "
              f"counters: {b!r}"
          )
          assert b["misses"] == 0.0 and b["errors"] == 0.0, (
              f"warm read must not miss or error: {b!r}"
          )
          assert b["writethrough_errors"] == 0.0, (
              f"warm read must not write through again: {b!r}"
          )
          # No double-fill: the local key set is unchanged.
          assert_set_eq(
              bucket_chunk_keys("${localBucket}"),
              local_before,
              context="local bucket after warm read",
          )
    '';

    "local-none-passthrough" = ''
      with subtest("local-none-passthrough: express_bucket unset behaves like plain s3"):
          local_before = bucket_chunk_keys("${localBucket}")

          # A second, distinct path: its chunks exist only in the
          # remote bucket (nothing has read it through an
          # express-enabled replica yet), so any local-tier write by
          # replica C would surface as a NEW key - sharper than
          # re-reading the already-warmed path.
          client.succeed("echo 'rio vm-store-tiered local-none probe' > /tmp/p2")
          p2 = client.succeed("nix-store --add /tmp/p2").strip()
          client.succeed(
              f"nix copy --no-check-sigs --to 'ssh-ng://${gatewayHost}' {p2}"
          )

          get_path(9013, p2, "passthrough-p2")
          get_path(9013, BUSYBOX, "passthrough-busybox")

          c = tiered_metrics(9095)
          assert c == {"hits": 0.0, "misses": 0.0, "errors": 0.0,
                       "writethrough_errors": 0.0}, (
              f"local=None replica must never touch the tiered counters: {c!r}"
          )
          assert_set_eq(
              bucket_chunk_keys("${localBucket}"),
              local_before,
              context="local bucket after local=None reads",
          )
    '';
  };

  mkTest = common.mkFragmentTest {
    scenario = "store-tiered";
    inherit prelude fragments;
    fixture = tieredFixture;
    # 2-VM standalone boot + one seed + four grpcurl reads + one store
    # restart - well inside the standard standalone ceiling.
    defaultTimeout = 600;
    chains = [
      {
        before = "put-remote-only";
        after = "cold-miss-fallback";
        msg = "cold-miss-fallback fills the local bucket, which would break put-remote-only's empty-bucket assertion if reordered";
      }
      {
        before = "cold-miss-fallback";
        after = "replica-warm-via-read-through";
        msg = "replica-warm needs the local tier filled by cold-miss-fallback's read-through";
      }
    ];
  };
in
{
  inherit fragments mkTest;
}
