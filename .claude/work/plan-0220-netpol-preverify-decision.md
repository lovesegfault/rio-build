# Plan 0220: NetPol pre-verify — DECISION GATE (does kube-router enforce?)

**This is not a normal implementation plan.** It is a ~30-minute interactive spike whose **outcome reshapes P0241's design**. The phase4c.md assumption (A2) is that stock k3s ships kube-router with NetworkPolicy enforcement ON — the `--disable-network-policy` k3s flag implies the default is enabled. But "implied by a flag" is not "verified on our VM fixture." If the curl goes through, P0241 balloons from scenario-only (~50 lines) to Calico-preload (~3 docker images + fixture param + ~100 lines).

**Dispatch FIRST, before filling Wave 1 slots.** The outcome gates P0241's plan-doc body. Running this in parallel with Wave 1 is fine (no file conflicts — it writes nothing to the repo), but P0241 MUST NOT dispatch until the followup row is written.

**No `.#ci` gate.** This plan touches zero tracked files. Its only output is a `Followup` row + a one-line phase4c.md annotation.

## Tasks

### T1 — `test(vm):` interactive NetPol enforcement check on k3s-full

Launch the k3s-full VM fixture interactively:

```bash
nix-build-remote --dev --no-nom -- -L .#checks.x86_64-linux.vm-lifecycle-core-k3s.driverInteractive
```

Inside the driver Python REPL:

```python
# Wait for k3s server + worker to be Ready
server.wait_until_succeeds("kubectl get nodes | grep -c Ready | grep -q 2", timeout=300)

# Apply a deny-all-egress NetworkPolicy to the default namespace
server.succeed("""kubectl apply -f - <<'EOF'
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: deny-all-egress
  namespace: default
spec:
  podSelector: {}
  policyTypes: [Egress]
EOF
""")

# Give kube-router a beat to sync (it watches NetPol CRs)
import time; time.sleep(5)

# Find a running pod in default ns (worker pod or any rio pod)
pod = server.succeed("kubectl get pods -n default -o jsonpath='{.items[0].metadata.name}'").strip()

# Attempt egress to a public IP. Expect: FAIL (exit != 0) if kube-router enforces.
# curl --max-time 5 so we don't hang on a dropped packet.
rc = server.execute(f"kubectl exec {pod} -- curl --max-time 5 -sS http://1.1.1.1")[0]
print(f"egress curl exit code: {rc}")

# Also try IMDS (the real P0241 assertion target)
rc_imds = server.execute(f"kubectl exec {pod} -- curl --max-time 5 -sS http://169.254.169.254")[0]
print(f"IMDS curl exit code: {rc_imds}")
```

**Outcome A (kube-router enforces):** both curls exit non-zero (connection refused or timeout). Write followup `{"netpol":"kube-router-ok"}`. P0241 proceeds with stock k3s.

**Outcome B (kube-router does NOT enforce):** at least one curl exits 0. Write followup `{"netpol":"needs-calico"}`. Coordinator invokes `/plan` to promote a Calico-preload plan BEFORE P0241 dispatches. P0241's plan doc body gets a "see P<calico>" entry-criteria addition.

### T2 — `docs:` record outcome

Write the outcome to state:

```bash
# From /root/src/rio-build/<worktree>
python3 .claude/lib/state.py followup --origin P0220 --severity feature \
  --description 'NetPol pre-verify: kube-router {ok|needs-calico}' \
  --payload '{"netpol":"kube-router-ok"}'  # or "needs-calico"
```

Annotate [`docs/src/phases/phase4c.md:41`](../../docs/src/phases/phase4c.md) with a one-line result comment (in-place edit, NOT a new deferral block):

```markdown
<!-- P0220 verified 2026-MM-DD: kube-router enforces on stock k3s (outcome A) -->
```

## Exit criteria

- Followup row written to `.claude/state/followups-pending.jsonl` with `origin:"P0220"` and `payload.netpol ∈ {"kube-router-ok","needs-calico"}`
- [`phase4c.md:41`](../../docs/src/phases/phase4c.md) annotated with outcome + date
- P0241's plan doc is **updated** if outcome is B (coordinator responsibility — add entry-criteria for Calico-preload plan)

## Tracey

No markers — spike has no code.

## Files

```json files
[
  {"path": "docs/src/phases/phase4c.md", "action": "MODIFY", "note": "T2: one-line outcome annotation at :41"}
]
```

```
docs/src/phases/
└── phase4c.md   # T2: outcome annotation (1 line)
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "DECISION GATE — dispatch FIRST. Outcome gates P0241 design. No file conflicts (interactive spike + 1-line doc annotation)."}
```

**Depends on:** none. Runs against the existing `vm-lifecycle-core-k3s` fixture — no 4b changes needed.
**Conflicts with:** none. phase4c.md is touched by P0244 (doc-sync sweep) at phase closeout; P0220 merges first by a wide margin.

**Hidden check at dispatch:** verify `vm-lifecycle-core-k3s.driverInteractive` still builds (`.driverInteractive` is ~10s mypy+pyflakes per dev-workflow memory — cheap smoke before the full VM boot).
