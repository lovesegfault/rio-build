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
//! See `rio-migrations/src/migrations.rs` for the policy and the home
//! for migration commentary.

use std::time::Duration;

use rio_migrations::MIGRATOR;

/// bug_354 upgrade-shape fixture: a chunk row soft-deleted BEFORE 091
/// (no `deleted_at` column yet) must be backfilled by 091's in-place
/// `UPDATE … SET deleted_at = now() WHERE deleted AND deleted_at IS
/// NULL` so the pre-upgrade tombstone population becomes reapable —
/// the exact rows the reaper was added for.
///
/// Shape: truncate the migrator to ≤090, migrate, seed a soft-deleted
/// row through the pre-091 schema, then run the FULL suite (the
/// normal upgrade path) and assert the backfill stamped the row.
/// RED (recorded, backfill line removed): deleted_at stays NULL — the
/// reap predicate (`deleted_at < now() - grace`) can never match.
#[tokio::test]
async fn migration_091_backfills_preexisting_tombstones() {
    let db = rio_test_support::TestDb::new_empty().await;

    // Truncated migrator: everything strictly before 091.
    let mut pre = rio_migrations::migrator();
    pre.migrations = pre
        .migrations
        .iter()
        .filter(|m| m.version < 91)
        .cloned()
        .collect::<Vec<_>>()
        .into();
    assert_eq!(
        pre.migrations.last().map(|m| m.version),
        Some(90),
        "fixture expects the pre-091 prefix to top out at 090"
    );
    rio_migrations::migrate::run(&db.pool, pre)
        .await
        .expect("pre-091 prefix applies");

    // A tombstone the pre-091 world could produce: deleted, no
    // deleted_at column exists yet.
    sqlx::query("INSERT INTO chunks (blake3_hash, size, deleted) VALUES ($1, 42, TRUE)")
        .bind(&[0xB3u8; 32][..])
        .execute(&db.pool)
        .await
        .expect("pre-091 tombstone seeds");

    // The upgrade: the full suite applies 091+ on top.
    rio_migrations::migrate::run(&db.pool, rio_migrations::migrator())
        .await
        .expect("full suite applies over the prefix");

    let stamped: Option<bool> =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM chunks WHERE blake3_hash = $1")
            .bind(&[0xB3u8; 32][..])
            .fetch_optional(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        stamped,
        Some(true),
        "091 must backfill deleted_at for pre-091 tombstones"
    );

    // Reap-shape check: with deleted_at anchored at upgrade time the
    // row enters the reap predicate's domain once grace elapses
    // (rio-store's predicate-pin battery keeps the live statement's
    // conjuncts honest; this asserts the data side).
    let reapable_once_aged: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chunks \
         WHERE deleted AND deleted_at <= now() \
           AND NOT EXISTS (SELECT 1 FROM pending_s3_deletes p \
                            WHERE p.blake3_hash = chunks.blake3_hash)",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        reapable_once_aged, 1,
        "the backfilled tombstone is in the reap domain"
    );
}

/// I-194 regression: 3 concurrent `rio_migrations::migrate::run()` against
/// one fresh DB all complete, and the `CREATE INDEX CONCURRENTLY`
/// migration (022) lands a valid index. (011's CIC index is dropped
/// by 035, so nothing left to assert there.)
///
/// Under sqlx's default blocking `pg_advisory_lock`, replica B's
/// blocked `SELECT pg_advisory_lock(...)` holds a virtualxid that
/// replica A's CIC waits on → deadlock. The try-then-wait lock in
/// `rio_migrations::migrate::run` holds no long-lived vxid while polling.
///
/// 60s timeout: full migration set on the ephemeral PG runs in
/// well under 5s; the timeout is the deadlock detector. NOT
/// `tokio::time::pause()`-able — `pg_try_advisory_lock` round-trips
/// to a real server, and CIC's vxid wait is server-side.
// r[verify store.db.migrate-try-lock+2]
#[tokio::test]
async fn concurrent_migrations_no_deadlock() {
    let db = rio_test_support::TestDb::new_empty().await;

    // Three "replicas" racing on the same fresh DB. Each gets its
    // own owned Migrator value (`migrator()` re-invokes the macro;
    // `set_locking` in `rio_migrations::migrate::run` mutates).
    let r = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::try_join!(
            rio_migrations::migrate::run(&db.pool, rio_migrations::migrator()),
            rio_migrations::migrate::run(&db.pool, rio_migrations::migrator()),
            rio_migrations::migrate::run(&db.pool, rio_migrations::migrator()),
        )
    })
    .await
    .expect("concurrent migrations deadlocked (>60s) — I-194 regression")
    .expect("a replica failed to migrate");
    let _ = r;

    // CONCURRENTLY indexes from 022 exist AND are valid (CIC leaves
    // an INVALID shell on failure; IF NOT EXISTS would then no-op
    // the retry — assert validity, not just presence).
    for idx in ["builds_keyset_idx"] {
        let valid: Option<bool> = sqlx::query_scalar(
            "SELECT i.indisvalid \
             FROM pg_class c JOIN pg_index i ON i.indexrelid = c.oid \
             WHERE c.relname = $1",
        )
        .bind(idx)
        .fetch_optional(&db.pool)
        .await
        .unwrap();
        assert_eq!(valid, Some(true), "index {idx} missing or INVALID");
    }

    // All 3 saw the same final schema version (followers' run() is a
    // no-op re-check, not a partial apply).
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(applied, MIGRATOR.iter().count() as i64);
}

/// `assert_current` is what store/scheduler run at startup instead of
/// migrating (migrations run out-of-band via `rio-store migrate`).
/// It must catch BOTH failure shapes a mis-ordered deploy
/// produces — never-migrated database and partially-migrated database
/// — with an error naming the migration runner, and must accept a
/// fully-migrated one.
// r[verify store.db.schema-current+2]
#[tokio::test]
async fn assert_current_schema_check() {
    let db = rio_test_support::TestDb::new_empty().await;

    // Fresh database: no _sqlx_migrations table at all.
    let err = rio_migrations::migrate::assert_current(&db.pool)
        .await
        .expect_err("empty database must fail the schema check");
    assert!(
        format!("{err:#}").contains("rio-store migrate"),
        "schema-missing error must name the runner, got: {err:#}"
    );

    // Fully migrated: check passes.
    rio_migrations::migrate::run(&db.pool, rio_migrations::migrator())
        .await
        .expect("migrations apply on the ephemeral PG");
    rio_migrations::migrate::assert_current(&db.pool)
        .await
        .expect("current schema must pass");

    // Stale: simulate a binary that embeds one migration more than
    // the database has applied (deleting the newest applied row is
    // equivalent — `assert_current` only compares version sets, and
    // the gap sits at the tail like a real missed hook run).
    let newest: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
        .bind(newest)
        .execute(&db.pool)
        .await
        .unwrap();
    let err = rio_migrations::migrate::assert_current(&db.pool)
        .await
        .expect_err("stale database must fail the schema check");
    let msg = format!("{err:#}");
    assert!(
        msg.contains(&newest.to_string()) && msg.contains("rio-store migrate"),
        "stale error must name the missing version and the runner, got: {msg}"
    );

    // Non-42P01 failure (closed pool here; connection drops or
    // permission errors in production) must NOT claim the schema is
    // missing — the runner hint would send the operator to re-run
    // migrations when the actual problem is connectivity.
    db.pool.close().await;
    let err = rio_migrations::migrate::assert_current(&db.pool)
        .await
        .expect_err("closed pool must fail the schema check");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("schema check query failed") && !msg.contains("rio-store migrate"),
        "non-42P01 error must use the neutral context, got: {msg}"
    );
}

/// M_050 regression: 020's `[[:space:]]` / `trim()` are ASCII-only;
/// Rust `NormalizedName::new` is Unicode-aware. NBSP (U+00A0) passed
/// 020's CHECK but failed Rust → manual-INSERT zombie row. 050's
/// `^[a-zA-Z0-9._-]+$` allowlist rejects ALL non-allowlisted chars,
/// closing the gap.
///
/// Direct INSERT bypasses CreateTenant's Rust-side validation, so a
/// failing INSERT proves the *database* layer rejects it.
#[tokio::test]
async fn migration_050_allowlist_rejects_unicode_ws() {
    let db = rio_test_support::TestDb::new(&MIGRATOR).await;

    // Interior NBSP: passed 020 (NBSP ∉ POSIX [[:space:]]); rejected
    // by 050's allowlist.
    let err = sqlx::query("INSERT INTO tenants (tenant_name) VALUES ('team' || chr(160) || 'a')")
        .execute(&db.pool)
        .await
        .expect_err("interior NBSP must fail tenant_name_allowlist");
    assert!(
        err.to_string().contains("tenant_name_allowlist"),
        "expected tenant_name_allowlist violation, got: {err}"
    );

    // Trailing NBSP: PG trim() does NOT strip it (only U+0020), so
    // 020's `tenant_name = trim(tenant_name)` passed; allowlist rejects.
    let err = sqlx::query("INSERT INTO tenants (tenant_name) VALUES ('team-a' || chr(160))")
        .execute(&db.pool)
        .await
        .expect_err("trailing NBSP must fail tenant_name_allowlist");
    assert!(
        err.to_string().contains("tenant_name_allowlist"),
        "expected tenant_name_allowlist violation, got: {err}"
    );

    // Positive control: allowlisted name inserts cleanly. Without
    // this, a `CHECK (false)` typo would pass both negatives above.
    sqlx::query("INSERT INTO tenants (tenant_name) VALUES ('team-a.0_1')")
        .execute(&db.pool)
        .await
        .expect("allowlisted name must pass both 020 and 050 CHECKs");
}

/// sqlx checksums migration files by content — editing a comment
/// changes the checksum and bricks persistent-DB deploys. This test
/// pins the checksum of each migration after it ships.
///
/// **Adding a NEW migration:** the test fails with `unpinned migration
/// NNN: add to PINNED`. Copy the hex-SHA from the panic message into
/// the `PINNED` table below, commit alongside the new `.sql`.
///
/// **Checksum CHANGED for an existing migration:** the edit is almost
/// certainly wrong. Move commentary to `rio-migrations/src/migrations.rs`
/// instead — see `M_018` there for the pattern. The ONLY legitimate
/// reason to update a pinned checksum is a pre-production behavior
/// change, and only after verifying no persistent DB has applied it.
#[test]
fn migration_checksums_frozen() {
    // (version, hex-SHA384). Regenerate with:
    //   for f in rio-migrations/migrations/*.sql; do \
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
        (19, "a99257dac42f2583fcb3e322f14b1e06c89580ad9ac76be6c93dfa0694d79304e72ffeebd2c25a67e1d5a1c99ff00aa1"),
        (20, "9706a30c7b0ea8f71072b50de001f057d6a82f08b426f04d840312426d55c3b1d0028249fe4460f29459b2a29b1991e0"),
        (21, "a1fc3b25b1dad3d1ac7c968365919a8b74340d07f3f598dbbfab0272205b348b99eac177bc85926c4297eff870bec368"),
        (22, "6dae53e530cb6df6566b0d0ca155aab2693d54a5bc6ba5b120c409a70d6f38b5aac233b839b8a3ff35dc5644bf1809cf"),
        (23, "2eca5033f4bca1eb8188740e3ec548619fc8f55efc264090cb9b8b48ac7d0b8510db8aef2c2443207d7ccc07c76af02b"),
        (24, "ba9abd593da5a705acdc1b7fae1286e0cef5d01fefdc69402e583411d5c06b95961e9d48f0d66be7e09f08df1fa5ed5d"),
        (25, "379cfce286596ae971fd9d82dbb9f2ebd3c4c6fb2eb4e95dad4160261bfba94121d710af14fa6e5ed018b118ab74fa99"),
        (26, "4066f4a8771982b2631f627721c7ee3c60cb73b158279cffbeaf2dec263b5b7eba24e901fdd95de0da4212e069af28e6"),
        (27, "0da10f2b4f720813c7025cd5f936cd81684554bf0e0354a619d3932c40cc9cc9458594f75b3fbd96b68451504b7e0e28"),
        (28, "cc328bd04e362377e11cb9478292f0b0eee8eceb9efeed498d3e1b34328ecd404b653362694942822daef5486d72d4b7"),
        (29, "6539f105727e68272b29fcfeb5fdab78e8b00119928bb568c7ca727087bda654ce356060cc264c11bd377cd0415387c9"),
        (30, "44c25986f536f48d83f02c337205fca2c036b8b8f016b020a543d3d99c5a2aa4155611f46cc4697e017525f37d9e2fab"),
        (31, "2b14416a3f10727edbf5eed0532e6e6c3825df5a57f27b6c08b3b8fc8727bfe5acc9e0b5cbeace6da15430b843f3acbb"),
        (32, "fac4e2a1a72c75c832b66bc0c26dd9a521ee8ed51dadd2f1f6f79f50da755128cb063d18048769f764e56e5283434fa5"),
        (33, "4e370e504aa3c15a25272156f659d2a0f2740934e33eae2cef33c2c7848978b091acc83e3c215480c04a6732681fbcfc"),
        (34, "83e2abe90e43267f4b676dfe7d2fe2d3edb8daa2951eb9016c88128c7f221a5cd71861ba92466ce14c5d3513eee908f7"),
        (35, "7277e889fdeabd8db54f6f93bb48bd166a95764aece25ae3cfc3e0de9cbd5c1de6d09634ec3cca9216c3a2f60a1e3ecb"),
        (36, "5096464933e2fd91f4257cecc1cd49f545144835498757436e2d20c3f4644864d80fb7f9cd2df5dddc81fa274a2fff55"),
        (37, "dbb29fe26d66b31faa6e8334f3d3ffb8e9c59f459e828b4aa34ba82bded5e9c24b1ff22d6ba3a74ca272962e41a88404"),
        (38, "f6e87f21eba80678c0859cfc076d536795bdc34918a7197914dcd4167a6027e4f0da9105a017c4973a5fc3d692a20bd6"),
        (39, "456057539dca597ed07a293908bd3dc8a0017933aa307ef37a27e6b2b78f00f9ac4872a5234f2974c50c6a985a3d20c6"),
        (40, "e96bb3bbd04b26745530fa0af092379b363b7ca5ed26759beb1c67c8303002c2d5faa8d4021c13fac45610652d418b70"),
        (41, "a9f847937d1c69f0c1f96bbb65727ce489c99b58279385f9c64d5617de4fa9d71f6c5feb7680b162f4700cb4c2b0ab0e"),
        (42, "9ef0d88cfeff0e811f6a35311e442a4b53c277f909485a683184503f6aa0f49ccd955fd043dc44e3c5171b4d94abd4ff"),
        (43, "7c65b6a6cd4e4edfd97c35e42fbbad8ee9b3c5e00a7596059aa55f1e71a30dd25b5261c5f497a16e12a5d776b9d0b259"),
        (44, "c3cdf929a531fdc4e89e40277ad47264537e7b9d69b60dd8b511449c27838ad766ad6193faae72fc159c2995188c5c13"),
        (45, "5887729e799f5aa01a59b8f153c74101418377ed8a262507727dc0adbabbe05cb951c6509cc5a22be4e35b4612cc73fd"),
        (46, "bacacfa2a3a0e0f516d868b6a3f70c9bbc5235fc5f1a510e3f32e272affb80c7517ec1007c1a775acc30b67922cea156"),
        (47, "0217805341c7bdceb715de6b59c7a7d96db79ed9bcd5d5647ee0266a55f58d69e991edd775f6fa84d66939f0f1517886"),
        (48, "324d53bc92cf2964c8dc4384603a7fc02723c19a74cb714b2aeaef9b32e3dbd73cb3e792ddd345a2cc661d7660340091"),
        (49, "267516c69f42c689bc0accf6916179555fffc49a3cab334f8c17b0f539ddcaf4ff3b9444313d143ed21ab2db015ed692"),
        (50, "b8093c0b8573af60de08ee8eb07c12453023a790a99fca6678442dafd1867a8b4c66cc2c92d9df2b9bdd90a4a84ca0c6"),
        (51, "654cceb3e862473a43a18cee730637e31b9712a2bf89acdcee3586efd830fdaf2db4119ab447ef4cce77b2e381194b87"),
        (52, "ada24ea4f6799486d131027e7a8c995e64bd3e56d0716b614eabb683ca3c18beb2eea860de60dc1b6c63003002ab7516"),
        (53, "5b63404691d5229bbdfb2936686f2014f84e11fbc142b9fc945c8f18e1fd3035973f3780e4494f5cd87cc00cab90af3a"),
        (54, "b82007070c87e904074836f8d1d737ed593163cbb0ae87f112dbde4de6be97fef66e31a91b40412f8674d7a6e4a2250c"),
        // 055 deleted (dead schema on never-queried hw_cost_factors) —
        // sqlx tolerates the gap; see rio-migrations/src/migrations.rs.
        (56, "b456694bdc1a9b6dbc5cb36025ec198e389b77960ce783ed0afd276ff37476ad632c6c468826239316624433de4e8672"),
        (57, "6c626a27371ef3f46b23a2cfcdcd0052f487c1a90cbd6cade384ad7dda48e71835f94d40b718354ef0c2b46c1c1bae92"),
        (58, "3e2f05cc03b48c2e82bbaa8dc3b36fe89260c12bb9a5f921acac104dc2b9772e8f4c3b9a0ba9867599ec6901e20984d7"),
        (59, "b6c1260819a0892cb5cf0cefc5c9345bf387994f644ce55da9ca145860d1b545be029e1d1d7910fbc364d279d69a0e7a"),
        (60, "060370d165a5d1e481f0a1e57cac142027c496cccea820b9a200fce53ac3ed0a4003e9e4773858952691a19a4f5be6ad"),
        (61, "0f8a0d1b417ea958833fe47dafa7cea83599193fa31014933b1c3fad9aa2229087ac5ff2c16a2c45d67d003cca60e725"),
        (62, "ef8bc881b70d800f4eba8923d56500243083ac855716636b4e916ef2b8c94ebabea751bb87e274f65cbb45baf8cb0e63"),
        (63, "3590687681bee63e3254a1ae52c20768b547f6d26d3ee126aeca5e0e284f869afea4f25d03cd15cb49d68655af24531a"),
        (64, "0b64d644a1fdb2f381c8655bbe2a8ab4258dba6fe8bf4fa428cc1df7e627b3b9c4eb8152cc640215dff290bd3ac72dea"),
        (65, "fbefd9e2249865351d6901071bfcb8f06ff85e89e64661b4b68073569c7fb88c2437a2907df5fdb77448e1277b6f8497"),
        (66, "a74322821f1b7a97c76185dba48bbd25643fe56cee466ad6dbf0006c28e4efe16aeef0d9e8bfa3eaa0794f957721dc84"),
        (67, "9c86fce4d1a7f94ea1f77933c9431edc4e85f9b75177b73b5273bc17adee6fbbda261ff9d4bd14b57356606dd538d8c2"),
        (68, "0d79185ed7da045ef6974f1682898ee7ba3f3ca22ba4b360648bfffc5d0ae837ffcb9e18f5dfb11f06542f63047f07fe"),
        (70, "5f0ba369a3f1bc78429f52d6bde049d40315cc098364c0ca2062e03ecdb090bf099a206028f884bb8410909ab468962f"),
        (71, "128130fa5b9e7c3e6ce01a1365e0562d412350421925eb15b2034895911d2c7400292306824f45f8e7a2646b9e522ff1"),
        (72, "c3c2a2d3b673adf0e4f47eb4a852c396e3072e7de25215fe374f2d9f88aa9ae1a7cded67f74973526d7037288306d98c"),
        (73, "21523ee30d7dd8b0c4b472eceed7f5363f989b4fd9ff835891f0d6dd84a85da7e493d7657576ff8b8e2569ddc7fdaeb3"),
        (74, "0d7a01df2e514f43862ca6046a5163c9f5a5cae36d6cfce78bf4e2cb35e1920cdaaeea6cfc9534828ca17ba01f27f1db"),
        (75, "59adad7c2e33bec0ae77bf525f32d133a4821b560cfb65779adc0a285dddf677607d470303692dfa068690043781c57b"),
        (76, "6122899143cc4f340a4a59d2f450e859b4ffc41339bb8c455e6520c4a7b92b3c4b95fadd577d8fb4537a53e3a032f64f"),
        (77, "6516c066cd4643aa5aea0154ab6cae62ec8944f3db2c073f0276500bf297171efbab08d1489b5cee7f5eaf3ff6fb42cd"),
        (78, "da278b2ad23df8c9621186db1b83999940bdd783b35e642355e3e52eb2bba6171f709287393337779ca0bee8d0a20fc5"),
        (79, "62de39af349c2526bdbfca6cb04b8451606e1f56b8052b00415c99a42d5eac7833accca44a9b735751aba6feb2c0a82e"),
        (80, "8c6b10572611e1c918efb5d68875df5347a1df89f3557326008f51d92bed8ab548add50cabac49d0590c0b75489173d4"),
        (81, "ff6653bc6e1c54ab375a4d0611dc5c403fbd9d86cdcf8542744d96030158c41943f3c215ee739128033cf9717729046b"),
        (82, "d51816c03b9b4d1f0fa2eb500810fc1512469171d5b6b178c9c6694b508fb4f8e1332b5838cbe10f91e1b4221b9dbcb5"),
        (83, "0815204229364f972a7325ec74c0f240668f3b565d4b563ad3b3dc6c914fd0303a13c0890ace54e2a932c60cb6f20bd8"),
        (84, "6c3152482ae12cc583843f9f65cdf66eabb5e6de4ceb9aed8c9e79975d98fe51927d38fc96b0de74f052c8a8d11dd20c"),
        (85, "b6f613676739870e7bd8305b7cbf507229af4301d4b05bdde30ee1473a460bdabafa07de22421cb455f49051633931f1"),
        (86, "300674a4976131530bcb7455f92962807ae9fb29bfdd11b12c02120166e04ad1a13ea5e80f96af06510a4651ecdb35b9"),
        (87, "77abeb4469311da1409b815ba37a67e1c5db4e43df7c293a728e559651080f4f61186ad9eac30a68acc01f05562f4e4d"),
    (88, "ffc918db47cd782757451292b3096e8ebafc128edcd52826bc557a9df0a3c702235e61a89a85ccbc7889ef93c511bd99"),
        (89, "b86bc43a0f76241cf5bcc11e794ed3104837e35558bd9817c511450d3d2281f62e635ce931dbffe4ad2acde0a4ec1fc3"),
        (90, "2cfae4d9a7ed79b10c3c0431266a59a8eba0f26efa5658935b263aebc7d81911e53598c38c0983504f2167417c1514ac"),
        (91, "400bff815b3b78d40ec589e483f7632a8128e90b2bdd70305bbf2bddaf304877ff45973b2918b5618a16404bd2afdebe"),
        (92, "a015e5b79a7fbb8d5ce18de91fe8052f8103a067f1d4a59922e8edc17b8c1474146c2ddd1058cd172da7e5539ad36dba"),
        (93, "aaeaf06e1fed0b64593e28ddd3f31ecf5214c14f8c826a34d91495c23246277a33aa1c017163a914c9ba3beabb651100"),
        (94, "cf331bf43708600be422b7d2bdbb30366cf8688920a2cd65894f911795ebc978c2dc7909f2254903c55a148cd9aab744"),
        (95, "d97a2280a397223fff4e1214cca423083797a8f6e8ad6f8b006ed456c06d175ac17c4e9eb37b49c62abc46994c83075b"),
        (96, "f3780ddaa211bb56f70759b6a5a4f4bbd6a6b39060a8d97a177b9009e516f6a14c6150d945580bca7f937311ff52e4c3"),
        (97, "8bffa8dd9c129e8e189b4246b4da79ead1531ee2c8b3448abacfff0501d39ef8531e36804e625824cea06865dcaca02e"),
        (100, "c812072c13e5252c831d04a57d09f7cb9eaa472fbf475e7189f9cea9dde5e4a2bc537b856b1fa9072543f4a4a3e288cf"),
        (101, "2305f91970e5c32487042d08ac6686dd95c2545f3d80aa0093417f5e20ebad5afb216ec61ea6c1cf7bcd374c0239d490"),
        (102, "3e9c3c698084e17e7b0ee62a3f44e18f00f8eaf7bf79ff04941facdc4d34dc849de7b9a67f1acb320d525a2b868cba5a"),
        (103, "692d0bd80b7a5f6f8d00b0e8c55f06e606efd2e8c206b0fa77bc32ab71d15b1334e16645ead3d46f898b2d8df4852245"),
        (104, "d8b9c60ff575b15aaec8cdac717d5f5617c4882597d91d4400245134f22ff9e9eb43cb198ce98f239f06c1c138b4a665"),
        (105, "9647e7a9852c6358a11c2eef4bc835dfa7f381ce1707b3d1ccc9468200c1b5e4d4346c7097330fae41ff65c3ddd5eeab"),
        (106, "c85491748c90654df3c400dce87689ce063402f5f02333abb81214b01269c6319b97543176c8b2ea98e2e27d21edf032"),
        (107, "dc4b93f9f33c544febde3dfab424be1ee0e5fb7f257572707a06ac36f7aecc8e0fdaf8336a4fc16d5d4ca28e5d18d07b"),
        (108, "671eed8cea4c35e144bcdfe24e7f75f7a4f36c2dba772d6e3256942d9f600d878e7775a6e32a86c7e7f328459799e257"),
        (109, "f67b3275cc36e98eb86cca422095562d6c4c9ba588fd46ee1064c26d4a9765c675b6f4518f7d2e050514bcdcb42f30df"),
        (110, "43fe0d52b25f6adf8f613b8d52e9a8f964aeeab506d470f8712aaebb1d6a93a771f9f9e6b9ce1d929d6fd21d6ceb6a09"),
        (111, "067c0fcc5c4c5dd82b07cdd164160613af2fcd2c5254ec588578a0aa71f30bba9c8e1af12fef0aaf87545e688e858452"),
        (112, "69532c6f49ca11a388cb4a52dcbc40762586afe81f6e8c34137f19a01c56db377b27a554c920a65335035fe8f6fdec55"),
        (113, "c9a381c459f6cd143b9c93ec227c0edb886f6d2ec0ee54380dd5c74ad0d8fa1b050aaa36fed771e53c846ad689aac75e"),
        (114, "0f1038327e2ab5841d42d47d4fc11a77fbcf13347e1e4310f121cfc081c2f5bb1e0f37e21aaa6f27b0fbfe36cbabe8b0"),
        (115, "a9498164f9c31b1e31f0adb2be12f96a6679f106588252a85fd7e1efd63ec3d65247e42769fb35102a65b95ffeba61c9"),
        (116, "5e7e9cfaffd659354d6ec9e7062dc3e99c1454af5f5c31ebd952e538d0e6da79ce2de7cfbb347e16b566bf46e2c97e97"),
    ];

    let pinned: std::collections::HashMap<i64, &str> = PINNED.iter().copied().collect();

    let mut unpinned = Vec::new();
    for m in MIGRATOR.iter() {
        let actual = hex::encode(m.checksum.as_ref());
        match pinned.get(&m.version) {
            Some(&expected) => assert_eq!(
                actual, expected,
                "\n  migration {:03} checksum changed — move commentary to \
                 rio-migrations/src/migrations.rs (M_{:03}), do NOT edit the .sql.\n  \
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
         rio-migrations/tests/migrations.rs:\n{}",
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

/// Cross-service schema contract: column shapes that rio-store reads
/// from tables that rio-SCHEDULER owns/writes.
///
/// rio-store's GC reads `scheduler_live_pins` directly (`gc/mark.rs`,
/// `gc/sweep.rs`), GC quotas read `tenants` (`gc/tenant.rs`), and the
/// build-log subsystem reads `assignments`/`derivations`/
/// `drv_executions` (the binding gate in `logs/gate.rs`, latest-exec
/// resolution in `logs/tail.rs`, the TTL sweep in `logs/sweep.rs`).
///
/// **Primary** enforcement is now compile-time: both crates
/// `query_as!` into `rio_migrations::schema::{LivePin, TenantRow}`, so a
/// column rename/retype breaks `cargo build`. THIS test is
/// defense-in-depth — it catches a regression where someone swaps a
/// `query_as!` site back to runtime `query_as` (silently dropping
/// the compile check) or where the const-string CTEs in mark/sweep
/// drift independently of the macro-checked anchors.
///
/// If this test fails after you wrote a migration: either keep the old
/// column shape (add a view, or rename back), or update BOTH the
/// rio-store query sites listed above AND this contract table.
#[tokio::test]
async fn cross_service_schema_contract() {
    let db = rio_test_support::TestDb::new(&MIGRATOR).await;

    // (table, column, udt_name) tuples rio-store reads. udt_name is
    // PG's underlying type name (text/uuid/int4/int8) — more stable
    // than data_type for assertions.
    #[rustfmt::skip]
    const STORE_READS: &[(&str, &str, &str)] = &[
        // gc/mark.rs ROOTS_SQL (UNION arm), gc/sweep.rs RECHECK_SQL
        ("scheduler_live_pins", "store_path_hash", "bytea"),
        // gc/tenant.rs quota lookup (full TenantRow projection — incl.
        // cache_token IS NOT NULL for the shared struct shape)
        ("tenants", "tenant_id",          "uuid"),
        ("tenants", "tenant_name",        "text"),
        ("tenants", "cache_token",        "text"),
        ("tenants", "gc_max_store_bytes", "int8"),
        // logs (063): the AppendLog binding gate resolves the latest
        // assignment for a derivation (assignments JOIN derivations)
        // to verify the claimed exec_id/builder_id.
        ("assignments", "exec_id",        "uuid"),
        ("assignments", "builder_id",     "text"),
        ("assignments", "status",         "text"),
        ("assignments", "assigned_at",    "timestamptz"),
        ("assignments", "derivation_id",  "uuid"),
        ("derivations", "derivation_id",  "uuid"),
        ("derivations", "drv_hash",       "text"),
        // logs (authz): TailLog ownership is build-membership
        // (store.log.tail-ownership) — authorize_tail joins the exec's
        // assignment (or, swept, its drv_executions hash ⨝ derivations)
        // through build_derivations to builds.tenant_id and compares
        // against the verified claims. derivations.tenant_id was
        // never production-written and is dropped by migration 095.
        ("build_derivations", "build_id",      "uuid"),
        ("build_derivations", "derivation_id", "uuid"),
        ("builds", "build_id",  "uuid"),
        ("builds", "tenant_id", "uuid"),
        // logs (063): latest-exec resolution + the completeness
        // predicate. drv_executions is scheduler-WRITTEN, store-READ.
        ("drv_executions", "exec_id",          "uuid"),
        ("drv_executions", "drv_hash",         "bpchar"),
        ("drv_executions", "status",           "text"),
        ("drv_executions", "final_line_count", "int8"),
        // logs (089): the AppendLog write-authority gate verifies the
        // claimed execution's kind is 'build', and the kind-filtered
        // `latest_build_exec` view (the unpinned TailLog resolver)
        // depends on it.
        ("drv_executions", "attempt_kind",     "text"),
        // logs (062): the TTL sweep's expiry predicate
        // (logs/sweep.rs: WHERE started_at < now() - retention).
        ("drv_executions", "started_at",       "timestamptz"),
    ];

    for &(table, col, want_udt) in STORE_READS {
        let got: Option<String> = sqlx::query_scalar(
            "SELECT udt_name FROM information_schema.columns \
             WHERE table_name = $1 AND column_name = $2",
        )
        .bind(table)
        .bind(col)
        .fetch_optional(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            got.as_deref(),
            Some(want_udt),
            "cross-service contract broken: rio-store reads {table}.{col} as {want_udt}, \
             but schema has {got:?} — see gc/mark.rs, gc/sweep.rs, gc/tenant.rs, \
             rio-store/src/logs/gate.rs, rio-store/src/logs/tail.rs, and \
             rio-store/src/logs/sweep.rs for the dependent queries",
        );
    }
}

/// M_048 regression: M_030's backfill matched only `'completed'`, missing
/// `'skipped'` (M_021). For terminal builds — which never re-fire
/// `persist_build_counts` — the undercount was permanent. M_048 recounts
/// with `IN ('completed','skipped')`, guarded to terminal builds so it
/// doesn't race the live write path on active ones.
///
/// Seeds the post-030/pre-048 undercount directly (TestDb already
/// applied 048 against an empty DB), then re-executes 048 idempotently.
#[tokio::test]
async fn migration_048_recounts_skipped_as_completed() {
    let db = rio_test_support::TestDb::new(&MIGRATOR).await;

    // Two builds: b1 terminal (succeeded) — must be recounted;
    // b2 active — must be left alone (terminal-status guard).
    // Each linked to 3 derivations: completed, completed, skipped.
    // Seeded completed_drvs=2 simulates M_030's undercount.
    let (b1, b2): (sqlx::types::Uuid, sqlx::types::Uuid) = sqlx::query_as(
        r#"
        WITH b AS (
          INSERT INTO builds (status, total_drvs, completed_drvs, cached_drvs)
          VALUES ('succeeded', 3, 2, 2), ('active', 3, 2, 2)
          RETURNING build_id
        ),
        bid AS (SELECT array_agg(build_id) AS ids FROM b),
        d AS (
          INSERT INTO derivations (drv_hash, drv_path, system, status)
          VALUES
            ('h1', '/nix/store/h1-x.drv', 'x86_64-linux', 'completed'),
            ('h2', '/nix/store/h2-x.drv', 'x86_64-linux', 'completed'),
            ('h3', '/nix/store/h3-x.drv', 'x86_64-linux', 'skipped'),
            ('h4', '/nix/store/h4-x.drv', 'x86_64-linux', 'completed'),
            ('h5', '/nix/store/h5-x.drv', 'x86_64-linux', 'completed'),
            ('h6', '/nix/store/h6-x.drv', 'x86_64-linux', 'skipped')
          RETURNING derivation_id
        ),
        did AS (SELECT array_agg(derivation_id) AS ids FROM d),
        bd AS (
          INSERT INTO build_derivations (build_id, derivation_id)
          SELECT (SELECT ids[1] FROM bid), unnest((SELECT ids[1:3] FROM did))
          UNION ALL
          SELECT (SELECT ids[2] FROM bid), unnest((SELECT ids[4:6] FROM did))
        )
        SELECT ids[1], ids[2] FROM bid
        "#,
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();

    // Re-execute 048 idempotently against the seeded data. raw_sql:
    // file has a comment line + statement, `query` would treat it as
    // a single prepared statement and choke on the leading comment in
    // some PG configs — raw_sql sends it as a simple-query batch.
    sqlx::raw_sql(include_str!(
        "../migrations/048_builds_denorm_recount_skipped.sql"
    ))
    .execute(&db.pool)
    .await
    .unwrap();

    let row = |b| {
        sqlx::query_as::<_, (i32, i32)>(
            "SELECT completed_drvs, cached_drvs FROM builds WHERE build_id = $1",
        )
        .bind(b)
    };
    // b1 terminal → recounted: skipped now counts.
    assert_eq!(row(b1).fetch_one(&db.pool).await.unwrap(), (3, 3));
    // b2 active → guard skipped it: seeded undercount preserved (live
    // path owns active builds; 048 must not race it).
    assert_eq!(row(b2).fetch_one(&db.pool).await.unwrap(), (2, 2));
}

// ─────────────────────────────────────────────────────────────────────────
// Migration 078 schema contract (substitution-replacement Phase A).
//
// Pins the structural properties the materialization-job machinery
// depends on: the partial-unique dedup index, the state CHECK alphabet,
// wanted-relation PK isolation, the derived interest view's liveness
// filter, and — the dormancy half — the DEFAULT values that keep every
// existing writer (the build pull mint, pin_live_inputs) untouched.
// ─────────────────────────────────────────────────────────────────────────

/// 078: at most one unresolved (state='pending') job per derivation,
/// enforced by the `materialization_jobs_unresolved` partial-unique
/// index. Resolved jobs leave the index, so a new pending job for the
/// same derivation inserts cleanly afterwards.
#[tokio::test]
async fn materialization_jobs_unresolved_dedup() {
    let db = rio_test_support::TestDb::new(&MIGRATOR).await;

    let drv = uuid::Uuid::now_v7();
    let job1 = uuid::Uuid::now_v7();
    let insert = "INSERT INTO materialization_jobs \
                  (job_id, derivation_id, drv_hash, origin, created_generation) \
                  VALUES ($1, $2, 'h-dedup', 'cache_opportunity', 1)";

    sqlx::query(insert)
        .bind(job1)
        .bind(drv)
        .execute(&db.pool)
        .await
        .expect("first pending job inserts");

    // Second pending job for the same derivation: rejected by the
    // partial-unique index (the database-enforced C3-class dedup).
    let err = sqlx::query(insert)
        .bind(uuid::Uuid::now_v7())
        .bind(drv)
        .execute(&db.pool)
        .await
        .expect_err("second pending job for one derivation must violate the dedup index");
    assert!(
        err.to_string().contains("materialization_jobs_unresolved"),
        "expected materialization_jobs_unresolved violation, got: {err}"
    );

    // Resolve the first job: it leaves the partial index.
    sqlx::query(
        "UPDATE materialization_jobs \
         SET state = 'resolved_success', resolved_at = now() WHERE job_id = $1",
    )
    .bind(job1)
    .execute(&db.pool)
    .await
    .unwrap();

    // A new pending job for the same derivation now inserts cleanly.
    sqlx::query(insert)
        .bind(uuid::Uuid::now_v7())
        .bind(drv)
        .execute(&db.pool)
        .await
        .expect("pending job after resolution must insert (the index covers pending only)");
}

/// 078: the `state` CHECK rejects literals outside the job state-machine
/// alphabet ("claimed" is deliberately NOT a job state — a claim is an
/// open attempt, not a job-row mutation).
#[tokio::test]
async fn materialization_jobs_state_alphabet() {
    let db = rio_test_support::TestDb::new(&MIGRATOR).await;

    let err = sqlx::query(
        "INSERT INTO materialization_jobs \
         (job_id, derivation_id, drv_hash, origin, state, created_generation) \
         VALUES ($1, $2, 'h-alpha', 'pruned', 'claimed', 1)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(uuid::Uuid::now_v7())
    .execute(&db.pool)
    .await
    .expect_err("unknown state literal must fail the CHECK");
    assert!(
        err.to_string().contains("materialization_jobs_state_check"),
        "expected materialization_jobs_state_check violation, got: {err}"
    );

    // Positive control: a valid non-default state inserts cleanly.
    sqlx::query(
        "INSERT INTO materialization_jobs \
         (job_id, derivation_id, drv_hash, origin, state, created_generation) \
         VALUES ($1, $2, 'h-alpha2', 'pruned', 'cancelled', 1)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(uuid::Uuid::now_v7())
    .execute(&db.pool)
    .await
    .expect("valid state literal must pass the CHECK");
}

/// 078: two builds' wanted rows for one derivation coexist (PK is
/// (build_id, derivation_id)); upserting one build's contribution
/// replaces only that build's row, never another's.
#[tokio::test]
async fn build_wanted_outputs_pk_isolation() {
    let db = rio_test_support::TestDb::new(&MIGRATOR).await;

    let drv = uuid::Uuid::now_v7();
    let (b1, b2): (sqlx::types::Uuid, sqlx::types::Uuid) = sqlx::query_as(
        "WITH b AS (
            INSERT INTO builds (status) VALUES ('active'), ('active') RETURNING build_id
         ), ids AS (SELECT array_agg(build_id) AS a FROM b)
         SELECT a[1], a[2] FROM ids",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();

    // The production upsert shape (db/wanted.rs since 086): saturating
    // union on conflict — either side '{}' saturates, else sorted
    // distinct union. PK isolation across builds is what this pins.
    let upsert = "INSERT INTO build_wanted_outputs \
                  (build_id, derivation_id, wanted_output_names) VALUES ($1, $2, $3) \
                  ON CONFLICT (build_id, derivation_id) DO UPDATE \
                  SET wanted_output_names = CASE \
                          WHEN build_wanted_outputs.wanted_output_names = '{}'::text[] \
                            OR EXCLUDED.wanted_output_names = '{}'::text[] THEN '{}'::text[] \
                          ELSE ARRAY(SELECT DISTINCT x \
                                       FROM UNNEST(build_wanted_outputs.wanted_output_names \
                                                   || EXCLUDED.wanted_output_names) AS t(x) \
                                      ORDER BY x) \
                      END, \
                      recorded_at = now()";

    // Both builds contribute a row for the same derivation.
    sqlx::query(upsert)
        .bind(b1)
        .bind(drv)
        .bind(vec!["out".to_string()])
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query(upsert)
        .bind(b2)
        .bind(drv)
        .bind(vec!["dev".to_string()])
        .execute(&db.pool)
        .await
        .unwrap();

    // Re-record b1 with a different wanted set: replaces b1's row only.
    sqlx::query(upsert)
        .bind(b1)
        .bind(drv)
        .bind(vec!["out".to_string(), "lib".to_string()])
        .execute(&db.pool)
        .await
        .unwrap();

    let rows: Vec<(sqlx::types::Uuid, Vec<String>)> = sqlx::query_as(
        "SELECT build_id, wanted_output_names FROM build_wanted_outputs \
         WHERE derivation_id = $1",
    )
    .bind(drv)
    .fetch_all(&db.pool)
    .await
    .unwrap();

    assert_eq!(rows.len(), 2, "both builds' rows coexist");
    let names_of = |b: sqlx::types::Uuid| {
        rows.iter()
            .find(|(rb, _)| *rb == b)
            .map(|(_, n)| n.clone())
            .expect("row present")
    };
    assert_eq!(
        names_of(b1),
        vec!["lib".to_string(), "out".to_string()],
        "b1's upsert unioned into b1's row (sorted, distinct)"
    );
    assert_eq!(
        names_of(b2),
        vec!["dev".to_string()],
        "b2's row untouched by b1's upsert (PK isolation)"
    );
}

/// 078: the `materialization_interest` view derives interest from the
/// live-build join — rows appear only for builds with status
/// pending/active, and flipping a build terminal drops its row without
/// touching any wanted/job row.
#[tokio::test]
async fn materialization_interest_view_liveness() {
    let db = rio_test_support::TestDb::new(&MIGRATOR).await;

    let drv: sqlx::types::Uuid = sqlx::query_scalar(
        "INSERT INTO derivations \
             (drv_hash, drv_path, system, status, output_names, expected_output_paths) \
         VALUES ('h-interest', '/nix/store/h-interest.drv', 'x', 'ready', '{out}', '{/nix/store/x-out}') \
         RETURNING derivation_id",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let job = uuid::Uuid::now_v7();

    sqlx::query(
        "INSERT INTO materialization_jobs \
         (job_id, derivation_id, drv_hash, origin, created_generation) \
         VALUES ($1, $2, 'h-interest', 'pruned', 1)",
    )
    .bind(job)
    .bind(drv)
    .execute(&db.pool)
    .await
    .unwrap();

    // Three builds — active, pending, succeeded — each with a wanted row.
    let (b_active, b_pending, b_done): (sqlx::types::Uuid, sqlx::types::Uuid, sqlx::types::Uuid) =
        sqlx::query_as(
            "WITH b AS (
            INSERT INTO builds (status)
            VALUES ('active'), ('pending'), ('succeeded') RETURNING build_id
         ), ids AS (SELECT array_agg(build_id) AS a FROM b)
         SELECT a[1], a[2], a[3] FROM ids",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();

    for b in [b_active, b_pending, b_done] {
        // 086: interest derives from MEMBERSHIP; the wanted row only
        // narrows the width.
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(b)
            .bind(drv)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO build_wanted_outputs (build_id, derivation_id) VALUES ($1, $2)")
            .bind(b)
            .bind(drv)
            .execute(&db.pool)
            .await
            .unwrap();
    }

    let interested = sqlx::query_as::<_, (sqlx::types::Uuid,)>(
        "SELECT build_id FROM materialization_interest WHERE job_id = $1",
    );
    let got: std::collections::HashSet<_> = interested
        .bind(job)
        .fetch_all(&db.pool)
        .await
        .unwrap()
        .into_iter()
        .map(|(b,)| b)
        .collect();
    assert!(
        got.contains(&b_active) && got.contains(&b_pending),
        "live (active/pending) builds are interested"
    );
    assert!(
        !got.contains(&b_done),
        "terminal build must not appear in the interest view"
    );

    // Flip the active build terminal: its interest row disappears.
    sqlx::query("UPDATE builds SET status = 'succeeded' WHERE build_id = $1")
        .bind(b_active)
        .execute(&db.pool)
        .await
        .unwrap();

    let after: std::collections::HashSet<_> = sqlx::query_as::<_, (sqlx::types::Uuid,)>(
        "SELECT build_id FROM materialization_interest WHERE job_id = $1",
    )
    .bind(job)
    .fetch_all(&db.pool)
    .await
    .unwrap()
    .into_iter()
    .map(|(b,)| b)
    .collect();
    assert_eq!(
        after,
        std::collections::HashSet::from([b_pending]),
        "only the still-live build remains interested after the flip"
    );
}

/// 078 dormancy guarantee: an INSERT into `drv_executions` that does not
/// mention `attempt_kind` — i.e. every existing writer, the fenced pull
/// mint — gets 'build'.
#[tokio::test]
async fn attempt_kind_default_is_build() {
    let db = rio_test_support::TestDb::new(&MIGRATOR).await;

    let exec = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO drv_executions (exec_id, drv_hash, executor_id, started_at) \
         VALUES ($1, $2, 'builder-0', now())",
    )
    .bind(exec)
    .bind("a".repeat(32))
    .execute(&db.pool)
    .await
    .unwrap();

    let kind: String =
        sqlx::query_scalar("SELECT attempt_kind FROM drv_executions WHERE exec_id = $1")
            .bind(exec)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(kind, "build", "kind-less INSERT must default to 'build'");
}

/// 078 dormancy guarantee: an INSERT into `scheduler_live_pins` that does
/// not mention `pin_kind` — i.e. the as-built `pin_live_inputs` writer —
/// gets 'build_input' and a NULL job_id.
#[tokio::test]
async fn live_pins_kind_default_is_build_input() {
    let db = rio_test_support::TestDb::new(&MIGRATOR).await;

    sqlx::query(
        "INSERT INTO scheduler_live_pins (store_path_hash, drv_hash) \
         VALUES ($1, 'h-pin-default')",
    )
    .bind(vec![0xabu8; 20])
    .execute(&db.pool)
    .await
    .unwrap();

    let (kind, job): (String, Option<sqlx::types::Uuid>) = sqlx::query_as(
        "SELECT pin_kind, job_id FROM scheduler_live_pins WHERE drv_hash = 'h-pin-default'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        kind, "build_input",
        "kind-less pin INSERT must default to 'build_input'"
    );
    assert_eq!(job, None, "job_id defaults to NULL for build-input pins");
}

/// Shared grant assertions for the ensure_roles tests: `rio_app`
/// holds full DML on EVERY public table AND sequence, plus default
/// privileges for both object kinds. Idempotent-STATE assertions
/// only — never "role did not exist before": under llvm-cov `cargo
/// test` the PG server is process-shared and `rio_app` is
/// cluster-wide, so a sibling test may have created it already.
async fn assert_rio_app_grants(pool: &sqlx::PgPool) {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p')",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(!tables.is_empty(), "migrated DB must have public tables");
    for t in &tables {
        let ok: bool = sqlx::query_scalar(
            "SELECT has_table_privilege('rio_app', format('%I.%I', 'public', $1::text), \
             'SELECT,INSERT,UPDATE,DELETE')",
        )
        .bind(t)
        .fetch_one(pool)
        .await
        .unwrap();
        assert!(ok, "rio_app lacks DML on table {t}");
    }

    let sequences: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relkind = 'S'",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    for sq in &sequences {
        let ok: bool = sqlx::query_scalar(
            "SELECT has_sequence_privilege('rio_app', format('%I.%I', 'public', $1::text), \
             'USAGE,SELECT,UPDATE')",
        )
        .bind(sq)
        .fetch_one(pool)
        .await
        .unwrap();
        assert!(ok, "rio_app lacks privileges on sequence {sq}");
    }

    // Default privileges registered for BOTH object kinds ('r' tables,
    // 'S' sequences) — a tables-only default leaves serial/identity
    // sequences permission-denied on the first post-migration insert.
    for objtype in ["r", "S"] {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_default_acl d \
             JOIN pg_namespace n ON n.oid = d.defaclnamespace \
             WHERE n.nspname = 'public' AND d.defaclobjtype = $1::\"char\" \
               AND array_to_string(d.defaclacl, ',') LIKE '%rio_app%')",
        )
        .bind(objtype)
        .fetch_one(pool)
        .await
        .unwrap();
        assert!(ok, "no default privileges for rio_app on objtype {objtype}");
    }
}

/// ensure_roles creates `rio_app` with full grants and is idempotent
/// (the second run must change nothing and fail nothing). Exercised
/// through the lock-holding combined entry point — production never
/// calls ensure_roles bare, and the advisory lock is what serializes
/// the cluster-wide role DDL under the shared-server test topology.
/// Also pins the superuser-runner behavior: on non-RDS (no rds_iam
/// role) the role+grants ARE created — intentional, not accidental.
// r[verify store.db.ensure-roles]
#[tokio::test]
async fn ensure_roles_creates_and_is_idempotent() {
    let db = rio_test_support::TestDb::new(&MIGRATOR).await;

    rio_migrations::migrate::run_with_roles(&db.pool, rio_migrations::migrator())
        .await
        .expect("first run_with_roles");
    assert_rio_app_grants(&db.pool).await;

    // Idempotent: second pass over the converged state.
    rio_migrations::migrate::run_with_roles(&db.pool, rio_migrations::migrator())
        .await
        .expect("second run_with_roles must be a clean no-op");
    assert_rio_app_grants(&db.pool).await;

    let login: bool =
        sqlx::query_scalar("SELECT rolcanlogin FROM pg_roles WHERE rolname = 'rio_app'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(login, "rio_app must have LOGIN");
}

/// Regression for the live REASSIGN ACL-strip incident: replay the
/// retired role migrations' bodies (069 ownership transfer + 070
/// REASSIGN-detach — kept as fixtures; PG >= 16 required for their
/// pg_has_role(..., 'SET')), confirm the strip occurred, then assert
/// ensure_roles re-grants everything on tables AND sequences.
// r[verify store.db.ensure-roles]
#[tokio::test]
async fn ensure_roles_regrants_after_legacy_acl_strip() {
    let db = rio_test_support::TestDb::new(&MIGRATOR).await;

    sqlx::raw_sql(include_str!("fixtures/legacy_069_rio_app_role.sql"))
        .execute(&db.pool)
        .await
        .expect("replay legacy 069");
    sqlx::raw_sql(include_str!(
        "fixtures/legacy_070_master_detach_rio_app.sql"
    ))
    .execute(&db.pool)
    .await
    .expect("replay legacy 070");

    // Precondition: the replayed REASSIGN really stripped rio_app's
    // privileges (the live symptom was `permission denied for table
    // _sqlx_migrations` in every app pod). Without this assert the
    // test would pass vacuously if PG's REASSIGN semantics changed.
    let stripped: bool = sqlx::query_scalar(
        "SELECT NOT has_table_privilege('rio_app', '_sqlx_migrations', 'SELECT')",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(
        stripped,
        "fixture replay did not reproduce the ACL strip — fixture or PG semantics drifted"
    );

    rio_migrations::migrate::run_with_roles(&db.pool, rio_migrations::migrator())
        .await
        .expect("run_with_roles over the stripped database");
    assert_rio_app_grants(&db.pool).await;
}

/// The k3s contract: an unprivileged runner (bitnami app user — no
/// CREATEROLE) must degrade to a WARNING, not a failure. A bare
/// CREATE ROLE here once crash-looped every store/scheduler pod on
/// k3s. Called bare (not via run_with_roles): the migrator cannot run
/// as this user at all, and the unprivileged path performs no DDL —
/// nothing to serialize.
#[tokio::test]
async fn ensure_roles_unprivileged_user_degrades_to_warning() {
    use sqlx::ConnectOptions as _;

    let db = rio_test_support::TestDb::new(&MIGRATOR).await;

    // Unique name: roles are cluster-wide and the llvm-cov topology
    // shares one server across tests.
    let role = format!(
        "rio_test_limited_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE ROLE \"{role}\" WITH LOGIN"
    )))
    .execute(&db.pool)
    .await
    .unwrap();

    let opts = (*db.pool.connect_options()).clone().username(&role);
    let mut conn = opts.connect().await.expect("connect as limited role");
    rio_migrations::ensure_roles::ensure_roles(&mut conn)
        .await
        .expect("unprivileged ensure_roles must warn, not fail");
    drop(conn);

    sqlx::query(sqlx::AssertSqlSafe(format!("DROP ROLE \"{role}\"")))
        .execute(&db.pool)
        .await
        .unwrap();
}

/// Rollback / un-embedding regression: `migrate::run` and
/// `assert_current` both tolerate an applied row whose version is not
/// in the embedded set. This is the documented binary-rollback path
/// (previous binary against a newer schema). Without
/// `ignore_missing(true)` sqlx fails `VersionMissing` before applying
/// anything.
#[tokio::test]
async fn migrate_run_tolerates_applied_but_not_embedded_rows() {
    let db = rio_test_support::TestDb::new(&MIGRATOR).await;

    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, installed_on, success, checksum, execution_time) \
         VALUES (9999, 'from-a-newer-binary', now(), true, '\\x00'::bytea, 0)",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    rio_migrations::migrate::run(&db.pool, rio_migrations::migrator())
        .await
        .expect("run must ignore the un-embedded applied row");
    rio_migrations::migrate::assert_current(&db.pool)
        .await
        .expect("assert_current accepts applied-but-not-embedded");
}

/// The rds_iam branch: where the role exists (RDS/Aurora — simulated
/// here by creating it), ensure_roles must grant it to rio_app. That
/// membership is the entire IAM-auth switch — without it RDS PAM
/// rejects rio_app's token and every app pod fails auth.
#[tokio::test]
async fn ensure_roles_grants_rds_iam_membership_where_role_exists() {
    let db = rio_test_support::TestDb::new(&MIGRATOR).await;

    // Cluster-wide role; IF NOT EXISTS shape because the llvm-cov
    // topology shares one server across tests. NOLOGIN like the real
    // rds_iam. Never dropped — other tests' ensure_roles passes then
    // take the grant path too, which is idempotent.
    sqlx::raw_sql(
        "DO $$ BEGIN \
           IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'rds_iam') THEN \
             CREATE ROLE rds_iam NOLOGIN; \
           END IF; \
         END $$",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    rio_migrations::migrate::run_with_roles(&db.pool, rio_migrations::migrator())
        .await
        .expect("run_with_roles with rds_iam present");

    let member: bool = sqlx::query_scalar(
        "SELECT EXISTS (
           SELECT 1 FROM pg_auth_members m
           JOIN pg_roles g ON g.oid = m.roleid
           JOIN pg_roles mem ON mem.oid = m.member
           WHERE g.rolname = 'rds_iam' AND mem.rolname = 'rio_app')",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(
        member,
        "ensure_roles must grant rds_iam to rio_app where the role exists"
    );
}

/// The defensive-detach branch: a legacy database where the runner's
/// user holds an INHERITING rio_app membership (the incident-1 shape —
/// inherited rds_iam made RDS PAM reject the master's password). The
/// ACL-strip replay test does NOT reach this branch (its superuser
/// replay short-circuits the legacy grant), so pin it directly:
/// ensure_roles must REASSIGN+REVOKE the membership and re-assert
/// grants in the same run.
// r[verify store.db.ensure-roles]
#[tokio::test]
async fn ensure_roles_detaches_inheriting_master_membership() {
    let db = rio_test_support::TestDb::new(&MIGRATOR).await;

    // First pass creates rio_app + grants.
    rio_migrations::migrate::run_with_roles(&db.pool, rio_migrations::migrator())
        .await
        .expect("initial run_with_roles");

    // Reproduce the legacy grant (explicit, INHERIT — the default).
    sqlx::query("GRANT rio_app TO CURRENT_USER")
        .execute(&db.pool)
        .await
        .unwrap();
    let inheriting = "SELECT EXISTS (
           SELECT 1 FROM pg_auth_members m
           JOIN pg_roles g ON g.oid = m.roleid
           JOIN pg_roles mem ON mem.oid = m.member
           WHERE g.rolname = 'rio_app' AND mem.rolname = current_user
             AND m.inherit_option)";
    let before: bool = sqlx::query_scalar(inheriting)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(before, "test setup must create an inheriting membership");

    rio_migrations::migrate::run_with_roles(&db.pool, rio_migrations::migrator())
        .await
        .expect("run_with_roles over the legacy membership");

    let after: bool = sqlx::query_scalar(inheriting)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(
        !after,
        "ensure_roles must detach the inheriting master membership (RDS PAM \
         treats inherited rds_iam as IAM-only and rejects the password)"
    );
    // The detach's REASSIGN rewrites owner ACLs — the same run must
    // have re-asserted every grant.
    assert_rio_app_grants(&db.pool).await;
}
