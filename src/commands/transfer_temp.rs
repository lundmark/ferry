use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const NONCE_BYTES: usize = 16;
const NONCE_HEX_LEN: usize = NONCE_BYTES * 2;
const MARKER: &str = ".ferry-tmp.";
const REMOTE_MARKER: &str = "ferry-tmp.";

pub(crate) fn fresh_nonce() -> Result<String> {
    let mut bytes = [0_u8; NONCE_BYTES];
    getrandom::fill(&mut bytes).context("generating transfer temp name")?;
    let mut nonce = String::with_capacity(NONCE_HEX_LEN);
    for byte in bytes {
        nonce.push(hex_digit(byte >> 4));
        nonce.push(hex_digit(byte & 0x0f));
    }
    Ok(nonce)
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'a' + nibble - 10) as char,
        _ => unreachable!("nibble is masked to four bits"),
    }
}

pub(crate) fn fresh_local_candidate(target: &Path) -> Result<PathBuf> {
    local_candidate(target, &fresh_nonce()?)
}

pub(crate) fn local_candidate(target: &Path, nonce: &str) -> Result<PathBuf> {
    validate_nonce(nonce)?;
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("transfer target {} has no parent", target.display()))?;
    let leaf = target
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("transfer target {} has no file name", target.display()))?;
    let mut temp_leaf = OsString::from(".");
    temp_leaf.push(leaf);
    temp_leaf.push(MARKER);
    temp_leaf.push(nonce);
    Ok(parent.join(temp_leaf))
}

pub(crate) fn fresh_remote_candidate(target: &str) -> Result<String> {
    remote_candidate(target, &fresh_nonce()?)
}

pub(crate) fn remote_candidate(target: &str, nonce: &str) -> Result<String> {
    validate_nonce(nonce)?;
    let (parent, leaf) = match target.rsplit_once('/') {
        Some((parent, leaf)) => (Some(parent), leaf),
        None => (None, target),
    };
    if leaf.is_empty() {
        anyhow::bail!("remote transfer target has no file name");
    }
    let temp_leaf = format!("{REMOTE_MARKER}{nonce}");
    Ok(match parent {
        Some("") => format!("/{temp_leaf}"),
        Some(parent) => format!("{parent}/{temp_leaf}"),
        None => temp_leaf,
    })
}

pub(crate) fn is_reserved_local_transfer_temp(relative: &str) -> bool {
    let Some(leaf) = relative.rsplit('/').next() else {
        return false;
    };
    let Some(body) = leaf.strip_prefix('.') else {
        return false;
    };
    let Some((target, nonce)) = body.rsplit_once(MARKER) else {
        return false;
    };
    !target.is_empty() && valid_nonce(nonce)
}

pub(crate) fn is_reserved_remote_transfer_temp(relative: &str) -> bool {
    let Some(leaf) = relative.rsplit('/').next() else {
        return false;
    };
    leaf.strip_prefix(REMOTE_MARKER).is_some_and(valid_nonce)
        || is_reserved_local_transfer_temp(relative)
}

#[cfg(test)]
pub(crate) fn is_reserved_transfer_temp(relative: &str) -> bool {
    is_reserved_local_transfer_temp(relative) || is_reserved_remote_transfer_temp(relative)
}

fn validate_nonce(nonce: &str) -> Result<()> {
    if !valid_nonce(nonce) {
        anyhow::bail!("invalid transfer temp nonce");
    }
    Ok(())
}

fn valid_nonce(nonce: &str) -> bool {
    nonce.len() == NONCE_HEX_LEN
        && nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::{
        is_reserved_local_transfer_temp, is_reserved_remote_transfer_temp,
        is_reserved_transfer_temp, local_candidate, remote_candidate,
    };
    use std::path::Path;

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn candidates_are_reserved_siblings() {
        let local = local_candidate(Path::new("area/page.txt"), NONCE).unwrap();
        let remote = remote_candidate("/remote/area/page.txt", NONCE).unwrap();

        assert_eq!(
            local,
            Path::new("area/.page.txt.ferry-tmp.0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            remote,
            "/remote/area/ferry-tmp.0123456789abcdef0123456789abcdef"
        );
        assert!(is_reserved_transfer_temp(local.to_str().unwrap()));
        assert!(is_reserved_transfer_temp(&remote));
        assert!(is_reserved_local_transfer_temp(local.to_str().unwrap()));
        assert!(!is_reserved_local_transfer_temp(&remote));
        assert!(is_reserved_remote_transfer_temp(&remote));
        assert!(is_reserved_remote_transfer_temp(
            "/remote/area/.page.txt.ferry-tmp.0123456789abcdef0123456789abcdef"
        ));
    }

    #[test]
    fn remote_candidate_is_visible_fixed_length_for_dotfiles_and_long_targets() {
        let dotfile = remote_candidate("/remote/.env", NONCE).unwrap();
        let long_target = format!("/remote/{}", "x".repeat(255));
        let long = remote_candidate(&long_target, NONCE).unwrap();

        assert_eq!(
            dotfile,
            "/remote/ferry-tmp.0123456789abcdef0123456789abcdef"
        );
        assert_eq!(long, dotfile);
        assert_eq!(long.rsplit('/').next().unwrap().len(), 42);
    }

    #[test]
    fn recognizer_rejects_near_misses() {
        for name in [
            ".page.txt.ferry-tmp.0123456789abcdef0123456789abcde",
            ".page.txt.ferry-tmp.0123456789abcdef0123456789abcdeF",
            "page.txt.ferry-tmp.0123456789abcdef0123456789abcdef",
            "..ferry-tmp.0123456789abcdef0123456789abcdef",
            "ferry-tmp.0123456789abcdef0123456789abcde",
            "ferry-tmp.0123456789abcdef0123456789abcdeF",
            "xferry-tmp.0123456789abcdef0123456789abcdef",
        ] {
            assert!(!is_reserved_transfer_temp(name), "{name}");
        }
    }
}
