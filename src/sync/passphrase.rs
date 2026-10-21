//! `llmon sync passphrase`: sets, generates or removes the key that encrypts
//! the sync files (see `crypto`). Only the key the passphrase derives is
//! stored, in a private file in the llmon home; the passphrase is not, so a
//! stored key cannot be got back once it is replaced or removed. That is why
//! changing it asks first (`confirm_change`).

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

/// `force` skips the question that is asked when a key is already stored.
pub(crate) fn run(llmon_home: &Path, action: Action, force: bool) -> Result<()> {
    confirm_change(
        llmon_home,
        action,
        force,
        can_ask(action, std::io::stdin().is_terminal()),
        &mut std::io::stdin().lock(),
        &mut std::io::stderr(),
    )?;
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

/// `--stdin` already uses standard input for the passphrase, so only the
/// other actions can ask, and only when someone is there to answer.
fn can_ask(action: Action, stdin_is_terminal: bool) -> bool {
    action != Action::Stdin && stdin_is_terminal
}

/// A stored key is the one the other PCs' files are encrypted with, and it
/// cannot be recovered from this PC once it is gone, so replacing or removing
/// it by accident leaves the PCs unable to read each other's files until the
/// passphrase is set again everywhere. With a key stored, ask first; where
/// nobody can answer, refuse unless `force` is set.
fn confirm_change(
    llmon_home: &Path,
    action: Action,
    force: bool,
    can_ask: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    let path = crypto::key_file_path(llmon_home);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        _ if force => return Ok(()),
        _ => {}
    }
    let (verb, effect) = if action == Action::Remove {
        (
            "Remove",
            "This PC would publish plain files again and ignore the other PCs' encrypted ones, and setting the key back needs the passphrase.",
        )
    } else {
        (
            "Replace",
            "The other PCs' files are encrypted with the passphrase they have. With a different one, this PC and they cannot read each other's files until every PC has the same passphrase.",
        )
    };
    if !can_ask {
        bail!(
            "A sync key is already stored in {}. {effect} Run this in a terminal to confirm, or add --force.",
            path.display()
        );
    }
    write!(
        output,
        "A sync key is already stored in {}.\n{effect}\n{verb} it? [y/N] ",
        path.display()
    )
    .and_then(|()| output.flush())
    .context("Unable to ask for confirmation")?;
    let mut answer = String::new();
    std::io::Read::take(input, MAX_PASSPHRASE_BYTES)
        .read_line(&mut answer)
        .context("Unable to read the answer")?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        bail!("Nothing was changed");
    }
    Ok(())
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

    fn ask(
        home: &Path,
        action: Action,
        force: bool,
        can_ask: bool,
        answer: &str,
    ) -> (Result<()>, String) {
        let mut output = Vec::new();
        let result = confirm_change(
            home,
            action,
            force,
            can_ask,
            &mut answer.as_bytes(),
            &mut output,
        );
        (result, String::from_utf8(output).expect("utf-8 output"))
    }

    #[test]
    fn nothing_is_asked_while_no_key_is_stored() {
        let home = make_temp_dir("ask-none");
        for action in [
            Action::Prompt,
            Action::Generate,
            Action::Stdin,
            Action::Remove,
        ] {
            for can_ask in [true, false] {
                let (result, output) = ask(&home, action, false, can_ask, "");
                result.expect("no key to lose");
                assert_eq!(output, "");
            }
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_stored_key_is_replaced_only_after_a_yes() {
        let home = make_temp_dir("ask-yes");
        store(&home, "a long enough passphrase").expect("store");
        let path = crypto::key_file_path(&home);

        for answer in ["y\n", "Y\n", "yes\r\n", " YES \n", "y"] {
            let (result, output) = ask(&home, Action::Generate, false, true, answer);
            result.unwrap_or_else(|error| panic!("{answer:?}: {error}"));
            assert!(output.contains(&path.display().to_string()), "{output}");
            assert!(output.ends_with("Replace it? [y/N] "), "{output}");
        }
        // Anything else, including no answer at all, keeps the key.
        for answer in ["", "\n", "n\n", "no\n", "yep\n", "ye\n", "please\n"] {
            let (result, _) = ask(&home, Action::Prompt, false, true, answer);
            let error = result.expect_err(answer);
            assert!(error.to_string().contains("Nothing was changed"), "{error}");
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn without_anyone_to_ask_a_stored_key_needs_force() {
        let home = make_temp_dir("ask-force");
        store(&home, "a long enough passphrase").expect("store");

        for action in [
            Action::Prompt,
            Action::Generate,
            Action::Stdin,
            Action::Remove,
        ] {
            // The answer on the input must not be used.
            let (result, output) = ask(&home, action, false, false, "y\n");
            let error = result.expect_err("refused");
            let text = error.to_string();
            assert!(text.contains("--force"), "{text}");
            assert!(text.contains(&crypto::key_file_path(&home).display().to_string()));
            assert_eq!(output, "", "nothing is asked");

            let (result, output) = ask(&home, action, true, false, "");
            result.expect("forced");
            assert_eq!(output, "");
            let (result, output) = ask(&home, action, true, true, "");
            result.expect("forced, and not asked in a terminal either");
            assert_eq!(output, "");
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn removing_the_key_says_what_it_does() {
        let home = make_temp_dir("ask-remove");
        store(&home, "a long enough passphrase").expect("store");
        let (result, output) = ask(&home, Action::Remove, false, true, "n\n");
        assert!(result.is_err());
        assert!(output.contains("plain files again"), "{output}");
        assert!(output.ends_with("Remove it? [y/N] "), "{output}");

        let (_, output) = ask(&home, Action::Generate, false, true, "n\n");
        assert!(output.contains("same passphrase"), "{output}");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn only_a_terminal_can_answer_and_stdin_cannot() {
        assert!(can_ask(Action::Prompt, true));
        assert!(can_ask(Action::Generate, true));
        assert!(can_ask(Action::Remove, true));
        assert!(
            !can_ask(Action::Stdin, true),
            "the pipe holds the passphrase"
        );
        for action in [
            Action::Prompt,
            Action::Generate,
            Action::Stdin,
            Action::Remove,
        ] {
            assert!(!can_ask(action, false));
        }
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
