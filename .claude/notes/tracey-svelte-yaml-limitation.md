# Tracey parser limitations — Svelte and YAML

**Checked at P0295 dispatch (tracey 1.3.0 / 2446b4f7):**

Tracey does NOT support `.svelte` or `.yaml` file extensions. Attempting to
add them to `config.styx` `include` triggers `IncludeUnparseableFile` errors
at `tracey query validate`.

## Affected markers

### T67 — dash.* markers in .svelte files

| File | Marker | Status |
|---|---|---|
| `rio-dashboard/src/pages/Cluster.svelte:2` | `// r[impl dash.journey.build-to-logs]` | INVISIBLE to tracey |
| `rio-dashboard/src/pages/Builds.svelte:2` | `// r[impl dash.journey.build-to-logs]` | INVISIBLE to tracey |

The `r[verify]` sites in `.ts` test files ARE scanned (`.ts` is supported).
`tracey query rule dash.journey.build-to-logs` shows verify-only coverage.
The markers in `.svelte` are documentary — humans grep them, tracey doesn't.

### T84 — dash.* markers in .yaml files

| File | Marker | Status |
|---|---|---|
| `infra/helm/rio-build/templates/dashboard-gateway.yaml:3` | `# r[impl dash.envoy.grpc-web-translate+2]` | INVISIBLE to tracey |
| `infra/helm/rio-build/templates/dashboard-gateway.yaml:4` | `# r[impl dash.auth.method-gate]` | INVISIBLE to tracey |

The YAML comment at `:5-6` already acknowledges "markers are documentary —
tracey does not scan yaml". The `r[verify]` sites at `nix/tests/default.nix`
ARE scanned. `tracey query uncovered` lists these rules as missing impl
coverage — technically true, but the implementation IS the YAML.

## Options considered (T67/T84)

**(a) Add glob:** REJECTED — tracey errors on unparseable extensions.

**(b) Suppress from uncovered:** tracey lacks a `#[skip-uncovered]` pragma.

**(c) Rust anchor file:** CREATE a `const _: () = ();` with `// r[impl ...]`
above it in a `.rs` file that conceptually "owns" the YAML/Svelte behavior.
Hacky but works. NOT applied here — the verify coverage at default.nix is
sufficient proof-of-implementation for infrastructure markers.

## Current disposition

Left as documentary-only. The verify annotations (at default.nix subtests)
provide sufficient traceability. If `tracey query uncovered` noise becomes
operational friction, implement (c) with a `rio-test-support/src/tracey_anchors.rs`
listing all infra-only impl markers.
