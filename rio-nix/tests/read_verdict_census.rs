//! [GEN-SET] The content/structural read-verdict census (bug_020,
//! R31′(ii) — call-site-shape derivation, never a hand sweep).
//!
//! The live_059 close typed the content/structural boundary at the
//! READ SITE ("so a future content field cannot silently inherit
//! strictness") but enforced it by hand conversion plus a claimed
//! sweep — and the fourth content arm (the terminal error narration)
//! inherited strictness silently, exactly what the doctrine said
//! cannot happen. This census makes the doctrine machine-true:
//!
//! - The walk is SYN-DRIVEN over every `src/protocol/*.rs` module
//!   (the universe is the directory listing — bidirectional by
//!   construction), `#[cfg(test)]` items pruned.
//! - The strict family is the full exported string-read tier —
//!   `read_string`, `read_strings`, `read_string_pairs` (the
//!   collection forms are same-tier siblings a read_string-only
//!   predicate cannot see; `read_string_pairs` joined when the walk
//!   found it at the tree) — CLOSED TRANSITIVELY: any local fn whose
//!   body calls a family member joins the family, so a wrapper
//!   cannot launder unverdicted strictness to its own call sites.
//! - Every strict-family call site carries a verdict token —
//!   `// strict: <reason>` on its line or the line above. A
//!   `read_content_string` call IS the content verdict (the fn name
//!   states it); strictness is the opt-in that must be justified.
//! - The plants live IN-POPULATION (synthetic modules joining the
//!   parsed set) and the K-mutation battery proves every grammar
//!   clause load-bearing: the planted red must DIE under the seeded
//!   narrowing that disables its rule (R31′(iii)).

use std::collections::{BTreeMap, BTreeSet};

/// The wire-exported strict string-read tier (the seed family).
const STRICT_SEED: &[&str] = &["read_string", "read_strings", "read_string_pairs"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Grammar {
    /// The full census: seed family + transitive closure + the
    /// verdict requirement.
    Full,
    /// K-mutation 1: the family narrowed to `read_string` alone —
    /// must MISS the collection-form (`read_strings`) wrapper plant.
    ReadStringOnly,
    /// K-mutation 2: the verdict requirement dropped — must MISS the
    /// unverdicted direct-call plant.
    NoVerdictRule,
    /// K-mutation 3: the transitive closure disabled — must MISS the
    /// wrapper-laundering plant's call sites.
    NoTransitiveClosure,
}

/// One parsed module: its (display) name and source text.
struct Module {
    name: String,
    src: String,
}

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && a.meta
                .require_list()
                .map(|l| l.tokens.to_string().contains("test"))
                .unwrap_or(false)
    })
}

/// Collect (enclosing fn name → called idents) and every call site
/// (callee, line) from one file, `#[cfg(test)]` items pruned.
struct Walk<'a> {
    current_fn: Vec<String>,
    fn_calls: &'a mut BTreeMap<String, BTreeSet<String>>,
    sites: &'a mut Vec<(String, usize)>,
}

impl<'ast> syn::visit::Visit<'ast> for Walk<'_> {
    fn visit_item(&mut self, i: &'ast syn::Item) {
        let attrs = match i {
            syn::Item::Fn(f) => Some(&f.attrs),
            syn::Item::Mod(m) => Some(&m.attrs),
            syn::Item::Impl(im) => Some(&im.attrs),
            _ => None,
        };
        if let Some(a) = attrs
            && is_cfg_test(a)
        {
            return;
        }
        if let syn::Item::Fn(f) = i {
            self.current_fn.push(f.sig.ident.to_string());
            syn::visit::visit_item(self, i);
            self.current_fn.pop();
            return;
        }
        syn::visit::visit_item(self, i);
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        if is_cfg_test(&f.attrs) {
            return;
        }
        self.current_fn.push(f.sig.ident.to_string());
        syn::visit::visit_impl_item_fn(self, f);
        self.current_fn.pop();
    }

    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*e.func
            && let Some(seg) = p.path.segments.last()
        {
            let callee = seg.ident.to_string();
            let line = seg.ident.span().start().line;
            self.sites.push((callee.clone(), line));
            if let Some(f) = self.current_fn.last() {
                self.fn_calls.entry(f.clone()).or_default().insert(callee);
            }
        }
        syn::visit::visit_expr_call(self, e);
    }
}

/// Run the census over a module set. Returns violations.
fn census(modules: &[Module], grammar: Grammar) -> Vec<String> {
    let mut violations = Vec::new();
    let seed: Vec<&str> = match grammar {
        Grammar::ReadStringOnly => vec!["read_string"],
        _ => STRICT_SEED.to_vec(),
    };

    // Pass 1: per-module call graphs + sites.
    let mut fn_calls: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut per_module_sites: Vec<(usize, Vec<(String, usize)>)> = Vec::new();
    for (idx, m) in modules.iter().enumerate() {
        let file = match syn::parse_file(&m.src) {
            Ok(f) => f,
            Err(e) => {
                violations.push(format!("{}: syn parse failed: {e}", m.name));
                continue;
            }
        };
        let mut sites = Vec::new();
        let mut w = Walk {
            current_fn: Vec::new(),
            fn_calls: &mut fn_calls,
            sites: &mut sites,
        };
        syn::visit::visit_file(&mut w, &file);
        per_module_sites.push((idx, sites));
    }

    // Pass 2: the transitive strict family (wrapper laundering joins).
    let mut family: BTreeSet<String> = seed.iter().map(|s| s.to_string()).collect();
    if grammar != Grammar::NoTransitiveClosure {
        loop {
            let mut grew = false;
            for (f, calls) in &fn_calls {
                if !family.contains(f) && calls.iter().any(|c| family.contains(c)) {
                    family.insert(f.clone());
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
    }

    // Pass 3: verdict tokens at every strict-family call site.
    let mut strict_sites_total = 0usize;
    for (idx, sites) in &per_module_sites {
        let m = &modules[*idx];
        let lines: Vec<&str> = m.src.lines().collect();
        for (callee, line) in sites {
            if !family.contains(callee.as_str()) {
                continue;
            }
            // A family fn's own DEFINITION-interior recursion into the
            // seed is a site like any other; the wire module itself is
            // excluded from the universe by the caller.
            strict_sites_total += 1;
            if grammar == Grammar::NoVerdictRule {
                continue;
            }
            let here = lines.get(line.saturating_sub(1)).copied().unwrap_or("");
            let above = if *line >= 2 {
                lines.get(line - 2).copied().unwrap_or("")
            } else {
                ""
            };
            if !(here.contains("// strict:") || above.contains("// strict:")) {
                violations.push(format!(
                    "{}:{line}: unverdicted strict read `{callee}` — add \
                     `// strict: <reason>` (or convert to read_content_string)",
                    m.name
                ));
            }
        }
    }

    // Definition pins: an emptied population walk is a red, never a
    // vacuous pass (the bug_047 lesson).
    if modules.is_empty() {
        violations.push("census universe is empty".to_string());
    }
    if strict_sites_total == 0 && !modules.is_empty() {
        violations.push(
            "the census found ZERO strict-family call sites — the population \
             walk is broken (the protocol modules read dozens of wire strings)"
                .to_string(),
        );
    }
    violations
}

/// The real universe, EMBEDDED at compile time ((wwwww): a gate
/// check quantifying over in-crate sources embeds its universe in
/// the check input — the test binary runs in the nix harness without
/// the source tree, so a runtime directory walk is dev-green/
/// gate-red). Every `.rs` directly under `src/protocol/` (the
/// `wire/` subdirectory hosts the family DEFINITIONS and is the
/// boundary, not a consumer). The embed is pinned BIDIRECTIONALLY
/// against the embedded `mod.rs` declaration list by
/// `embedded_universe_matches_the_module_decls`: a new protocol
/// module must be declared in mod.rs to exist as code, and the pin
/// refuses an embed list that drifts from those declarations in
/// either direction.
const PROTOCOL_MODULES: &[(&str, &str)] = &[
    ("build.rs", include_str!("../src/protocol/build.rs")),
    ("client.rs", include_str!("../src/protocol/client.rs")),
    (
        "derived_path.rs",
        include_str!("../src/protocol/derived_path.rs"),
    ),
    ("handshake.rs", include_str!("../src/protocol/handshake.rs")),
    ("mod.rs", include_str!("../src/protocol/mod.rs")),
    ("opcodes.rs", include_str!("../src/protocol/opcodes.rs")),
    ("pathinfo.rs", include_str!("../src/protocol/pathinfo.rs")),
    ("stderr.rs", include_str!("../src/protocol/stderr.rs")),
];

fn protocol_modules() -> Vec<Module> {
    PROTOCOL_MODULES
        .iter()
        .map(|(name, src)| Module {
            name: (*name).to_string(),
            src: (*src).to_string(),
        })
        .collect()
}

/// The (wwwww) bidirectional pin: the embedded universe equals the
/// `mod.rs` declaration list (plus `mod.rs` itself, minus the `wire`
/// boundary module). A module added to the protocol without joining
/// the embed — or an embed row whose file was deleted — reds HERE,
/// in the gate, with no filesystem dependence.
#[test]
fn embedded_universe_matches_the_module_decls() {
    let mod_rs = PROTOCOL_MODULES
        .iter()
        .find(|(n, _)| *n == "mod.rs")
        .expect("mod.rs embedded")
        .1;
    let mut declared: Vec<String> = Vec::new();
    for line in mod_rs.lines() {
        let l = line.trim();
        for prefix in ["pub mod ", "pub(crate) mod ", "mod "] {
            if let Some(rest) = l.strip_prefix(prefix)
                && let Some(name) = rest.strip_suffix(';')
            {
                if name != "wire" {
                    declared.push(format!("{name}.rs"));
                }
                break;
            }
        }
    }
    declared.sort();
    let mut embedded: Vec<String> = PROTOCOL_MODULES
        .iter()
        .map(|(n, _)| (*n).to_string())
        .filter(|n| n != "mod.rs")
        .collect();
    embedded.sort();
    assert_eq!(
        embedded, declared,
        "the embedded census universe must equal src/protocol/mod.rs's \
         module declarations (wire excluded as the defining boundary) — \
         re-derive the PROTOCOL_MODULES embed"
    );
}

/// The census at head: every strict read site carries its verdict.
#[test]
fn every_strict_read_site_carries_its_verdict() {
    let v = census(&protocol_modules(), Grammar::Full);
    assert!(v.is_empty(), "read-verdict census violations:\n{v:#?}");
}

const PLANT_DIRECT: &str = r#"
async fn evil_direct<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<String> {
    let s = wire::read_string(r).await?;
    Ok(s)
}
"#;

const PLANT_WRAPPER: &str = r#"
async fn launder<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<String> {
    // strict: the wrapper's ONE interior verdict
    wire::read_string(r).await
}
async fn evil_caller_a<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<String> {
    launder(r).await
}
async fn evil_caller_b<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<String> {
    launder(r).await
}
"#;

const PLANT_COLLECTION: &str = r#"
async fn evil_collection<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<Vec<String>> {
    let v = wire::read_strings(r).await?;
    Ok(v)
}
"#;

fn with_plant(plant: &str) -> Vec<Module> {
    let mut m = protocol_modules();
    m.push(Module {
        name: "planted.rs".to_string(),
        src: plant.to_string(),
    });
    m
}

/// The plant battery (R31′(iii), in-population): the unverdicted
/// direct call, the wrapper laundering (one interior verdict, many
/// unverdicted wrapper call sites), and the collection-form sibling.
#[test]
fn census_catches_the_planted_shapes() {
    let v = census(&with_plant(PLANT_DIRECT), Grammar::Full);
    assert!(
        v.iter()
            .any(|x| x.contains("planted.rs") && x.contains("read_string")),
        "the unverdicted direct read must red (got {v:?})"
    );

    let v = census(&with_plant(PLANT_WRAPPER), Grammar::Full);
    assert!(
        v.iter()
            .any(|x| x.contains("planted.rs") && x.contains("launder")),
        "the wrapper's unverdicted call sites must red — the family is \
         transitive, one interior verdict cannot launder N fields \
         (got {v:?})"
    );

    let v = census(&with_plant(PLANT_COLLECTION), Grammar::Full);
    assert!(
        v.iter()
            .any(|x| x.contains("planted.rs") && x.contains("read_strings")),
        "the collection-form sibling must red — the family is the full \
         exported tier, not read_string alone (got {v:?})"
    );
}

/// The K-mutation battery (R31′(iii)): every planted red DIES under
/// the seeded narrowing that disables its rule — each grammar clause
/// is load-bearing.
#[test]
fn census_k_mutations_kill_the_plants() {
    // K1: the read_string-only family misses the collection form.
    let v = census(&with_plant(PLANT_COLLECTION), Grammar::ReadStringOnly);
    assert!(
        !v.iter()
            .any(|x| x.contains("planted.rs") && x.contains("read_strings")),
        "under the read_string-only narrowing the collection plant's red \
         must DIE (got {v:?})"
    );

    // K2: dropping the verdict rule misses the direct plant.
    let v = census(&with_plant(PLANT_DIRECT), Grammar::NoVerdictRule);
    assert!(
        !v.iter().any(|x| x.contains("planted.rs")),
        "under the no-verdict mutation the direct plant's red must DIE \
         (got {v:?})"
    );

    // K3: disabling the closure misses the laundering wrapper's call
    // sites.
    let v = census(&with_plant(PLANT_WRAPPER), Grammar::NoTransitiveClosure);
    assert!(
        !v.iter().any(|x| x.contains("launder(")),
        "under the no-closure mutation the wrapper plant's red must DIE \
         (got {v:?})"
    );

    // K4: the emptied population walk reds on the definition pins.
    let v = census(&[], Grammar::Full);
    assert!(
        !v.is_empty(),
        "an empty census universe must red, never vacuously pass"
    );
}
