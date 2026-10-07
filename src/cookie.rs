//! Portal session cookie resolution — C20, reworked.
//!
//! The incumbent (`Main.kt:21-40`) read `~/.tt-cookie` and fell back to `TT_COOKIE`.
//! The cookie no longer lives on disk: `make auth` writes it into 1Password, and
//! `session_cookie` in `~/.config/tt-devpro/config.yaml` holds the secret reference. Every run
//! reads it through `op read`, which asks for approval each time. `TT_COOKIE` still
//! stands in when it is set, and now comes first, since there is no file to prefer.
//!
//! The resolution is a pure function of its inputs ([`resolve`]); the I/O — the
//! environment, the config, the `op` process — sits in the thin wrapper above it.

use anyhow::{Context, Result, bail};
use std::process::Command;

pub const COOKIE_ENV: &str = "TT_COOKIE";

/// The command that issues a fresh cookie, with the repository this binary was built
/// from. Taken from the build rather than written down, so it stays right wherever
/// the repository lives.
#[macro_export]
macro_rules! auth_command {
    () => {
        concat!("make -C ", env!("CARGO_MANIFEST_DIR"), " auth")
    };
}

/// The message for a run that has neither `TT_COOKIE` nor a reference to read.
pub const MISSING_COOKIE: &str = concat!(
    "No Dev.Pro session cookie found. Set session_cookie in ~/.config/tt-devpro/config.yaml to its ",
    "1Password reference and run '",
    auth_command!(),
    "' to issue one."
);

pub fn session_cookie() -> Result<String> {
    let env_value = std::env::var(COOKIE_ENV).ok();
    // The config is read for the reference alone, and only when the environment does
    // not already answer. A missing config file means no reference, so the message is
    // about the cookie, not about the config.
    let reference = match env_value.as_deref() {
        Some(value) if !value.is_empty() => None,
        _ => configured_reference()?,
    };
    resolve(env_value.as_deref(), reference.as_deref(), op_read)
}

fn configured_reference() -> Result<Option<String>> {
    let path = crate::config::config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    Ok(crate::config::load_from(&path)?.session_cookie)
}

/// `op read --no-newline <reference>`. The value goes back to the caller and is never
/// printed; `op`'s own stderr is kept for the error, since it says why the read failed
/// (the approval was dismissed, the item is gone).
fn op_read(reference: &str) -> Result<String> {
    let output = Command::new("op")
        .args(["read", "--no-newline", reference])
        .output()
        .context("running `op` (the 1Password CLI) to read the session cookie")?;
    if !output.status.success() {
        bail!(
            "1Password could not read {reference}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("the session cookie in 1Password is not UTF-8")
}

/// The whole resolution once the three lookups are parameters.
///
/// `TT_COOKIE` is used as it came, spaces included: a cookie header is sent verbatim,
/// and "obviously harmless whitespace" is the kind of difference that turns into a
/// 401 nobody can explain. The value from 1Password is trimmed, because the item is
/// edited by hand as well as by `make auth`.
pub fn resolve(
    env_value: Option<&str>,
    reference: Option<&str>,
    read: impl FnOnce(&str) -> Result<String>,
) -> Result<String> {
    if let Some(value) = env_value {
        if !value.is_empty() {
            return Ok(value.to_string());
        }
    }

    let Some(reference) = reference else {
        bail!(MISSING_COOKIE)
    };
    let value = read(reference)?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!(
            "{reference} is empty. Run '{}' to issue a session cookie.",
            auth_command!()
        );
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFERENCE: &str = "op://Dev.Pro/TT DevPro Session/credential";

    fn never_read(_: &str) -> Result<String> {
        panic!("the reference must not be read")
    }

    #[test]
    fn the_environment_wins_and_1password_is_not_asked() {
        let cookie = resolve(Some("from-env"), Some(REFERENCE), never_read).expect("resolves");
        assert_eq!(cookie, "from-env");
    }

    /// The environment value is not trimmed: whitespace makes it non-empty, and it is
    /// sent as it came.
    #[test]
    fn the_environment_value_is_not_trimmed() {
        let cookie = resolve(Some(" SESSION=abc123 "), None, never_read).expect("resolves");
        assert_eq!(cookie, " SESSION=abc123 ");
    }

    #[test]
    fn an_empty_environment_value_falls_through_to_1password() {
        let cookie = resolve(Some(""), Some(REFERENCE), |reference| {
            assert_eq!(reference, REFERENCE);
            Ok("SESSION=abc123\n".to_string())
        })
        .expect("resolves");
        assert_eq!(cookie, "SESSION=abc123");
    }

    #[test]
    fn neither_source_names_the_setting_and_the_auth_command() {
        let message = resolve(None, None, never_read)
            .expect_err("must fail")
            .to_string();
        assert_eq!(message, MISSING_COOKIE);
        assert!(message.contains("session_cookie"), "{message}");
        assert!(message.contains(env!("CARGO_MANIFEST_DIR")), "{message}");
    }

    #[test]
    fn a_failed_read_is_reported_as_it_came() {
        let message = resolve(None, Some(REFERENCE), |_| {
            bail!("authorization prompt dismissed")
        })
        .expect_err("must fail")
        .to_string();
        assert_eq!(message, "authorization prompt dismissed");
    }

    #[test]
    fn an_empty_item_asks_for_auth() {
        let message = resolve(None, Some(REFERENCE), |_| Ok("  \n".to_string()))
            .expect_err("must fail")
            .to_string();
        assert!(message.contains(auth_command!()), "{message}");
    }

    /// A cookie is `name=value` and may carry `=` in the value; nothing here may
    /// split or re-encode it.
    #[test]
    fn the_value_is_returned_verbatim_apart_from_the_trim() {
        let raw = "SESSION=eyJhbGciOi==; Path=/; Domain=.dev.pro";
        assert_eq!(resolve(Some(raw), None, never_read).unwrap(), raw);
        assert_eq!(
            resolve(None, Some(REFERENCE), |_| Ok(raw.to_string())).unwrap(),
            raw
        );
    }
}
