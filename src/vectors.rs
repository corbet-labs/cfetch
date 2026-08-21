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
//! The store is APPEND-ONLY. A vector whose text left this host's tree is not
//! deleted here: the same content may still live in a slice another host
//! holds, and this file is the group's record, not one host's cache. (The
//! local cache in index.db IS pruned on every scan — that is what keeps
//! coverage numbers honest.) Compaction, when it comes, has to be a
//! group-wide operation that knows every holder; a local delete never is.
//! `embed-index` and `status` print artifact count and block coverage side by
//! side, so the gap between them is visible rather than mysterious.
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

/// First line of every `.idx` — a file that is not one of these is not ours.
///
/// v1: magic, model, dim, precision.
/// v2: the same plus `doc_prefix`, written only when there IS one, so a store
/// written by an older cfetch stays byte-identical and keeps working.
const MAGIC_V1: &str = "cfetch-vectors v1";
const MAGIC_V2: &str = "cfetch-vectors v2";

const HEADER_LINES_V1: usize = 4;
const HEADER_LINES_V2: usize = 5;

/// How many header lines this spec's artifact carries.
fn header_lines(spec: &VectorSpec) -> usize {
    if spec.doc_prefix.is_empty() { HEADER_LINES_V1 } else { HEADER_LINES_V2 }
}

fn magic_for(spec: &VectorSpec) -> &'static str {
    if spec.doc_prefix.is_empty() { MAGIC_V1 } else { MAGIC_V2 }
}

/// Short, stable, filename-safe digest of a document prefix.
fn prefix_tag(prefix: &str) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(prefix.as_bytes());
    h.finalize().iter().take(4).map(|b| format!("{b:02x}")).collect()
}

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
    let base = format!("{model}-{}-{}", spec.dim, spec.precision.as_str());
    // A document prefix changes every vector in the file, so it changes the
    // FILE — otherwise two hosts with different prefixes would append
    // incompatible records to one artifact and the header check would only
    // catch it on the unlucky one that opened it second.
    if spec.doc_prefix.is_empty() {
        base
    } else {
        format!("{base}-{}", prefix_tag(&spec.doc_prefix))
    }
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
        anyhow::ensure!(
            !spec.doc_prefix.contains(['\n', '\r']),
            "embeddings.document_prefix must not contain newlines — it is stored on one \
             header line, and a newline there would be read as the end of the header"
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
        // Version by magic line: v1 has no doc_prefix and is what every store
        // written before asymmetric documents looks like. Reading it stays
        // supported forever — these files are expensive to rebuild.
        let magic = lines.next().unwrap_or_default();
        let count = match magic {
            MAGIC_V1 => HEADER_LINES_V1,
            MAGIC_V2 => HEADER_LINES_V2,
            _ => anyhow::bail!("{} is not a cfetch vector index", idx.display()),
        };
        let mut header = std::collections::HashMap::new();
        for _ in 0..count - 1 {
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
        // A v1 file carries no doc_prefix line, which MEANS the empty prefix —
        // documents were embedded raw. Comparing it explicitly is what stops a
        // host that has since configured a prefix from appending vectors of a
        // different shape to the same file.
        let stored_prefix = header.get("doc_prefix").map(String::as_str).unwrap_or("");
        anyhow::ensure!(
            stored_prefix == self.spec.doc_prefix,
            "{} holds vectors embedded with document prefix {stored_prefix:?}, not {:?} — \
             these are different artifacts and must not share a file",
            idx.display(),
            self.spec.doc_prefix
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
            let magic = magic_for(&self.spec);
            write!(
                idx,
                "{magic}\nmodel {}\ndim {}\nprecision {}\n",
                self.spec.model,
                self.spec.dim,
                self.spec.precision.as_str()
            )?;
            if !self.spec.doc_prefix.is_empty() {
                writeln!(idx, "doc_prefix {}", self.spec.doc_prefix)?;
            }
        } else {
            // Rewrite the hash list whenever the view is shorter than the
            // file (an index line whose record never landed).
            let raw = std::fs::read_to_string(self.idx_path())?;
            let hl = header_lines(&self.spec);
            let listed = raw.lines().skip(hl).filter(|l| !l.is_empty()).count();
            if listed != self.hashes.len() {
                let header: String = raw.lines().take(hl).map(|l| format!("{l}\n")).collect();
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
    let spec = store.spec();
    // The cache and the meta rows that describe it move together: a cache
    // left over from another model/width/precision is unusable ballast, and
    // leaving meta disagreeing with the rows is how the NEXT run decides to
    // drop work it did not need to.
    index::ensure_vector_spec(conn, spec)?;
    if store.is_empty() {
        return Ok(0);
    }
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
        VectorSpec { model: "test-model".into(), dim, precision, doc_prefix: String::new() }
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
        let s = VectorSpec { model: "vendor/embed-8b".into(), dim: 8, precision: Precision::F32, doc_prefix: String::new() };
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
            &VectorSpec { model: "a/b".into(), dim: 4, precision: Precision::F16, doc_prefix: String::new() },
        )
        .unwrap();
        a.begin_write().unwrap().put("aa", &[1.0, 0.0, 0.0, 0.0]).unwrap();
        let err = VectorStore::open(
            brain.path(),
            &VectorSpec { model: "a_b".into(), dim: 4, precision: Precision::F16, doc_prefix: String::new() },
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
    fn hydrate_drops_a_cache_left_over_from_another_spec() {
        let brain = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(brain.path().join("knowledge")).unwrap();
        std::fs::write(brain.path().join("knowledge/a.md"), "- one\n").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = index::open(state.path()).unwrap();
        index::scan(&mut conn, brain.path(), None, &crate::config::RingRules::default()).unwrap();

        let old = VectorSpec { model: "old-model".into(), dim: 2, precision: Precision::F16, doc_prefix: String::new() };
        index::ensure_vector_spec(&conn, &old).unwrap();
        let hash = index::content_hash("- one");
        index::insert_vector(&conn, &hash, &old, &[1.0, 0.0]).unwrap();

        let s = spec(2, Precision::F16);
        let mut store = VectorStore::open(brain.path(), &s).unwrap();
        store.begin_write().unwrap().put(&hash, &[0.0, 1.0]).unwrap();
        assert_eq!(hydrate(&conn, &store).unwrap(), 1);
        assert_eq!(index::stored_vector_spec(&conn).as_ref(), Some(&s), "meta follows the cache");
        assert_eq!(index::vector_coverage(&conn, &s).unwrap(), (1, 1));
        assert_eq!(index::vector_coverage(&conn, &old).unwrap(), (0, 1), "the old spec is gone");
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

    // ---- document prefix as artifact identity

    fn spec_pfx(dim: usize, prefix: &str) -> VectorSpec {
        VectorSpec {
            model: "test-model".into(),
            dim,
            precision: Precision::F16,
            doc_prefix: prefix.to_string(),
        }
    }

    #[test]
    fn a_document_prefix_makes_a_different_artifact_file() {
        // Same model, same width, different prefix: the vectors are not
        // comparable, so they must not land in one file.
        let raw = slug(&spec_pfx(4, ""));
        let pfx = slug(&spec_pfx(4, "passage: "));
        assert_ne!(raw, pfx);
        assert!(pfx.starts_with(&raw), "the prefixed name extends the base: {pfx}");
        // Stable across runs, and different prefixes never collide.
        assert_eq!(pfx, slug(&spec_pfx(4, "passage: ")));
        assert_ne!(pfx, slug(&spec_pfx(4, "query: ")));
    }

    #[test]
    fn a_store_refuses_vectors_embedded_under_another_prefix() {
        let brain = tempfile::tempdir().unwrap();
        let a = spec_pfx(2, "passage: ");
        {
            let mut store = VectorStore::open(brain.path(), &a).unwrap();
            let mut w = store.begin_write().unwrap();
            w.put("hash-one", &[1.0, 0.0]).unwrap();
        }
        // Reading it back under the SAME prefix works.
        assert_eq!(VectorStore::open(brain.path(), &a).unwrap().len(), 1);

        // Now force a different prefix onto the same filename and confirm the
        // header check catches it, since the filename tag is only a hint.
        let idx = VectorStore::open(brain.path(), &a).unwrap().idx_path();
        let raw = std::fs::read_to_string(&idx).unwrap();
        std::fs::write(&idx, raw.replace("doc_prefix passage: ", "doc_prefix other: ")).unwrap();
        let e = VectorStore::open(brain.path(), &a).unwrap_err().to_string();
        assert!(e.contains("document prefix"), "{e}");
    }

    #[test]
    fn a_v1_store_still_opens_and_means_raw_documents() {
        // Files written before document prefixes existed hold raw-document
        // vectors. They are expensive to rebuild; reading them stays supported.
        let brain = tempfile::tempdir().unwrap();
        let raw_spec = spec_pfx(2, "");
        {
            let mut store = VectorStore::open(brain.path(), &raw_spec).unwrap();
            let mut w = store.begin_write().unwrap();
            w.put("hash-one", &[1.0, 0.0]).unwrap();
        }
        let idx = VectorStore::open(brain.path(), &raw_spec).unwrap().idx_path();
        let head = std::fs::read_to_string(&idx).unwrap();
        assert!(head.starts_with(MAGIC_V1), "no prefix must still write v1: {head}");
        assert!(!head.contains("doc_prefix"), "v1 carries no prefix line");
        assert_eq!(VectorStore::open(brain.path(), &raw_spec).unwrap().len(), 1);
    }

    #[test]
    fn a_prefix_containing_a_newline_is_refused_before_it_corrupts_a_header() {
        let brain = tempfile::tempdir().unwrap();
        let e = VectorStore::open(brain.path(), &spec_pfx(2, "a\nb")).unwrap_err().to_string();
        assert!(e.contains("must not contain newlines"), "{e}");
    }
}
