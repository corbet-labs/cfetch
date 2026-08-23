//! Grants: who may read or write which slice, and the invites that create them.
//!
//! The trust model, because it decides the file format:
//!
//! A ticket is NOT an authorization. It is an address plus a one-time secret.
//! Authorization lives on the ORIGIN — the host that holds the slice — and a
//! peer presenting a forged ticket is simply refused, because the origin looks
//! the secret up in its own records. That means tickets can be pasted through
//! any channel without the channel being trusted.
//!
//! The origin stores the invite's HASH, never the secret. A grants file that
//! leaks therefore leaks no usable invite, only the fact that one exists.
//!
//! An origin cannot revoke. A granted slice is the recipient's, the way a
//! pushed git branch is: revocation is a request, not a mechanism. So the
//! records here are append-only in spirit and the code never rewrites history
//! — it only ever adds, or binds a pending invite to the peer that redeemed it.

use anyhow::Context;
use std::path::{Path, PathBuf};

/// Access a grant confers. `fork` is deliberately absent: forking is taking a
/// copy and becoming its origin, which needs no permission from anyone and so
/// is not a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Consume the slice.
    Ro,
    /// Co-write the slice.
    Rw,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Ro => "ro",
            Mode::Rw => "rw",
        }
    }
}

impl std::str::FromStr for Mode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ro" => Ok(Mode::Ro),
            "rw" => Ok(Mode::Rw),
            other => anyhow::bail!("unknown access mode {other:?} (expected ro or rw)"),
        }
    }
}

/// Human-visible ticket prefix. Versioned so a future format is refused by
/// name rather than misparsed.
const TICKET_PREFIX: &str = "cfetch-network1-invite-3:";

/// What `cfetch invite` prints and `cfetch join` consumes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Ticket {
    pub network_major: u32,
    /// Authenticated endpoint plus the direct/relay routes known when the
    /// invite was minted. The endpoint id is the authority; the routes only
    /// make it reachable without depending on a discovery lookup succeeding.
    pub origin: iroh::EndpointAddr,
    pub slice: String,
    pub mode: Mode,
    /// The one-time secret, hex. The origin holds only its hash.
    pub secret: String,
    /// Unix seconds. `None` = no expiry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl Ticket {
    /// Base64url, no padding, behind a versioned prefix — one token with no
    /// spaces, safe to paste into a chat or a terminal.
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("ticket serializes");
        format!("{TICKET_PREFIX}{}", b64_encode(&json))
    }

    pub fn decode(text: &str) -> anyhow::Result<Ticket> {
        anyhow::ensure!(text.trim().len() <= 32 * 1024, "invite is unreasonably large");
        let body = text.trim().strip_prefix(TICKET_PREFIX).ok_or_else(|| {
            anyhow::anyhow!("not a cfetch invite (expected it to start with {TICKET_PREFIX:?})")
        })?;
        let raw = b64_decode(body).context("invite is not valid base64url")?;
        let t: Ticket = serde_json::from_slice(&raw).context("invite payload is not a ticket")?;
        anyhow::ensure!(
            t.network_major == crate::embedding_profile::NETWORK_MAJOR,
            "invite is for cfetch network major {}, this host requires {}",
            t.network_major,
            crate::embedding_profile::NETWORK_MAJOR
        );
        validate_slice_name(&t.slice)?;
        anyhow::ensure!(
            t.secret.len() == 64
                && t.secret.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "invite carries an invalid secret"
        );
        Ok(t)
    }

    pub fn expired(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|e| now >= e)
    }
}

/// One record on the origin's side.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Grant {
    #[serde(default)]
    pub network_major: u32,
    pub slice: String,
    pub mode: Mode,
    /// Hash of the invite secret, hex. Never the secret itself.
    pub secret_hash: String,
    /// The peer that redeemed the invite. `None` while it is still pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl Grant {
    pub fn pending(&self) -> bool {
        self.peer.is_none()
    }
}

/// Where an origin keeps the grants for the slices it holds.
pub fn grants_dir(brain_root: &Path) -> PathBuf {
    brain_root.join("state/cfetch/grants")
}

fn grants_file(brain_root: &Path, slice: &str) -> PathBuf {
    grants_dir(brain_root).join(format!("{slice}.json"))
}

// Grant mutation is an explicit CLI or daemon operation, not a deadline-bound
// agent hook. Windows durable replacement writes can serialize slowly under a
// burst of concurrent invites, so give every writer enough time to take its
// turn instead of dropping otherwise valid grants after two seconds.
const GRANT_LOCK_WAIT_MS: u64 = 10_000;

/// Slice names become filenames on every supported platform. Keep them to a
/// portable identifier rather than letting separators or drive syntax escape
/// the grants directory.
pub fn validate_slice_name(slice: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!slice.is_empty(), "a slice must have a name");
    anyhow::ensure!(slice != "." && slice != "..", "invalid slice name {slice:?}");
    anyhow::ensure!(
        slice
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
        "invalid slice name {slice:?}: use only ASCII letters, digits, dot, underscore, or dash"
    );
    Ok(())
}

fn grant_lock(brain_root: &Path, slice: &str) -> anyhow::Result<crate::lockfile::Lock> {
    validate_slice_name(slice)?;
    let dir = grants_dir(brain_root);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!(".{slice}.lock"));
    crate::lockfile::acquire(&path, GRANT_LOCK_WAIT_MS, 0)
        .ok_or_else(|| anyhow::anyhow!("timed out waiting for {}", path.display()))
}

/// Reads a slice's grants. A missing file is an empty list, not an error: a
/// slice nobody has been invited to is the normal case.
pub fn read(brain_root: &Path, slice: &str) -> anyhow::Result<Vec<Grant>> {
    validate_slice_name(slice)?;
    let path = grants_file(brain_root, slice);
    match std::fs::read(&path) {
        Ok(raw) => serde_json::from_slice(&raw)
            .with_context(|| format!("{} is not a grants document", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Replaces a slice's grants atomically while the caller holds its slice
/// lock. A reader either sees the old list or the new one, never a torn one.
fn write_unlocked(brain_root: &Path, slice: &str, grants: &[Grant]) -> anyhow::Result<()> {
    validate_slice_name(slice)?;
    let path = grants_file(brain_root, slice);
    let body = serde_json::to_vec_pretty(grants)?;
    crate::fsutil::atomic_write(&path, body)
}

/// Mints an invite for `slice` and records it as pending on the origin.
///
/// Returns the ticket, which is the ONLY place the secret ever exists in
/// readable form — it is not stored, so an invite that is lost must be
/// re-minted rather than looked up.
pub fn invite(
    brain_root: &Path,
    origin: &iroh::EndpointAddr,
    slice: &str,
    mode: Mode,
    now: u64,
    expires_at: Option<u64>,
) -> anyhow::Result<Ticket> {
    validate_slice_name(slice)?;
    let _lock = grant_lock(brain_root, slice)?;
    let secret = random_hex_32();
    let mut grants = read(brain_root, slice)?;
    grants.push(Grant {
        network_major: crate::embedding_profile::NETWORK_MAJOR,
        slice: slice.to_string(),
        mode,
        secret_hash: hash_hex(&secret),
        peer: None,
        created_at: now,
        expires_at,
    });
    write_unlocked(brain_root, slice, &grants)?;
    Ok(Ticket {
        network_major: crate::embedding_profile::NETWORK_MAJOR,
        origin: origin.clone(),
        slice: slice.to_string(),
        mode,
        secret,
        expires_at,
    })
}

/// Redeems a presented secret against a slice's grants, binding it to `peer`.
///
/// Refuses an unknown secret, an expired invite, and one already bound to a
/// DIFFERENT peer. Re-presenting the same secret from the same peer succeeds
/// unchanged, so a retried join is not an error.
pub fn redeem(
    brain_root: &Path,
    slice: &str,
    secret: &str,
    mode: Mode,
    peer: &str,
    now: u64,
) -> anyhow::Result<Grant> {
    validate_slice_name(slice)?;
    let _lock = grant_lock(brain_root, slice)?;
    let want = hash_hex(secret);
    let mut grants = read(brain_root, slice)?;
    let idx = grants
        .iter()
        .position(|g| constant_time_eq(&g.secret_hash, &want))
        .context("invite is not known to this host")?;
    let g = &grants[idx];
    anyhow::ensure!(
        g.network_major == crate::embedding_profile::NETWORK_MAJOR,
        "invite grant belongs to cfetch network major {}, this host requires {}",
        g.network_major,
        crate::embedding_profile::NETWORK_MAJOR
    );
    anyhow::ensure!(
        g.mode == mode,
        "invite mode does not match the origin's grant"
    );
    anyhow::ensure!(
        g.expires_at.is_none_or(|e| now < e),
        "invite expired"
    );
    match &g.peer {
        Some(bound) if bound == peer => return Ok(g.clone()),
        Some(_) => anyhow::bail!("invite was already redeemed by another host"),
        None => {}
    }
    grants[idx].peer = Some(peer.to_string());
    write_unlocked(brain_root, slice, &grants)?;
    Ok(grants[idx].clone())
}

/// A remotely joined slice remembered on the consuming host. This is routing
/// state, not authority: every request is still authorized by the origin
/// against its grant record and the caller's authenticated iroh endpoint id.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Membership {
    #[serde(default)]
    pub network_major: u32,
    pub origin: iroh::EndpointAddr,
    pub slice: String,
    pub mode: Mode,
    pub joined_at: u64,
}

const MEMBERSHIPS_FILE: &str = "memberships.json";

fn memberships_path(state_dir: &Path) -> PathBuf {
    state_dir.join(MEMBERSHIPS_FILE)
}

fn read_memberships(state_dir: &Path) -> anyhow::Result<Vec<Membership>> {
    let path = memberships_path(state_dir);
    match std::fs::read(&path) {
        Ok(raw) => serde_json::from_slice(&raw)
            .with_context(|| format!("{} is not a memberships document", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Lists every remembered remote origin without exposing an invite secret.
///
/// This is diagnostic routing state, not proof that a peer is online. Callers
/// that need liveness must probe the origin and report that observation
/// separately; the presence of a membership file alone only means a join once
/// succeeded.
pub fn memberships(state_dir: &Path) -> anyhow::Result<Vec<Membership>> {
    let mut memberships = read_memberships(state_dir)?;
    memberships.sort_by(|a, b| {
        a.slice
            .cmp(&b.slice)
            .then_with(|| a.origin.id.to_string().cmp(&b.origin.id.to_string()))
    });
    Ok(memberships)
}

/// Records a successful remote redemption without ever persisting its secret.
pub fn remember_membership(state_dir: &Path, membership: Membership) -> anyhow::Result<()> {
    validate_slice_name(&membership.slice)?;
    anyhow::ensure!(
        membership.network_major == crate::embedding_profile::NETWORK_MAJOR,
        "membership belongs to cfetch network major {}, this host requires {}",
        membership.network_major,
        crate::embedding_profile::NETWORK_MAJOR
    );
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("create {}", state_dir.display()))?;
    let lock_path = state_dir.join("memberships.lock");
    let _lock = crate::lockfile::acquire(&lock_path, 2_000, 0)
        .ok_or_else(|| anyhow::anyhow!("timed out waiting for {}", lock_path.display()))?;
    let mut all = read_memberships(state_dir)?;
    if let Some(existing) = all.iter_mut().find(|m| {
        m.slice == membership.slice && m.origin.id == membership.origin.id
    }) {
        *existing = membership;
    } else {
        all.push(membership);
    }
    crate::fsutil::atomic_write(&memberships_path(state_dir), serde_json::to_vec_pretty(&all)?)
}

/// Returns the one origin joined for `slice`. Ambiguous same-named slices are
/// refused instead of silently querying whichever record happened to come
/// first; a future origin selector can make that choice explicit.
pub fn membership_for_slice(state_dir: &Path, slice: &str) -> anyhow::Result<Option<Membership>> {
    validate_slice_name(slice)?;
    let mut matches = read_memberships(state_dir)?
        .into_iter()
        .filter(|m| {
            m.slice == slice && m.network_major == crate::embedding_profile::NETWORK_MAJOR
        });
    let first = matches.next();
    anyhow::ensure!(
        matches.next().is_none(),
        "slice {slice:?} was joined from more than one origin; refusing an ambiguous route"
    );
    Ok(first)
}

/// The access `peer` holds on `slice`, if any.
///
/// This is the authorization check the SERVING path will make once the iroh
/// transport can tell it which endpoint is asking. Over the local socket the
/// caller is this host, and over plain TCP the bearer token is the whole
/// authorization — neither carries a peer identity to check. Kept here,
/// tested, and deliberately not called yet rather than reinvented later.
#[cfg_attr(not(test), allow(dead_code))]
pub fn access(brain_root: &Path, slice: &str, peer: &str, now: u64) -> anyhow::Result<Option<Mode>> {
    Ok(read(brain_root, slice)?
        .into_iter()
        .find(|g| {
            g.network_major == crate::embedding_profile::NETWORK_MAJOR
                && g.peer.as_deref() == Some(peer)
                && g.expires_at.is_none_or(|e| now < e)
        })
        .map(|g| g.mode))
}

// ---- small helpers, kept local so the format has one definition ----

fn random_hex_32() -> String {
    // The identity crate already depends on a CSPRNG; use the same one rather
    // than inventing a second source of randomness.
    let sk = iroh::SecretKey::generate();
    hex_encode(&sk.to_bytes())
}

fn hash_hex(secret: &str) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(secret.as_bytes());
    hex_encode(&h.finalize())
}

/// Compares two hex digests without an early return on the first differing
/// byte. Both sides are public hashes here, so this is defence in depth rather
/// than a load-bearing guarantee — but a lookup that leaks how far it matched
/// is a bad habit to leave in a security path.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        let take = chunk.len() + 1;
        for i in 0..take {
            out.push(B64[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

fn b64_decode(text: &str) -> anyhow::Result<Vec<u8>> {
    let mut val = Vec::with_capacity(text.len());
    for c in text.bytes() {
        let i = B64
            .iter()
            .position(|&x| x == c)
            .ok_or_else(|| anyhow::anyhow!("invalid base64url character {:?}", c as char))?;
        val.push(i as u8);
    }
    let mut out = Vec::with_capacity(val.len() * 3 / 4);
    for chunk in val.chunks(4) {
        let mut n = 0u32;
        for (i, v) in chunk.iter().enumerate() {
            n |= (*v as u32) << (18 - 6 * i);
        }
        let bytes = chunk.len() - 1;
        for i in 0..bytes {
            out.push(((n >> (16 - 8 * i)) & 0xff) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER: &str = "dd44ee55ff66";
    const OTHER: &str = "9900aabbccdd";

    fn origin() -> iroh::EndpointAddr {
        iroh::SecretKey::from_bytes(&[7; 32]).public().into()
    }

    #[test]
    fn a_ticket_survives_a_round_trip_through_text() {
        let dir = tempfile::tempdir().unwrap();
        let t = invite(dir.path(), &origin(), "hosts", Mode::Ro, 100, Some(500)).unwrap();
        let text = t.encode();
        assert!(text.starts_with(TICKET_PREFIX));
        assert!(!text.contains(' '), "a ticket must paste as one token: {text}");
        assert_eq!(Ticket::decode(&text).unwrap(), t);
        // Whitespace from a copy-paste is not an error.
        assert_eq!(Ticket::decode(&format!("  {text}\n")).unwrap(), t);
    }

    #[test]
    fn base64url_round_trips_every_length_remainder() {
        for n in 0..40usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 % 256) as u8).collect();
            assert_eq!(b64_decode(&b64_encode(&data)).unwrap(), data, "length {n}");
        }
    }

    #[test]
    fn a_ticket_that_is_not_ours_is_refused_by_name() {
        assert!(Ticket::decode("hello").unwrap_err().to_string().contains("not a cfetch invite"));
        assert!(
            Ticket::decode(&format!("{TICKET_PREFIX}!!!")).unwrap_err().to_string().contains("base64url")
        );
    }

    #[test]
    fn a_ticket_from_another_network_major_is_refused() {
        let mut ticket = Ticket {
            network_major: crate::embedding_profile::NETWORK_MAJOR + 1,
            origin: origin(),
            slice: "hosts".into(),
            mode: Mode::Ro,
            secret: "00".repeat(32),
            expires_at: None,
        };
        let err = Ticket::decode(&ticket.encode()).unwrap_err().to_string();
        assert!(err.contains("network major"), "{err}");

        ticket.network_major = crate::embedding_profile::NETWORK_MAJOR;
        assert_eq!(Ticket::decode(&ticket.encode()).unwrap(), ticket);
    }

    #[test]
    fn the_origin_stores_the_hash_and_never_the_secret() {
        // A leaked grants file must not hand anyone a usable invite.
        let dir = tempfile::tempdir().unwrap();
        let t = invite(dir.path(), &origin(), "hosts", Mode::Rw, 100, None).unwrap();
        let raw = std::fs::read_to_string(grants_dir(dir.path()).join("hosts.json")).unwrap();
        assert!(!raw.contains(&t.secret), "the secret is on disk: {raw}");
        assert!(raw.contains(&hash_hex(&t.secret)));
    }

    #[test]
    fn redeeming_binds_the_invite_to_the_peer_that_used_it() {
        let dir = tempfile::tempdir().unwrap();
        let t = invite(dir.path(), &origin(), "hosts", Mode::Ro, 100, None).unwrap();
        assert!(read(dir.path(), "hosts").unwrap()[0].pending());

        let g = redeem(dir.path(), "hosts", &t.secret, t.mode, PEER, 200).unwrap();
        assert_eq!(g.peer.as_deref(), Some(PEER));
        assert_eq!(access(dir.path(), "hosts", PEER, 300).unwrap(), Some(Mode::Ro));
        assert_eq!(access(dir.path(), "hosts", OTHER, 300).unwrap(), None);
    }

    #[test]
    fn a_retried_join_from_the_same_host_is_not_an_error() {
        // Networks drop replies; the second attempt must not look like theft.
        let dir = tempfile::tempdir().unwrap();
        let t = invite(dir.path(), &origin(), "hosts", Mode::Ro, 100, None).unwrap();
        redeem(dir.path(), "hosts", &t.secret, t.mode, PEER, 200).unwrap();
        assert!(redeem(dir.path(), "hosts", &t.secret, t.mode, PEER, 201).is_ok());
    }

    #[test]
    fn an_invite_cannot_be_redeemed_twice_by_different_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let t = invite(dir.path(), &origin(), "hosts", Mode::Ro, 100, None).unwrap();
        redeem(dir.path(), "hosts", &t.secret, t.mode, PEER, 200).unwrap();
        let e = redeem(dir.path(), "hosts", &t.secret, t.mode, OTHER, 201)
            .unwrap_err()
            .to_string();
        assert!(e.contains("already redeemed"), "{e}");
        // And the original holder keeps its access.
        assert_eq!(access(dir.path(), "hosts", PEER, 202).unwrap(), Some(Mode::Ro));
    }

    #[test]
    fn a_tampered_mode_does_not_consume_the_invite() {
        let dir = tempfile::tempdir().unwrap();
        let t = invite(dir.path(), &origin(), "hosts", Mode::Ro, 100, None).unwrap();
        let e = redeem(dir.path(), "hosts", &t.secret, Mode::Rw, PEER, 200)
            .unwrap_err()
            .to_string();
        assert!(e.contains("mode does not match"), "{e}");
        assert!(read(dir.path(), "hosts").unwrap()[0].pending());
        redeem(dir.path(), "hosts", &t.secret, Mode::Ro, PEER, 201).unwrap();
    }

    #[test]
    fn an_unknown_secret_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        invite(dir.path(), &origin(), "hosts", Mode::Ro, 100, None).unwrap();
        let e = redeem(dir.path(), "hosts", "not-a-real-secret", Mode::Ro, PEER, 200)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not known to this host"), "{e}");
        assert!(read(dir.path(), "hosts").unwrap()[0].pending(), "a failed redeem binds nothing");
    }

    #[test]
    fn expiry_is_enforced_on_redeem_and_on_access() {
        let dir = tempfile::tempdir().unwrap();
        let t = invite(dir.path(), &origin(), "hosts", Mode::Ro, 100, Some(500)).unwrap();
        assert!(
            redeem(dir.path(), "hosts", &t.secret, t.mode, PEER, 500)
                .unwrap_err()
                .to_string()
                .contains("expired")
        );

        let t2 = invite(dir.path(), &origin(), "docs", Mode::Ro, 100, Some(500)).unwrap();
        redeem(dir.path(), "docs", &t2.secret, t2.mode, PEER, 200).unwrap();
        assert_eq!(access(dir.path(), "docs", PEER, 499).unwrap(), Some(Mode::Ro));
        assert_eq!(access(dir.path(), "docs", PEER, 500).unwrap(), None, "expired access is gone");
        assert!(t2.expired(500));
    }

    #[test]
    fn many_invites_to_one_slice_coexist() {
        let dir = tempfile::tempdir().unwrap();
        let a = invite(dir.path(), &origin(), "hosts", Mode::Ro, 100, None).unwrap();
        let b = invite(dir.path(), &origin(), "hosts", Mode::Rw, 101, None).unwrap();
        redeem(dir.path(), "hosts", &a.secret, a.mode, PEER, 200).unwrap();
        redeem(dir.path(), "hosts", &b.secret, b.mode, OTHER, 201).unwrap();
        assert_eq!(access(dir.path(), "hosts", PEER, 300).unwrap(), Some(Mode::Ro));
        assert_eq!(access(dir.path(), "hosts", OTHER, 300).unwrap(), Some(Mode::Rw));
        assert_eq!(read(dir.path(), "hosts").unwrap().len(), 2);
    }

    #[test]
    fn concurrent_invites_do_not_clobber_one_another() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::sync::Arc::new(dir.path().to_path_buf());
        let workers: Vec<_> = (0..24)
            .map(|n| {
                let root = root.clone();
                std::thread::spawn(move || {
                    invite(&root, &origin(), "hosts", Mode::Ro, n, None).unwrap()
                })
            })
            .collect();
        let tickets: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        let grants = read(dir.path(), "hosts").unwrap();
        assert_eq!(grants.len(), tickets.len());
        for ticket in tickets {
            assert!(grants.iter().any(|g| g.secret_hash == hash_hex(&ticket.secret)));
        }
    }

    #[test]
    fn slice_names_cannot_escape_the_grants_directory() {
        for bad in ["", ".", "..", "../outside", r"..\outside", "C:drive", "has space"] {
            assert!(validate_slice_name(bad).is_err(), "must reject {bad:?}");
            assert!(read(tempfile::tempdir().unwrap().path(), bad).is_err());
        }
        for good in ["hosts", "team.eu", "project_2", "read-only"] {
            validate_slice_name(good).unwrap();
        }
    }

    #[test]
    fn a_slice_nobody_was_invited_to_reads_as_empty_not_as_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path(), "never-shared").unwrap().is_empty());
        assert_eq!(access(dir.path(), "never-shared", PEER, 1).unwrap(), None);
    }

    #[test]
    fn modes_parse_and_render_exactly_the_two_that_exist() {
        assert_eq!("ro".parse::<Mode>().unwrap(), Mode::Ro);
        assert_eq!("RW".parse::<Mode>().unwrap(), Mode::Rw);
        assert_eq!(Mode::Rw.as_str(), "rw");
        // fork is not a grant: it needs no permission from the origin.
        assert!("fork".parse::<Mode>().is_err());
        assert!("admin".parse::<Mode>().is_err());
    }

    #[test]
    fn membership_routing_is_atomic_idempotent_and_never_stores_the_secret() {
        let dir = tempfile::tempdir().unwrap();
        let membership = Membership {
            network_major: crate::embedding_profile::NETWORK_MAJOR,
            origin: origin(),
            slice: "hosts".to_string(),
            mode: Mode::Ro,
            joined_at: 100,
        };
        remember_membership(dir.path(), membership.clone()).unwrap();
        remember_membership(dir.path(), membership.clone()).unwrap();
        assert_eq!(membership_for_slice(dir.path(), "hosts").unwrap(), Some(membership));
        let raw = std::fs::read_to_string(memberships_path(dir.path())).unwrap();
        assert!(!raw.contains("secret"), "routing state must not grow a credential field: {raw}");
    }

    #[test]
    fn same_named_remote_slices_are_never_routed_ambiguously() {
        let dir = tempfile::tempdir().unwrap();
        remember_membership(
            dir.path(),
            Membership {
                network_major: crate::embedding_profile::NETWORK_MAJOR,
                origin: origin(),
                slice: "hosts".into(),
                mode: Mode::Ro,
                joined_at: 1,
            },
        )
        .unwrap();
        let other = iroh::SecretKey::from_bytes(&[8; 32]).public().into();
        remember_membership(
            dir.path(),
            Membership {
                network_major: crate::embedding_profile::NETWORK_MAJOR,
                origin: other,
                slice: "hosts".into(),
                mode: Mode::Ro,
                joined_at: 2,
            },
        )
        .unwrap();
        assert!(
            membership_for_slice(dir.path(), "hosts")
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
    }
}
