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
const TICKET_PREFIX: &str = "cfetch-invite-1:";

/// What `cfetch invite` prints and `cfetch join` consumes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Ticket {
    /// Endpoint id of the origin — who to dial. iroh discovery resolves this
    /// to addresses, so no volatile IP is baked into the ticket.
    pub origin: String,
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
        let body = text.trim().strip_prefix(TICKET_PREFIX).ok_or_else(|| {
            anyhow::anyhow!("not a cfetch invite (expected it to start with {TICKET_PREFIX:?})")
        })?;
        let raw = b64_decode(body).context("invite is not valid base64url")?;
        let t: Ticket = serde_json::from_slice(&raw).context("invite payload is not a ticket")?;
        anyhow::ensure!(!t.origin.is_empty(), "invite names no origin");
        anyhow::ensure!(!t.slice.is_empty(), "invite names no slice");
        anyhow::ensure!(!t.secret.is_empty(), "invite carries no secret");
        Ok(t)
    }

    pub fn expired(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|e| now >= e)
    }
}

/// One record on the origin's side.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Grant {
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

/// Reads a slice's grants. A missing file is an empty list, not an error: a
/// slice nobody has been invited to is the normal case.
pub fn read(brain_root: &Path, slice: &str) -> anyhow::Result<Vec<Grant>> {
    let path = grants_file(brain_root, slice);
    match std::fs::read(&path) {
        Ok(raw) => serde_json::from_slice(&raw)
            .with_context(|| format!("{} is not a grants document", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Replaces a slice's grants atomically: write a sibling temp file, fsync,
/// rename. A reader either sees the old list or the new one, never a torn one.
///
/// Single-writer by construction — only a slice's ORIGIN writes its grants,
/// and a slice has exactly one origin.
pub fn write(brain_root: &Path, slice: &str, grants: &[Grant]) -> anyhow::Result<()> {
    let dir = grants_dir(brain_root);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = grants_file(brain_root, slice);
    let tmp = dir.join(format!(".{slice}.json.tmp"));
    let body = serde_json::to_vec_pretty(grants)?;
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        use std::io::Write as _;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Mints an invite for `slice` and records it as pending on the origin.
///
/// Returns the ticket, which is the ONLY place the secret ever exists in
/// readable form — it is not stored, so an invite that is lost must be
/// re-minted rather than looked up.
pub fn invite(
    brain_root: &Path,
    origin: &str,
    slice: &str,
    mode: Mode,
    now: u64,
    expires_at: Option<u64>,
) -> anyhow::Result<Ticket> {
    anyhow::ensure!(!slice.is_empty(), "an invite must name a slice");
    let secret = random_hex_32();
    let mut grants = read(brain_root, slice)?;
    grants.push(Grant {
        slice: slice.to_string(),
        mode,
        secret_hash: hash_hex(&secret),
        peer: None,
        created_at: now,
        expires_at,
    });
    write(brain_root, slice, &grants)?;
    Ok(Ticket {
        origin: origin.to_string(),
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
    peer: &str,
    now: u64,
) -> anyhow::Result<Grant> {
    let want = hash_hex(secret);
    let mut grants = read(brain_root, slice)?;
    let idx = grants
        .iter()
        .position(|g| constant_time_eq(&g.secret_hash, &want))
        .context("invite is not known to this host")?;
    let g = &grants[idx];
    anyhow::ensure!(
        !g.expires_at.is_some_and(|e| now >= e),
        "invite expired"
    );
    match &g.peer {
        Some(bound) if bound == peer => return Ok(g.clone()),
        Some(_) => anyhow::bail!("invite was already redeemed by another host"),
        None => {}
    }
    grants[idx].peer = Some(peer.to_string());
    write(brain_root, slice, &grants)?;
    Ok(grants[idx].clone())
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
            g.peer.as_deref() == Some(peer) && !g.expires_at.is_some_and(|e| now >= e)
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

    const ORIGIN: &str = "aa11bb22cc33";
    const PEER: &str = "dd44ee55ff66";
    const OTHER: &str = "9900aabbccdd";

    #[test]
    fn a_ticket_survives_a_round_trip_through_text() {
        let dir = tempfile::tempdir().unwrap();
        let t = invite(dir.path(), ORIGIN, "hosts", Mode::Ro, 100, Some(500)).unwrap();
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
            Ticket::decode("cfetch-invite-1:!!!").unwrap_err().to_string().contains("base64url")
        );
    }

    #[test]
    fn the_origin_stores_the_hash_and_never_the_secret() {
        // A leaked grants file must not hand anyone a usable invite.
        let dir = tempfile::tempdir().unwrap();
        let t = invite(dir.path(), ORIGIN, "hosts", Mode::Rw, 100, None).unwrap();
        let raw = std::fs::read_to_string(grants_dir(dir.path()).join("hosts.json")).unwrap();
        assert!(!raw.contains(&t.secret), "the secret is on disk: {raw}");
        assert!(raw.contains(&hash_hex(&t.secret)));
    }

    #[test]
    fn redeeming_binds_the_invite_to_the_peer_that_used_it() {
        let dir = tempfile::tempdir().unwrap();
        let t = invite(dir.path(), ORIGIN, "hosts", Mode::Ro, 100, None).unwrap();
        assert!(read(dir.path(), "hosts").unwrap()[0].pending());

        let g = redeem(dir.path(), "hosts", &t.secret, PEER, 200).unwrap();
        assert_eq!(g.peer.as_deref(), Some(PEER));
        assert_eq!(access(dir.path(), "hosts", PEER, 300).unwrap(), Some(Mode::Ro));
        assert_eq!(access(dir.path(), "hosts", OTHER, 300).unwrap(), None);
    }

    #[test]
    fn a_retried_join_from_the_same_host_is_not_an_error() {
        // Networks drop replies; the second attempt must not look like theft.
        let dir = tempfile::tempdir().unwrap();
        let t = invite(dir.path(), ORIGIN, "hosts", Mode::Ro, 100, None).unwrap();
        redeem(dir.path(), "hosts", &t.secret, PEER, 200).unwrap();
        assert!(redeem(dir.path(), "hosts", &t.secret, PEER, 201).is_ok());
    }

    #[test]
    fn an_invite_cannot_be_redeemed_twice_by_different_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let t = invite(dir.path(), ORIGIN, "hosts", Mode::Ro, 100, None).unwrap();
        redeem(dir.path(), "hosts", &t.secret, PEER, 200).unwrap();
        let e = redeem(dir.path(), "hosts", &t.secret, OTHER, 201).unwrap_err().to_string();
        assert!(e.contains("already redeemed"), "{e}");
        // And the original holder keeps its access.
        assert_eq!(access(dir.path(), "hosts", PEER, 202).unwrap(), Some(Mode::Ro));
    }

    #[test]
    fn an_unknown_secret_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        invite(dir.path(), ORIGIN, "hosts", Mode::Ro, 100, None).unwrap();
        let e = redeem(dir.path(), "hosts", "not-a-real-secret", PEER, 200)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not known to this host"), "{e}");
        assert!(read(dir.path(), "hosts").unwrap()[0].pending(), "a failed redeem binds nothing");
    }

    #[test]
    fn expiry_is_enforced_on_redeem_and_on_access() {
        let dir = tempfile::tempdir().unwrap();
        let t = invite(dir.path(), ORIGIN, "hosts", Mode::Ro, 100, Some(500)).unwrap();
        assert!(redeem(dir.path(), "hosts", &t.secret, PEER, 500).unwrap_err().to_string().contains("expired"));

        let t2 = invite(dir.path(), ORIGIN, "docs", Mode::Ro, 100, Some(500)).unwrap();
        redeem(dir.path(), "docs", &t2.secret, PEER, 200).unwrap();
        assert_eq!(access(dir.path(), "docs", PEER, 499).unwrap(), Some(Mode::Ro));
        assert_eq!(access(dir.path(), "docs", PEER, 500).unwrap(), None, "expired access is gone");
        assert!(t2.expired(500));
    }

    #[test]
    fn many_invites_to_one_slice_coexist() {
        let dir = tempfile::tempdir().unwrap();
        let a = invite(dir.path(), ORIGIN, "hosts", Mode::Ro, 100, None).unwrap();
        let b = invite(dir.path(), ORIGIN, "hosts", Mode::Rw, 101, None).unwrap();
        redeem(dir.path(), "hosts", &a.secret, PEER, 200).unwrap();
        redeem(dir.path(), "hosts", &b.secret, OTHER, 201).unwrap();
        assert_eq!(access(dir.path(), "hosts", PEER, 300).unwrap(), Some(Mode::Ro));
        assert_eq!(access(dir.path(), "hosts", OTHER, 300).unwrap(), Some(Mode::Rw));
        assert_eq!(read(dir.path(), "hosts").unwrap().len(), 2);
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
}
