//! Checking that an APK really is Roblox's before it is installed.
//!
//! The game's native code runs with the user's full permissions, so an APK
//! with a modified `libroblox.so` (cheats, malware) must never be installed.
//! Android apps carry a signature over every byte of the file (APK Signature
//! Scheme v2, v3 and v3.1, stored in the "APK Signing Block" just before the
//! ZIP central directory). An APK is accepted only if one of those
//! signatures is valid and was made with Roblox's key.
//!
//! Roblox's original signing certificate (2014) is pinned by its SHA-256.
//! Roblox has since rotated to a newer key for Android 13+ (the v3.1 block);
//! that key is accepted because the v3.1 block carries a proof-of-rotation
//! chain in which the original key signs the new certificate.
//!
//! Format: https://source.android.com/docs/security/features/apksigning/v2
//! (and the v3 / v3.1 pages next to it).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use ring::digest;
use ring::signature::{self, UnparsedPublicKey, VerificationAlgorithm};

/// SHA-256 of Roblox's original APK signing certificate ("Roblox
/// Corporation, Mobile, San Mateo", 2014). Also published for every Roblox
/// release on APKMirror (SHA-1 `f450e842186fe75cfeb0ad887ea95f8d98383b38`).
const ROBLOX_CERT_SHA256: [u8; 32] = [
    0x44, 0x93, 0x2e, 0xa3, 0x5a, 0x17, 0xa2, 0x67, 0x37, 0x2d, 0x71, 0xb5, 0x4d, 0x1a, 0x0c, 0xb3,
    0xda, 0x0d, 0xca, 0x51, 0x13, 0xe9, 0x44, 0x06, 0xae, 0x2f, 0xe1, 0x80, 0x90, 0xba, 0x14, 0x77,
];

const V2: u32 = 0x7109_871a;
const V3: u32 = 0xf053_68c0;
const V3_1: u32 = 0x1b93_ad61;
/// The proof-of-rotation attribute in a v3 / v3.1 signer.
const PROOF_OF_ROTATION: u32 = 0x3ba0_6f8c;

const MAGIC: &[u8; 16] = b"APK Sig Block 42";
const EOCD_LEN: u64 = 22;
const CHUNK: usize = 1 << 20;

type Result<T> = std::result::Result<T, String>;

/// Which signature accepted an APK.
#[derive(Debug, PartialEq)]
pub struct Verified {
    pub scheme: &'static str,
    /// The key was a newer one, trusted through the proof-of-rotation chain.
    pub rotated: bool,
}

/// Check that `apk` is signed by Roblox (see the module documentation).
pub fn verify(apk: &Path) -> Result<Verified> {
    verify_schemes(apk, &[V3_1, V3, V2])
}

fn scheme_name(id: u32) -> &'static str {
    match id {
        V2 => "v2",
        V3 => "v3",
        _ => "v3.1",
    }
}

/// A length-checked reader over a byte slice, in the format's little endian.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.0.len() {
            return Err("truncated signature data".into());
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    /// A `u32`-length-prefixed slice.
    fn prefixed(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    /// A sequence of `u32`-length-prefixed slices filling the rest.
    fn sequence(mut self) -> Result<Vec<&'a [u8]>> {
        let mut out = Vec::new();
        while !self.0.is_empty() {
            out.push(self.prefixed()?);
        }
        Ok(out)
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Where the parts of the ZIP are.
struct Layout {
    file_len: u64,
    /// Start of the APK Signing Block (= end of the ZIP entries).
    block_start: u64,
    cd_offset: u64,
    cd_size: u64,
    eocd_offset: u64,
}

fn read_at(file: &mut File, offset: u64, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0; len];
    file.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    file.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

/// Find the signing block. The layout is checked strictly so that what is
/// verified is exactly what the unzipper reads: no ZIP comment (so there is
/// only one possible end record), no ZIP64, and the central directory runs
/// right up to the end record.
fn layout(file: &mut File) -> Result<(Layout, Vec<u8>)> {
    let file_len = file.metadata().map_err(|e| e.to_string())?.len();
    if file_len < EOCD_LEN + 32 {
        return Err("too small to be an APK".into());
    }
    let eocd_offset = file_len - EOCD_LEN;
    let eocd = read_at(file, eocd_offset, EOCD_LEN as usize)?;
    if &eocd[..4] != b"PK\x05\x06" || eocd[20..22] != [0, 0] {
        return Err("unexpected ZIP layout (no end record at the end, or a ZIP comment)".into());
    }
    let cd_size = u32::from_le_bytes(eocd[12..16].try_into().unwrap()) as u64;
    let cd_offset = u32::from_le_bytes(eocd[16..20].try_into().unwrap()) as u64;
    if cd_offset == 0xffff_ffff || cd_offset + cd_size != eocd_offset {
        return Err("unexpected ZIP layout (ZIP64, or data after the central directory)".into());
    }
    if cd_offset < 32 {
        return Err("no APK signing block (the APK is not signed)".into());
    }
    let footer = read_at(file, cd_offset - 24, 24)?;
    if &footer[8..] != MAGIC {
        return Err("no APK signing block (the APK is not signed with scheme v2 or later)".into());
    }
    let size = u64::from_le_bytes(footer[..8].try_into().unwrap());
    // The size counts everything but its own leading copy.
    if size < 24 || size > 64 << 20 || size + 8 > cd_offset {
        return Err("damaged APK signing block".into());
    }
    let block_start = cd_offset - size - 8;
    let block = read_at(file, block_start, (size + 8) as usize)?;
    if block[..8] != footer[..8] {
        return Err("damaged APK signing block".into());
    }
    let layout = Layout { file_len, block_start, cd_offset, cd_size, eocd_offset };
    Ok((layout, block))
}

/// The ID-value pairs of the signing block.
fn pairs(block: &[u8]) -> Result<Vec<(u32, &[u8])>> {
    let mut r = Reader(&block[8..block.len() - 24]);
    let mut out = Vec::new();
    while !r.is_empty() {
        let len = r.u64()?;
        if len < 4 || len > r.0.len() as u64 {
            return Err("damaged APK signing block".into());
        }
        let mut pair = Reader(r.take(len as usize)?);
        let id = pair.u32()?;
        out.push((id, pair.0));
    }
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hash {
    Sha256,
    Sha512,
}

/// Signature algorithm IDs this verifier supports, best first.
const PREFERENCE: [u32; 5] = [0x0104, 0x0102, 0x0103, 0x0101, 0x0201];

fn content_hash(algorithm: u32) -> Option<Hash> {
    match algorithm {
        0x0101 | 0x0103 | 0x0201 => Some(Hash::Sha256),
        0x0102 | 0x0104 => Some(Hash::Sha512),
        _ => None,
    }
}

/// Verify `signature` over `message` with a SubjectPublicKeyInfo.
fn verify_signature(algorithm: u32, spki: &[u8], message: &[u8], signature: &[u8]) -> Result<()> {
    let key = PublicKey::parse(spki)?;
    let alg: &dyn VerificationAlgorithm = match (algorithm, &key.kind) {
        (0x0101, KeyKind::Rsa) => &signature::RSA_PSS_2048_8192_SHA256,
        (0x0102, KeyKind::Rsa) => &signature::RSA_PSS_2048_8192_SHA512,
        // Roblox's original key is 1024-bit RSA, which ring only accepts
        // through its explicitly named legacy variants.
        (0x0103, KeyKind::Rsa) => &signature::RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY,
        (0x0104, KeyKind::Rsa) => &signature::RSA_PKCS1_1024_8192_SHA512_FOR_LEGACY_USE_ONLY,
        (0x0201, KeyKind::P256) => &signature::ECDSA_P256_SHA256_ASN1,
        (0x0201, KeyKind::P384) => &signature::ECDSA_P384_SHA256_ASN1,
        _ => return Err(format!("unsupported signature algorithm {algorithm:#06x} for this key")),
    };
    UnparsedPublicKey::new(alg, key.bits)
        .verify(message, signature)
        .map_err(|_| "signature does not verify".to_string())
}

/// The content digest over the three APK sections, as the format defines it:
/// the ZIP entries, the central directory, and the end record with its
/// central-directory offset pointing at the signing block instead. Each
/// section is cut into 1 MiB chunks; each chunk is hashed with a `0xa5`
/// prefix, and the chunk hashes are hashed with a `0x5a` prefix.
fn content_digest(file: &mut File, l: &Layout, hash: Hash) -> Result<Vec<u8>> {
    let alg = match hash {
        Hash::Sha256 => &digest::SHA256,
        Hash::Sha512 => &digest::SHA512,
    };
    let mut eocd = read_at(file, l.eocd_offset, (l.file_len - l.eocd_offset) as usize)?;
    eocd[16..20].copy_from_slice(&(l.block_start as u32).to_le_bytes());

    let mut chunk_digests = Vec::new();
    let mut count: u32 = 0;
    let mut add_chunk = |data: &[u8]| {
        let mut ctx = digest::Context::new(alg);
        ctx.update(&[0xa5]);
        ctx.update(&(data.len() as u32).to_le_bytes());
        ctx.update(data);
        chunk_digests.extend_from_slice(ctx.finish().as_ref());
        count += 1;
    };
    let mut buf = vec![0; CHUNK];
    for (start, len) in [(0, l.block_start), (l.cd_offset, l.cd_size)] {
        file.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
        let mut left = len;
        while left > 0 {
            let n = left.min(CHUNK as u64) as usize;
            file.read_exact(&mut buf[..n]).map_err(|e| e.to_string())?;
            add_chunk(&buf[..n]);
            left -= n as u64;
        }
    }
    for part in eocd.chunks(CHUNK) {
        add_chunk(part);
    }
    let mut top = digest::Context::new(alg);
    top.update(&[0x5a]);
    top.update(&count.to_le_bytes());
    top.update(&chunk_digests);
    Ok(top.finish().as_ref().to_vec())
}

fn sha256(data: &[u8]) -> [u8; 32] {
    digest::digest(&digest::SHA256, data).as_ref().try_into().unwrap()
}

/// One signer of a v2 / v3 / v3.1 block, with its signature over the signed
/// data already checked.
struct Signer<'a> {
    /// The algorithm whose signature was verified (decides the content digest).
    algorithm: u32,
    /// The content digest the signer vouches for.
    digest: &'a [u8],
    certificate: &'a [u8],
    /// The proof-of-rotation attribute, if any (v3 / v3.1).
    lineage: Option<&'a [u8]>,
}

fn check_signer(scheme: u32, raw: &[u8]) -> Result<Signer<'_>> {
    let mut r = Reader(raw);
    let signed_data = r.prefixed()?;
    if scheme != V2 {
        r.u32()?; // min SDK
        r.u32()?; // max SDK
    }
    let signatures = Reader(r.prefixed()?).sequence()?;
    let public_key = r.prefixed()?;

    let mut available = Vec::new();
    for s in signatures {
        let mut s = Reader(s);
        let algorithm = s.u32()?;
        available.push((algorithm, s.prefixed()?));
    }
    let (algorithm, sig) = PREFERENCE
        .iter()
        .find_map(|want| available.iter().find(|(a, _)| a == want).copied())
        .ok_or("no supported signature algorithm")?;
    verify_signature(algorithm, public_key, signed_data, sig)?;

    // The signed data is now authentic.
    let mut d = Reader(signed_data);
    let digests = Reader(d.prefixed()?).sequence()?;
    let certificates = Reader(d.prefixed()?).sequence()?;
    if scheme != V2 {
        d.u32()?;
        d.u32()?;
    }
    let attributes = Reader(d.prefixed()?).sequence()?;

    let mut digest = None;
    for entry in digests {
        let mut e = Reader(entry);
        if e.u32()? == algorithm {
            digest = Some(e.prefixed()?);
        }
    }
    let digest = digest.ok_or("no content digest for the verified algorithm")?;
    let certificate = *certificates.first().ok_or("no certificate")?;
    if certificate_spki(certificate)? != public_key {
        return Err("the certificate does not match the signing key".into());
    }
    let mut lineage = None;
    for attr in attributes {
        let mut a = Reader(attr);
        if a.u32()? == PROOF_OF_ROTATION {
            lineage = Some(a.0);
        }
    }
    Ok(Signer { algorithm, digest, certificate, lineage })
}

/// Whether a proof-of-rotation chain is valid, starts from (or passes
/// through) Roblox's pinned certificate, and ends at `certificate`. Each node
/// holds a certificate and is signed by the previous node's key.
fn lineage_trusts(lineage: &[u8], certificate: &[u8]) -> Result<bool> {
    let mut r = Reader(lineage);
    let version = r.u32()?;
    if version != 1 {
        return Err(format!("unknown proof-of-rotation version {version}"));
    }
    let nodes = r.sequence()?;
    let mut previous: Option<(&[u8], u32)> = None; // (certificate, algorithm it signs the next node with)
    let mut pinned_seen = false;
    let mut last = None;
    for node in nodes {
        let mut n = Reader(node);
        let signed = n.prefixed()?;
        let _flags = n.u32()?;
        let next_algorithm = n.u32()?;
        let sig = n.prefixed()?;
        let mut s = Reader(signed);
        let cert = s.prefixed()?;
        let algorithm = s.u32()?;
        if let Some((prev_cert, prev_algorithm)) = previous {
            if algorithm != prev_algorithm {
                return Err("proof-of-rotation algorithm mismatch".into());
            }
            verify_signature(algorithm, certificate_spki(prev_cert)?, signed, sig)?;
        }
        pinned_seen |= sha256(cert) == ROBLOX_CERT_SHA256;
        previous = Some((cert, next_algorithm));
        last = Some(cert);
    }
    Ok(pinned_seen && last == Some(certificate))
}

fn verify_schemes(apk: &Path, schemes: &[u32]) -> Result<Verified> {
    let mut file = File::open(apk).map_err(|e| e.to_string())?;
    let (layout, block) = layout(&mut file)?;
    let pairs = pairs(&block)?;
    let mut problems = Vec::new();
    for &scheme in schemes {
        let Some(&(_, value)) = pairs.iter().find(|(id, _)| *id == scheme) else {
            continue;
        };
        let name = scheme_name(scheme);
        let signers = match Reader(value).prefixed().and_then(|s| Reader(s).sequence()) {
            Ok(s) => s,
            Err(e) => {
                problems.push(format!("{name}: {e}"));
                continue;
            }
        };
        for raw in signers {
            let signer = match check_signer(scheme, raw) {
                Ok(s) => s,
                Err(e) => {
                    problems.push(format!("{name}: {e}"));
                    continue;
                }
            };
            let pinned = sha256(signer.certificate) == ROBLOX_CERT_SHA256;
            let rotated = !pinned
                && match signer.lineage.map(|l| lineage_trusts(l, signer.certificate)) {
                    Some(Ok(trusted)) => trusted,
                    Some(Err(e)) => {
                        problems.push(format!("{name}: proof of rotation: {e}"));
                        false
                    }
                    None => false,
                };
            if !pinned && !rotated {
                problems.push(format!("{name}: signed with a key that is not Roblox's"));
                continue;
            }
            // Only now read the whole file: the content must match.
            let hash = content_hash(signer.algorithm).unwrap();
            if content_digest(&mut file, &layout, hash)? != signer.digest {
                problems.push(format!("{name}: the contents were modified after signing"));
                continue;
            }
            return Ok(Verified { scheme: name, rotated });
        }
    }
    if problems.is_empty() {
        problems.push("no v2, v3 or v3.1 signature".into());
    }
    Err(problems.join("; "))
}

// --- Minimal DER, just enough to find a certificate's public key. ---

/// One DER element: (tag, contents, the whole element), and the rest.
fn der(input: &[u8]) -> Result<(u8, &[u8], &[u8], &[u8])> {
    let bad = || "malformed certificate".to_string();
    let tag = *input.first().ok_or_else(bad)?;
    let first = *input.get(1).ok_or_else(bad)?;
    let (len, header) = if first < 0x80 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return Err(bad());
        }
        let bytes = input.get(2..2 + n).ok_or_else(bad)?;
        (bytes.iter().fold(0usize, |acc, b| (acc << 8) | *b as usize), 2 + n)
    };
    let end = header.checked_add(len).ok_or_else(bad)?;
    let contents = input.get(header..end).ok_or_else(bad)?;
    Ok((tag, contents, &input[..end], &input[end..]))
}

/// The SubjectPublicKeyInfo (whole element) of an X.509 certificate.
fn certificate_spki(cert: &[u8]) -> Result<&[u8]> {
    let (_, certificate, _, _) = der(cert)?;
    let (_, tbs, _, _) = der(certificate)?;
    let mut rest = tbs;
    let (tag, _, _, r) = der(rest)?;
    if tag == 0xa0 {
        rest = r; // explicit version
    }
    // serial, signature algorithm, issuer, validity, subject
    for _ in 0..5 {
        rest = der(rest)?.3;
    }
    let (tag, _, spki, _) = der(rest)?;
    if tag != 0x30 {
        return Err("malformed certificate".into());
    }
    Ok(spki)
}

enum KeyKind {
    Rsa,
    P256,
    P384,
}

struct PublicKey<'a> {
    kind: KeyKind,
    /// The BIT STRING contents ring expects: PKCS#1 RSAPublicKey, or an EC point.
    bits: &'a [u8],
}

const OID_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const OID_EC: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_P384: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];

impl<'a> PublicKey<'a> {
    fn parse(spki: &'a [u8]) -> Result<Self> {
        let bad = || "unsupported public key".to_string();
        let (_, inner, _, _) = der(spki)?;
        let (_, algorithm, _, rest) = der(inner)?;
        let (tag, bits, _, _) = der(rest)?;
        if tag != 0x03 || bits.first() != Some(&0) {
            return Err(bad());
        }
        let (_, oid, _, params) = der(algorithm)?;
        let kind = if oid == OID_RSA {
            KeyKind::Rsa
        } else if oid == OID_EC {
            let (_, curve, _, _) = der(params)?;
            match curve {
                c if c == OID_P256 => KeyKind::P256,
                c if c == OID_P384 => KeyKind::P384,
                _ => return Err(bad()),
            }
        } else {
            return Err(bad());
        };
        Ok(PublicKey { kind, bits: &bits[1..] })
    }
}

#[cfg(test)]
mod tests {
    //! The real-APK tests need Roblox's APKs, which cannot be in the
    //! repository: point `LUCID_TEST_APK_DIR` at a folder with `base.apk`
    //! and `split_config.x86_64.apk` (Sober keeps them in
    //! `~/.var/app/org.vinegarhq.Sober/data/sober/packages/x86_64/com.roblox.client`)
    //! and run `cargo test -p lucid apk_signature -- --include-ignored`.
    use super::*;
    use std::path::PathBuf;

    fn apks() -> Vec<PathBuf> {
        let dir = PathBuf::from(std::env::var_os("LUCID_TEST_APK_DIR").expect("set LUCID_TEST_APK_DIR"));
        vec![dir.join("base.apk"), dir.join("split_config.x86_64.apk")]
    }

    /// A copy of `apk` with one byte changed by `edit` (offset from layout).
    fn tampered(apk: &Path, name: &str, edit: impl Fn(&Layout) -> u64) -> PathBuf {
        let mut data = std::fs::read(apk).unwrap();
        let (l, _) = layout(&mut File::open(apk).unwrap()).unwrap();
        let at = edit(&l) as usize;
        data[at] ^= 0x01;
        let out = std::env::temp_dir().join(format!("veneer-apk-test-{}-{name}", std::process::id()));
        std::fs::write(&out, data).unwrap();
        out
    }

    #[test]
    #[ignore]
    fn genuine_apks_pass_every_scheme() {
        for apk in apks() {
            assert_eq!(verify(&apk).unwrap(), Verified { scheme: "v3.1", rotated: true });
            assert_eq!(verify_schemes(&apk, &[V3]).unwrap(), Verified { scheme: "v3", rotated: false });
            assert_eq!(verify_schemes(&apk, &[V2]).unwrap(), Verified { scheme: "v2", rotated: false });
        }
    }

    #[test]
    #[ignore]
    fn modified_contents_are_rejected() {
        let apk = &apks()[1];
        for (name, edit) in [
            ("entry", (|_: &Layout| 1000) as fn(&Layout) -> u64),
            ("central-directory", |l: &Layout| l.cd_offset + 50),
        ] {
            let bad = tampered(apk, name, edit);
            let err = verify(&bad).unwrap_err();
            std::fs::remove_file(&bad).unwrap();
            assert!(err.contains("modified after signing"), "{name}: {err}");
        }
    }

    #[test]
    #[ignore]
    fn proof_of_rotation_is_checked() {
        let mut file = File::open(&apks()[0]).unwrap();
        let (_, block) = layout(&mut file).unwrap();
        let pairs = pairs(&block).unwrap();
        let (_, v31) = pairs.iter().find(|(id, _)| *id == V3_1).unwrap();
        let raw = Reader(Reader(v31).prefixed().unwrap()).sequence().unwrap()[0];
        let signer = check_signer(V3_1, raw).unwrap();
        let lineage = signer.lineage.expect("Roblox's v3.1 signer has a proof of rotation");
        assert_ne!(sha256(signer.certificate), ROBLOX_CERT_SHA256);

        // Genuine: the original key vouches for the new certificate.
        assert!(lineage_trusts(lineage, signer.certificate).unwrap());
        // The chain does not end at some other certificate.
        let other = lineage_cert(lineage, 0);
        assert!(!lineage_trusts(lineage, other).unwrap());
        // A broken signature in the chain (its last byte) is rejected.
        let mut broken = lineage.to_vec();
        *broken.last_mut().unwrap() ^= 0x01;
        assert!(lineage_trusts(&broken, signer.certificate).is_err());
    }

    /// The certificate of node `i` of a proof-of-rotation chain.
    fn lineage_cert(lineage: &[u8], i: usize) -> &[u8] {
        let mut r = Reader(lineage);
        r.u32().unwrap();
        let node = r.sequence().unwrap()[i];
        Reader(Reader(node).prefixed().unwrap()).prefixed().unwrap()
    }

    #[test]
    #[ignore]
    fn a_byte_in_the_v3_1_block_only_disables_that_scheme() {
        // Flip the last byte of the v3.1 proof of rotation, inside the v3.1
        // signed data: v3.1 no longer verifies, the untouched v3 still does.
        let apk = &apks()[0];
        let data = std::fs::read(apk).unwrap();
        let (l, _) = layout(&mut File::open(apk).unwrap()).unwrap();
        let id = PROOF_OF_ROTATION.to_le_bytes();
        let block = &data[l.block_start as usize..l.cd_offset as usize];
        let at = l.block_start as usize + block.windows(4).rposition(|w| w == id).unwrap();
        let len = u32::from_le_bytes(data[at - 4..at].try_into().unwrap()) as u64;
        let bad = tampered(apk, "v31", |_| at as u64 + len - 1);
        let only_v31 = verify_schemes(&bad, &[V3_1]);
        let all = verify(&bad);
        std::fs::remove_file(&bad).unwrap();
        assert!(only_v31.is_err());
        assert_eq!(all.unwrap(), Verified { scheme: "v3", rotated: false });
    }

    #[test]
    fn non_apks_are_rejected() {
        let path = std::env::temp_dir().join(format!("veneer-apk-test-{}-empty", std::process::id()));
        std::fs::write(&path, b"PK\x05\x06 not an apk at all, just some bytes").unwrap();
        assert!(verify(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
