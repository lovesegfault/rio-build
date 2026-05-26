# Stock-Nix binary-cache compatibility (ADR-022 §10, P0580): with
# `binary_cache_compat` enabled (the default), every path committed via
# the gateway's buffered PutPath is ALSO published to the S3-standard
# bucket as `{hash-part}.narinfo` + `nar/{file-hash}.nar.zst` (plus a
# one-time `nix-cache-info`), so a completely stock `nix` client can
# substitute from the bucket with **no rio process running** — the
# migration on-ramp and the PG-outage disaster floor (U6).
#
# ── Topology ──────────────────────────────────────────────────────────
#
# Standalone fixture (control: PG + rio-store + rio-scheduler +
# rio-gateway; client: stock CppNix) plus Garage on the control node as
# the S3 stand-in (same choice and rationale as vm-store-tiered: minio
# is insecure-flagged in nixpkgs, Garage has a NixOS module and speaks
# the S3 surface both the aws-sdk and CppNix's s3:// store need). One
# bucket ("rio") holds chunks/ AND the compat objects — exactly the
# default deployment shape (`binary_cache_compat.bucket` unset → the
# chunk backend's bucket). Garage's S3 API binds [::]:3900 and the
# firewall opens it so the CLIENT can read the bucket directly; the
# store reaches it over loopback.
#
# The store's chunk backend is configured via env (RIO_CHUNK_BACKEND__*
# = s3 + AWS_ENDPOINT_URL at Garage); the fixture's default filesystem
# TOML is dropped so the env vars are the whole backend config.
#
# Seeds (prelude, compat ON): the static busybox closure and the
# pkgs.hello closure (hello + glibc + friends — a real multi-path
# closure with References) are uploaded from the client through the
# gateway (`nix copy --to ssh-ng://control`), i.e. the buffered PutPath
# path whose inline compat write is synchronous with the upload.
#
# Subtest contract (markers live at default.nix:subtests, NOT here):
#
# compat-off-no-narinfo — flip the runtime toggle OFF (a systemd
#   drop-in setting RIO_BINARY_CACHE_COMPAT__ENABLED=false + restart,
#   the same env surface helm renders), upload a fresh path, and assert
#   the bucket gained NO `.narinfo` for it while the pre-toggle objects
#   are still there: the toggle stops new compat writes and only new
#   ones.
#
# reconciler-backfill-on-reenable — remove the drop-in (compat ON
#   again) and assert the path uploaded while OFF gets its compat pair
#   published by the reconciler (compat_file_hash IS NULL backfill —
#   no re-upload happens), observable as the `.narinfo` appearing in
#   the bucket and rio_store_compat_reconcile_total{result="ok"} > 0
#   with rio_store_compat_backlog back at 0.
#
# stock-nix-substitute — `systemctl stop rio-store`, then on the client
#   `nix copy --from 's3://rio?endpoint=…'` the busybox path, the hello
#   closure root, and the backfilled path into a fresh chroot store:
#   every path (including hello's References closure) lands and
#   `nix store verify` passes — substitution worked with no rio process
#   involved, straight off the bucket.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (pkgs) lib;
  inherit (fixture) gatewayHost;

  bucket = "rio";
  s3Endpoint = "http://127.0.0.1:3900";

  # Fixed test credentials, imported into Garage in the prelude (same
  # shape + rationale as vm-store-tiered: Garage-generated keys would
  # only exist at runtime, after the store already started; the
  # repeated low-entropy patterns are deliberately not secrets).
  s3AccessKey = "GK0123456789abcdef01234567";
  s3SecretKey = "cafef00dcafef00dcafef00dcafef00dcafef00dcafef00dcafef00dcafef00d";
  garageRpcSecret = "deadbeefcafebabedeadbeefcafebabedeadbeefcafebabedeadbeefcafebabe";

  # mc with a fixed config dir so alias state is independent of $HOME.
  mc = "mc --config-dir /tmp/mc";

  # aws-sdk env for the store. Loopback-IP endpoint → path-style
  # addressing (Garage needs no vhost DNS); static credentials; IMDS
  # probing disabled.
  awsEnv = {
    AWS_ENDPOINT_URL = s3Endpoint;
    AWS_ACCESS_KEY_ID = s3AccessKey;
    AWS_SECRET_ACCESS_KEY = s3SecretKey;
    AWS_REGION = "us-east-1";
    AWS_EC2_METADATA_DISABLED = "true";
  };

  # The S3 chunk backend (which is also where the compat objects land:
  # binary_cache_compat.bucket is left unset). Compat itself runs on
  # compiled defaults: enabled, zstd, sync-after-commit, 30s reconciler.
  storeEnv = awsEnv // {
    RIO_CHUNK_BACKEND__KIND = "s3";
    RIO_CHUNK_BACKEND__BUCKET = bucket;
    RIO_CHUNK_BACKEND__PREFIX = "";
  };

  # The client substitutes straight from Garage; nix's s3:// store uses
  # the same credential env vars as the aws-sdk. The endpoint here is
  # the control node's hostname (cross-VM), not loopback.
  clientAwsEnv = lib.concatStringsSep " " [
    "AWS_ACCESS_KEY_ID=${s3AccessKey}"
    "AWS_SECRET_ACCESS_KEY=${s3SecretKey}"
    "AWS_EC2_METADATA_DISABLED=true"
  ];
  cacheUrl = "s3://${bucket}?endpoint=http://control:3900&region=us-east-1";

  # A real multi-path closure (hello + glibc + …) so the substitution
  # exercises References traversal, not just single-path fetches.
  inherit (pkgs) hello;
  helloClosure = pkgs.closureInfo { rootPaths = [ hello ]; };

  controlExtension = {
    services.garage = {
      enable = true;
      package = pkgs.garage_1;
      settings = {
        replication_factor = 1;
        rpc_bind_addr = "127.0.0.1:3901";
        rpc_public_addr = "127.0.0.1:3901";
        rpc_secret = garageRpcSecret;
        s3_api = {
          # [::] (not loopback): the CLIENT reads the bucket directly in
          # stock-nix-substitute. Dual-stack via v4-mapped addresses.
          api_bind_addr = "[::]:3900";
          # Must match the SigV4 credential scope both the store's
          # aws-sdk and the client's s3:// URL sign with.
          s3_region = "us-east-1";
          root_domain = ".s3.garage.localhost";
        };
      };
    };

    systemd.services.rio-store.environment = storeEnv;

    networking.firewall.allowedTCPPorts = [ 3900 ];

    environment.systemPackages = [ pkgs.minio-client ];

    # PG + scheduler + gateway + store + Garage, plus a glibc-sized
    # upload buffered in the store during PutPath — give it headroom.
    virtualisation.memorySize = lib.mkForce (2048 + (if common.coverage then 256 else 0));
    virtualisation.diskSize = lib.mkForce 6144;
  };

  clientExtension = {
    # hello (and thereby its runtime closure) registered in the
    # client's store so `nix copy --to ssh-ng://…` can read it; the
    # closureInfo provides the explicit path list (same pattern as the
    # fixture's busybox seeding).
    environment.systemPackages = [ hello ];
    environment.etc."rio/hello-closure".source = "${helloClosure}";
  };

  compatFixture = fixture // {
    nodes = fixture.nodes // {
      control = {
        imports = [
          fixture.nodes.control
          controlExtension
        ];
      };
      client = {
        imports = [
          fixture.nodes.client
          clientExtension
        ];
      };
    };
  };

  prelude = ''
    ${common.mkBootstrap {
      fixture = compatFixture;
      inherit gatewayHost;
    }}

    control.wait_for_unit("garage.service")
    control.wait_for_open_port(3900)
    # Single-node Garage bring-up: storage role + layout, the bucket,
    # and the fixed key the store already carries in its env. Must
    # precede the first PutPath — the seed upload below immediately
    # writes chunks AND the compat objects to this bucket.
    node_id = control.succeed("garage node id -q").strip().split("@")[0]
    control.succeed(f"garage layout assign -z dc1 -c 1G {node_id}")
    control.succeed("garage layout apply --version 1")
    control.succeed("garage bucket create ${bucket}")
    control.succeed(
        "garage key import --yes -n vmtest ${s3AccessKey} ${s3SecretKey}"
    )
    control.succeed(
        "garage bucket allow --read --write --owner ${bucket} --key vmtest"
    )
    control.succeed(
        "${mc} alias set garage ${s3Endpoint} ${s3AccessKey} ${s3SecretKey}"
    )

    BUSYBOX = "${common.busybox}"
    HELLO = "${hello}"


    def narinfo_key(store_path):
        """`<hash-part>.narinfo` — the compat object key for a path."""
        return store_path.removeprefix("/nix/store/").split("-")[0] + ".narinfo"


    def bucket_has(key):
        """True iff the object exists in the bucket (mc stat exit code)."""
        rc, _ = control.execute(f"${mc} stat garage/${bucket}/{key}")
        return rc == 0


    def compat_metric(name, labels=""):
        """One rio_store_compat_* series from the store exporter
        (absent-until-first-increment counters read as 0). `labels` is
        the raw `{k="v"}` string parse_prometheus uses as the key, or
        "" for unlabeled series."""
        scraped = scrape_metrics(control, 9092)
        return scraped.get(name, {}).get(labels, 0.0)


    # Seed (compat ON, the compiled default): busybox + the hello
    # closure through the gateway. The inline compat write is
    # synchronous with PutPath, so once these copies return the bucket
    # already holds the narinfo/NAR pairs.
    ${common.seedBusybox gatewayHost}
    client.succeed(
        "nix copy --no-check-sigs --to 'ssh-ng://${gatewayHost}' "
        "$(cat /etc/rio/hello-closure/store-paths)"
    )

    # Sanity: the seeds' compat objects are present before any subtest
    # mutates the toggle (also proves nix-cache-info bootstrap).
    for p in [BUSYBOX, HELLO]:
        assert bucket_has(narinfo_key(p)), (
            f"compat narinfo for {p} missing right after PutPath - "
            "is binary_cache_compat enabled (it should be the default)?"
        )
    assert bucket_has("nix-cache-info"), "nix-cache-info bootstrap object missing"
  '';

  fragments = {
    "compat-off-no-narinfo" = ''
      with subtest("compat-off: ENABLED=false stops new compat writes"):
          # Flip the runtime toggle exactly the way helm does - the env
          # var - via a runtime drop-in, and restart the store.
          control.succeed("mkdir -p /run/systemd/system/rio-store.service.d")
          control.succeed(
              "{ echo '[Service]'; "
              "echo 'Environment=RIO_BINARY_CACHE_COMPAT__ENABLED=false'; } "
              "> /run/systemd/system/rio-store.service.d/compat-off.conf"
          )
          control.succeed("systemctl daemon-reload")
          control.succeed("systemctl restart rio-store.service")
          control.wait_for_open_port(9002)

          # A fresh path uploaded while compat is OFF. The first copy
          # after the restart may race the gateway's lazy reconnect to
          # the store, so retry until it lands.
          client.succeed("echo 'rio vm-store-compat: uploaded while compat is off' > /tmp/p-off")
          p_off = client.succeed("nix-store --add /tmp/p-off").strip()
          client.wait_until_succeeds(
              f"nix copy --no-check-sigs --to 'ssh-ng://${gatewayHost}' {p_off}",
              timeout=60,
          )
          with open("/tmp/p-off-path", "w") as f:
              f.write(p_off)

          assert not bucket_has(narinfo_key(p_off)), (
              f"compat is OFF but {narinfo_key(p_off)} appeared in the bucket - "
              "the runtime toggle is not gating the writer"
          )
          # Pre-toggle objects are untouched: OFF only stops NEW writes.
          assert bucket_has(narinfo_key(BUSYBOX)), (
              "pre-toggle narinfo disappeared after flipping compat off"
          )
    '';

    "reconciler-backfill-on-reenable" = ''
      with subtest("re-enable: the reconciler backfills the gap"):
          p_off = open("/tmp/p-off-path").read().strip()
          assert not bucket_has(narinfo_key(p_off)), (
              "p-off already has compat objects before re-enabling - "
              "compat-off-no-narinfo did not run first?"
          )

          control.succeed("rm /run/systemd/system/rio-store.service.d/compat-off.conf")
          control.succeed("systemctl daemon-reload")
          control.succeed("systemctl restart rio-store.service")
          control.wait_for_open_port(9002)

          # No re-upload happens here: the only mechanism that can
          # publish p-off's pair is the compat reconciler finding its
          # narinfo row with compat_file_hash IS NULL.
          control.wait_until_succeeds(
              f"${mc} stat garage/${bucket}/{narinfo_key(p_off)}",
              timeout=120,
          )
          assert compat_metric(
              "rio_store_compat_reconcile_total", '{result="ok"}'
          ) >= 1, "reconciler ok-counter did not move although the narinfo appeared"
          # Backlog drained (the gauge is refreshed at the start of
          # every reconciler batch; the next tick after the backfill
          # reports 0).
          control.wait_until_succeeds(
              "curl -sf http://localhost:9092/metrics | "
              "grep -E '^rio_store_compat_backlog 0(\\.0)?$'",
              timeout=90,
          )
    '';

    "stock-nix-substitute" = ''
      with subtest("stock nix substitutes from the bucket with rio-store stopped"):
          p_off = open("/tmp/p-off-path").read().strip()
          for p in [BUSYBOX, HELLO, p_off]:
              assert bucket_has(narinfo_key(p)), (
                  f"narinfo for {p} missing from the bucket before the substitution check"
              )

          # The load-bearing part: no rio process serves these reads.
          control.succeed("systemctl stop rio-store.service")
          control.fail("systemctl is-active --quiet rio-store.service")

          # Stock CppNix on the client reads the bucket directly (the
          # store URL is plain s3:// + endpoint; the credentials are the
          # same fixed Garage key the store used).
          out_store = "/tmp/substituted"
          client.succeed(
              f"${clientAwsEnv} nix copy --no-check-sigs "
              f"--from '${cacheUrl}' --to {out_store} "
              f"{BUSYBOX} {HELLO} {p_off}"
          )

          # Every requested path landed - including hello's References
          # closure, which stock nix resolved from the narinfo objects
          # alone.
          for p in [BUSYBOX, HELLO, p_off]:
              client.succeed(f"test -e {out_store}{p}")
          closure = client.succeed(
              f"${clientAwsEnv} nix path-info --store {out_store} -r {HELLO}"
          ).strip().splitlines()
          assert len(closure) >= 2, (
              f"hello's closure in the substituted store is suspiciously small: {closure!r}"
          )
          for dep in closure:
              client.succeed(f"test -e {out_store}{dep.strip()}")

          # Content integrity per the local store's metadata (NAR hashes
          # recorded from the narinfo objects). --no-trust: the compat
          # narinfo carries no signature in this fixture (no signing key
          # configured) and trust is not the property under test.
          client.succeed(
              f"${clientAwsEnv} nix store verify --store {out_store} --no-trust --all"
          )
    '';
  };

  mkTest = common.mkFragmentTest {
    scenario = "store-compat";
    inherit prelude fragments;
    fixture = compatFixture;
    # Boot + Garage bring-up + a glibc-sized closure upload (PutPath +
    # inline zstd compat write) + two store restarts + the client-side
    # substitution downloads. Generous tail headroom for loaded CI
    # builders.
    defaultTimeout = 900;
    chains = [
      {
        before = "compat-off-no-narinfo";
        after = "reconciler-backfill-on-reenable";
        msg = "the backfill subtest re-enables compat for the path uploaded while it was off";
      }
      {
        before = "reconciler-backfill-on-reenable";
        after = "stock-nix-substitute";
        msg = "the substitution set includes the backfilled path and stops rio-store for good";
      }
    ];
  };
in
{
  inherit fragments mkTest;
}
