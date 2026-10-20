//! Encryption of the sync files.
//!
//! The sync folder is usually a cloud folder or a share, so whoever runs it
//! can read what is in it. With a passphrase set, every file is encrypted and
//! authenticated, and nothing about it is readable, not even its name:
//!
//! - Passphrase to key: Argon2id with a fixed, public salt, so every PC that
//!   knows the passphrase derives the same 256-bit key without exchanging
//!   anything. A key file in the llmon home keeps that key (not the
//!   passphrase); `LLMON_SYNC_PASSPHRASE` derives it at start instead and
//!   stores nothing.
//! - File: `salt (16) | nonce (12) | ChaCha20-Poly1305 ciphertext | tag (16)`.
//!   The cipher key is HKDF-SHA256 of the key and the salt, so every file has
//!   its own. There is no magic number, so the file looks like random bytes.
//! - File name: the machine id alone (random, no `llmon`, no extension).
//!
//! A file that does not authenticate is rejected, so the contents of a
//! folder that someone else can write are no longer trusted blindly.

use anyhow::{anyhow, bail, Context, Result};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use ring::hkdf;
use ring::rand::{SecureRandom, SystemRandom};
use std::path::{Path, PathBuf};

/// Derives the key at start; nothing is stored.
pub(crate) const PASSPHRASE_ENV: &str = "LLMON_SYNC_PASSPHRASE";
/// Where the derived key lives, in the llmon home.
pub(crate) const KEY_FILE_NAME: &str = "sync.key";
const KEY_FILE_TAG: &str = "llmon-sync-key-v1";

const KEY_LEN: usize = 32;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = SALT_LEN + NONCE_LEN;
/// Bytes an encrypted file adds to its contents.
pub(crate) const OVERHEAD: usize = HEADER_LEN + TAG_LEN;

/// Public and fixed on purpose: PCs share nothing but the passphrase. Changing
/// it, or the cost below, would make PCs derive different keys.
const KDF_SALT: &[u8] = b"llmon sync passphrase v1";
const KDF_MEMORY_KIB: u32 = 64 * 1024;
const KDF_PASSES: u32 = 3;
const KDF_LANES: u32 = 1;

const FILE_KEY_INFO: &[u8] = b"llmon sync file key v1";
const FILE_AAD: &[u8] = b"llmon sync file v1";

/// The key all PCs of one sync folder share.
#[derive(Clone)]
pub(crate) struct SyncKey([u8; KEY_LEN]);

impl std::fmt::Debug for SyncKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SyncKey(..)")
    }
}

impl PartialEq for SyncKey {
    fn eq(&self, other: &Self) -> bool {
        // No early exit, so the time does not say where the keys differ.
        self.0
            .iter()
            .zip(&other.0)
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
    }
}

impl Eq for SyncKey {}

/// Why a file could not be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenError {
    TooShort,
    Authentication,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => f.write_str("too short to be an encrypted sync file"),
            Self::Authentication => {
                f.write_str("cannot decrypt (wrong passphrase, or not an llmon file)")
            }
        }
    }
}

impl SyncKey {
    /// The key of a passphrase. It takes a fraction of a second on purpose.
    pub(crate) fn from_passphrase(passphrase: &str) -> Result<Self> {
        Self::derive(passphrase, KDF_MEMORY_KIB, KDF_PASSES)
    }

    fn derive(passphrase: &str, memory_kib: u32, passes: u32) -> Result<Self> {
        // A trailing newline comes from a file or a pipe, not from the user.
        let passphrase = passphrase.trim_end_matches(['\r', '\n']);
        if passphrase.is_empty() {
            bail!("The sync passphrase is empty");
        }
        let params = argon2::Params::new(memory_kib, passes, KDF_LANES, Some(KEY_LEN))
            .map_err(|error| anyhow!("Invalid key derivation settings: {error}"))?;
        let kdf = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let mut key = [0_u8; KEY_LEN];
        kdf.hash_password_into(passphrase.as_bytes(), KDF_SALT, &mut key)
            .map_err(|error| anyhow!("Unable to derive the sync key: {error}"))?;
        Ok(Self(key))
    }

    /// The cipher key of one file, from the key and the file's salt.
    fn file_key(&self, salt: &[u8]) -> Option<LessSafeKey> {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&self.0);
        let okm = prk.expand(&[FILE_KEY_INFO], &CHACHA20_POLY1305).ok()?;
        Some(LessSafeKey::new(UnboundKey::from(okm)))
    }

    /// Encrypts `plaintext` into a file's bytes.
    pub(crate) fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let rng = SystemRandom::new();
        let mut salt = [0_u8; SALT_LEN];
        let mut nonce = [0_u8; NONCE_LEN];
        rng.fill(&mut salt)
            .and_then(|()| rng.fill(&mut nonce))
            .map_err(|_| anyhow!("The system has no random source"))?;
        let key = self
            .file_key(&salt)
            .ok_or_else(|| anyhow!("Unable to derive the file key"))?;
        let mut out = Vec::with_capacity(OVERHEAD + plaintext.len());
        out.extend_from_slice(&salt);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(plaintext);
        let tag = key
            .seal_in_place_separate_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(FILE_AAD),
                &mut out[HEADER_LEN..],
            )
            .map_err(|_| anyhow!("Unable to encrypt the sync file"))?;
        out.extend_from_slice(tag.as_ref());
        Ok(out)
    }

    /// Decrypts a file's bytes. Any change to them, or another key, fails.
    pub(crate) fn open(&self, bytes: &[u8]) -> std::result::Result<Vec<u8>, OpenError> {
        if bytes.len() < OVERHEAD {
            return Err(OpenError::TooShort);
        }
        let (salt, rest) = bytes.split_at(SALT_LEN);
        let (nonce, sealed) = rest.split_at(NONCE_LEN);
        let key = self.file_key(salt).ok_or(OpenError::Authentication)?;
        let nonce =
            Nonce::try_assume_unique_for_key(nonce).map_err(|_| OpenError::Authentication)?;
        let mut buffer = sealed.to_vec();
        let plain_len = key
            .open_in_place(nonce, Aad::from(FILE_AAD), &mut buffer)
            .map_err(|_| OpenError::Authentication)?
            .len();
        buffer.truncate(plain_len);
        Ok(buffer)
    }

    /// What `sync.key` holds for this key.
    pub(crate) fn key_file_text(&self) -> String {
        let hex: String = self.0.iter().map(|byte| format!("{byte:02x}")).collect();
        format!("{KEY_FILE_TAG} {hex}\n")
    }

    fn from_key_file_text(text: &str) -> Option<Self> {
        let hex = text.trim().strip_prefix(KEY_FILE_TAG)?.trim();
        if hex.len() != KEY_LEN * 2 || !hex.is_ascii() {
            return None;
        }
        let mut key = [0_u8; KEY_LEN];
        for (byte, pair) in key.iter_mut().zip(hex.as_bytes().chunks(2)) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        Some(Self(key))
    }
}

#[cfg(test)]
impl SyncKey {
    /// The same derivation at a cost tests can afford many times.
    pub(crate) fn for_tests(passphrase: &str) -> Self {
        Self::derive(passphrase, 64, 1).expect("derive")
    }
}

pub(crate) fn key_file_path(llmon_home: &Path) -> PathBuf {
    llmon_home.join(KEY_FILE_NAME)
}

/// The key to encrypt with, if any: the passphrase in `LLMON_SYNC_PASSPHRASE`,
/// else the key file in the llmon home, else none (files stay plain).
pub(crate) fn resolve_key(llmon_home: &Path) -> Result<Option<SyncKey>> {
    let passphrase = match std::env::var_os(PASSPHRASE_ENV) {
        Some(value) => Some(
            value
                .into_string()
                .map_err(|_| anyhow!("{PASSPHRASE_ENV} is not valid UTF-8"))?,
        ),
        None => None,
    };
    resolve_key_from(passphrase.as_deref(), llmon_home)
}

fn resolve_key_from(passphrase: Option<&str>, llmon_home: &Path) -> Result<Option<SyncKey>> {
    let passphrase = passphrase
        .map(|text| text.trim_end_matches(['\r', '\n']))
        .filter(|text| !text.is_empty());
    match passphrase {
        Some(passphrase) => SyncKey::from_passphrase(passphrase).map(Some),
        None => load_key_file(llmon_home),
    }
}

fn load_key_file(llmon_home: &Path) -> Result<Option<SyncKey>> {
    let path = key_file_path(llmon_home);
    crate::storage::enforce_private_file_if_exists(&path)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Unable to read {}", path.display()))
        }
    };
    SyncKey::from_key_file_text(&text)
        .map(Some)
        .with_context(|| {
            format!(
                "{} is not a valid sync key; set the passphrase again",
                path.display()
            )
        })
}

/// Stores the key in the llmon home as a private file. The passphrase is not
/// stored, only the key it derives.
pub(crate) fn save_key_file(llmon_home: &Path, key: &SyncKey) -> Result<PathBuf> {
    let path = key_file_path(llmon_home);
    crate::storage::write_private_file_atomic(&path, key.key_file_text().as_bytes())?;
    Ok(path)
}

/// Deletes the stored key. `false` when there was none.
pub(crate) fn remove_key_file(llmon_home: &Path) -> Result<bool> {
    let path = key_file_path(llmon_home);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            std::fs::remove_file(&path)
                .with_context(|| format!("Unable to remove {}", path.display()))?;
            Ok(true)
        }
        Ok(_) => bail!(
            "Refusing to remove {}: expected a regular file",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("Unable to inspect {}", path.display())),
    }
}

/// Letters and digits without the ones that look alike (0, 1, I, O).
const PASSPHRASE_ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
const PASSPHRASE_GROUPS: usize = 5;
const PASSPHRASE_GROUP_LEN: usize = 5;

/// A random passphrase of 125 bits, written in groups to be read out and typed:
/// `XXXXX-XXXXX-XXXXX-XXXXX-XXXXX`.
pub(crate) fn generate_passphrase() -> Result<String> {
    let mut random = [0_u8; PASSPHRASE_GROUPS * PASSPHRASE_GROUP_LEN];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| anyhow!("The system has no random source"))?;
    let mut text = String::new();
    for (index, byte) in random.iter().enumerate() {
        if index > 0 && index % PASSPHRASE_GROUP_LEN == 0 {
            text.push('-');
        }
        // 256 is a multiple of 32, so the low five bits are uniform.
        text.push(char::from(PASSPHRASE_ALPHABET[usize::from(byte & 31)]));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let unique = format!(
            "{}-{}",
            std::process::id(),
            TEMP_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(format!("llmon-crypto-{prefix}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn cheap_key(passphrase: &str) -> SyncKey {
        SyncKey::for_tests(passphrase)
    }

    #[test]
    fn a_file_opens_with_its_key_and_adds_a_fixed_overhead() {
        let key = cheap_key("synthetic passphrase");
        for size in [0_usize, 1, 100, 1 << 20] {
            let plain = vec![b'x'; size];
            let sealed = key.seal(&plain).expect("seal");
            assert_eq!(sealed.len(), size + OVERHEAD);
            assert_eq!(key.open(&sealed).expect("open"), plain);
        }
    }

    #[test]
    fn every_file_is_sealed_differently_and_hides_its_contents() {
        let key = cheap_key("synthetic passphrase");
        let plain = br#"{"format":"llmon-sync","machine":{"name":"Visible Name"}}"#;
        let first = key.seal(plain).expect("seal");
        let second = key.seal(plain).expect("seal");
        assert_ne!(first, second, "salt and nonce are fresh for every file");
        for sealed in [&first, &second] {
            assert!(!sealed.windows(5).any(|window| window == b"llmon"));
            assert!(!sealed.windows(12).any(|window| window == b"Visible Name"));
        }
    }

    #[test]
    fn another_key_or_any_changed_byte_is_rejected() {
        let key = cheap_key("synthetic passphrase");
        let sealed = key.seal(b"the contents").expect("seal");
        assert_eq!(
            cheap_key("another passphrase").open(&sealed),
            Err(OpenError::Authentication)
        );
        // The salt, the nonce, the first and the last byte (the tag).
        for index in [0, SALT_LEN - 1, SALT_LEN, HEADER_LEN, sealed.len() - 1] {
            let mut changed = sealed.clone();
            changed[index] ^= 0x01;
            assert_eq!(
                key.open(&changed),
                Err(OpenError::Authentication),
                "byte {index}"
            );
        }
        let truncated = &sealed[..sealed.len() - 1];
        assert_eq!(key.open(truncated), Err(OpenError::Authentication));
        assert_eq!(key.open(&sealed[..OVERHEAD - 1]), Err(OpenError::TooShort));
        assert_eq!(key.open(&[]), Err(OpenError::TooShort));
    }

    #[test]
    fn the_key_follows_the_passphrase_and_ignores_a_trailing_newline() {
        let key = cheap_key("synthetic passphrase");
        assert_eq!(key, cheap_key("synthetic passphrase\n"));
        assert_eq!(key, cheap_key("synthetic passphrase\r\n"));
        assert_ne!(key, cheap_key("synthetic passphrase "));
        assert_ne!(key, cheap_key("Synthetic passphrase"));
        assert!(SyncKey::derive("", 64, 1).is_err());
        assert!(SyncKey::derive("\n", 64, 1).is_err());
    }

    #[test]
    fn the_key_is_never_printed() {
        let key = cheap_key("synthetic passphrase");
        assert_eq!(format!("{key:?}"), "SyncKey(..)");
    }

    /// PCs must derive one key from one passphrase, now and after an update:
    /// this fails if the algorithm, the cost or the salt ever changes.
    #[test]
    fn the_passphrase_derives_a_pinned_key() {
        let key = SyncKey::from_passphrase("synthetic passphrase").expect("derive");
        let hex: String = key.0.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(hex, PINNED_KEY_HEX);
    }

    /// A file written by this version stays readable by later ones.
    #[test]
    fn a_pinned_file_still_opens() {
        let key = SyncKey::from_key_file_text(&format!("{KEY_FILE_TAG} {PINNED_KEY_HEX}"))
            .expect("key file");
        let sealed: Vec<u8> = PINNED_FILE_HEX
            .as_bytes()
            .chunks(2)
            .map(|pair| {
                u8::from_str_radix(std::str::from_utf8(pair).expect("ascii"), 16).expect("hex")
            })
            .collect();
        assert_eq!(key.open(&sealed).expect("open"), b"pinned contents");
    }

    #[test]
    fn the_key_comes_from_the_passphrase_first_then_the_key_file() {
        let home = make_temp_dir("resolve");
        assert_eq!(resolve_key_from(None, &home).expect("none"), None);
        assert_eq!(resolve_key_from(Some(""), &home).expect("empty"), None);
        assert_eq!(resolve_key_from(Some("\n"), &home).expect("newline"), None);

        let stored = cheap_key("stored passphrase");
        std::fs::write(key_file_path(&home), stored.key_file_text()).expect("write key file");
        assert_eq!(
            resolve_key_from(None, &home).expect("file"),
            Some(stored.clone())
        );
        assert_eq!(
            resolve_key_from(Some(""), &home).expect("file"),
            Some(stored)
        );

        let from_env = resolve_key_from(Some("env passphrase"), &home)
            .expect("env")
            .expect("a key");
        assert_eq!(
            from_env,
            SyncKey::from_passphrase("env passphrase").expect("derive")
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_broken_key_file_is_an_error_that_names_the_file() {
        let home = make_temp_dir("broken");
        for text in [
            "",
            "garbage",
            "llmon-sync-key-v1 00",
            "llmon-sync-key-v2 0000000000000000000000000000000000000000000000000000000000000000",
            "llmon-sync-key-v1 zz00000000000000000000000000000000000000000000000000000000000000",
        ] {
            std::fs::write(key_file_path(&home), text).expect("write key file");
            let error = resolve_key_from(None, &home).expect_err(text);
            assert!(error.to_string().contains(KEY_FILE_NAME), "{error}");
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[cfg(unix)]
    #[test]
    fn a_key_file_that_others_can_read_is_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let home = make_temp_dir("mode");
        let path = key_file_path(&home);
        std::fs::write(&path, cheap_key("synthetic passphrase").key_file_text())
            .expect("write key file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        resolve_key_from(None, &home).expect("key");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(home);
    }

    const PINNED_KEY_HEX: &str = "08f868aab992819147f9bcd8d6b96efef22bb04c262ff856ed42513bd266a63e";
    const PINNED_FILE_HEX: &str = "33e0bcc5b1f11bcb0e98899cdb9356dffe9b1c2418249c9d9b318e7701d772e7b7b72c734be8438c47bbde019183c72b023f9f6b089ac97c9fc333";
}
