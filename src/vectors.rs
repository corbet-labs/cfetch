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

/// Up to this many missing vectors, a hydrate seeks per record instead of
/// streaming the whole artifact file.
const SEEK_UNTIL: usize = 64;

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
#[derive(Debug)]
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
        anyhow::ensure!(
            !spec.model.contains(['\n', '\r']),
            "embeddings.model must not contain newlines"
        );
        anyhow::ensure!(spec.dim > 0, "embeddings.dimensions must be at least 1");
        let dir = crate::paths::shared_vector_dir(brain_root);
        let mut store =
            VectorStore { dir, spec: spec.clone(), hashes: Vec::new(), present: HashSet::new() };
        store.reload()?;
        Ok(store)
    }

    fn idx_path(&self) -> PathBuf {
        self.dir.join(format!("{}.idx", slug(&self.spec)))
    }

    fn bin_path(&self) -> PathBuf {
        self.dir.join(format!("{}.bin", slug(&self.spec)))
    }

    /// Rereads the index file. Only records that are BOTH listed and present
    /// in the `.bin` count — a torn tail is invisible to readers.
    fn reload(&mut self) -> anyhow::Result<()> {
        self.hashes.clear();
        self.present.clear();
        let idx = self.idx_path();
        let raw = match std::fs::read_to_string(&idx) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(anyhow::anyhow!("read {}: {e}", idx.display())),
        };
        let mut lines = raw.lines();
        anyhow::ensure!(
            lines.next() == Some(MAGIC),
            "{} is not a cfetch vector index",
            idx.display()
        );
        let mut header = std::collections::HashMap::new();
        for _ in 0..HEADER_LINES - 1 {
            let line = lines.next().context("truncated vector index header")?;
            let (k, v) = line.split_once(' ').context("malformed vector index header")?;
            header.insert(k.to_string(), v.to_string());
        }
        // The slug is lossy (a slash becomes an underscore), so the exact
        // model string is what actually decides whether these vectors are
        // ours. A mismatch is loud: two models' vectors in one ranking are
        // numbers that only LOOK like similarity.
        let stored_model = header.get("model").map(String::as_str).unwrap_or_default();
        anyhow::ensure!(
            stored_model == self.spec.model,
            "{} holds vectors of model {stored_model:?}, not {:?}",
            idx.display(),
            self.spec.model
        );
        anyhow::ensure!(
            header.get("dim").map(String::as_str) == Some(self.spec.dim.to_string().as_str()),
            "{} holds a different dimension than embeddings.dimensions={}",
            idx.display(),
            self.spec.dim
        );
        anyhow::ensure!(
            header.get("precision").map(String::as_str) == Some(self.spec.precision.as_str()),
            "{} holds a different precision than embeddings.precision={}",
            idx.display(),
            self.spec.precision.as_str()
        );
        let listed: Vec<String> = lines.filter(|l| !l.is_empty()).map(str::to_string).collect();
        let stored_records = std::fs::metadata(self.bin_path()).map(|m| m.len()).unwrap_or(0) as usize
            / self.stride();
        self.hashes = listed;
        self.hashes.truncate(stored_records);
        self.present = self.hashes.iter().cloned().collect();
        Ok(())
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
        let Some(record) = self.hashes.iter().position(|h| h == hash) else {
            return Ok(None);
        };
        let path = self.bin_path();
        let mut file = std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
        file.seek(std::io::SeekFrom::Start((record * self.stride()) as u64))?;
        let mut buf = vec![0u8; self.stride()];
        file.read_exact(&mut buf).with_context(|| format!("read record {record} of {}", path.display()))?;
        Ok(Some(index::blob_to_vec(&buf, self.spec.precision)))
    }

    /// Streams the whole store in record order — one sequential read, the
    /// shape a hydrate wants.
    pub fn for_each(
        &self,
        mut f: impl FnMut(&str, Vec<f32>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        if self.hashes.is_empty() {
            return Ok(());
        }
        let path = self.bin_path();
        let file = std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let mut reader = std::io::BufReader::new(file);
        let mut buf = vec![0u8; self.stride()];
        for hash in &self.hashes {
            reader
                .read_exact(&mut buf)
                .with_context(|| format!("read {} (short of its index)", path.display()))?;
            f(hash, index::blob_to_vec(&buf, self.spec.precision))?;
        }
        Ok(())
    }

    /// Takes the store's write lock and repairs any torn tail. Only a host
    /// with the embed capability ever calls this; every other host reads.
    pub fn begin_write(&mut self) -> anyhow::Result<VectorWriter<'_>> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("create {}", self.dir.display()))?;
        // Derived bytes belong in the tree, never in the tree's git history.
        let ignore = self.dir.join(".gitignore");
        if !ignore.exists() {
            std::fs::write(&ignore, "# Derived vector artifacts: shared as files, never as commits.\n*\n!.gitignore\n")
                .with_context(|| format!("write {}", ignore.display()))?;
        }
        let lock = crate::lockfile::acquire(&self.dir.join("store.lock"), 5_000, 0).context(
            "another embed run holds the shared vector store (derive-once: one writer per group)",
        )?;
        // Another writer may have appended since we opened: reload UNDER the
        // lock, then repair whatever a crash left behind.
        self.reload()?;
        let bin = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.bin_path())?;
        // `reload` already dropped unlisted records from the view; make the
        // file agree, so the next append lands where its index line says.
        bin.set_len((self.hashes.len() * self.stride()) as u64)?;
        let mut idx = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.idx_path())?;
        if idx.metadata()?.len() == 0 {
            write!(
                idx,
                "{MAGIC}\nmodel {}\ndim {}\nprecision {}\n",
                self.spec.model,
                self.spec.dim,
                self.spec.precision.as_str()
            )?;
        } else {
            // Rewrite the hash list whenever the view is shorter than the
            // file (an index line whose record never landed).
            let raw = std::fs::read_to_string(self.idx_path())?;
            let listed = raw.lines().skip(HEADER_LINES).filter(|l| !l.is_empty()).count();
            if listed != self.hashes.len() {
                let header: String =
                    raw.lines().take(HEADER_LINES).map(|l| format!("{l}\n")).collect();
                let body: String = self.hashes.iter().map(|h| format!("{h}\n")).collect();
                idx.set_len(0)?;
                idx.seek(std::io::SeekFrom::Start(0))?;
                idx.write_all(header.as_bytes())?;
                idx.write_all(body.as_bytes())?;
            }
        }
        idx.seek(std::io::SeekFrom::End(0))?;
        let mut bin = bin;
        bin.seek(std::io::SeekFrom::End(0))?;
        Ok(VectorWriter { store: self, _lock: lock, bin, idx })
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
        anyhow::ensure!(
            !hash.contains(['\n', '\r']) && !hash.is_empty(),
            "content hash {hash:?} is not a hash"
        );
        anyhow::ensure!(
            vector.len() == self.store.spec.dim,
            "vector has {} components, the store holds {}",
            vector.len(),
            self.store.spec.dim
        );
        if self.store.present.contains(hash) {
            return Ok(false);
        }
        // Record FIRST, index line second: a crash between them leaves an
        // orphan record (invisible, truncated by the next writer), never an
        // index line pointing at bytes that are not there.
        self.bin.write_all(&index::vec_to_blob(vector, self.store.spec.precision))?;
        writeln!(self.idx, "{hash}")?;
        self.store.hashes.push(hash.to_string());
        self.store.present.insert(hash.to_string());
        Ok(true)
    }

    /// Durably lands everything written so far — called per batch, so an
    /// interrupted run leaves committed work behind for the next one.
    pub fn flush(&mut self) -> anyhow::Result<()> {
        self.bin.flush()?;
        self.bin.sync_all()?;
        self.idx.flush()?;
        self.idx.sync_all()?;
        Ok(())
    }
}

/// Fills the local index cache from the shared store: every block hash that
/// has no cached vector but IS in the store. Returns how many were imported.
/// This is what lets a second host answer semantically without ever holding
/// an embeddings key.
pub fn hydrate(conn: &Connection, store: &VectorStore) -> anyhow::Result<usize> {
    if store.is_empty() {
        return Ok(0);
    }
    let spec = store.spec();
    let missing = index::hashes_without_vectors(conn, spec, usize::MAX)?;
    let wanted: HashSet<String> = missing
        .into_iter()
        .map(|(hash, _)| hash)
        .filter(|hash| store.contains(hash))
        .collect();
    if wanted.is_empty() {
        return Ok(0);
    }
    let tx = conn.unchecked_transaction()?;
    let mut imported = 0usize;
    if wanted.len() <= SEEK_UNTIL {
        // The everyday case after an edit: a handful of hashes. Seeking to
        // each record beats streaming a 40 MB artifact file to find five.
        for hash in &wanted {
            if let Some(vector) = store.get(hash)? {
                index::insert_vector(&tx, hash, spec, &vector)?;
                imported += 1;
            }
        }
    } else {
        // A first hydrate wants everything: one sequential pass, not 20k
        // seeks over what may be an NFS-mounted tree.
        store.for_each(|hash, vector| {
            if wanted.contains(hash) {
                index::insert_vector(&tx, hash, spec, &vector)?;
                imported += 1;
            }
            Ok(())
        })?;
    }
    tx.commit()?;
    Ok(imported)
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
