//! Encryption of biometric templates at rest.
//!
//! The 256-bit key is a systemd credential (`frogger.key`), sealed to the
//! TPM 2.0 chip by `frogger setup` (`systemd-creds encrypt --with-key=tpm2`).
//! systemd decrypts it when starting the daemon (`ImportCredential=`) and
//! exposes it below `$CREDENTIALS_DIRECTORY`; it never touches the disk in
//! plain text. Templates copied off the device are useless without the TPM.
//!
//! Format: a small JSON envelope with ChaCha20-Poly1305 ciphertext. The user
//! name is authenticated as associated data, so a template cannot be moved
//! to another account.

use anyhow::{bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CREDENTIAL_NAME: &str = "frogger.key";
pub const KEY_LEN: usize = 32;
const ALG: &str = "chacha20poly1305";

pub struct TemplateKey([u8; KEY_LEN]);

impl std::fmt::Debug for TemplateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TemplateKey(..)")
    }
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    frogger_enc: u32,
    alg: String,
    nonce: String,
    ct: String,
}

/// Fill a buffer from the kernel CSPRNG.
pub fn random_bytes(buf: &mut [u8]) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        // SAFETY: writes at most `len - filled` bytes into the buffer.
        let r = unsafe {
            libc::getrandom(
                buf[filled..].as_mut_ptr() as *mut libc::c_void,
                buf.len() - filled,
                0,
            )
        };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e).context("getrandom");
        }
        filled += r as usize;
    }
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        bail!("odd hex length");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).context("bad hex"))
        .collect()
}

impl TemplateKey {
    pub fn from_bytes(b: &[u8]) -> Result<TemplateKey> {
        let k: [u8; KEY_LEN] = b
            .try_into()
            .map_err(|_| anyhow::anyhow!("key must be {KEY_LEN} bytes"))?;
        Ok(TemplateKey(k))
    }

    pub fn generate() -> Result<TemplateKey> {
        let mut k = [0u8; KEY_LEN];
        random_bytes(&mut k)?;
        Ok(TemplateKey(k))
    }

    pub fn bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// Key handed over by systemd, if the service was started with it.
    pub fn from_credentials() -> Result<Option<TemplateKey>> {
        let Some(dir) = std::env::var_os("CREDENTIALS_DIRECTORY") else {
            return Ok(None);
        };
        Self::from_file(&PathBuf::from(dir).join(CREDENTIAL_NAME))
    }

    pub fn from_file(path: &Path) -> Result<Option<TemplateKey>> {
        match std::fs::read(path) {
            Ok(b) => Ok(Some(
                Self::from_bytes(&b).with_context(|| path.display().to_string())?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| path.display().to_string()),
        }
    }

    pub fn seal(&self, user: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; 12];
        random_bytes(&mut nonce)?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.0));
        let ct = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: user.as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("encryption failed"))?;
        let env = Envelope {
            frogger_enc: 1,
            alg: ALG.into(),
            nonce: hex(&nonce),
            ct: hex(&ct),
        };
        Ok(serde_json::to_vec(&env)?)
    }

    pub fn open(&self, user: &str, data: &[u8]) -> Result<Vec<u8>> {
        let env: Envelope = serde_json::from_slice(data).context("parsing encrypted template")?;
        if env.frogger_enc != 1 || env.alg != ALG {
            bail!("unsupported template encryption {}/{}", env.frogger_enc, env.alg);
        }
        let nonce = unhex(&env.nonce)?;
        if nonce.len() != 12 {
            bail!("bad nonce");
        }
        let ct = unhex(&env.ct)?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.0));
        cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ct,
                    aad: user.as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("template cannot be decrypted (wrong key or tampered)"))
    }
}

/// Whether `data` is an encrypted envelope (as opposed to a plain template).
pub fn is_sealed(data: &[u8]) -> bool {
    data.starts_with(b"{\"frogger_enc\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_binding() {
        let k = TemplateKey::generate().unwrap();
        let sealed = k.seal("alice", b"secret template").unwrap();
        assert!(is_sealed(&sealed));
        assert_eq!(k.open("alice", &sealed).unwrap(), b"secret template");
        // Bound to the user name.
        assert!(k.open("bob", &sealed).is_err());
        // Wrong key.
        assert!(TemplateKey::generate().unwrap().open("alice", &sealed).is_err());
        // Tampering.
        let mut t = sealed.clone();
        let i = t.len() - 4;
        t[i] = if t[i] == b'0' { b'1' } else { b'0' };
        assert!(k.open("alice", &t).is_err());
    }

    #[test]
    fn nonces_differ() {
        let k = TemplateKey::generate().unwrap();
        assert_ne!(k.seal("a", b"x").unwrap(), k.seal("a", b"x").unwrap());
    }

    #[test]
    fn key_length_enforced() {
        assert!(TemplateKey::from_bytes(&[0u8; 16]).is_err());
        assert!(TemplateKey::from_bytes(&[0u8; 32]).is_ok());
    }
}
