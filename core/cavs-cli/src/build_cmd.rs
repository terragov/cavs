//! `cavs build sign/verify/encrypt/decrypt` (v0.9.0): local release
//! authenticity and optional at-rest encryption for build artifacts.
//!
//! Scope: release/manifest authenticity and optional locally encrypted
//! artifacts. This is **not DRM**, not license enforcement and not
//! anti-tamper — a client with the key has the content, full stop.

use anyhow::{bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use std::path::Path;

/// Detached signature file: magic + hex Ed25519 signature over the
/// artifact's BLAKE3-256 digest.
const SIG_MAGIC: &str = "CAVSBSG1";
/// Encrypted artifact: magic + 24-byte nonce + XChaCha20-Poly1305 body.
const ENC_MAGIC: &[u8; 8] = b"CAVSENC1";

fn read_hex_key(path: &Path, expected_len: usize) -> Result<Vec<u8>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read key file {}", path.display()))?;
    let hex = raw.trim();
    if hex.len() != expected_len * 2 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!(
            "{} does not look like a {expected_len}-byte hex key",
            path.display()
        );
    }
    Ok((0..expected_len)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
        .collect())
}

fn artifact_digest(path: &Path) -> Result<[u8; 32]> {
    let mut hasher = cavs_hash::Hasher::new();
    let mut file =
        std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        use std::io::Read;
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

/// Sign any artifact (build, manifest, signature, plan) with an Ed25519
/// secret key as produced by `cavs keygen` / `cavs key generate`.
pub fn sign(artifact: &Path, key: &Path, out: Option<&Path>) -> Result<()> {
    let secret = read_hex_key(key, 32)?;
    let signing = SigningKey::from_bytes(&secret.try_into().unwrap());
    let digest = artifact_digest(artifact)?;
    let signature = signing.sign(&digest);
    let sig_hex: String = signature
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let default_out = artifact.with_extension(format!(
        "{}.sig",
        artifact
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default()
    ));
    let out = out.unwrap_or(&default_out);
    std::fs::write(out, format!("{SIG_MAGIC}\n{sig_hex}\n"))?;
    println!("signed  : {} → {}", artifact.display(), out.display());
    println!(
        "pubkey  : verify with `cavs build verify {} --pub <key.pub>`",
        artifact.display()
    );
    Ok(())
}

/// Verify a detached signature. Fails (non-zero exit) on any tampering.
pub fn verify(artifact: &Path, pubkey: &Path, sig: Option<&Path>) -> Result<()> {
    let public = read_hex_key(pubkey, 32)?;
    let verifying = VerifyingKey::from_bytes(&public.clone().try_into().unwrap())
        .map_err(|e| anyhow::anyhow!("CAVS-E-SIGNATURE-INVALID: bad public key: {e}"))?;
    let default_sig = artifact.with_extension(format!(
        "{}.sig",
        artifact
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default()
    ));
    let sig_path = sig.unwrap_or(&default_sig);
    let raw = std::fs::read_to_string(sig_path)
        .with_context(|| format!("cannot read signature {}", sig_path.display()))?;
    let mut lines = raw.lines();
    if lines.next() != Some(SIG_MAGIC) {
        bail!(
            "CAVS-E-SIGNATURE-INVALID: {} is not a CAVS build signature",
            sig_path.display()
        );
    }
    let sig_hex = lines.next().unwrap_or_default().trim();
    if sig_hex.len() != 128 {
        bail!("CAVS-E-SIGNATURE-INVALID: truncated signature");
    }
    let sig_bytes: Vec<u8> = (0..64)
        .map(|i| u8::from_str_radix(&sig_hex[i * 2..i * 2 + 2], 16))
        .collect::<Result<_, _>>()
        .map_err(|_| anyhow::anyhow!("CAVS-E-SIGNATURE-INVALID: non-hex signature"))?;
    let signature = Signature::from_bytes(&sig_bytes.try_into().unwrap());
    let digest = artifact_digest(artifact)?;
    match verifying.verify(&digest, &signature) {
        Ok(()) => {
            println!(
                "verify  : OK — {} matches its signature",
                artifact.display()
            );
            Ok(())
        }
        Err(_) => bail!(
            "CAVS-E-SIGNATURE-INVALID: {} does NOT match {} (artifact or signature tampered)",
            artifact.display(),
            sig_path.display()
        ),
    }
}

/// Generate a random 32-byte content key (for encrypt/decrypt).
pub fn generate_content_key(out: &Path) -> Result<()> {
    use rand_core::{OsRng, RngCore};
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(out, format!("{hex}\n"))?;
    println!(
        "content key : {} (keep private; anyone with it can decrypt)",
        out.display()
    );
    Ok(())
}

/// Encrypt an artifact for local storage/transport. Optional and not DRM.
pub fn encrypt(artifact: &Path, key: &Path, out: &Path) -> Result<()> {
    use rand_core::{OsRng, RngCore};
    let secret = read_hex_key(key, 32)?;
    let cipher = XChaCha20Poly1305::new(secret.as_slice().into());
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut nonce);
    let plaintext = std::fs::read(artifact)?;
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext.as_slice())
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    let mut body = Vec::with_capacity(8 + 24 + ciphertext.len());
    body.extend_from_slice(ENC_MAGIC);
    body.extend_from_slice(&nonce);
    body.extend_from_slice(&ciphertext);
    std::fs::write(out, body)?;
    println!(
        "encrypt : {} → {} (XChaCha20-Poly1305; local at-rest protection, not DRM)",
        artifact.display(),
        out.display()
    );
    Ok(())
}

pub fn decrypt(artifact: &Path, key: &Path, out: &Path) -> Result<()> {
    let secret = read_hex_key(key, 32)?;
    let cipher = XChaCha20Poly1305::new(secret.as_slice().into());
    let body = std::fs::read(artifact)?;
    if body.len() < 8 + 24 || &body[..8] != ENC_MAGIC {
        bail!("{} is not a CAVS-encrypted artifact", artifact.display());
    }
    let nonce = XNonce::from_slice(&body[8..32]);
    let plaintext = cipher
        .decrypt(nonce, &body[32..])
        .map_err(|_| anyhow::anyhow!("decryption failed: wrong key or tampered data"))?;
    std::fs::write(out, plaintext)?;
    println!("decrypt : {} → {}", artifact.display(), out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keypair(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        use rand_core::OsRng;
        let key = SigningKey::generate(&mut OsRng);
        let secret: String = key.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let public: String = key
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let sk = dir.join("cavs.key");
        let pk = dir.join("cavs.key.pub");
        std::fs::write(&sk, secret).unwrap();
        std::fs::write(&pk, public).unwrap();
        (sk, pk)
    }

    #[test]
    fn sign_verify_and_tamper_detection() {
        let dir = tempfile::tempdir().unwrap();
        let (sk, pk) = keypair(dir.path());
        let artifact = dir.path().join("build.cavs");
        std::fs::write(&artifact, b"release payload").unwrap();
        let sig = dir.path().join("build.cavs.sig");
        sign(&artifact, &sk, Some(&sig)).unwrap();
        verify(&artifact, &pk, Some(&sig)).unwrap();

        std::fs::write(&artifact, b"release payloaX").unwrap();
        let err = verify(&artifact, &pk, Some(&sig)).unwrap_err().to_string();
        assert!(err.contains("CAVS-E-SIGNATURE-INVALID"), "{err}");
    }

    #[test]
    fn encrypt_round_trip_and_wrong_key() {
        let dir = tempfile::tempdir().unwrap();
        let k1 = dir.path().join("a.key");
        let k2 = dir.path().join("b.key");
        generate_content_key(&k1).unwrap();
        generate_content_key(&k2).unwrap();
        let artifact = dir.path().join("build.cavs");
        std::fs::write(&artifact, vec![7u8; 100_000]).unwrap();

        let enc = dir.path().join("build.enc");
        let dec = dir.path().join("build.dec");
        encrypt(&artifact, &k1, &enc).unwrap();
        assert_ne!(std::fs::read(&enc).unwrap()[32..132], vec![7u8; 100][..]);
        decrypt(&enc, &k1, &dec).unwrap();
        assert_eq!(std::fs::read(&dec).unwrap(), vec![7u8; 100_000]);

        assert!(decrypt(&enc, &k2, &dec).is_err());
    }
}
