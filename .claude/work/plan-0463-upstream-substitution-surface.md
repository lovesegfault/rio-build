# Plan 0463: Upstream substitution — surface (CLI, gateway wopQueryMissing, helm NetPol)

[P0462](plan-0462-upstream-substitution-core.md) landed the core `Substituter` and gRPC hooks. This plan surfaces the feature: `rio-cli upstream` subcommand for admin CRUD, gateway `wopQueryMissing` wiring `substitutable_paths` into the Nix wire protocol's `willSubstitute` set, and helm NetworkPolicy allowlist egress so rio-store pods can reach upstream caches.

**Third of four.** [P0464](plan-0464-upstream-substitution-validation.md) adds VM validation.

## Entry criteria

- [P0462](plan-0462-upstream-substitution-core.md) merged — `Substituter` exists, StoreAdmin upstream RPCs implemented, `FindMissingPathsResponse.substitutable_paths` populated

## Tasks

### T1 — `feat(rio-cli):` upstream subcommand — list/add/remove

New module at [`rio-cli/src/upstream.rs`](../../rio-cli/src/upstream.rs) following the existing subcommand pattern (see [`gc.rs`](../../rio-cli/src/gc.rs), [`status.rs`](../../rio-cli/src/status.rs)):

```rust
#[derive(clap::Subcommand)]
pub enum UpstreamCmd {
    /// List configured upstreams for a tenant
    List { #[arg(long)] tenant: String },
    /// Add an upstream cache
    Add {
        #[arg(long)] tenant: String,
        #[arg(long)] url: String,
        #[arg(long, default_value = "50")] priority: i32,
        #[arg(long = "trusted-key")] trusted_keys: Vec<String>,
        #[arg(long, default_value = "keep")] sig_mode: String,
    },
    /// Remove an upstream cache
    Remove { #[arg(long)] tenant: String, #[arg(long)] url: String },
}
```

Wire into [`main.rs`](../../rio-cli/src/main.rs) alongside the existing subcommands. Calls the `StoreAdminService` `ListUpstreams`/`AddUpstream`/`RemoveUpstream` RPCs. Table-format output for `list` (url, priority, sig_mode, key count).

### T2 — `feat(rio-gateway):` wopQueryMissing — wire substitutable_paths → willSubstitute

At [`rio-gateway/src/handler/opcodes_read.rs:692`](../../rio-gateway/src/handler/opcodes_read.rs) (`handle_query_missing`), the response at `:763` currently writes an empty `willSubstitute` set:

```rust
wire::write_strings(w, wire::NO_STRINGS).await?; // willSubstitute: always empty
```

Change to consume the new proto field:

```rust
let resp = r.into_inner();
let missing_set: HashSet<String> = resp.missing_paths.iter().cloned().collect();
let substitutable_set: HashSet<String> = resp.substitutable_paths.into_iter().collect();
// ...
// Paths that are missing AND substitutable → willSubstitute (not willBuild)
let mut will_substitute = Vec::new();
for dp in derived {
    let sp = dp.store_path();
    if !missing_set.contains(sp.as_str()) { continue; }
    if substitutable_set.contains(sp.as_str()) {
        will_substitute.push(sp.as_str().to_string());
        continue;
    }
    // existing will_build / unknown branches
}
wire::write_strings(w, &will_build).await?;
wire::write_strings(w, &will_substitute).await?;  // was NO_STRINGS
wire::write_strings(w, &unknown).await?;
```

This makes `nix build` show "N paths will be substituted" instead of "N paths will be built" when the upstream has them — the user-visible payoff of the whole feature.

### T3 — `feat(helm):` networkpolicy.yaml — store-egress allowlist for upstream caches

At [`infra/helm/rio-build/templates/networkpolicy.yaml`](../../infra/helm/rio-build/templates/networkpolicy.yaml), add a `store-egress` NetworkPolicy in the `rio-store` namespace (P0454 moved store to its own ns). Allowlist egress to configured upstream-cache FQDNs:

```yaml
{{- if .Values.store.upstreamCaches }}
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: store-egress-upstream
  namespace: rio-store
spec:
  podSelector:
    matchLabels:
      app.kubernetes.io/name: rio-store
  policyTypes: [Egress]
  egress:
  # DNS resolution for upstream hostnames
  - to: [{namespaceSelector: {}, podSelector: {matchLabels: {k8s-app: kube-dns}}}]
    ports: [{protocol: UDP, port: 53}]
  # Allowlist upstream cache CIDRs (operator-configured)
  {{- range .Values.store.upstreamCaches }}
  - to: [{ipBlock: {cidr: {{ .cidr | quote }}}}]
    ports: [{protocol: TCP, port: 443}]
  {{- end }}
{{- end }}
```

At [`infra/helm/rio-build/values.yaml`](../../infra/helm/rio-build/values.yaml), add:

```yaml
store:
  # Upstream binary-cache egress allowlist. Each entry opens 443/TCP
  # to the given CIDR. Resolve cache.nixos.org etc. to their current
  # CIDRs at deploy time (or use a DNS-aware CNI like Cilium for FQDN).
  upstreamCaches: []
  # - name: cache.nixos.org
  #   cidr: 151.101.0.0/16  # Fastly
```

The allowlist is CIDR-based (standard NetworkPolicy limitation — no native FQDN support). Document that Cilium/Calico FQDN policies are the upgrade path if CIDR churn is operational pain.

## Exit criteria

- `/nixbuild .#ci` green
- `rio-cli upstream add --tenant t1 --url https://cache.nixos.org --trusted-key cache.nixos.org-1:... --sig-mode keep` succeeds; `rio-cli upstream list --tenant t1` shows the row
- Gateway `wopQueryMissing` integration test: mock store returning `substitutable_paths=[p1]`, verify wire bytes include `p1` in the second `write_strings` slot
- `helm template infra/helm/rio-build --set 'store.upstreamCaches[0].cidr=1.2.3.0/24' | yq 'select(.metadata.name=="store-egress-upstream")'` renders the policy with one egress rule to `1.2.3.0/24:443`
- With `upstreamCaches: []` the policy does NOT render (`{{- if }}` gate)

## Tracey

References existing markers:
- `r[store.substitute.upstream]` — T3 implements (NetPol egress enablement)
- `r[gw.opcode.query-missing]` — T2 extends (was implemented, now populates willSubstitute)

## Files

```json files
[
  {"path": "rio-cli/src/upstream.rs", "action": "NEW", "note": "T1: upstream subcommand"},
  {"path": "rio-cli/src/main.rs", "action": "MODIFY", "note": "T1: wire UpstreamCmd"},
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "T2: willSubstitute from substitutable_paths"},
  {"path": "infra/helm/rio-build/templates/networkpolicy.yaml", "action": "MODIFY", "note": "T3: store-egress-upstream policy"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T3: store.upstreamCaches allowlist"}
]
```

```
rio-cli/src/
├── upstream.rs               # T1: NEW
└── main.rs                   # T1: subcommand wire
rio-gateway/src/handler/
└── opcodes_read.rs           # T2: willSubstitute
infra/helm/rio-build/
├── templates/networkpolicy.yaml  # T3: egress rule
└── values.yaml               # T3: allowlist config
```

## Dependencies

```json deps
{"deps": [0462], "soft_deps": [454], "note": "needs Substituter + admin RPCs; NetPol layout assumes P0454 four-namespace split"}
```

**Depends on:** [P0462](plan-0462-upstream-substitution-core.md) — `Substituter`, StoreAdmin upstream handlers, populated `substitutable_paths`.

**Soft dep:** [P0454](plan-0454-four-namespace-helm-netpol-karpenter.md) (DONE) — the NetworkPolicy targets the `rio-store` namespace introduced there.

**Conflicts with:** [`opcodes_read.rs`](../../rio-gateway/src/handler/opcodes_read.rs) and [`networkpolicy.yaml`](../../infra/helm/rio-build/templates/networkpolicy.yaml) are moderately warm — check `onibus collisions check` before implementing. `rio-cli/src/upstream.rs` is new.
