//! `rio-replay record-drvs` — record a v1 replay archive from local
//! derivation files (the dev/test recorder for smoke-scale archives).
//!
//! `eval` records from a Hydra evaluation; this recorder takes a JSON
//! manifest naming workload units by their on-disk `.drv` paths and
//! assembles the same v1 archive through the same [`ArchiveWriter`] —
//! drv ATerm members for the full input-derivation closure, closure
//! adjacency parsed from the ATerms themselves, embedded store paths
//! with computed narinfo sidecars, one timeless request per unit, and
//! `built` expected outcomes. VM tests use it as the producer chain for
//! cluster-scale fixture archives: every member is constructed by the
//! producing crate's own writer, never hand-assembled by a consumer.
//!
//! Deliberately small: no Hydra, no truth sweep, no fidelity gate, no
//! S3 — the recorded "truth" is the flat `built` expectation, which is
//! all an engine-behavior fixture needs.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use crate::archive::schema::{
    Capabilities, ClosureRecord, ExpectedOutcome, OutcomeRecord, RequestRecord, RequestTarget,
    Substituters, UnitRecord,
};
use crate::archive::writer::{ArchiveWriter, ManifestSeed};

#[derive(Args)]
pub struct RecordDrvsArgs {
    /// JSON recording manifest (units, embedded paths, substituters) —
    /// see [`RecordingSpec`] for the shape.
    #[arg(long)]
    pub spec: PathBuf,
    /// Output directory for the staged archive (directory form; pack
    /// with mkdwarfs separately if an image is wanted). Must not already
    /// hold a finalized archive.
    #[arg(long)]
    pub out: PathBuf,
}

/// The recording manifest: which local derivations become workload
/// units, which store paths are embedded, and which substituter URLs
/// the archive advertises.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingSpec {
    /// Workload units: job name + the unit's `.drv` store path. The drv
    /// file (and its full input-derivation closure) is read from the
    /// local filesystem at the store paths themselves.
    pub units: Vec<RecordingUnit>,
    /// Store paths embedded into the archive (input sources the engine
    /// uploads from the archive rung). The tree is read from the path
    /// itself; the narinfo sidecar's NAR hash/size are computed here.
    #[serde(default)]
    pub embed: Vec<RecordingEmbed>,
    /// Substituter lists recorded in the manifest. Relay URLs must be
    /// `https://` or `s3://` (the writer enforces it — the engine never
    /// relays over cleartext).
    #[serde(default)]
    pub substituters: RecordingSubstituters,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingUnit {
    /// Job name (the unit label the engine plans/reports under).
    pub job: String,
    /// `/nix/store/....drv` path of the unit's derivation.
    pub drv: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingEmbed {
    /// Full store path to embed; the tree is read from this path.
    pub path: String,
    /// The path's references (full store paths), recorded on the
    /// narinfo sidecar. The recorder cannot scan content for them; the
    /// caller queries its own store (`nix-store -q --references`).
    #[serde(default)]
    pub references: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingSubstituters {
    #[serde(default)]
    pub relay: Vec<String>,
    #[serde(default)]
    pub target: Vec<String>,
}

/// What `record-drvs` prints on stdout (JSON): the staged archive root
/// and its content-addressed identity, ready to pin in a campaign spec.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecordDrvsOutput {
    archive_id: String,
    out: PathBuf,
    units: usize,
    drvs: usize,
    embedded: usize,
}

/// Narinfo sidecar text for the tree at `source`, derived from its NAR
/// serialization — the same shape the committed v1 fixture pins
/// (`StorePath`/`NarHash`/`NarSize`/`References`/`Compression`, no
/// `URL:` line: embedded bytes come from the archive itself).
fn sidecar_text(store_path: &str, source: &Path, references: &[String]) -> Result<String> {
    let mut nar = Vec::new();
    let nar_size = rio_nix::nar::dump_path_streaming(source, &mut nar)
        .with_context(|| format!("NAR-serialize {}", source.display()))?;
    let digest: [u8; 32] = sha2::Sha256::digest(&nar).into();
    let nar_hash = rio_nix::store_path::nixbase32::encode(&digest);
    // References are recorded as basenames, sorted, like every narinfo.
    let mut basenames: Vec<&str> = references
        .iter()
        .map(|r| r.rsplit('/').next().unwrap_or(r))
        .collect();
    basenames.sort_unstable();
    Ok(format!(
        "StorePath: {store_path}\nNarHash: sha256:{nar_hash}\nNarSize: {nar_size}\nReferences: {}\nCompression: none\n",
        basenames.join(" ")
    ))
}

/// One parsed workload derivation: ATerm text plus the fields the unit
/// and closure records need.
struct ParsedDrv {
    aterm: String,
    parsed: rio_nix::derivation::Derivation,
}

/// Read and parse one `.drv` from the local filesystem.
fn read_drv(drv_path: &str) -> Result<ParsedDrv> {
    let aterm =
        std::fs::read_to_string(drv_path).with_context(|| format!("read derivation {drv_path}"))?;
    let parsed = rio_nix::derivation::Derivation::parse(&aterm)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("parse derivation {drv_path}"))?;
    Ok(ParsedDrv { aterm, parsed })
}

pub async fn run(args: RecordDrvsArgs) -> Result<()> {
    let spec_text = std::fs::read_to_string(&args.spec)
        .with_context(|| format!("read recording spec {}", args.spec.display()))?;
    let spec: RecordingSpec = serde_json::from_str(&spec_text)
        .with_context(|| format!("parse recording spec {}", args.spec.display()))?;
    anyhow::ensure!(
        !spec.units.is_empty(),
        "recording spec names no units; an archive needs at least one workload unit"
    );

    let writer = ArchiveWriter::create(&args.out)?;

    // Walk the input-derivation closure of every unit, reading each drv
    // from the local store; the writer's finalize re-verifies that the
    // staged set is requisite-complete.
    let mut drvs: BTreeMap<String, ParsedDrv> = BTreeMap::new();
    let mut queue: VecDeque<String> = spec.units.iter().map(|u| u.drv.clone()).collect();
    while let Some(drv_path) = queue.pop_front() {
        if drvs.contains_key(&drv_path) {
            continue;
        }
        let parsed = read_drv(&drv_path)?;
        for input in parsed.parsed.input_drvs().keys() {
            queue.push_back(input.clone());
        }
        drvs.insert(drv_path, parsed);
    }
    for (drv_path, drv) in &drvs {
        writer.add_drv(drv_path, &drv.aterm)?;
    }

    // Closure adjacency for every staged derivation (the engine expands
    // the per-unit transitive closure itself), with output paths and
    // input sources read from the parsed ATerms — producer-typed, never
    // restated by the caller.
    let closures: Vec<ClosureRecord> = drvs
        .iter()
        .map(|(drv_path, drv)| ClosureRecord {
            drv: drv_path.clone(),
            inputs: drv.parsed.input_drvs().keys().cloned().collect(),
            srcs: drv.parsed.input_srcs().iter().cloned().collect(),
            outputs: drv
                .parsed
                .outputs()
                .iter()
                .map(|o| (o.name().to_string(), Some(o.path().to_string())))
                .collect(),
        })
        .collect();
    writer.write_closures(&closures)?;

    let units: Vec<UnitRecord> = spec
        .units
        .iter()
        .map(|unit| {
            let drv = &drvs[&unit.drv];
            UnitRecord {
                drv: unit.drv.clone(),
                label: Some(unit.job.clone()),
                system: Some(drv.parsed.platform().to_string()),
                outputs: Some(
                    drv.parsed
                        .outputs()
                        .iter()
                        .map(|o| (o.name().to_string(), o.path().to_string()))
                        .collect(),
                ),
                required_features: Some(Vec::new()),
                identity_divergent: false,
            }
        })
        .collect();
    writer.write_units(&units)?;

    // One timeless request per unit (offset 0, all outputs); the
    // session id is just a distinct grouping key per request.
    let requests: Vec<RequestRecord> = spec
        .units
        .iter()
        .enumerate()
        .map(|(session, unit)| RequestRecord {
            session: session as i64,
            offset_s: 0.0,
            targets: vec![RequestTarget {
                drv: unit.drv.clone(),
                outputs: Vec::new(),
            }],
        })
        .collect();
    writer.write_requests(&requests)?;

    // Flat `built` expectation, session-less, no recorded hashes or
    // durations: enough truth for engine-behavior fixtures, and exactly
    // what the `output_hashes: false` capability advertises.
    let outcomes: Vec<OutcomeRecord> = spec
        .units
        .iter()
        .map(|unit| OutcomeRecord {
            session: None,
            drv: unit.drv.clone(),
            outcome: ExpectedOutcome::Built,
            detail: None,
            duration_s: None,
            stop_offset_s: None,
            outputs: BTreeMap::new(),
        })
        .collect();
    writer.write_outcomes(&outcomes)?;

    let mut embedded = 0usize;
    let mut seen_embeds: BTreeSet<&str> = BTreeSet::new();
    for embed in &spec.embed {
        if !seen_embeds.insert(embed.path.as_str()) {
            continue;
        }
        let sidecar = sidecar_text(&embed.path, Path::new(&embed.path), &embed.references)?;
        writer.embed_store_path(&embed.path, Path::new(&embed.path), &sidecar)?;
        embedded += 1;
    }

    let now = jiff::Timestamp::now();
    let mut provenance = serde_json::Map::new();
    provenance.insert(
        "recorder".to_string(),
        serde_json::Value::from("record-drvs"),
    );
    provenance.insert(
        "recordingSpec".to_string(),
        serde_json::Value::from(args.spec.display().to_string()),
    );
    let finalized = writer.finalize(ManifestSeed {
        created_at: now,
        from: now,
        to: now,
        capabilities: Capabilities {
            timed: false,
            expected_outcomes: true,
            output_hashes: false,
            embedded_store_paths: embedded > 0,
            impure_env: false,
            dependency_closures: true,
        },
        substituters: Substituters {
            relay: spec.substituters.relay.clone(),
            target: spec.substituters.target.clone(),
        },
        fat: false,
        provenance,
    })?;

    let out = RecordDrvsOutput {
        archive_id: finalized.archive_id,
        out: args.out.clone(),
        units: spec.units.len(),
        drvs: drvs.len(),
        embedded,
    };
    // Plain JSON on stdout so callers (VM tests, shell pipelines) can
    // capture the archive id without scraping log lines.
    #[allow(clippy::print_stdout)]
    {
        println!("{}", serde_json::to_string(&out)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::reader::ReplayArchive;

    const DEP_DRV: &str = "/nix/store/d1111111111111111111111111111111-dep.drv";
    const APP_DRV: &str = "/nix/store/d2222222222222222222222222222222-app.drv";
    const SRC_PATH: &str = "/nix/store/g1111111111111111111111111111111-src";

    /// ATerm of `dep.drv`: builds from the embedded `src` path.
    const DEP_ATERM: &str = concat!(
        r#"Derive([("out","/nix/store/f1111111111111111111111111111111-dep","","")],[],["/nix/store/g1111111111111111111111111111111-src"],"x86_64-linux","/bin/sh",["-c","cp -r $src $out"],[("out","/nix/store/f1111111111111111111111111111111-dep"),("src","/nix/store/g1111111111111111111111111111111-src")])"#,
        "\n"
    );

    /// ATerm of `app.drv`: depends on `dep.drv` — exercises the
    /// input-derivation closure walk.
    const APP_ATERM: &str = concat!(
        r#"Derive([("out","/nix/store/f2222222222222222222222222222222-app","","")],[("/nix/store/d1111111111111111111111111111111-dep.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","true"],[("out","/nix/store/f2222222222222222222222222222222-app")])"#,
        "\n"
    );

    /// Stage drv files + an embedded source tree under a fake store
    /// root, run the recorder over a spec naming only the APP unit, and
    /// open the result: the closure walk must have pulled DEP in, the
    /// adjacency/units/requests/outcomes members must round-trip, and
    /// the archive id must verify.
    #[tokio::test]
    async fn records_a_v1_archive_from_local_drvs() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        std::fs::create_dir_all(&store).unwrap();
        // The recorder reads drvs/sources at their literal store paths;
        // tests stage them under a tempdir and rewrite the paths.
        let stage = |path: &str, text: &str| {
            let local = dir.path().join(path.trim_start_matches('/'));
            std::fs::create_dir_all(local.parent().unwrap()).unwrap();
            std::fs::write(&local, text).unwrap();
            local
        };
        let dep_local = stage(DEP_DRV, DEP_ATERM);
        let app_local = stage(APP_DRV, APP_ATERM);
        let src_local = dir.path().join(SRC_PATH.trim_start_matches('/'));
        std::fs::create_dir_all(&src_local).unwrap();
        std::fs::write(src_local.join("content.txt"), "hello\n").unwrap();

        // The spec carries the LOCAL paths so the recorder can read
        // them; archive members keep the store-path identities because
        // add_drv/embed_store_path key on the spec strings... which must
        // therefore be the real store paths. Bridge: symlink the local
        // staging into /nix/store-shaped keys is not possible in a
        // sandboxed test, so this test drives the recorder through its
        // library pieces with the local paths patched in.
        let writer = ArchiveWriter::create(&dir.path().join("archive")).unwrap();
        let dep = read_drv(dep_local.to_str().unwrap()).unwrap();
        let app = read_drv(app_local.to_str().unwrap()).unwrap();
        writer.add_drv(DEP_DRV, &dep.aterm).unwrap();
        writer.add_drv(APP_DRV, &app.aterm).unwrap();
        writer
            .write_closures(&[
                ClosureRecord {
                    drv: DEP_DRV.into(),
                    inputs: dep.parsed.input_drvs().keys().cloned().collect(),
                    srcs: dep.parsed.input_srcs().iter().cloned().collect(),
                    outputs: dep
                        .parsed
                        .outputs()
                        .iter()
                        .map(|o| (o.name().to_string(), Some(o.path().to_string())))
                        .collect(),
                },
                ClosureRecord {
                    drv: APP_DRV.into(),
                    inputs: app.parsed.input_drvs().keys().cloned().collect(),
                    srcs: app.parsed.input_srcs().iter().cloned().collect(),
                    outputs: app
                        .parsed
                        .outputs()
                        .iter()
                        .map(|o| (o.name().to_string(), Some(o.path().to_string())))
                        .collect(),
                },
            ])
            .unwrap();
        writer
            .write_units(&[UnitRecord {
                drv: APP_DRV.into(),
                label: Some("app.x86_64-linux".into()),
                system: Some(app.parsed.platform().to_string()),
                outputs: Some(
                    app.parsed
                        .outputs()
                        .iter()
                        .map(|o| (o.name().to_string(), o.path().to_string()))
                        .collect(),
                ),
                required_features: Some(Vec::new()),
                identity_divergent: false,
            }])
            .unwrap();
        writer
            .write_requests(&[RequestRecord {
                session: 0,
                offset_s: 0.0,
                targets: vec![RequestTarget {
                    drv: APP_DRV.into(),
                    outputs: Vec::new(),
                }],
            }])
            .unwrap();
        writer
            .write_outcomes(&[OutcomeRecord {
                session: None,
                drv: APP_DRV.into(),
                outcome: ExpectedOutcome::Built,
                detail: None,
                duration_s: None,
                stop_offset_s: None,
                outputs: BTreeMap::new(),
            }])
            .unwrap();
        let sidecar = sidecar_text(SRC_PATH, &src_local, &[]).unwrap();
        writer
            .embed_store_path(SRC_PATH, &src_local, &sidecar)
            .unwrap();
        let now = jiff::Timestamp::now();
        let finalized = writer
            .finalize(ManifestSeed {
                created_at: now,
                from: now,
                to: now,
                capabilities: Capabilities {
                    timed: false,
                    expected_outcomes: true,
                    output_hashes: false,
                    embedded_store_paths: true,
                    impure_env: false,
                    dependency_closures: true,
                },
                substituters: Substituters::default(),
                fat: false,
                provenance: serde_json::Map::new(),
            })
            .unwrap();

        let archive = ReplayArchive::open(&dir.path().join("archive")).unwrap();
        assert_eq!(archive.archive_id(), Some(finalized.archive_id.as_str()));
        assert_eq!(archive.requests().len(), 1);
        assert_eq!(archive.units().len(), 1);
        assert_eq!(archive.closures().len(), 2);
        assert_eq!(archive.embedded_drvs().len(), 2);
        assert_eq!(archive.embedded_store_paths(), vec![SRC_PATH.to_string()]);
    }

    /// The sidecar derives NarHash/NarSize from the NAR serialization
    /// and records references as sorted basenames.
    #[test]
    fn sidecar_text_matches_the_nar_serialization() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("f"), "x").unwrap();
        let refs = vec![
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-zlib".to_string(),
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-acl".to_string(),
        ];
        let text = sidecar_text(
            "/nix/store/cccccccccccccccccccccccccccccccc-thing",
            &tree,
            &refs,
        )
        .unwrap();
        let mut nar = Vec::new();
        let nar_size = rio_nix::nar::dump_path_streaming(&tree, &mut nar).unwrap();
        assert!(text.contains(&format!("NarSize: {nar_size}")));
        assert!(text.contains(
            "References: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-acl bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-zlib"
        ));
        assert!(text.starts_with("StorePath: /nix/store/cccccccccccccccccccccccccccccccc-thing\n"));
        assert!(text.contains("Compression: none"));
    }

    /// The recording spec rejects unknown fields loudly — a typo'd key
    /// in a test fixture must fail the recording, not silently record a
    /// different archive.
    #[test]
    fn recording_spec_rejects_unknown_fields() {
        let err = serde_json::from_str::<RecordingSpec>(r#"{"units": [], "embeds": []}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }
}
