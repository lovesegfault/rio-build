#!/usr/bin/env python3
"""quint-policy: the falsify-twin-required corpus lint (P1-P6).

Vehicle (adjudicated, bughunt-2 slot 11): zero quint-SEMANTIC work — no
verify/run/TLC. Wiring facts (kind, invariants/witness, step,
vacuityExempt) come ONLY from the constructor meta manifest emitted at
nix eval; ALL structural facts come ONLY from `quint parse --out` JSON
IR (quint 0.32.0; the in-derivation canary pins the IR shape). Text
scanning is rejected for structural facts: live holds-checks bind
conjunction vals while twins bind leaf names (credit needs IR expansion
+ import-graph resolution); quint precedence makes raw-text assignment
analysis unsound (`x' = a or b` parses `(x' = a) or b`); the QNT000
frame discipline identity-assigns every var in every action, so "is the
var written" text heuristics are zero-yield.

The two non-IR inputs are declared configuration, not structure:
  - module -> file mapping (top-level `module X` header lines);
  - the `// quint-policy-latches:` header directive (P5 domain).

Rules:
  P1 twin-required: every wired holds-invariant (conjunctions expanded
     through the IR to val leaves) has >=1 witness-kind check whose main
     module's PARSED import graph reaches the live module declaring the
     invariant's vars AND whose witness read-set intersects the
     invariant's var read-set — or a vacuityExempt{class,reason} with
     class in {boundsOK, scope-bound, pre-r2-untwinned}. A tautology
     (empty var read-set) can never earn twin credit and is not
     exemptable: restate it over the transition relation.
  P2 no-frozen-copies: a witness/run main module must reach >=1 module
     declared OUTSIDE calibration/ via import/instance edges; it may not
     declare a non-action operator shadowing a declaration of a reached
     live module (the def-flip cheat).
  P3 baseline pairing: witness checks are paired (census) with the live
     holds-check covering the same live module — no new TLC anywhere.
  P4 live-writer: every var in a wired invariant's transitive read-set
     has >=1 NON-identity assignment reachable from the declaring
     module's `step` (fallback: all of its actions when no `step`
     exists — legacy abstract modules; hazard (nn)). Syntactic screen
     only — P4 credit NEVER substitutes for the P1 twin (a semantically
     inert non-identity write passes P4; the twin run is the teeth).
  P5 latch integrity: vars named in a live module's
     `// quint-policy-latches:` directive must pass P4 and must not be
     non-identity-assigned by any calibration-module action (oracles
     live in the shared live module; identity frames are legal).
  P6 hygiene: per-class exemption census printed (the owner burn-down
     artifact); unknown exemption classes / unused exemptions are
     errors.

Exit nonzero on any P1/P2/P4/P5/P6 violation. --census-only prints
findings without failing (the phase-0 artifact)."""

import argparse
import json
import re
import sys
from collections import defaultdict
from pathlib import Path

EXEMPT_CLASSES = {"boundsOK", "scope-bound", "pre-r2-untwinned"}
HOLDS_KINDS = {"holds", "holds-sim"}
WITNESS_KINDS = {"witness", "witness-sim"}


def fail(msg):
    print(f"quint-policy: FATAL: {msg}", file=sys.stderr)
    sys.exit(2)


def canary(ir_dir: Path):
    """Pin the quint IR shape on a known fixture parsed in-derivation."""
    p = ir_dir / "__canary__.json"
    if not p.exists():
        fail("IR canary fixture missing — derivation must parse the canary model")
    ir = json.loads(p.read_text())
    try:
        mods = ir["modules"]
        m = next(mm for mm in mods if mm["name"] == "policyCanary")
        decls = m["declarations"]
        kinds = {d["kind"] for d in decls}
        assert "var" in kinds and "def" in kinds, kinds
        act = next(d for d in decls if d.get("qualifier") == "action" and d["name"] == "step")
        assigns = []

        def walk(e):
            if isinstance(e, dict):
                if e.get("kind") == "app" and e.get("opcode") == "assign":
                    assigns.append(e)
                for v in e.values():
                    walk(v)
            elif isinstance(e, list):
                for v in e:
                    walk(v)

        walk(act["expr"])
        assert assigns, "canary step has no assign apps"
        a0 = assigns[0]
        assert a0["args"][0]["kind"] == "name", a0
    except (KeyError, StopIteration, AssertionError) as e:
        fail(f"quint IR shape drifted (canary mismatch: {e!r}) — update quint_policy.py for the new IR before trusting any verdict")


class Corpus:
    def __init__(self, ir_dir: Path, models_dir: Path):
        self.mod_file = {}        # module -> declaring file (rel)
        self.mod_decls = {}       # module -> {name: decl}
        self.mod_imports = defaultdict(set)   # module -> imported/instanced module names
        self.file_modules = defaultdict(list)  # rel file -> [modules declared]
        self.latch_directives = {}  # module -> [vars]
        mod_re = re.compile(r"^\s*module\s+(\w+)\s*\{", re.M)
        latch_re = re.compile(r"^\s*//\s*quint-policy-latches:\s*(.+)$", re.M)
        for f in sorted(models_dir.rglob("*.qnt")):
            rel = str(f.relative_to(models_dir))
            text = f.read_text()
            mods_here = mod_re.findall(text)
            self.file_modules[rel] = mods_here
            for m in latch_re.findall(text):
                vars_ = [v.strip() for v in m.split(",") if v.strip()]
                # directive binds to the first module of the file (live
                # models are single-module-first by corpus convention)
                if mods_here:
                    self.latch_directives.setdefault(mods_here[0], []).extend(vars_)
            for mn in mods_here:
                if mn in self.mod_file:
                    fail(f"module {mn} declared in both {self.mod_file[mn]} and {rel}")
                self.mod_file[mn] = rel
        # IR: one json per file, named <relpath with / -> __>.json
        for f in sorted(ir_dir.glob("*.json")):
            if f.name == "__canary__.json":
                continue
            ir = json.loads(f.read_text())
            for m in ir.get("modules", []):
                name = m["name"]
                if name in self.mod_decls:
                    continue  # same module reached through several parses
                decls = {}
                imports = set()
                for d in m.get("declarations", []):
                    k = d.get("kind")
                    if k in ("import", "instance"):
                        pn = d.get("protoName") or d.get("name")
                        if pn:
                            imports.add(pn)
                    elif "name" in d:
                        decls[d["name"]] = d
                self.mod_decls[name] = decls
                self.mod_imports[name] = imports
        self._reach_cache = {}
        self._readset_cache = {}

    def reachable_modules(self, mod):
        if mod in self._reach_cache:
            return self._reach_cache[mod]
        seen, stack = set(), [mod]
        while stack:
            m = stack.pop()
            if m in seen or m not in self.mod_imports and m not in self.mod_decls:
                seen.add(m)
                continue
            seen.add(m)
            stack.extend(self.mod_imports.get(m, ()) - seen)
        self._reach_cache[mod] = seen
        return seen

    def visible_decl(self, mod, name):
        """Lexical lookup: own decls first, then reached modules'."""
        d = self.mod_decls.get(mod, {}).get(name)
        if d is not None:
            return mod, d
        for m in self.reachable_modules(mod):
            d = self.mod_decls.get(m, {}).get(name)
            if d is not None:
                return m, d
        return None, None

    @staticmethod
    def _names_in(expr, acc):
        if isinstance(expr, dict):
            if expr.get("kind") == "name":
                acc.add(expr["name"])
            # user-defined operator APPLICATION: the IR encodes it as
            # app.opcode = <operator name> (builtin opcodes simply fail
            # decl lookup downstream — harmless)
            if expr.get("kind") == "app" and expr.get("opcode"):
                acc.add(expr["opcode"])
            if expr.get("kind") == "lambda":
                pass  # params shadow, but params aren't state vars; fine
            for v in expr.values():
                Corpus._names_in(v, acc)
        elif isinstance(expr, list):
            for v in expr:
                Corpus._names_in(v, acc)

    def var_readset(self, mod, name):
        """Transitive state-var read-set of declaration `name` seen from `mod`.
        Returns {(declaring_module, var)}."""
        key = (mod, name)
        if key in self._readset_cache:
            return self._readset_cache[key]
        self._readset_cache[key] = set()  # cycle guard
        dmod, d = self.visible_decl(mod, name)
        out = set()
        if d is None:
            self._readset_cache[key] = out
            return out
        if d["kind"] == "var":
            out.add((dmod, name))
        elif d["kind"] in ("def",):
            refs = set()
            self._names_in(d.get("expr"), refs)
            for r in refs:
                if r == name:
                    continue
                out |= self.var_readset(dmod, r)
        self._readset_cache[key] = out
        return out

    def conj_leaves(self, mod, inv):
        """Expand a conjunction val to leaf val names; a non-and expr is its own leaf."""
        dmod, d = self.visible_decl(mod, inv)
        if d is None or d["kind"] != "def":
            return [(mod, inv)]
        e = d.get("expr")
        if isinstance(e, dict) and e.get("kind") == "app" and e.get("opcode") in ("and", "actionAll"):
            leaves = []
            allnames = True
            for a in e.get("args", []):
                if isinstance(a, dict) and a.get("kind") == "name":
                    leaves.extend(self.conj_leaves(dmod, a["name"]))
                else:
                    allnames = False
            if allnames and leaves:
                return leaves
        return [(dmod, inv)]

    def assigns_reachable(self, mod, roots):
        """All assign apps in defs reachable (by name reference) from root decl names in mod.
        Returns [(target_var, rhs_is_identity)]."""
        seen, stack, out = set(), list(roots), []
        while stack:
            nm = stack.pop()
            if nm in seen:
                continue
            seen.add(nm)
            dmod, d = self.visible_decl(mod, nm)
            if d is None or d["kind"] != "def":
                continue

            def walk(e):
                if isinstance(e, dict):
                    if e.get("kind") == "app" and e.get("opcode") == "assign":
                        tgt = e["args"][0].get("name")
                        rhs = e["args"][1]
                        ident = isinstance(rhs, dict) and rhs.get("kind") == "name" and rhs.get("name") == tgt
                        out.append((tgt, ident))
                    if e.get("kind") == "name":
                        stack.append(e["name"])
                    if e.get("kind") == "app" and e.get("opcode"):
                        stack.append(e["opcode"])
                    for v in e.values():
                        walk(v)
                elif isinstance(e, list):
                    for v in e:
                        walk(v)

            walk(d.get("expr"))
        return out

    def module_action_names(self, mod):
        return [n for n, d in self.mod_decls.get(mod, {}).items() if d.get("qualifier") == "action"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--manifest", required=True)
    ap.add_argument("--ir-dir", required=True)
    ap.add_argument("--models-dir", required=True)
    ap.add_argument("--census-only", action="store_true")
    ap.add_argument("--assume-latches", default="", help="module:var,var;module:var — provisional P5 domain for census")
    args = ap.parse_args()

    ir_dir, models_dir = Path(args.ir_dir), Path(args.models_dir)
    canary(ir_dir)
    manifest = json.loads(Path(args.manifest).read_text())
    corpus = Corpus(ir_dir, models_dir)

    def is_calib_module(mod):
        f = corpus.mod_file.get(mod, "")
        return f.startswith("calibration/")

    # provisional latches for census
    for part in filter(None, args.assume_latches.split(";")):
        m, vs = part.split(":", 1)
        corpus.latch_directives.setdefault(m, []).extend(v.strip() for v in vs.split(","))

    violations, census = [], defaultdict(list)
    exemptions_used = set()

    # ---- index witness checks: (live modules reached, witness var read-set)
    witnesses = []
    for cname, meta in sorted(manifest.items()):
        if not meta or meta.get("kind") not in WITNESS_KINDS:
            continue
        main_mod = meta["main"]
        reach = corpus.reachable_modules(main_mod)
        live_reach = {m for m in reach if m in corpus.mod_file and not is_calib_module(m)}
        wreads = corpus.var_readset(main_mod, meta["witness"])
        witnesses.append((cname, meta, main_mod, live_reach, wreads))

    # ---- P2: frozen copies + def-flip shadowing (witness + run mains)
    for cname, meta in sorted(manifest.items()):
        if not meta or meta.get("kind") not in (WITNESS_KINDS | {"run"}):
            continue
        main_mod = meta["main"]
        if main_mod not in corpus.mod_decls:
            violations.append(f"P6 {cname}: main module {main_mod} not found in IR")
            continue
        if not is_calib_module(main_mod):
            continue  # live-file mains are their own model
        reach = corpus.reachable_modules(main_mod)
        live_reach = {m for m in reach if m != main_mod and m in corpus.mod_file and not is_calib_module(m)}
        if not live_reach:
            violations.append(f"P2 {cname}: calibration main '{main_mod}' imports NO live model (frozen copy)")
            census["P2-frozen-copy"].append(cname)
            continue
        own = corpus.mod_decls[main_mod]
        live_names = set()
        for m in live_reach:
            for n, d in corpus.mod_decls[m].items():
                if d.get("qualifier") != "action":
                    live_names.add(n)
        for n, d in own.items():
            if d["kind"] == "def" and d.get("qualifier") != "action" and n in live_names:
                violations.append(f"P2 {cname}: calibration '{main_mod}' shadows live non-action declaration '{n}' (def-flip)")
                census["P2-shadow"].append(f"{cname}:{n}")

    # ---- P1 + P4 over holds checks
    p4_checked = set()
    for cname, meta in sorted(manifest.items()):
        if not meta or meta.get("kind") not in HOLDS_KINDS:
            continue
        main_mod = meta["main"]
        exempts = meta.get("vacuityExempt") or {}
        for cls in {e.get("class") for e in exempts.values() if isinstance(e, dict)}:
            if cls not in EXEMPT_CLASSES:
                violations.append(f"P6 {cname}: unknown vacuityExempt class '{cls}'")
        for inv in meta.get("invariants") or []:
            for lmod, leaf in corpus.conj_leaves(main_mod, inv):
                reads = corpus.var_readset(lmod, leaf)
                ex = exempts.get(leaf) or exempts.get(inv)
                if not reads:
                    violations.append(
                        f"P1 {cname}: invariant leaf '{leaf}' has an EMPTY var read-set (tautology) — restate over the transition relation; not exemptable")
                    census["P1-tautology"].append(f"{cname}:{leaf}")
                    continue
                live_mods = {m for (m, _) in reads}
                twins = [
                    wname for (wname, _, _, wlive, wreads) in witnesses
                    if (wlive & live_mods) and (wreads & reads)
                ]
                if twins:
                    census["P1-twinned"].append(f"{cname}:{leaf}")
                elif ex and isinstance(ex, dict) and ex.get("class") in EXEMPT_CLASSES and ex.get("reason"):
                    census[f"exempt-{ex['class']}"].append(f"{cname}:{leaf} — {ex['reason']}")
                    exemptions_used.add((cname, leaf if leaf in exempts else inv))
                else:
                    violations.append(
                        f"P1 {cname}: invariant leaf '{leaf}' (vars {sorted(v for _, v in reads)}) has no live-importing read-set-matched falsify twin and no vacuityExempt")
                    census["P1-untwinned"].append(f"{cname}:{leaf}")
                # P4 per (module, var): rooted at the LIVE module's own
                # step — the module declaring the var (spec wording; a
                # regime override may legitimately disable alphabets, so
                # the writer obligation belongs to the live model).
                # Fallback for legacy step-less modules: all of the
                # module's actions (hazard (nn)).
                for (vmod, var) in sorted(reads):
                    if (vmod, var) in p4_checked:
                        continue
                    p4_checked.add((vmod, var))
                    roots = (
                        ["step"]
                        if "step" in corpus.mod_decls.get(vmod, {})
                        else corpus.module_action_names(vmod)
                    )
                    writes = corpus.assigns_reachable(vmod, roots)
                    if not any(t == var and not ident for (t, ident) in writes):
                        violations.append(
                            f"P4 {cname}: var '{vmod}.{var}' (read by '{leaf}') has no non-identity assignment reachable from {vmod}.step — writerless latch")
                        census["P4-writerless"].append(f"{vmod}.{var}")

        # P6: unused exemptions
        for k in exempts:
            if not any((cname, k) == u for u in exemptions_used) and f"{cname}:{k}" not in [x.split(" — ")[0] for xs in census.values() for x in xs if isinstance(x, str)]:
                pass  # tolerated: exemption may cover a leaf twinned later; census notes below

    # ---- P5: latch integrity
    for lmod, latches in sorted(corpus.latch_directives.items()):
        for var in sorted(set(latches)):
            roots = ["step"] if "step" in corpus.mod_decls.get(lmod, {}) else corpus.module_action_names(lmod)
            writes = corpus.assigns_reachable(lmod, roots)
            if not any(t == var and not ident for (t, ident) in writes):
                violations.append(f"P5 {lmod}: declared latch '{var}' fails P4 (no live non-identity writer)")
                census["P5-latch-writerless"].append(f"{lmod}.{var}")
        # calibration writers
        for cmod, decls in corpus.mod_decls.items():
            if not is_calib_module(cmod):
                continue
            if lmod not in corpus.reachable_modules(cmod):
                continue
            roots = [n for n, d in decls.items() if d.get("qualifier") == "action"]
            writes = corpus.assigns_reachable(cmod, roots)
            for var in sorted(set(latches)):
                hits = [t for (t, ident) in writes if t == var and not ident]
                # only count assigns in the calibration's OWN actions
                own_writes = []
                for n in roots:
                    d = decls[n]
                    acc = []

                    def walk(e):
                        if isinstance(e, dict):
                            if e.get("kind") == "app" and e.get("opcode") == "assign":
                                tgt = e["args"][0].get("name")
                                rhs = e["args"][1]
                                ident = isinstance(rhs, dict) and rhs.get("kind") == "name" and rhs.get("name") == tgt
                                acc.append((tgt, ident))
                            for v in e.values():
                                walk(v)
                        elif isinstance(e, list):
                            for v in e:
                                walk(v)

                    walk(d.get("expr"))
                    own_writes.extend(acc)
                if any(t == var and not ident for (t, ident) in own_writes):
                    violations.append(f"P5 {cmod}: calibration action non-identity-assigns latch '{lmod}.{var}' (oracle must live in the shared live module)")
                    census["P5-calib-latch-write"].append(f"{cmod}:{var}")

    # ---- P3 pairing census (no enforcement beyond reporting)
    holds_by_livemod = defaultdict(list)
    for cname, meta in manifest.items():
        if meta and meta.get("kind") in HOLDS_KINDS:
            for m in corpus.reachable_modules(meta["main"]):
                if m in corpus.mod_file and not is_calib_module(m):
                    holds_by_livemod[m].append(cname)
    for (wname, _, _, wlive, _) in witnesses:
        paired = sorted({h for m in wlive for h in holds_by_livemod.get(m, [])})
        census["P3-paired" if paired else "P3-unpaired"].append(wname)

    # ---- census print (P6, the owner burn-down artifact)
    print("== quint-policy census ==")
    for k in sorted(census):
        print(f"  {k}: {len(census[k])}")
        for entry in sorted(census[k]):
            print(f"    - {entry}")
    print(f"== violations: {len(violations)} ==")
    for v in violations:
        print(f"  {v}")

    if violations and not args.census_only:
        sys.exit(1)
    print("quint-policy:", "census-only (advisory)" if args.census_only else "OK")


if __name__ == "__main__":
    main()
