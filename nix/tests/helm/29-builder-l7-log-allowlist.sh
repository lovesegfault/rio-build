# Builder/fetcher → store edge posture (bug_290): build-log READS are
# unreachable from untrusted build pods. The enforcing control is the
# store's per-method credential-class layer — TailLog requires
# verified tenant claims (rio-store/src/authz.rs,
# store.log.method-credential), and worker pods hold assignment
# tokens, not tenant tokens.
#
# The network leg is L4 endpoint scoping ONLY, deliberately: an L7
# HTTP allow-list on this edge redirects the worker's gRPC data plane
# through Cilium's embedded-envoy proxy, which breaks uploads in the
# supported k3s/Cilium configuration (vm-fetcher-split-k3s pinned the
# regression: FOD upload starves, dispatch never progresses).
#
# ACCEPTANCE OF RISK — the L4-only builder->store edge (bug_290
# network leg, relocated from the retired build-log invariant map):
# the edges ship as L4 endpoint scoping only (toEndpoints pinned to
# the rio-store namespace/name pair, toPorts 9002, no rules block on
# either policy — this fragment asserts both directions: the edge
# exists AND no L7 block reappears). The compensating control is the
# per-method credential-class layer: TailLog's class is
# verified-tenant-JWT (the CredentialClass table in
# rio-store/src/authz.rs, pinned table-vs-bound-set both ways by the
# descriptor-walk test), and a worker pod's credential — the
# assignment token — never produces TenantClaims, so it is
# STRUCTURALLY incapable of satisfying the gate; that exact arm is
# pinned by taillog_assignment_token_rejected alongside the
# tokenless, foreign-tenant, wildcard and tenant-less reds. A removed
# network-layer defense means the application-layer control must be
# PINNED BY TEST, not assumed. L7 RE-INTRODUCTION PATH: if the
# chart's supported Cilium configuration gains a working L7 proxy for
# this edge, flip this fragment's no-L7 assertion back to the
# allow-list form and restore the rules block — the assertion failing
# loudly is the reminder.
#
# (documentary — .sh is not tracey-scanned; the normative rule is
# store.log.method-credential. This fragment is the merge-gate render
# proof of the chart half: the builder/fetcher → store:9002 edge
# exists, is endpoint-scoped, and carries NO L7 rules — the silent
# reintroduction of an L7 block (which would re-break the data plane)
# and the loss of the edge entirely (which would break builds at L4)
# both fail here.)

out=$TMPDIR/builder-l7.yaml
helm template rio . --set global.image.tag=test >"$out"

for policy in builder-egress fetcher-egress; do
  doc=$(yq -N "select(.kind==\"CiliumClusterwideNetworkPolicy\" and .metadata.name==\"$policy\")" "$out")
  test -n "$doc" || {
    echo "FAIL: $policy did not render — assertions vacuous" >&2
    exit 1
  }

  # The store:9002 egress entry must exist (endpoint-scoped to rio-store).
  edge=$(echo "$doc" | yq -N '.spec.egress[] | select(.toPorts[0].ports[0].port=="9002" and (.toEndpoints[0].matchLabels["k8s:app.kubernetes.io/name"]=="rio-store"))')
  if [ -z "$edge" ] || [ "$edge" = "null" ]; then
    echo "FAIL: $policy lost its store:9002 edge — builds cannot upload" >&2
    exit 1
  fi

  # The edge must NOT carry an L7 rules block (the embedded-envoy
  # data-plane breakage class; see the header).
  l7=$(echo "$edge" | yq -N '.toPorts[0].rules')
  if [ -n "$l7" ] && [ "$l7" != "null" ]; then
    echo "FAIL: $policy store:9002 edge carries L7 rules — this broke the" >&2
    echo "      gRPC data plane under embedded-envoy (vm-fetcher-split-k3s);" >&2
    echo "      TailLog denial belongs to the authz layer, not netpol" >&2
    exit 1
  fi
done

echo "PASS: builder/fetcher store:9002 edges are L4 endpoint-scoped, no L7 rules"
