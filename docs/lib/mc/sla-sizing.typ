// ADR-023 compile-time validations.
//
// Exports:
//   geom-lognormal-figure()  — §2.7 single-ε vs iid-sum shortfall (MC)
//   geom-lognormal-max       — box-max shortfall, for prose interpolation
//   lever-arm-figure()       — §2.3 σ(Ŝ) vs c-spacing (MC)
//   lever-arm-ratio          — {8,16}/{4,32} σ(Ŝ) ratio, for prose interpolation
//   headroom-coverage-figure() — §2.2 1.25 memory-floor coverage (closed-form)
//   duration-margin(σ, q)    — §Hardware-heterogeneity p_q-to-median gap
//
// MC sample counts are fixed at convergence (geom-lognormal N=50000, ~27s;
// lever-arm N=20000, ~2s). Asserts always engage. Prose-interpolated values
// (geom-lognormal-max, lever-arm-ratio) are computed once here so the doc
// body cannot drift from the figure/assert.

#import "@preview/suiji:0.5.1": gen-rng-f, normal-f, random-f

// Normal CDF Φ(x) via Abramowitz–Stegun 7.1.26 (|ε| < 1.5e-7).
#let normal-cdf(x) = {
  let (a1, a2, a3, a4, a5, p) = (
    0.254829592,
    -0.284496736,
    1.421413741,
    -1.453152027,
    1.061405429,
    0.3275911,
  )
  let s = if x < 0 { -1.0 } else { 1.0 }
  let z = calc.abs(x) / calc.sqrt(2.0)
  let t = 1.0 / (1.0 + p * z)
  let erf = (
    1.0 - ((((a5 * t + a4) * t + a3) * t + a2) * t + a1) * t * calc.exp(-z * z)
  )
  0.5 * (1.0 + s * erf)
}

// p_q-to-median gap of a lognormal with scale σ: e^{z_q σ} − 1.
#let duration-margin(σ, q: 0.9) = {
  // z_q via bisection on Φ (sufficient precision, no inverse-CDF dep).
  let (lo, hi) = (-6.0, 6.0)
  for _ in range(60) {
    let m = (lo + hi) / 2
    if normal-cdf(m) < q { lo = m } else { hi = m }
  }
  calc.exp((lo + hi) / 2 * σ) - 1.0
}

#let σ-grid = (0.10, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40)
#let p-grid = (0.05, 0.10, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40, 0.45, 0.50)
#let q-grid = range(50, 100).map(q => q / 100)

// One (σ, p) cell: returns max relative shortfall over q-grid.
#let cell-shortfall(rng, σ, p, N) = {
  // G ~ Geometric on {1,2,...}: g = 1 + floor(ln(u)/ln(p))
  let (rng, us) = random-f(rng, size: N)
  let lp = calc.ln(p)
  let gs = us.map(u => 1 + calc.floor(calc.ln(calc.max(u, 1e-300)) / lp))
  let g-total = gs.sum()

  // single-ε model: g · exp(σ·z), one z per draw
  let (rng, zs) = normal-f(rng, size: N)
  let upper = range(N).map(i => gs.at(i) * calc.exp(σ * zs.at(i))).sorted()

  // iid-sum model: Σ_{k<g} exp(σ·z_k), g draws per sample
  let (rng, zs2) = normal-f(rng, size: g-total)
  let truth = ()
  let off = 0
  for g in gs {
    let s = 0.0
    for k in range(g) { s += calc.exp(σ * zs2.at(off + k)) }
    truth.push(s)
    off += g
  }
  let truth = truth.sorted()

  let short = 0.0
  for q in q-grid {
    let i = calc.min(N - 1, calc.floor(q * N))
    let d = (truth.at(i) - upper.at(i)) / truth.at(i)
    if d > short { short = d }
  }
  (rng, short)
}

#let geom-lognormal-mc(N: 10000, seed: 0xADE023) = {
  let rng = gen-rng-f(seed)
  let rows = ()
  let box-max = 0.0
  for σ in σ-grid {
    let row = ()
    for p in p-grid {
      let (rng2, s) = cell-shortfall(rng, σ, p, N)
      rng = rng2
      row.push(s)
      if s > box-max { box-max = s }
    }
    rows.push((σ, row))
  }
  let body = rows
    .map(((σ, row)) => (
      [$#calc.round(σ, digits: 2)$],
      ..row.map(s => [#calc.round(100 * s, digits: 1)]),
    ))
    .flatten()
  (body, box-max)
}

// threshold guards the §2.7 prose budget claim; prose interpolates
// #geom-lognormal-max so cannot drift; assert is qualitative-regression guard.
#let geom-lognormal-figure(N: 50000, threshold: 0.10) = {
  let (body, box-max) = geom-lognormal-mc(N: N)
  assert(
    box-max <= threshold,
    message: "§2.7 single-ε approximation: max shortfall "
      + str(calc.round(100 * box-max, digits: 1))
      + "% exceeds "
      + str(calc.round(100 * threshold))
      + "% threshold",
  )
  figure(
    placement: auto,
    caption: [
      Single-$epsilon$ model vs iid-sum: max relative shortfall
      $(Q_"iid" (q) - Q_"single" (q)) slash Q_"iid" (q)$ over $q in [0.5, 0.99]$,
      in % ($N = #N$ per cell). Box max *#calc.round(100 * box-max, digits: 1)%*.
    ],
    table(
      columns: (auto,) + (1fr,) * p-grid.len(),
      align: (right,) * (1 + p-grid.len()),
      stroke: none,
      table.hline(),
      [$sigma backslash p$], ..p-grid.map(p => [$#p$]),
      table.hline(),
      ..body,
      table.hline(),
    ),
  )
}

// ─── §2.3 lever-arm: σ(Ŝ)/S vs c-pair spacing under Amdahl T(c)=S+P/c ───

#let lever-arm-mc(designs, σ-noise, S, P, N) = {
  let rng = gen-rng-f(0xADE023 + 1)
  let out = ()
  for cs in designs {
    let (c1, c2) = cs
    let (T1, T2) = (S + P / c1, S + P / c2)
    let (rng2, e1) = normal-f(rng, scale: σ-noise, size: N)
    let (rng3, e2) = normal-f(rng2, scale: σ-noise, size: N)
    rng = rng3
    let inv = 1.0 / (1.0 / c1 - 1.0 / c2)
    let s-hat = range(N).map(i => {
      let (t1, t2) = (T1 * calc.exp(e1.at(i)), T2 * calc.exp(e2.at(i)))
      t1 - (t1 - t2) * inv / c1
    })
    let mean = s-hat.sum() / N
    let var = s-hat.map(x => calc.pow(x - mean, 2)).sum() / (N - 1)
    out.push((cs, calc.sqrt(var) / S))
  }
  out
}

#let lever-arm-figure(
  N: 20000,
  σ-noise: 0.14, // ~±15% run-to-run
  S: 30.0,
  P: 240.0, // T(1)=270, T(∞)=30 — typical 9× speedup ceiling
  designs: ((8, 16), (4, 32), (2, 64)),
) = {
  let res = lever-arm-mc(designs, σ-noise, S, P, N)
  let r = res.map(x => x.at(1))
  // The doc states the MC-computed ratio inline via #lever-arm-ratio; this
  // assert is the floor below which the qualitative argument breaks.
  assert(
    r.at(0) / r.at(1) >= 2.0,
    message: "lever-arm: {8,16} σ(Ŝ)/S = "
      + str(calc.round(100 * r.at(0)))
      + "% is not ≥2× the {4,32} value "
      + str(calc.round(100 * r.at(1)))
      + "%",
  )
  figure(
    placement: auto,
    caption: [
      $sigma(hat(S)) slash S$ from a 2-point Amdahl fit at noise
      $sigma_epsilon = #σ-noise$ ($approx ±15%$), $S = #S$, $P = #P$ ($N = #N$).
      Wider $c$-spacing gives a longer lever arm on the $1 slash c$ basis.
    ],
    table(
      columns: 3,
      align: (center, center, right),
      stroke: none,
      table.hline(),
      [$c$-pair], [span ratio], [$sigma(hat(S)) slash S$],
      table.hline(),
      ..res
        .map(((cs, rel)) => (
          [${#cs.at(0), #cs.at(1)}$],
          [$#calc.round(cs.at(1) / cs.at(0)) times$],
          [#calc.round(100 * rel, digits: 0)%],
        ))
        .flatten(),
      table.hline(),
    ),
  )
}

// Prose-interpolation values: computed once at module load so the doc body
// states the same numbers the figures show. See §2.3 #lever-arm-ratio,
// §2.7 #geom-lognormal-max.
#let _lever-r = lever-arm-mc(((8, 16), (4, 32)), 0.14, 30.0, 240.0, 20000).map(
  x => x.at(1),
)
#let lever-arm-ratio = [#calc.round(_lever-r.at(0) / _lever-r.at(1), digits: 1)×]
#let (_, _glm-max) = geom-lognormal-mc(N: 50000)
#let geom-lognormal-max = [#calc.round(100 * _glm-max, digits: 1)%]

// ─── §2.2 memory headroom-floor coverage: P(M_obs ≤ k·M_p90) = Φ(z_0.9 + ln k / σ_M) ───

#let headroom-coverage-figure(
  k: 1.25,
  σ-grid: (0.10, 0.12, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40),
) = {
  let z90 = 1.2816
  let cov = σ-grid.map(σ => normal-cdf(z90 + calc.ln(k) / σ))
  let min-cov = calc.min(..cov)
  assert(
    min-cov >= 0.95,
    message: "headroom floor "
      + str(k)
      + "× gives only p"
      + str(calc.round(100 * min-cov))
      + " at σ_M="
      + str(σ-grid.last()),
  )
  figure(
    placement: auto,
    caption: [
      Memory headroom-floor coverage $Phi(z_0.9 + ln #k slash sigma_M)$ — the probability that
      $M_"obs" <= #k dot.op M_"p90"$ under lognormal residuals (the $z_0.9$ term is the p90 fit's own quantile).
      Min *p#calc.round(100 * min-cov)* at $sigma_M = #σ-grid.last()$.
    ],
    table(
      columns: 1 + σ-grid.len(), align: right, stroke: none,
      table.hline(),
      [$sigma_M$], ..σ-grid.map(σ => [$#σ$]),
      table.hline(),
      [coverage], ..cov.map(c => [p#calc.round(100 * c)]),
      table.hline(),
    ),
  )
}
