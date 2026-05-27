//! Local eval-set artifact directory layout and JSON/JSONL writers.
//!
//! One directory per eval set, holding the same files (and filenames)
//! the S3 prefix layout uses, so a local build and an uploaded build
//! are byte-for-byte comparable.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// Filenames inside an eval-set prefix (local dir and S3 alike).
pub const MANIFEST_FILE: &str = "manifest.jsonl";
pub const EVAL_ERRORS_FILE: &str = "eval-errors.jsonl";
pub const FIDELITY_FILE: &str = "fidelity.json";
pub const DEP_CLOSURE_FILE: &str = "dep-closure.jsonl";
pub const DRVS_ARCHIVE_FILE: &str = "drvs.tar.zst";
pub const EVALSET_FILE: &str = "evalset.json";

/// Local output directory for one eval set.
#[derive(Debug, Clone)]
pub struct EvalSetDir {
    pub root: PathBuf,
}

impl EvalSetDir {
    pub fn create(root: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    pub fn path(&self, file: &str) -> PathBuf {
        self.root.join(file)
    }

    /// Write a JSONL file (one serialized record per line). Any
    /// existing file at that path is overwritten.
    pub fn write_jsonl<T: serde::Serialize>(
        &self,
        file: &str,
        records: &[T],
    ) -> anyhow::Result<PathBuf> {
        use std::io::Write as _;
        let path = self.path(file);
        let mut w = std::io::BufWriter::new(
            std::fs::File::create(&path).with_context(|| format!("create {}", path.display()))?,
        );
        for rec in records {
            serde_json::to_writer(&mut w, rec).context("serialize record")?;
            w.write_all(b"\n").context("write newline")?;
        }
        w.flush().context("flush")?;
        Ok(path)
    }

    /// Write a pretty-printed JSON file. Any existing file at that
    /// path is overwritten.
    pub fn write_json<T: serde::Serialize>(
        &self,
        file: &str,
        value: &T,
    ) -> anyhow::Result<PathBuf> {
        let path = self.path(file);
        let body = serde_json::to_vec_pretty(value).context("serialize json")?;
        std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evalset::evaluator::EvalErrorRecord;

    #[test]
    fn writes_jsonl_one_record_per_line() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = EvalSetDir::create(&tmp.path().join("set")).unwrap();
        let records = vec![
            EvalErrorRecord {
                attr: "a".into(),
                error: "boom".into(),
            },
            EvalErrorRecord {
                attr: "b".into(),
                error: "bang".into(),
            },
        ];
        let path = dir.write_jsonl(EVAL_ERRORS_FILE, &records).unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let back: EvalErrorRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(back, records[0]);
    }

    #[test]
    fn writes_json_files_under_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = EvalSetDir::create(tmp.path()).unwrap();
        let path = dir
            .write_json(FIDELITY_FILE, &serde_json::json!({"divergent": false}))
            .unwrap();
        assert!(path.starts_with(tmp.path()));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(v["divergent"], false);
    }
}
