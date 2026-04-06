//! Checksum-freeze guard for `migrations/*.sql`.
//!
//! sqlx checksums each migration file by content (SHA-384 over the
//! full file body — `fs::read_to_string` → `Sha384::digest`). A
//! comment edit changes the checksum. Any persistent DB that already
//! applied the old checksum fails with `VersionMismatch` on next
//! deploy.
//!
//! This test pins the checksum of every migration after it ships,
//! turning checksum drift into a CI failure instead of a deploy-time
//! surprise.
//!
//! See `rio-store/src/migrations.rs` for the policy and the home for
//! migration commentary.

// Integration tests compile the lib WITHOUT cfg(test), so the
// lib-level MIGRATOR static at rio-store/src/lib.rs:34 is invisible
// here. Keep a separate copy (same path, same content — the macro
// expands at compile time).
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// sqlx checksums migration files by content — editing a comment
/// changes the checksum and bricks persistent-DB deploys. This test
/// pins the checksum of each migration after it ships.
///
/// **Adding a NEW migration:** the test fails with `unpinned migration
/// NNN: add to PINNED`. Copy the hex-SHA from the panic message into
/// the `PINNED` table below, commit alongside the new `.sql`.
///
/// **Checksum CHANGED for an existing migration:** the edit is almost
/// certainly wrong. Move commentary to `rio-store/src/migrations.rs`
/// instead — see `M_018` there for the pattern. The ONLY legitimate
/// reason to update a pinned checksum is a pre-production behavior
/// change, and only after verifying no persistent DB has applied it.
#[test]
fn migration_checksums_frozen() {
    // (version, hex-SHA384). Regenerate with:
    //   for f in migrations/*.sql; do \
    //     v=$(basename "$f" | sed 's/_.*//'); \
    //     echo "($((10#$v)), \"$(sha384sum "$f" | cut -d' ' -f1)\"),"; \
    //   done
    //
    // sqlx computes the same SHA-384 internally (over the full file
    // body, verbatim — see sqlx-core/src/migrate/source.rs:118 and
    // migration.rs:25). sha384sum output matches exactly.
    #[rustfmt::skip]
    const PINNED: &[(i64, &str)] = &[
        (1,  "6e0a805dc2771f402124d3567a877261eccf0a71a2e93aa336fe938d6b35d0fddb75825a8487783ea8a5b26844893334"),
        (2,  "7c35c4bb93a833850182b6f1c68d12fbd187a9e6f33dad896b40a3ee0b69fe4fe5739a5f7a2d71a172625c414e0fa50a"),
        (3,  "41e422334a4f802767442f2438aa374e5a4890b12e4bfdde19ea3b6a3e92ec5b4a2b9c49b8dc26b59540fe6f17cc0b3e"),
        (4,  "18d44fe2a0547a521918d595c98f7ba7f344fd3e95c8b2fa9481b8b692cb7b657c8b03c051ed5ca74d92d26d3ecf0384"),
        (5,  "b33e00c42765502a61849eb9f6faa3236afb999e2477461f45c1e70f8149dc17592b4fd6174015d9e90c9b1e41a89963"),
        (6,  "fb986422e78d116c0a96c83afbf5132ad61534335bea385378848c56017c6abfd1ddea134accaafcd9fc8259512465d1"),
        (7,  "237e80a0770e7fe532777f7b0d95b066075d7795441a00ba72efbeec6ed0fe78424d4b8d8b21acd1c8f601a9026385fc"),
        (8,  "a44613e65354d4dff9bebc41104acbc4f9d618603c3cf87d1956a22abc0a29e45e0e7e581e821d54bcb25940e8ecc5ab"),
        (9,  "9da12e7a1e9aaaa1b6cbaf0eb05be4149b4c253073c68f7536255a909950adafaba4c4d6a4fac5bf361de53a3a54b4f3"),
        (10, "f1ab14c3b70a79e1b20e9c37a5510e388d340b1c8dcb184c3938ef7376e69c05b47359252ba79bffbc73483716037e1d"),
        (11, "d24f637e0891321bd66eeac2df69b80f0b81ac39dcc051870ffda9f66f34d94996b634bf4484662ddabb5208e4376f4a"),
        (12, "4114fa9cc33051c280fbfde47f9ef87a7f51f964df258ca4dad9e9b6397d83612459f23d2a483e060287c6265f5fe642"),
        (13, "70eaee615087c627763aec486a013d606719b2a7aed0abdb3f829397ca4312891ac30911bad58fe145b14ab439b21d60"),
        (14, "744cb318880493778f0ef5fbf7555630e6408ac34f02fbdd9541c28c5769bd0713374b99c3b1a847cdd533398b3a3431"),
        (15, "e433dbdca36c8b17eae2ed6c44f703bac1c4b35145f134be215e1b04af184f0eb3114038164ee88a3c1b7455c96ab7cc"),
        (16, "1aa234880380efacd85b0578a87a69bfa50d767e40a079ac0a5ddabc287071e4718d7f957f367a662f4d5e616e6d54ef"),
        (17, "3b1e59ae0504f23864283c55bd2b2a7e42dd3c6655df0104b50683b489e58b630ec193d28f1c151bcf83fab25aed106d"),
        (18, "c8fa9d2b6a8c895ca8d549ea31ecaa3f4a3abdebf4377c02dfeb1a4bbb837825d95f309619055ab185e86a16b312a916"),
    ];

    let pinned: std::collections::HashMap<i64, &str> = PINNED.iter().copied().collect();

    let mut unpinned = Vec::new();
    for m in MIGRATOR.iter() {
        let actual = hex::encode(m.checksum.as_ref());
        match pinned.get(&m.version) {
            Some(&expected) => assert_eq!(
                actual, expected,
                "\n  migration {:03} checksum changed — move commentary to \
                 rio-store/src/migrations.rs (M_{:03}), do NOT edit the .sql.\n  \
                 If this is an intentional pre-prod behavior change AND no \
                 persistent DB has applied it, update PINNED here.",
                m.version, m.version,
            ),
            None => unpinned.push((m.version, actual)),
        }
    }

    // Batch-report unpinned rows so adding N new migrations is one
    // test cycle, not N.
    assert!(
        unpinned.is_empty(),
        "\n  unpinned migration(s) — add to PINNED in \
         rio-store/tests/migrations.rs:\n{}",
        unpinned
            .iter()
            .map(|(v, h)| format!("        ({v}, \"{h}\"),"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    // Reverse check: no stale pinned rows for migrations that were
    // deleted/renumbered. Cheap — `migrator.iter()` is static.
    let present: std::collections::HashSet<i64> = MIGRATOR.iter().map(|m| m.version).collect();
    for (v, _) in PINNED {
        assert!(
            present.contains(v),
            "PINNED lists migration {v} but migrations/ has no such file — \
             remove the stale row",
        );
    }
}
