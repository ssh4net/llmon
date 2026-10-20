//! `llmon sync passphrase`: sets, generates or removes the key that encrypts
//! the sync files (see `crypto`). Only the key the passphrase derives is
//! stored, in a private file in the llmon home; the passphrase is not.

use super::crypto::{self, SyncKey};
use anyhow::{bail, Context, Result};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

/// Short enough to guess is short enough to try offline against a file in a
/// cloud folder; `--generate` makes a strong one.
const MIN_PASSPHRASE_CHARS: usize = 12;
/// A pipe is not a place for megabytes.
const MAX_PASSPHRASE_BYTES: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// Ask for the passphrase twice, without echo.
    Prompt,
    /// Read the passphrase from the first line of standard input.
    Stdin,
    /// Make a random passphrase, show it once, and store its key.
    Generate,
    /// Delete the stored key.
    Remove,
}

pub(crate) fn run(llmon_home: &Path, action: Action) -> Result<()> {
    match action {
        Action::Remove => remove(llmon_home),
        Action::Generate => {
            let passphrase = crypto::generate_passphrase()?;
            let path = store(llmon_home, &passphrase)?;
            // Only the passphrase goes to standard output, so it can be piped.
            println!("{passphrase}");
            eprintln!("Generated passphrase above: write it down (a password manager is a good place); it is not stored and cannot be shown again.");
            finish_set(&path);
            Ok(())
        }
        Action::Stdin => {
            let passphrase = read_line(&mut std::io::stdin().lock())?;
            let path = store(llmon_home, &passphrase)?;
            finish_set(&path);
            Ok(())
        }
        Action::Prompt => {
            if !std::io::stdin().is_terminal() {
                bail!("Standard input is not a terminal; use --stdin to read the passphrase from a pipe");
            }
            let passphrase = read_hidden("Sync passphrase: ")?;
            check_length(&passphrase)?;
            if read_hidden("Repeat it: ")? != passphrase {
                bail!("The two passphrases differ; nothing was changed");
            }
            let path = store(llmon_home, &passphrase)?;
            finish_set(&path);
            Ok(())
        }
    }
}

fn check_length(passphrase: &str) -> Result<()> {
    if passphrase.chars().count() < MIN_PASSPHRASE_CHARS {
        bail!(
            "The passphrase needs at least {MIN_PASSPHRASE_CHARS} characters (llmon sync passphrase --generate makes a strong one)"
        );
    }
    Ok(())
}

/// Derives the key and stores it. Returns the key file's path.
fn store(llmon_home: &Path, passphrase: &str) -> Result<std::path::PathBuf> {
    check_length(passphrase)?;
    let key = SyncKey::from_passphrase(passphrase)?;
    crypto::save_key_file(llmon_home, &key)
}

fn finish_set(path: &Path) {
    eprintln!(
        "Stored the key in {} (readable by you only; the passphrase itself is not stored).",
        path.display()
    );
    eprintln!("Set the same passphrase on your other PCs with: llmon sync passphrase");
    note_environment();
}

fn remove(llmon_home: &Path) -> Result<()> {
    if crypto::remove_key_file(llmon_home)? {
        eprintln!(
            "Removed {}. This PC publishes plain files again; it removes its encrypted file when it does.",
            crypto::key_file_path(llmon_home).display()
        );
    } else {
        eprintln!(
            "There is no stored key at {}.",
            crypto::key_file_path(llmon_home).display()
        );
    }
    note_environment();
    Ok(())
}

/// The environment variable wins over the key file, which is easy to forget.
fn note_environment() {
    if std::env::var_os(crypto::PASSPHRASE_ENV).is_some_and(|value| !value.is_empty()) {
        eprintln!(
            "Note: {} is set in this environment and takes precedence over the stored key.",
            crypto::PASSPHRASE_ENV
        );
    }
}

/// The first line of `reader`, without its line ending.
fn read_line(reader: &mut impl BufRead) -> Result<String> {
    let mut line = String::new();
    std::io::Read::take(reader, MAX_PASSPHRASE_BYTES)
        .read_line(&mut line)
        .context("Unable to read the passphrase")?;
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        bail!("The passphrase is empty");
    }
    Ok(line.to_string())
}

/// Reads a line from the terminal without echoing it.
fn read_hidden(prompt: &str) -> Result<String> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal;

    let mut stderr = std::io::stderr();
    write!(stderr, "{prompt}").and_then(|()| stderr.flush())?;
    terminal::enable_raw_mode().context("Unable to read from the terminal")?;
    let result = (|| -> Result<String> {
        let mut text = String::new();
        loop {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Enter => return Ok(text),
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char('c' | 'd') if control => bail!("Cancelled; nothing was changed"),
                KeyCode::Char(ch) if !control => text.push(ch),
                _ => {}
            }
        }
    })();
    let _ = terminal::disable_raw_mode();
    let _ = writeln!(stderr);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_temp_dir(prefix: &str) -> std::path::PathBuf {
        let unique = format!(
            "{}-{}",
            std::process::id(),
            TEMP_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(format!("llmon-passphrase-{prefix}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn the_first_line_is_the_passphrase() {
        assert_eq!(
            read_line(&mut "a passphrase\n".as_bytes()).expect("line"),
            "a passphrase"
        );
        assert_eq!(
            read_line(&mut "with crlf\r\nsecond line".as_bytes()).expect("line"),
            "with crlf"
        );
        assert_eq!(
            read_line(&mut "no newline".as_bytes()).expect("line"),
            "no newline"
        );
        assert!(read_line(&mut "".as_bytes()).is_err());
        assert!(read_line(&mut "\n".as_bytes()).is_err());
    }

    #[test]
    fn a_short_passphrase_is_refused_and_nothing_is_stored() {
        let home = make_temp_dir("short");
        let error = store(&home, "too short").expect_err("short");
        assert!(error.to_string().contains("at least 12"), "{error}");
        assert!(!crypto::key_file_path(&home).exists());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_stored_key_is_the_one_the_passphrase_derives() {
        let home = make_temp_dir("store");
        let path = store(&home, "a long enough passphrase").expect("store");
        assert_eq!(path, crypto::key_file_path(&home));
        let text = std::fs::read_to_string(&path).expect("read key file");
        assert!(
            !text.contains("passphrase"),
            "the passphrase itself is not stored"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let stored = crypto::resolve_key(&home).expect("resolve");
        let derived = SyncKey::from_passphrase("a long enough passphrase").expect("derive");
        assert_eq!(stored, Some(derived));

        // Setting it again replaces the key.
        store(&home, "another long passphrase").expect("store again");
        assert_ne!(
            crypto::resolve_key(&home).expect("resolve"),
            Some(SyncKey::from_passphrase("a long enough passphrase").expect("derive"))
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_key_can_be_removed_once() {
        let home = make_temp_dir("remove");
        assert!(!crypto::remove_key_file(&home).expect("nothing to remove"));
        store(&home, "a long enough passphrase").expect("store");
        assert!(crypto::remove_key_file(&home).expect("remove"));
        assert!(!crypto::key_file_path(&home).exists());
        assert!(!crypto::remove_key_file(&home).expect("already gone"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn only_a_regular_file_is_removed() {
        let home = make_temp_dir("remove-dir");
        std::fs::create_dir(crypto::key_file_path(&home)).expect("make a directory");
        assert!(crypto::remove_key_file(&home).is_err());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_generated_passphrase_is_long_random_and_typeable() {
        let first = crypto::generate_passphrase().expect("generate");
        let second = crypto::generate_passphrase().expect("generate");
        assert_ne!(first, second);
        assert_eq!(first.len(), 5 * 5 + 4, "five groups of five, with dashes");
        for group in first.split('-') {
            assert_eq!(group.len(), 5, "{first}");
            assert!(
                group
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || (b'2'..=b'9').contains(&byte)),
                "{first}"
            );
            assert!(!group.contains(['I', 'O']), "{first}");
        }
        assert!(first.chars().count() >= MIN_PASSPHRASE_CHARS);
        let home = make_temp_dir("generated");
        store(&home, &first).expect("a generated passphrase is accepted");
        let _ = std::fs::remove_dir_all(home);
    }
}
