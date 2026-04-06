# Non-rustc check derivations (shared by checks.* and ci aggregate).
#
# Rust checks (clippy/nextest/doc/coverage) are in nix/checks.nix
# (per-crate caching, deps built once). These are the rest:
# workspace-level policy checks that don't invoke rustc.
{
  pkgs,
  inputs,
  config,
  version,
  unfilteredRoot,
  workspaceFileset,
  rustStable,
  rustPlatformStable,
  traceyPkg,
  subcharts,
  dockerImages,
  nodeAmi,
}:
{
  # License + advisory audit. Policy: deny GPL-3.0 (project is
  # MIT/Apache), fail on RustSec advisories with a curated
  # ignore list in .cargo/deny.toml. The advisory DB is a
  # flake input (hermetic — no network). Bump via `nix flake
  # update advisory-db` to pick up new advisories.
  #
  # cargo-deny internally runs `cargo metadata` to resolve
  # the full dep tree for license/advisory analysis. That
  # needs vendored sources (cargoSetupHook writes a source-
  # replacement config so cargo finds crates.io deps in the
  # vendored dir instead of the registry index).
  deny = pkgs.stdenv.mkDerivation {
    pname = "rio-deny";
    inherit version;
    src = pkgs.lib.fileset.toSource {
      root = unfilteredRoot;
      fileset = pkgs.lib.fileset.unions [
        ../.cargo/deny.toml
        workspaceFileset
      ];
    };
    cargoDeps = rustPlatformStable.importCargoLock {
      lockFile = ../Cargo.lock;
    };
    nativeBuildInputs = with pkgs; [
      cargo-deny
      rustStable
      rustPlatformStable.cargoSetupHook
      git
    ];
    # cargoSetupHook writes .cargo/config.toml with vendored
    # source replacement. cargo metadata reads it; no registry
    # access needed.
    buildPhase = ''
      # HOME defaults to /homeless-shelter (RO). deny.toml's
      # db-path = "~/.cargo/advisory-db" resolves against
      # HOME. cargo-deny expects the DB as a GIT REPO (reads
      # HEAD to determine DB version for the report). The
      # flake input is a plain dir (flake=false strips .git),
      # so we init a throwaway repo with the content.
      export HOME=$TMPDIR
      db=$HOME/.cargo/advisory-db/advisory-db-3157b0e258782691
      mkdir -p "$db"
      cp -r ${inputs.advisory-db}/. "$db"/
      chmod -R u+w "$db"
      git -C "$db" init -q
      git -C "$db" add -A
      git -C "$db" \
        -c user.name=nix -c user.email=nix@localhost \
        commit -q -m snapshot
      cargo deny \
        --manifest-path ./Cargo.toml \
        --offline \
        check \
        --config ./.cargo/deny.toml \
        --disable-fetch \
        advisories licenses bans sources \
        2>&1 | tee deny.out
    '';
    installPhase = ''
      cp deny.out $out
    '';
  };

  # Spec-coverage validation: fails on broken r[...]
  # references, duplicate requirement IDs, or unparseable
  # include files. Does NOT fail on uncovered/untested — those
  # are informational.
  #
  # Uses cleanSource because tracey needs docs/**/*.md and
  # .config/tracey/config.styx. tracey's daemon writes
  # .tracey/daemon.sock under the working dir, so we cp to a
  # writable tmpdir first.
  #
  # .claude/ is excluded via fileset.difference — tracey's config
  # doesn't scan it, so including it in the drv src causes spurious
  # rebuilds on every agent-file edit. Exclusion makes the
  # clause-4(a) fast-path premise ("`.claude/`-only edits are
  # hash-identical to `.#ci`") TRUE rather than merely
  # behavioral-identity. Saves one rebuild per `.claude/` commit.
  tracey-validate =
    pkgs.runCommand "rio-tracey-validate"
      {
        src = pkgs.lib.fileset.toSource {
          root = ../.;
          fileset = pkgs.lib.fileset.difference (pkgs.lib.fileset.fromSource (pkgs.lib.cleanSource ../.)) ../.claude;
        };
        nativeBuildInputs = [ traceyPkg ];
      }
      ''
        cp -r $src $TMPDIR/work
        chmod -R +w $TMPDIR/work
        cd $TMPDIR/work
        rm -rf .tracey/
        export HOME=$TMPDIR
        set -o pipefail
        # Retry once: `tracey query validate` auto-starts a daemon
        # and waits 5s for the socket. Under sandbox parallel-build
        # load the socket-wait races ("Daemon failed to start within
        # 5s" / "Error getting status: Cancelled"). tracey 1.3.0 has
        # no --no-daemon mode and no TRACEY_DAEMON_TIMEOUT knob, so
        # retry-once is the minimal fix. P0490.
        tracey query validate 2>&1 | tee $out || {
          echo "retry: first tracey attempt failed, retrying once" >&2
          rm -rf .tracey/  # clear partial daemon state
          sleep 2
          tracey query validate 2>&1 | tee $out
        }
      '';

  # Helm chart lint + template for all value profiles. Catches
  # Go-template syntax errors, missing required values, bad
  # YAML in rendered output. Subcharts symlinked from nixhelm
  # (FOD) — `helm dependency build` needs network.
  helm-lint =
    let
      chart = pkgs.lib.cleanSource ../infra/helm/rio-build;
    in
    pkgs.runCommand "rio-helm-lint"
      {
        nativeBuildInputs = [
          pkgs.kubernetes-helm
          pkgs.yq-go
          pkgs.jq
        ];
      }
      ''
        cp -r ${chart} $TMPDIR/chart
        chmod -R +w $TMPDIR/chart
        cd $TMPDIR/chart
        mkdir -p charts
        ln -s ${subcharts.postgresql} charts/postgresql
        ln -s ${subcharts.rustfs} charts/rustfs
        helm lint .
        # Default (prod) profile: tag must be set (empty → bad image ref).
        helm template rio . --set global.image.tag=test > /tmp/default.yaml
        helm template rio . -f values/dev.yaml > /dev/null
        helm template rio . -f values/kind.yaml > /dev/null
        helm template rio . -f values/vmtest-full.yaml > /dev/null
        # ADR-021: karpenter.enabled requires amiTag (NixOS AMI is
        # the only EC2NodeClass — no Bottlerocket fallback). The
        # `required` template func should fail without it.
        if helm template rio . --set global.image.tag=test \
          --set karpenter.enabled=true \
          --set karpenter.clusterName=ci \
          --set karpenter.nodeRoleName=ci 2>/dev/null; then
          echo "FAIL: karpenter.enabled=true without amiTag should fail render" >&2
          exit 1
        fi
        # monitoring-on: ServiceMonitor/PodMonitor/PrometheusRule
        # templates are gated and otherwise never rendered by CI.
        helm template rio . --set global.image.tag=test \
          --set monitoring.enabled=true > /tmp/monitoring.yaml
        for k in ServiceMonitor PodMonitor PrometheusRule; do
          grep -qx "kind: $k" /tmp/monitoring.yaml || {
            echo "FAIL: monitoring.enabled=true did not render kind: $k" >&2
            exit 1
          }
        done

        # dash-on: all CRD kinds + third-party images present. Rendered
        # here (before the digest-pin loop below and the Gateway API
        # CRD checks after) so one render feeds both.
        helm template rio . \
          --set dashboard.enabled=true \
          --set global.image.tag=test \
          --set postgresql.enabled=false \
          > /tmp/dash-on.yaml

        # ── Third-party image digest-pin enforcement ────────────────
        # Every image that isn't a rio-build image (those get `:test`
        # from --set global.image.tag=test above) MUST be digest-
        # pinned. A floating third-party tag that doesn't exist /
        # gets deleted / gets overwritten upstream → ImagePullBackOff
        # → component-specific silent brick:
        #   - devicePlugin: smarter-devices/fuse never registers →
        #     worker pods Pending (a prior default pointed at a
        #     never-published upstream tag; r[sec.pod.fuse-device-plugin])
        #   - envoyImage: gRPC-Web translation dead → dashboard loads
        #     but every RPC fails (r[dash.envoy.grpc-web-translate])
        #   - <future>: same failure mode, this loop catches it pre-merge
        #
        # yq drills into every container spec (DaemonSet + Deployment
        # + StatefulSet + Job) plus the EnvoyProxy CRD's image path
        # (different shape — .spec.provider.kubernetes.envoyDeployment
        # .container.image, not .spec.template.spec.containers[]).
        # Runs over BOTH default.yaml (prod profile) and dash-on.yaml
        # (dashboard-enabled superset). Filters out :test-tagged rio
        # images and fails on any remaining bare-tag image. @sha256:
        # is the pin marker. Subchart images (bitnami/ PG) are a
        # separate supply-chain boundary — postgresql.enabled=false
        # in both renders so they don't appear here.
        thirdparty=$(yq eval-all '
          ( select(.kind=="DaemonSet" or .kind=="Deployment"
                   or .kind=="StatefulSet" or .kind=="Job")
            | .spec.template.spec.containers[].image ),
          ( select(.kind=="EnvoyProxy")
            | .spec.provider.kubernetes.envoyDeployment.container.image )
        ' /tmp/default.yaml /tmp/dash-on.yaml \
          | grep -v ':test$' \
          | grep -v '^---$' | grep -v '^null$' \
          | sort -u)
        echo "third-party images in default+dash-on renders:" >&2
        echo "$thirdparty" >&2
        bad=$(echo "$thirdparty" | grep -v '@sha256:' || true)
        if [ -n "$bad" ]; then
          echo "FAIL: third-party image(s) not digest-pinned:" >&2
          echo "$bad" >&2
          exit 1
        fi

        # ── dashboard Gateway API CRDs ─────────────────────────────
        # dashboard.enabled=true MUST render exactly one each of
        # GatewayClass/Gateway/GRPCRoute/EnvoyProxy/SecurityPolicy/
        # ClientTrafficPolicy (+ BackendTLSPolicy when tls.enabled).
        # Any Go-template syntax error, bad nindent, or Values typo
        # surfaces here before the VM test has to spend 5min on k3s
        # bring-up to discover a YAML parse error. (/tmp/dash-on.yaml
        # rendered above, before the third-party image loop.)
        for k in GatewayClass Gateway GRPCRoute EnvoyProxy \
                 SecurityPolicy ClientTrafficPolicy BackendTLSPolicy; do
          grep -qx "kind: $k" /tmp/dash-on.yaml || {
            echo "FAIL: dashboard.enabled=true did not render kind: $k" >&2
            exit 1
          }
        done
        # grpc_web filter auto-inject is a runtime property of Envoy
        # Gateway's xDS translator, not something helm-lint can prove
        # — the GRPCRoute existence + Gateway single-listener is the
        # static contract.
        n=$(yq 'select(.kind=="Gateway" and .metadata.name=="rio-dashboard")
                | .spec.listeners | length' /tmp/dash-on.yaml)
        test "$n" -eq 1 || {
          echo "FAIL: rio-dashboard Gateway must have exactly 1 listener (dodges #7559), got $n" >&2
          exit 1
        }

        # ── r[dash.auth.method-gate] fail-closed proof ─────────────
        # Default values (enableMutatingMethods=false) MUST NOT render
        # the mutating GRPCRoute. If this assert fails, a values.yaml
        # typo (or a template guard regression) has fail-OPENED
        # ClearPoison/DrainWorker/CreateTenant/TriggerGC to any
        # browser that can reach the gateway.
        #
        # `grep -x >/dev/null`, NOT `grep -qx`: stdenv runs with
        # pipefail. grep -q exits on first match → closes pipe → yq's
        # next write SIGPIPEs → yq exit 141 → pipeline fails → false
        # FAIL. Go's stdout is unbuffered to a pipe, so each output
        # line is a separate write() — grep can race-close between
        # them. ~120 bytes fits the 64K pipe buffer normally; under
        # 192-core scheduler contention it doesn't. Observed: same
        # drv flapped FAIL at different yq sites on consecutive runs.
        # Dropping -q makes grep drain the pipe; no SIGPIPE.
        ! yq 'select(.kind=="GRPCRoute") | .metadata.name' /tmp/dash-on.yaml \
          | grep -x rio-scheduler-mutating >/dev/null || {
          echo "FAIL: rio-scheduler-mutating GRPCRoute rendered with default values (enableMutatingMethods should default false)" >&2
          exit 1
        }
        # Readonly route MUST render and MUST carry ClusterStatus
        # (proves the route-split didn't drop the load-bearing
        # unary-test target — dashboard-gateway.nix curl depends
        # on ClusterStatus routing).
        yq 'select(.kind=="GRPCRoute" and .metadata.name=="rio-scheduler-readonly")
            | .spec.rules[].matches[].method.method' /tmp/dash-on.yaml \
          | grep -x ClusterStatus >/dev/null || {
          echo "FAIL: rio-scheduler-readonly missing ClusterStatus match" >&2
          exit 1
        }
        # ClearPoison must NOT leak into the readonly route — a
        # one-line yaml indent mistake could silently attach it.
        ! yq 'select(.kind=="GRPCRoute" and .metadata.name=="rio-scheduler-readonly")
              | .spec.rules[].matches[].method.method' /tmp/dash-on.yaml \
          | grep -x ClearPoison >/dev/null || {
          echo "FAIL: ClearPoison leaked into readonly GRPCRoute" >&2
          exit 1
        }
        # CORS allowOrigins MUST NOT be wildcard by default. The
        # earlier MVP had "*" — regression guard. yq-go `select()`
        # doesn't short-circuit like jq; pipe to grep -x instead.
        ! yq 'select(.kind=="SecurityPolicy" and .metadata.name=="rio-dashboard-cors")
              | .spec.cors.allowOrigins[]' /tmp/dash-on.yaml \
          | grep -x '\*' >/dev/null || {
          echo "FAIL: SecurityPolicy rio-dashboard-cors allowOrigins contains wildcard" >&2
          exit 1
        }
        # Positive: flipping enableMutatingMethods=true DOES render
        # the mutating route + ClearPoison. Proves the flag is
        # wired (not a typo'd Values path that evals to nil —
        # helm treats undefined as false, so a bad path silently
        # gates forever-off).
        helm template rio . \
          --set dashboard.enabled=true \
          --set dashboard.enableMutatingMethods=true \
          --set global.image.tag=test \
          --set postgresql.enabled=false \
          > /tmp/dash-mut.yaml
        yq 'select(.kind=="GRPCRoute" and .metadata.name=="rio-scheduler-mutating")
            | .spec.rules[].matches[].method.method' /tmp/dash-mut.yaml \
          | grep -x ClearPoison >/dev/null || {
          echo "FAIL: enableMutatingMethods=true did not render mutating GRPCRoute with ClearPoison" >&2
          exit 1
        }

        # ── JWT mount assertions (r[sec.jwt.pubkey-mount]) ──────────
        # jwt.enabled=true MUST render the ConfigMap mount in
        # scheduler+store and the Secret mount in gateway.
        # Without the mount, RIO_JWT__KEY_PATH stays unset → the
        # interceptor is inert → silent fail-open (every JWT passes
        # unverified). The ConfigMap/Secret OBJECTS exist; the MOUNT
        # was missing. --set-string for the base64 values — trailing
        # '=' padding is fine (everything after the first '=' is the
        # value); --set would try to parse it as YAML.
        helm template rio . \
          --set jwt.enabled=true \
          --set-string jwt.publicKey=dGVzdA== \
          --set-string jwt.signingSeed=dGVzdA== \
          --set global.image.tag=test \
          --set postgresql.enabled=false \
          > $TMPDIR/jwt-on.yaml

        # 3 = scheduler + store + gateway. Each gets exactly one
        # RIO_JWT__KEY_PATH env var. >3 would mean a template got
        # included twice (leaky nindent loop); <3 = missing include.
        n=$(grep -c RIO_JWT__KEY_PATH $TMPDIR/jwt-on.yaml)
        test "$n" -eq 3 || {
          echo "FAIL: expected 3 RIO_JWT__KEY_PATH (sched+store+gw), got $n" >&2
          exit 1
        }

        # yq: structural asserts. grep would match the ConfigMap
        # resource's own `name: rio-jwt-pubkey` — yq drills into
        # the Deployment's pod spec so we're asserting the MOUNT,
        # not the object existing.
        for dep in rio-scheduler rio-store; do
          # volumes: entry — configMap ref to rio-jwt-pubkey.
          yq "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")
              | .spec.template.spec.volumes[]
              | select(.name==\"jwt-pubkey\")
              | .configMap.name" $TMPDIR/jwt-on.yaml \
            | grep -x rio-jwt-pubkey >/dev/null || {
            echo "FAIL: $dep missing jwt-pubkey configMap volume" >&2
            exit 1
          }
          # volumeMounts: entry — path + readOnly.
          yq "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")
              | .spec.template.spec.containers[0].volumeMounts[]
              | select(.name==\"jwt-pubkey\")
              | .mountPath" $TMPDIR/jwt-on.yaml \
            | grep -x /etc/rio/jwt >/dev/null || {
            echo "FAIL: $dep missing jwt-pubkey volumeMount at /etc/rio/jwt" >&2
            exit 1
          }
          # env: RIO_JWT__KEY_PATH points at the file the Rust side reads.
          yq "select(.kind==\"Deployment\" and .metadata.name==\"$dep\")
              | .spec.template.spec.containers[0].env[]
              | select(.name==\"RIO_JWT__KEY_PATH\")
              | .value" $TMPDIR/jwt-on.yaml \
            | grep -x /etc/rio/jwt/ed25519_pubkey >/dev/null || {
            echo "FAIL: $dep RIO_JWT__KEY_PATH != /etc/rio/jwt/ed25519_pubkey" >&2
            exit 1
          }
        done

        # Gateway: Secret mount (signing side).
        yq 'select(.kind=="Deployment" and .metadata.name=="rio-gateway")
            | .spec.template.spec.volumes[]
            | select(.name=="jwt-signing")
            | .secret.secretName' $TMPDIR/jwt-on.yaml \
          | grep -x rio-jwt-signing >/dev/null || {
          echo "FAIL: gateway missing jwt-signing Secret volume" >&2
          exit 1
        }
        yq 'select(.kind=="Deployment" and .metadata.name=="rio-gateway")
            | .spec.template.spec.containers[0].env[]
            | select(.name=="RIO_JWT__KEY_PATH")
            | .value' $TMPDIR/jwt-on.yaml \
          | grep -x /etc/rio/jwt/ed25519_seed >/dev/null || {
          echo "FAIL: gateway RIO_JWT__KEY_PATH != /etc/rio/jwt/ed25519_seed" >&2
          exit 1
        }

        # Negative: jwt.enabled=false renders NO mount. The or-gate
        # in scheduler/store/gateway elides the volumeMounts/volumes
        # keys when nothing is enabled, and the self-guarded
        # templates render nothing. "! grep" exits 0 on no-match,
        # 1 on match → && flips to fail-fast.
        helm template rio . \
          --set global.image.tag=test \
          --set tls.enabled=false \
          --set postgresql.enabled=false \
          > $TMPDIR/jwt-off.yaml
        ! grep -q 'RIO_JWT__KEY_PATH\|jwt-pubkey\|jwt-signing' $TMPDIR/jwt-off.yaml || {
          echo "FAIL: jwt mount rendered with jwt.enabled=false (default)" >&2
          grep -n 'RIO_JWT__KEY_PATH\|jwt-pubkey\|jwt-signing' $TMPDIR/jwt-off.yaml >&2
          exit 1
        }

        # ── rio.optBool: with-on-bool footgun guard ─────────────────
        # Explicit `false` MUST render. Helm's `with` is falsy-skip —
        # `hostUsers: false` in values produced NO key (controller
        # default applied instead of the explicit override). hasKey
        # renders the key whenever it's SET, regardless of value.
        # fetcherpool.yaml deep-merges fetcherPoolDefaults into each
        # fetcherPools[] entry; setting on defaults covers all CRs.
        helm template rio . \
          --set fetcherPoolDefaults.enabled=true \
          --set fetcherPoolDefaults.hostUsers=false \
          --set global.image.tag=test \
          --set postgresql.enabled=false \
          > $TMPDIR/fp-false.yaml
        yq 'select(.kind=="FetcherPool") | .spec.hostUsers' $TMPDIR/fp-false.yaml \
          | grep -x false >/dev/null || {
          echo "FAIL: fetcherPoolDefaults.hostUsers=false did not render (with-on-bool bug)" >&2
          exit 1
        }
        # Unset stays unset — no spurious key. --set key=null deletes
        # the key from the values map (Helm deep-merge semantics), so
        # hasKey sees it as absent and the template renders nothing.
        helm template rio . \
          --set fetcherPoolDefaults.enabled=true \
          --set fetcherPoolDefaults.hostUsers=null \
          --set global.image.tag=test \
          --set postgresql.enabled=false \
          > $TMPDIR/fp-unset.yaml
        test "$(yq 'select(.kind=="FetcherPool") | .spec | has("hostUsers")' $TMPDIR/fp-unset.yaml)" = false || {
          echo "FAIL: fetcherPoolDefaults.hostUsers unset but key rendered (spurious key)" >&2
          exit 1
        }

        # ── r[obs.metric.builder-util] dashboard regex ──────────────
        # builder-utilization.json's cAdvisor queries select pods by
        # regex. STS naming is `sts_name()` = `rio-builder-{pool}` →
        # pods `rio-builder-{pool}-{ordinal}`; ephemeral Jobs use
        # `rio-builder-{pool}-{6char}` (I-104). Assert the
        # `-builder-` infix so a future dashboard or controller
        # rename that desyncs the two fails here, not silently at
        # "why is this Grafana panel empty".
        jq -r '.panels[].targets[]?.expr' \
          ${../infra/helm/grafana/builder-utilization.json} \
          | grep 'container_cpu_usage\|container_memory' \
          | grep -- '-builder-' >/dev/null \
          || { echo "FAIL: builder-utilization.json pod regex doesn't match controller naming (rio-builder-{pool}-{N})" >&2; exit 1; }

        # ── r[sec.psa.control-plane-restricted] bootstrap-job ───────
        # P0460 missed bootstrap-job.yaml (default-off → CI never
        # rendered it). First prod install with bootstrap.enabled=true
        # failed PSA admission at the pre-install hook. Assert both
        # pod- and container-level securityContext render so future
        # helper drift can't re-break it.
        helm template rio . \
          --set bootstrap.enabled=true \
          --set global.image.tag=test \
          --set postgresql.enabled=false \
          > $TMPDIR/bootstrap-on.yaml
        yq 'select(.kind=="Job" and .metadata.name=="rio-bootstrap")
            | .spec.template.spec.securityContext.runAsNonRoot' \
          $TMPDIR/bootstrap-on.yaml | grep -x true >/dev/null || {
          echo "FAIL: rio-bootstrap Job pod securityContext.runAsNonRoot != true" >&2
          exit 1
        }
        yq 'select(.kind=="Job" and .metadata.name=="rio-bootstrap")
            | .spec.template.spec.containers[0].securityContext.capabilities.drop[0]' \
          $TMPDIR/bootstrap-on.yaml | grep -x ALL >/dev/null || {
          echo "FAIL: rio-bootstrap Job container securityContext.capabilities.drop[0] != ALL" >&2
          exit 1
        }

        # ── r[infra.node.nixos-ami] full cutover ────────────────────
        # karpenter.enabled=true MUST render the rio-default
        # EC2NodeClass with amiFamily: AL2023, the rio.build/ami
        # tag selector, and NO userData (sysctls/seccomp/device-
        # plugin are baked into the AMI — nix/nixos-node/).
        helm template rio . \
          --set karpenter.enabled=true \
          --set karpenter.clusterName=ci \
          --set karpenter.nodeRoleName=ci-role \
          --set karpenter.amiTag=test \
          --set global.image.tag=test \
          --set postgresql.enabled=false \
          > $TMPDIR/karp-on.yaml
        test "$(yq 'select(.kind=="EC2NodeClass" and .metadata.name=="rio-default")
                    | .spec.amiFamily' $TMPDIR/karp-on.yaml)" = AL2023 || {
          echo "FAIL: rio-default EC2NodeClass amiFamily != AL2023" >&2
          exit 1
        }
        test "$(yq 'select(.kind=="EC2NodeClass" and .metadata.name=="rio-default")
                    | .spec.amiSelectorTerms[0].tags."rio.build/ami"' \
                    $TMPDIR/karp-on.yaml)" = test || {
          echo "FAIL: rio-default amiSelectorTerms[0] missing rio.build/ami=test tag" >&2
          exit 1
        }
        test "$(yq 'select(.kind=="EC2NodeClass" and .metadata.name=="rio-default")
                    | .spec.userData' $TMPDIR/karp-on.yaml)" = null || {
          echo "FAIL: rio-default EC2NodeClass renders userData (should be baked into AMI)" >&2
          exit 1
        }
        # No Bottlerocket functionality in the karpenter render —
        # the cutover deletes the fallback entirely. Check for
        # functional markers (amiAlias key, Bottlerocket TOML
        # settings sections, the canary NodeClass), not the bare
        # word — comments mention it in past tense.
        for pat in 'amiAlias:' '\[settings\.' 'name: rio-nixos$' \
                   'rio-builder-nixos-canary' 'amiFamily: Bottlerocket'; do
          if grep -Eq "$pat" $TMPDIR/karp-on.yaml; then
            echo "FAIL: Bottlerocket-era pattern '$pat' present in karpenter render" >&2
            grep -En "$pat" $TMPDIR/karp-on.yaml >&2
            exit 1
          fi
        done
        # device-plugin-conf-parity: the chart's rio.devicePluginConf
        # (k3s DaemonSet) and the AMI's eks-node.nix devicePluginConf
        # MUST list the same devicematch regexes. Compare the regexes
        # only (nummaxdevices is a chart value vs a NixOS option,
        # both default 100 — drift there is intentional config, not
        # a bug). /tmp/default.yaml (rendered above with karpenter.
        # enabled=false) carries the DaemonSet ConfigMap.
        yq 'select(.kind=="ConfigMap" and .metadata.name=="rio-device-plugin-config")
            | .data."conf.yaml"' /tmp/default.yaml \
            | yq '.[].devicematch' | sort > $TMPDIR/dp-conf-chart
        grep 'devicematch:' ${./nixos-node/eks-node.nix} \
            | sed 's/.*devicematch: //' | sort > $TMPDIR/dp-conf-ami
        diff $TMPDIR/dp-conf-chart $TMPDIR/dp-conf-ami || {
          echo "FAIL: device-plugin conf.yaml drift: chart vs nix/nixos-node/eks-node.nix" >&2
          exit 1
        }
        # DaemonSet MUST NOT render with karpenter.enabled=true — the
        # AMI systemd unit and a DS would race to register the same
        # kubelet socket.
        ! yq 'select(.kind=="DaemonSet") | .metadata.name' \
          $TMPDIR/karp-on.yaml | grep -x rio-device-plugin >/dev/null || {
          echo "FAIL: rio-device-plugin DaemonSet rendered with karpenter.enabled=true (AMI systemd unit only)" >&2
          exit 1
        }
        # NodeOverlay (synthetic capacity) MUST still render —
        # Karpenter bin-packs from zero nodes, before any systemd
        # unit can register.
        yq 'select(.kind=="NodeOverlay") | .metadata.name' \
          $TMPDIR/karp-on.yaml | grep -x rio-builder-fuse >/dev/null || {
          echo "FAIL: NodeOverlay rio-builder-fuse missing with karpenter.enabled=true" >&2
          exit 1
        }

        touch $out
      '';

  # CRD drift: crdgen output (split per-CRD) must equal the
  # committed infra/helm/crds/. Catches the "Rust CRD struct
  # changed but nobody ran cargo xtask regen crds" drift — the committed
  # YAML is what Argo syncs, so a stale file means the deployed
  # schema diverges from what the controller expects.
  #
  # Calls scripts/split-crds.py (same script xtask uses) to split
  # multi-doc → one file per metadata.name, into $TMPDIR.
  # diff -r: recursive, exits non-zero on any difference.
  crds-drift =
    let
      crdsYaml = config.packages.crds;
      py = pkgs.python3.withPackages (p: [ p.pyyaml ]);
    in
    pkgs.runCommand "rio-crds-drift"
      {
        nativeBuildInputs = [
          py
          pkgs.diffutils
        ];
      }
      ''
        mkdir -p $TMPDIR/split
        python3 ${../scripts/split-crds.py} ${crdsYaml} $TMPDIR/split
        diff -r $TMPDIR/split ${../infra/helm/crds} > $TMPDIR/diff || {
          echo "FAIL: crdgen output drifted from infra/helm/crds/" >&2
          echo "Run: cargo xtask regen crds" >&2
          cat $TMPDIR/diff >&2
          exit 1
        }
        touch $out
      '';

  # infra/eks/generated.auto.tfvars.json must match nix/pins.nix.
  # Same pattern as crds-drift: regenerate-then-diff. jq on both
  # sides so key-order and whitespace don't matter (the committed
  # file is pretty-printed, writeText output is compact).
  tfvars-fresh =
    pkgs.runCommand "rio-tfvars-fresh"
      {
        nativeBuildInputs = [
          pkgs.jq
          pkgs.diffutils
        ];
      }
      ''
        jq -S . ${config.packages.tfvars} > $TMPDIR/gen
        jq -S . ${../infra/eks/generated.auto.tfvars.json} > $TMPDIR/committed
        diff $TMPDIR/gen $TMPDIR/committed || {
          echo "FAIL: nix/pins.nix drifted from infra/eks/generated.auto.tfvars.json" >&2
          echo "Run: nix build .#tfvars && jq -S . result > infra/eks/generated.auto.tfvars.json" >&2
          exit 1
        }
        touch $out
      '';

  # ADR-021: seccomp profiles dual-sourced under
  # nix/nixos-node/seccomp/ (canonical, AMI bakes them via
  # hardening.nix tmpfiles) and infra/helm/rio-build/files/
  # (helm `.Files.Get` for SPO/k3s — cleanSource can't follow
  # out-of-tree symlinks). Fail on drift.
  seccomp-fresh =
    pkgs.runCommand "rio-seccomp-fresh"
      {
        nativeBuildInputs = [ pkgs.diffutils ];
      }
      ''
        diff ${./nixos-node/seccomp/rio-builder.json} \
             ${../infra/helm/rio-build/files/seccomp-rio-builder.json}
        diff ${./nixos-node/seccomp/rio-fetcher.json} \
             ${../infra/helm/rio-build/files/seccomp-rio-fetcher.json}
        touch $out
      '';

  # Onibus state-machine tests (DAG runner / merger / plan-doc validation).
  # Source: .claude/lib/test_scripts.py + .claude/lib/onibus/.
  #
  # Why this gates: test_tracey_domains_matches_spec catches TRACEY_DOMAINS
  # drift vs docs/src/ spec markers. 120bab69 (worker→builder+fetcher rename)
  # desynced the frozenset; onibus plan tracey-markers silently dropped
  # r[builder.*]/r[fetcher.*] for weeks. The test was red on local pytest,
  # green on .#ci — nobody saw it. Gates now.
  #
  # The whole suite gates (~107 tests), not just the drift detector —
  # test_scripts.py IS the onibus tooling test suite. A red test there
  # means the merger / followup pipeline / state models have a bug.
  #
  # DEV-SHELL DIVERGENCE: `nix develop -c pytest` shows 10 MORE failures
  # than `nix develop -c python3 -m pytest`. The bare `pytest` binary is
  # a nixpkgs bash-wrapper that prepends bare-python3 (no site-packages)
  # to PATH; subprocess tests hit `#!/usr/bin/env python3` in onibus and
  # get no pydantic. `python -m pytest` below bypasses the wrapper — PATH
  # stays clean, subprocesses find the withPackages env. This check's
  # result is authoritative; a local bare-pytest run is NOT.
  onibus-pytest = pkgs.stdenv.mkDerivation {
    pname = "rio-onibus-pytest";
    inherit version;
    src = pkgs.lib.fileset.toSource {
      root = unfilteredRoot;
      fileset = pkgs.lib.fileset.unions [
        ../.claude/lib
        ../.claude/bin
        # test_tracey_domains_matches_spec scans docs/src for r[domain.*]
        # prefixes — needs the spec files present.
        ../docs/src
        # _no_dag skipif at test_scripts.py:3415 reads this directly.
        # Absent → test_dag_deps_cli etc. skip instead of run.
        ../.claude/dag.jsonl
        # onibus/__init__.py reads this at import time.
        ../.claude/integration-branch
      ];
    };
    nativeBuildInputs = [
      (pkgs.python3.withPackages (ps: [
        ps.pytest
        ps.pydantic
      ]))
      # conftest.py:18 tmp_repo fixture + several tests subprocess git.
      pkgs.git
    ];
    dontConfigure = true;
    dontBuild = true;
    doCheck = true;
    checkPhase = ''
      # onibus shebang is `#!/usr/bin/env python3`. The nix sandbox
      # has /bin/sh but NOT /usr/bin/env — subprocess exec fails
      # with ENOENT (reported against the script path, not the
      # shebang, which makes diagnosis confusing). patchShebangs
      # rewrites to the absolute withPackages-env python3 path.
      # _copy_harness copies this patched file into tmp_repo, so
      # the subprocess tests get a working shebang too.
      patchShebangs .claude/bin

      # `python -m pytest`, NOT bare `pytest` — see DEV-SHELL DIVERGENCE
      # note above. The bash wrapper for `pytest` prepends bare python3
      # to PATH; this derivation's PATH is clean going in, but the -m
      # form is defensive against nixpkgs python-wrapping changes.
      #
      # -x: stop at first failure. Suite runs ~60s; -x means the CI
      # log shows the ONE test that broke, not a cascade.
      python -m pytest .claude/lib/test_scripts.py -x -v
    '';
    installPhase = "touch $out";
  };
}
# Linux-only checks (dockerTools, nixosSystem). On Darwin
# miscChecks degrades to the policy checks above.
// pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
  # Seed↔ECR-push layer-digest parity. The seed warms
  # containerd's content store; the warm only works if the
  # seed's layer digests match what push.rs puts in ECR
  # (containerd checks blobs by digest). This rebuilds the
  # same skopeo transcode push.rs would do and asserts every
  # resulting blob is present in the seed. Builds the
  # builder+fetcher images — but those are already in the
  # VM-test closure, so no extra cold-cache cost in .#ci.
  executor-seed-layer-parity = dockerImages.executorSeedLayerParity;

  # Eval-only: instantiate both AMI nixosSystems so a typo in
  # the seedImages wiring (or any nixos-node module change)
  # fails .#ci without building the multi-GB disk image.
  # drvPath forces full module eval; unsafeDiscardStringContext
  # prevents the drvPath's context from making this derivation
  # depend on actually BUILDING the AMI.
  node-ami-eval = pkgs.runCommand "rio-node-ami-eval" { } ''
    cat > $out <<'EOF'
    ${builtins.unsafeDiscardStringContext (nodeAmi "x86_64-linux").drvPath}
    ${builtins.unsafeDiscardStringContext (nodeAmi "aarch64-linux").drvPath}
    EOF
  '';
}
