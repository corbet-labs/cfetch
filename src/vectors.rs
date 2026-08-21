//! The SHARED vector artifact store.
//!
//! Vectors are a derived artifact of shared CONTENT, not of a host: the same
//! block hashed on any machine embeds to the same vector under the same
//! `(model, dim, precision)`. So they are computed once per storage group and
//! written into the tree — `<brain_root>/state/cfetch/vectors/` — where every
//! host that can reach the tree READS them. Only a host with the embed
//! capability writes. The per-host index.db keeps a cache of the same vectors
//! for query speed; the cache is never the record, and losing it costs a
//! re-read, never an embedding run.
//!
//! Layout, per `(model, dim, precision)`:
//!
//! - `<slug>.bin` — records back to back, no per-record header. Every record
//!   is exactly `dim * precision.width()` bytes, so record i lives at offset
//!   `i * stride` and the offset table is implicit.
//! - `<slug>.idx` — a text header (magic, exact model, dim, precision) then
//!   one content hash per line, line i naming record i.
//!
//! One packed file plus one index beats one file per hash: this corpus has
//! ~20k blocks, and 20k files of ~2 KB would burn an inode each, turn a
//! hydrate into 20k network round trips on an NFS-mounted tree, and drop a
//! 20k-entry directory into a tree people open in Obsidian and rsync. The
//! packed form reads whole in one sequential pass.
//!
//! Crash consistency: the record is appended first, its index line second, so
//! a torn tail can only ever leave an ORPHAN RECORD — never an index line
//! pointing at bytes that are not there. Readers use `min(index lines,
//! records)`; the next writer truncates the orphan away under the lock.

use std::collections::HashSet;
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use rusqlite::Connection;

use crate::config::VectorSpec;
use crate::index;

/// First line of every `.idx` — a file that is not this is not ours.
const MAGIC: &str = "cfetch-vectors v1";

/// Header lines before the hash list: magic, model, dim, precision.
const HEADER_LINES: usize = 4;

/// Filename-safe form of a model id. Model names carry slashes and colons
/// (`sentence-transformers/all-MiniLM-L6-v2`), which are not filenames; the
/// mapping is lossy on purpose (two models CAN collide here), and the exact
/// model string in the `.idx` header is what actually gates a match.
fn slug(spec: &VectorSpec) -> String {
    let model: String = spec
        .model
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    format!("{model}-{}-{}", spec.dim, spec.precision.as_str())
}

/// A `(model, dim, precision)` artifact set on disk, with its hash list
/// loaded. Reading needs nothing else; writing goes through [`VectorWriter`].
pub struct VectorStore {
    dir: PathBuf,
    spec: VectorSpec,
    /// Content hash of record i, in record order.
    hashes: Vec<String>,
    present: HashSet<String>,
}

impl VectorStore {
    /// Opens (never creates) the store for `spec` under a brain root. A store
    /// that does not exist yet reads as empty — a host with no artifacts is a
    /// host with zero coverage, not an error.
    pub fn open(brain_root: &Path, spec: &VectorSpec) -> anyhow::Result<VectorStore> {
        let _ = (brain_root, spec);
        unimplemented!()
    }

    pub fn spec(&self) -> &VectorSpec {
        &self.spec
    }

    /// Bytes one vector occupies.
    fn stride(&self) -> usize {
        self.spec.dim * self.spec.precision.width()
    }

    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    pub fn contains(&self, hash: &str) -> bool {
        self.present.contains(hash)
    }

    /// One vector by content hash, widened to f32.
    pub fn get(&self, hash: &str) -> anyhow::Result<Option<Vec<f32>>> {
        let _ = hash;
        unimplemented!()
    }

    /// Streams the whole store in record order — one sequential read, the
    /// shape a hydrate wants.
    pub fn for_each(
        &self,
        f: impl FnMut(&str, Vec<f32>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let _ = f;
        unimplemented!()
    }

    /// Takes the store's write lock and repairs any torn tail. Only a host
    /// with the embed capability ever calls this; every other host reads.
    pub fn begin_write(&mut self) -> anyhow::Result<VectorWriter<'_>> {
        unimplemented!()
    }
}

/// Exclusive writer: holds the store lock for its whole lifetime, so two
/// embed runs (on one host or two) can never interleave appends.
pub struct VectorWriter<'a> {
    store: &'a mut VectorStore,
    _lock: crate::lockfile::Lock,
    bin: std::fs::File,
    idx: std::fs::File,
}

impl VectorWriter<'_> {
    /// Appends one vector. Returns false when the hash is already stored (the
    /// derive-once contract: an artifact that exists is never recomputed).
    pub fn put(&mut self, hash: &str, vector: &[f32]) -> anyhow::Result<bool> {
        let _ = (hash, vector);
        unimplemented!()
    }

    /// Durably lands everything written so far — called per batch, so an
    /// interrupted run leaves committed work behind for the next one.
    pub fn flush(&mut self) -> anyhow::Result<()> {
        unimplemented!()
    }
}

/// Fills the local index cache from the shared store: every block hash that
/// has no cached vector but IS in the store. Returns how many were imported.
/// This is what lets a second host answer semantically without ever holding
/// an embeddings key.
pub fn hydrate(conn: &Connection, store: &VectorStore) -> anyhow::Result<usize> {
    let _ = (conn, store);
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Precision;

    fn spec(dim: usize, precision: Precision) -> VectorSpec {
        VectorSpec { model: "test-model".into(), dim, precision }
    }

    #[test]
    fn missing_store_reads_as_empty_never_as_an_error() {
        let brain = tempfile::tempdir().unwrap();
        let store = VectorStore::open(brain.path(), &spec(4, Precision::F16)).unwrap();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert!(!store.contains("deadbeef"));
        assert!(store.get("deadbeef").unwrap().is_none());
    }

    #[test]
    fn round_trips_vectors_through_the_shared_tree() {
        let brain = tempfile::tempdir().unwrap();
        let s = spec(4, Precision::F16);
        let mut store = VectorStore::open(brain.path(), &s).unwrap();
        {
            let mut w = store.begin_write().unwrap();
            assert!(w.put("aa", &[1.0, 0.0, 0.0, 0.0]).unwrap());
            assert!(w.put("bb", &[0.0, 1.0, 0.0, 0.0]).unwrap());
            assert!(!w.put("aa", &[9.0, 9.0, 9.0, 9.0]).unwrap(), "already stored: never recomputed");
            w.flush().unwrap();
        }
        assert_eq!(store.len(), 2);

        // A SECOND host: nothing but the tree, no endpoint, no key.
        let reopened = VectorStore::open(brain.path(), &s).unwrap();
        assert_eq!(reopened.len(), 2);
        assert!(reopened.contains("aa") && reopened.contains("bb"));
        assert_eq!(reopened.get("aa").unwrap().unwrap(), vec![1.0, 0.0, 0.0, 0.0]);
        assert_eq!(reopened.get("bb").unwrap().unwrap(), vec![0.0, 1.0, 0.0, 0.0]);
        let mut seen = Vec::new();
        reopened
            .for_each(|hash, v| {
                seen.push((hash.to_string(), v));
                Ok(())
            })
            .unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0, "aa", "record order is index order");
    }

    #[test]
    fn store_files_are_named_by_model_dim_and_precision() {
        let brain = tempfile::tempdir().unwrap();
        let s = VectorSpec { model: "vendor/embed-8b".into(), dim: 8, precision: Precision::F32 };
        let mut store = VectorStore::open(brain.path(), &s).unwrap();
        store.begin_write().unwrap().put("aa", &[1.0; 8]).unwrap();
        let dir = crate::paths::shared_vector_dir(brain.path());
        assert!(dir.join("vendor_embed-8b-8-f32.bin").is_file(), "slug carries model-dim-precision");
        assert!(dir.join("vendor_embed-8b-8-f32.idx").is_file());
        // Derived bytes must never ride into the operator's git history.
        assert!(dir.join(".gitignore").is_file(), "the store ignores itself in git");
    }

    #[test]
    fn a_different_spec_is_a_different_store() {
        let brain = tempfile::tempdir().unwrap();
        let mut a = VectorStore::open(brain.path(), &spec(4, Precision::F16)).unwrap();
        a.begin_write().unwrap().put("aa", &[1.0, 0.0, 0.0, 0.0]).unwrap();
        let b = VectorStore::open(brain.path(), &spec(8, Precision::F16)).unwrap();
        assert!(b.is_empty(), "another dimension is another artifact, never a partial read");
        let c = VectorStore::open(brain.path(), &spec(4, Precision::F32)).unwrap();
        assert!(c.is_empty(), "another precision is another artifact");
    }

    #[test]
    fn a_foreign_model_behind_the_same_slug_is_refused() {
        // Slugging is lossy: "a/b" and "a_b" collide. The exact model string
        // in the header is the real gate — a mismatch is loud, never a silent
        // mix of two models' vectors.
        let brain = tempfile::tempdir().unwrap();
        let mut a = VectorStore::open(
            brain.path(),
            &VectorSpec { model: "a/b".into(), dim: 4, precision: Precision::F16 },
        )
        .unwrap();
        a.begin_write().unwrap().put("aa", &[1.0, 0.0, 0.0, 0.0]).unwrap();
        let err = VectorStore::open(
            brain.path(),
            &VectorSpec { model: "a_b".into(), dim: 4, precision: Precision::F16 },
        )
        .unwrap_err();
        assert!(err.to_string().contains("a/b"), "the stored model is named: {err}");
    }

    #[test]
    fn an_orphan_record_from_a_torn_write_is_truncated_not_misread() {
        let brain = tempfile::tempdir().unwrap();
        let s = spec(4, Precision::F16);
        let mut store = VectorStore::open(brain.path(), &s).unwrap();
        {
            let mut w = store.begin_write().unwrap();
            w.put("aa", &[1.0, 0.0, 0.0, 0.0]).unwrap();
            w.flush().unwrap();
        }
        // Simulate a crash between the record append and the index line.
        let dir = crate::paths::shared_vector_dir(brain.path());
        let bin = dir.join(format!("{}.bin", slug(&s)));
        let mut f = std::fs::OpenOptions::new().append(true).open(&bin).unwrap();
        f.write_all(&[0xffu8; 8]).unwrap();
        drop(f);

        let torn = VectorStore::open(brain.path(), &s).unwrap();
        assert_eq!(torn.len(), 1, "readers see only paired records");

        let mut repaired = VectorStore::open(brain.path(), &s).unwrap();
        {
            let mut w = repaired.begin_write().unwrap();
            w.put("bb", &[0.0, 1.0, 0.0, 0.0]).unwrap();
            w.flush().unwrap();
        }
        let after = VectorStore::open(brain.path(), &s).unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(
            after.get("bb").unwrap().unwrap(),
            vec![0.0, 1.0, 0.0, 0.0],
            "the orphan was truncated, so bb's line names bb's bytes"
        );
    }

    #[test]
    fn hydrate_fills_the_local_cache_from_the_tree() {
        let brain = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(brain.path().join("knowledge")).unwrap();
        std::fs::write(brain.path().join("knowledge/a.md"), "- one\n- two\n").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = index::open(state.path()).unwrap();
        index::scan(&mut conn, brain.path(), None, &crate::config::RingRules::default()).unwrap();

        let s = spec(2, Precision::F16);
        let mut store = VectorStore::open(brain.path(), &s).unwrap();
        let missing = index::hashes_without_vectors(&conn, &s, 10).unwrap();
        assert_eq!(missing.len(), 2);
        {
            let mut w = store.begin_write().unwrap();
            for (hash, _) in &missing {
                w.put(hash, &[1.0, 0.0]).unwrap();
            }
            w.flush().unwrap();
        }
        assert_eq!(index::vector_coverage(&conn, &s).unwrap(), (0, 2), "cache is not the record");
        assert_eq!(hydrate(&conn, &store).unwrap(), 2);
        assert_eq!(index::vector_coverage(&conn, &s).unwrap(), (2, 2));
        assert_eq!(hydrate(&conn, &store).unwrap(), 0, "a second hydrate imports nothing");
    }
}
