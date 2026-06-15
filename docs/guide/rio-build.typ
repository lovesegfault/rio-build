#import "/lib/rio.typ": *
#show: rio.with(domains: none)

`rio build` is the native-protocol build client (ADR-024). It evaluates flake
attributes locally --- in Nix's own libexpr, driven nix-eval-jobs-style ---
and submits the resulting derivation graph to the cluster over gRPC as digest
negotiation: the cluster reports which #gls("blake3")-keyed objects it
already holds, and the client uploads only the misses. It replaces
`nix build --store ssh-ng://rio` for interactive and CI submission:
post-eval time-to-first-build drops from 57~s (cold ssh-ng, measured) to
about one second. The `ssh-ng://` gateway path stays --- use it
for stock Nix clients that cannot install `rio`, for deployments where only
the SSH gateway is reachable from outside the cluster, or when you need
outputs realized into a real local `/nix/store` (`rio build --fetch`
materializes into the client cache, not `/nix/store`).

See #cross-link("/architecture-build-client.typ")[Build Client Architecture]
for the process model and the submission pipeline, and the
#cross-link("/spec/components/build-client.typ")[build-client component spec]
for the normative requirements. ADR-024 carries the full design rationale
and measurements.

= Getting the Client

`rio build` is two binaries that ship as a pair:

- the *coordinator* (`rio`) --- pure Rust, owns the cluster connection and
  the client CAS;
- the *eval parent* (`rio-eval`) --- embeds the flake-pinned Nix libexpr plus
  the rio eval store, and forks the actual evaluation workers.

```bash
# The usable pair: bin/rio with RIO_EVAL_PARENT pre-pointed at rio-eval.
nix build .#rio
./result/bin/rio build .#myPackage

# The eval parent alone (only needed if you wire eval_parent yourself).
nix build .#rio-eval
```

The `.#rio` wrapper sets `RIO_EVAL_PARENT` as a default; an explicit
`eval_parent` config key or `--eval-parent` flag still overrides it.

= Quickstart

== Cluster endpoints

The client talks to two gRPC doors: the scheduler (build submission and event
streams) and the store's castore door (presence negotiation, uploads, output
fetch). Both are `host:port` with no scheme. Configure them in
`./build.toml` or `/etc/rio/build.toml`, or via `RIO_*` environment
variables, or as flags --- precedence is CLI flags over environment over
config file over defaults
(#cross-link("/ref/configuration.typ")[Configuration Reference]).

```toml
# build.toml
scheduler_addr = "rio.example.com:9001"
store_addr = "rio.example.com:9002"
tenant_token_path = "/run/secrets/rio-tenant.jwt"
```

== Tenant token

Every RPC carries a tenant JWT as `x-rio-tenant-token`; the scheduler and
store verify its signature and expiry, and presence answers, drv blobs and
fetches are scoped to the tenant in its `sub` claim. The token is minted by
your cluster operator with the cluster's JWT signing key (the same EdDSA key
the gateway's JWT mode uses --- see
#cross-link("/guide/setup.typ")[Authentication Setup]); `sub` is your tenant
UUID. Point `tenant_token_path` (or `RIO_TENANT_TOKEN_PATH`) at a file
containing it.

#info[
  Leaving `tenant_token_path` unset sends no token. That only works against
  single-tenant development clusters; a production door rejects anonymous
  callers.
]

== Building

```bash
# Build a flake attribute against the cluster.
rio build .#hello

# Several attributes from the same flake, keep going past failures,
# and materialize the outputs locally.
rio build .#pkgA .#pkgB --keep-going --fetch

# Symlink the first output (implies --fetch).
rio build .#hello --out-link ./result-hello
```

All installables in one invocation must share a single flake reference ---
the eval parent locks one flake and fetches its inputs once before forking
workers. A bare reference (no `#attr`) evaluates the flake's default
package (`packages.<system>.default`).

An installable may also name an attribute set instead of a single
derivation:

```bash
# Every check derivation, spread across the eval workers.
rio build .#checks            # descends into checks.<system> first
rio build .#checks.x86_64-linux
```

Expansion follows nix-eval-jobs conventions: every immediate derivation
child becomes its own build root, named by its full attribute path
(`checks.x86_64-linux.clippy-rio-nix`); nested sets are entered only when
they set `recurseForDerivations = true`; anything else is skipped with a
warning. An attribute set that contains no derivations at all fails
evaluation. For `.#checks` the entry matching the evaluating system is
selected first --- a missing system entry surfaces as the zero-derivations
error naming it.

== Non-flake builds

`-f`/`--file` evaluates a plain Nix file (or a directory containing
`default.nix`) the way `nix-build` does: installables are attribute paths
into the file's top-level value, and with no installables the top-level
value itself is built (an attribute set expands into its derivation
children, like `nix-build` without `-A`). `<nixpkgs>`-style lookup paths
come from `NIX_PATH` or `-I`, and `--arg`/`--argstr` feed the file's
top-level function.

```bash
rio build -f default.nix
rio build -f release.nix pkgA pkgB --argstr version 1.2 -I nixpkgs=./nixpkgs
```

While the build runs, the client prints one status line per derivation event
(`queued`, `building`, `built`, …) and finishes with the output paths.
Pressing Ctrl-C cancels: the client cancels every build this invocation
submitted, prints the cancelled build ids, and exits non-zero. A second
Ctrl-C exits immediately without waiting for the cancel acknowledgements,
printing each remaining build id with its reattach hint. Pass `--detach`
when you want the old behaviour --- Ctrl-C exits the client, the builds keep
running cluster-side, and each in-flight build id is printed with a reattach
hint.

```bash
# Submit and leave running on Ctrl-C instead of cancelling.
rio build .#hello --detach

# Reattach to a running (or completed) build's event stream — from any
# machine that holds the tenant credential. Ctrl-C here only stops
# watching; it never cancels a build you did not submit.
rio build --attach 01HV5...

# Stop a build from anywhere.
rio build --cancel 01HV5...
```

= Command Reference

`rio build [INSTALLABLE]... [flags]`

#figure(
  caption: [`rio build` flags.],
  table(
    columns: (auto, 1fr),
    table.header([Flag], [Meaning]),
    [`INSTALLABLE...`],
    [Flake attributes to evaluate and build (`ref#attr`; bare `ref` = default
      attribute). All must share one flake reference. An attribute set
      (`.#checks`) expands into one build root per derivation child.],

    [`--attach BUILD_ID`],
    [Reattach to a build's event stream via `WatchBuild` and render it to
      completion. Conflicts with installables and `--cancel`; works without
      an eval parent configured.],

    [`--cancel BUILD_ID`],
    [Cancel a running build. Prints `cancelled BUILD_ID` on success; exits
      non-zero if the build is unknown or already terminal.],

    [`--fetch`],
    [Materialize completed outputs through the store read path into the
      client CAS (`<cas_root>/fetched/<basename>`), verifying the streamed
      NAR's SHA-256 against the server's claimed `nar_hash` before anything
      appears on disk.],

    [`--out-link PATH`],
    [Symlink the first fetched output at `PATH` (further outputs get
      `PATH-2`, `PATH-3`, …). Implies `--fetch`. The link target is the CAS
      materialization --- the client has no `/nix/store` to link into.],

    [`-f PATH`, `--file PATH`],
    [Evaluate a plain Nix file (or a directory containing `default.nix`)
      instead of a flake, `nix-build` style: installables are attribute
      paths into the file's top-level value, and with no installables the
      top-level value itself is the build root.],

    [`--arg NAME EXPR`],
    [Pass the Nix expression `EXPR` as argument `NAME` to the file's
      top-level function (file mode only; repeatable).],

    [`--argstr NAME VALUE`],
    [Pass the string `VALUE` as argument `NAME` to the file's top-level
      function (file mode only; repeatable).],

    [`-I PATH`, `--include PATH`],
    [Add an entry to the angle-bracket lookup path (`<nixpkgs>`), taking
      precedence over `NIX_PATH` (file mode only; repeatable).],

    [`--keep-going`],
    [Continue building independent derivations after a failure (the
      scheduler keeps dispatching unaffected subgraphs).],

    [`--log-lines N`],
    [When the build fails on a derivation that already failed in an earlier
      build, replay the last `N` lines (default 20) of the original
      failure's log on stderr.],

    [`-L`, `--print-build-logs`],
    [Replay the original failure's full build log instead of a tail.],

    [`--detach`],
    [On Ctrl-C (or SIGTERM), exit and leave the submitted builds running
      cluster-side instead of cancelling them; each in-flight build id is
      printed with its `--attach` reattach hint.],

    [`--local-ifd`],
    [Build import-from-derivation locally instead of remotely. Flag-gated
      fallback from ADR-024; *not wired yet* --- an IFD under this flag
      fails with an explicit message rather than silently going remote.],

    [`--scheduler-addr`, `--store-addr`, `--tenant-token-path`, `--cas-root`,
      `--eval-parent`],
    [Config overlay flags --- highest-precedence layer over `RIO_*`
      environment variables and the TOML file.],
  ),
)

Exit status is non-zero if any attribute failed to evaluate, any build
failed or was cancelled, or the run was interrupted (the default Ctrl-C
cancellation). A `--detach` run interrupted by Ctrl-C exits zero after
printing the reattach hints.

`RUST_LOG` sets the coordinator's log level and is mirrored into the eval
parent as nix's own verbosity --- `RUST_LOG=debug` also shows nix fetch and
eval detail.

= Inspecting build logs

`nix log` cannot work over ssh-ng (the daemon protocol has no log-read
opcode for remote stores); `rio log` is the native replacement. It streams a
derivation's stored build log straight from the cluster --- raw lines on
stdout, nothing else --- for any derivation that was built under one of your
tenant's builds, success or failure:

```bash
# The most recent execution of this derivation among your builds.
rio log /nix/store/…-openssl-3.5.1.drv

# Pin a build or a specific execution, or take just a tail.
rio log /nix/store/…-openssl-3.5.1.drv --build 01HV5… --log-lines 200
rio log /nix/store/…-openssl-3.5.1.drv --exec 0196ab…
```

The argument must be a `.drv` store path (resolving an installable to its
derivation needs an evaluation; pass the path printed in the build output or
failure message). Like `--attach`/`--cancel`, `rio log` works without an
eval parent configured. The exit status is non-zero when the derivation has
no log under your builds --- never built by your tenant, the execution
produced no output, or the log has expired.

When a build fails because one of its derivations *already failed in an
earlier build* (the scheduler fail-fasts on the still-poisoned node), `rio
build` does this for you: it names the original culprit derivation and
replays the tail of its original log (`--log-lines`, `-L`), or the persisted
failure reason when that execution produced no output.

#figure(
  caption: [`rio log` flags.],
  table(
    columns: (auto, 1fr),
    table.header([Flag], [Meaning]),
    [`DRV_PATH`], [Full `/nix/store/…-*.drv` path whose log to print.],

    [`--build BUILD_ID`],
    [Pin the build the log should belong to. Default: the most recent
      execution of the derivation among your own builds.],

    [`--exec EXEC_ID`],
    [Pin a specific execution id (e.g. from a failure message).],

    [`--log-lines N`], [Print only the last `N` lines. Default: the full log.],
  ),
)

= Configuration

The component name for config layering is `build`: TOML at
`/etc/rio/build.toml` / `./build.toml`, environment prefix `RIO_`. The full
generated key table lives in the
#cross-link("/ref/configuration.typ")[Configuration Reference]; the keys you
will actually touch:

- `scheduler_addr`, `store_addr` --- the two gRPC doors (required).
- `tenant_token_path` --- file containing the tenant JWT.
- `cas_root` --- client CAS root; defaults to `$XDG_CACHE_HOME/rio/evalstore`
  (falling back to `~/.cache/rio/evalstore`). Holds the pack store,
  fingerprint index, cluster-ack table and fetched outputs. Safe to delete;
  the next run re-ingests and re-negotiates.
- `eval_parent` --- path to the `rio-eval` binary. The `nix build .#rio`
  wrapper defaults this; only set it when running the coordinator binary
  directly.
- `ack_ttl_secs` --- how long a "the cluster has this digest" record is
  trusted (default 6~h). Must stay at or below the cluster's minimum
  unpinned-blob lifetime, otherwise every cluster GC turns into a stale-ack
  recovery cycle.
- `page_max_nodes` --- nodes per `SubmitBuild` page (default 50,000);
  submissions above it paginate automatically.

= What Happens Under the Hood

A single `rio build .#attr` runs evaluation and submission overlapped: the
coordinator spawns `rio-eval`, which locks the flake and fetches its inputs
once, then forks evaluation workers. Workers stream back derivation digests
plus the canonical derivation bytes; the coordinator folds them by digest,
asks the cluster which it already has, uploads only the misses, and submits
each root as a digest-only skeleton as soon as everything it references is
acked, then renders the `BuildEvent` stream. Derivation bytes never touch
the client disk; source trees are re-read from your working copy at upload
time and verified against what evaluation reported.

The full picture, with figures, is in
#cross-link("/architecture-build-client.typ")[Build Client Architecture];
the measurements behind the design are in ADR-024.

= Troubleshooting

*"config `eval_parent` is not set".* You are running the bare coordinator
binary. Use the `nix build .#rio` wrapper (which defaults `RIO_EVAL_PARENT`),
or set `eval_parent` / `--eval-parent` to a `rio-eval` binary. `--attach` and
`--cancel` work without it.

*`Unauthenticated` / `PermissionDenied` from the scheduler or store.* The
tenant JWT is missing, expired, or signed with a key the cluster does not
trust. Production doors reject anonymous callers, so `tenant_token_path` must
point at a current token whose `sub` is your tenant UUID. Ask the cluster
operator for a fresh token; nothing is cached --- the next invocation picks
the new file up.

*"all installables must share one flake ref".* One invocation locks one
flake. Split the command per flake.

*"origin tree … changed since eval: NAR sha256 … != reported".* A source
tree in your working copy was modified between evaluation and upload. The
client refuses to upload content the submitted skeleton never referenced ---
re-run the build on the settled tree. (The bounded re-ingest escape hatch
from ADR-024 is not implemented yet; mutation is currently a hard error.)

*"submission rejected twice on missing drv digests … giving up".* The
stale-ack recovery cycle ran once (evict acks, re-probe, re-upload, resubmit)
and the scheduler still reported missing digests. Per ADR-024 a second reject
is a hard error: either the cluster is collecting garbage faster than
`ack_ttl_secs` models (lower it, or have the operator check the store's
unpinned-blob lifetime) or the upload path is broken --- check the store door
logs before retrying.

*"drv digest … missing but its body is no longer retained … rerun the
build".* Stale-ack recovery needed a derivation body that was already dropped
after an earlier accepted submission in the same run. Re-running the build
recomputes it (derivations are memory-only client-side).

*"IFD stall: building remotely".* Not an error. Evaluation hit
import-from-derivation; the needed derivation is submitted as an immediate
mini-build, its output fetched back into the client CAS, and the worker
resumes. Deep import chains serialize on this --- expect a pause per link.
`--local-ifd` exists as the planned escape hatch but is not wired yet.

*"narHash mismatch fetching …: refusing to materialize".* The streamed NAR
did not hash to what the store claimed; nothing was written. Retry once; a
persistent mismatch points at store-side corruption and should go to the
operator.

*"detached: ‹attr› continues as build ‹id›".* That is Ctrl-C under
`--detach` behaving as designed. `rio build --attach ‹id›` resumes the
stream, `--cancel ‹id›` stops the build. Without `--detach`, Ctrl-C prints
"interrupted: cancelled ‹attr› (build ‹id›)" instead --- the build was
cancelled cluster-side; only a *second* Ctrl-C leaves builds possibly
running and prints the same reattach hint.

= Current Limitations

Implementation gaps in the current client, not design decisions.

- *`toFile` and single-file source roots.* Only origin-backed directory
  trees upload today. Input sources produced by `builtins.toFile` (streamed
  text) or rooted at a single file or symlink are skipped at skeleton
  assembly and rejected by the upload planner; a build that genuinely needs
  one fails at the cluster's submit-time verification rather than silently.
  Directory trees --- flake inputs and local working trees --- cover the
  dominant case.
- *IFD output references.* Outputs fetched back for import-from-derivation
  are recorded with empty reference sets; evaluation logic that depends on
  the references of an imported output does not see them.
- *`--fetch` buffers whole NARs.* Output materialization holds the full NAR
  in memory before verifying and restoring it. Fine for typical outputs;
  very large outputs are expensive until streaming restore lands.
- *Mutated-origin escape hatch.* ADR-024's bounded re-ingest (re-negotiate
  the delta at most twice, then snapshot) is not implemented; a mutated
  origin is a hard error for that root.
- *`--local-ifd` is not wired.* The flag is parsed and plumbed, but the
  local fallback build path does not exist yet; IFD always builds remotely.
- *One flake per invocation*, and fetched outputs land in the client CAS,
  not in `/nix/store`.
