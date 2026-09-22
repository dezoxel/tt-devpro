//! Portal session cookie resolution — C20.
//!
//! Ports `Main.kt:21-40`. Refreshing the cookie needs a host-side browser login
//! (`make auth`) that drives a GUI browser through Playwright, so this module only
//! ever reads what is already saved.
//!
//! Live parity cannot reach any of this. The run has a valid cookie on disk, and
//! every other branch needs it removed — so the unit tests below are the only
//! oracle C20 has. That is why the resolution is a pure function of its two inputs
//! and the I/O sits in a thin wrapper above it.

use anyhow::{Result, anyhow, bail};
use std::path::PathBuf;

pub const COOKIE_ENV: &str = "TT_COOKIE";

/// `Main.kt:22`. `dirs::home_dir()` rather than the JVM's `user.home` — see C30
/// for why the two differ only under an overridden `$HOME`.
pub fn cookie_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("Could not determine the home directory"))?;
    Ok(home.join(".tt-cookie"))
}

pub fn session_cookie() -> Result<String> {
    let path = cookie_path()?;
    let file_contents = if path.exists() {
        std::fs::read_to_string(&path).ok()
    } else {
        None
    };
    resolve(file_contents.as_deref(), std::env::var(COOKIE_ENV).ok().as_deref())
}

/// The whole of `Main.kt:24-39` once the two lookups are parameters.
///
/// The asymmetry in the middle is the part a port loses: `Main.kt:26` calls
/// `.trim()` on the file contents and `:34` does not call it on the environment
/// value, so `TT_COOKIE=" abc "` is used with its spaces intact. It is reproduced
/// rather than tidied up, because a cookie header is sent verbatim and "obviously
/// harmless whitespace" is exactly the kind of difference that turns into a 401
/// nobody can explain.
pub fn resolve(file_contents: Option<&str>, env_value: Option<&str>) -> Result<String> {
    if let Some(contents) = file_contents {
        let trimmed = contents.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    if let Some(value) = env_value {
        if !value.is_empty() {
            return Ok(value.to_string());
        }
    }

    bail!("No Dev.Pro session cookie found. Run 'make auth' on your host machine to create one.")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C20, case 1. `Main.kt:25-30`: the file wins whenever it has content.
    #[test]
    fn the_file_wins_over_the_environment() {
        let cookie = resolve(Some("from-file"), Some("from-env")).expect("resolves");
        assert_eq!(cookie, "from-file");
    }

    /// C20, case 2. `Main.kt:26-27`: the file is read, trimmed, and only then
    /// tested for emptiness — so a file holding nothing but whitespace falls
    /// through rather than returning an empty cookie.
    #[test]
    fn a_whitespace_only_file_falls_through_to_the_environment() {
        let cookie = resolve(Some("  \n\t "), Some("from-env")).expect("resolves");
        assert_eq!(cookie, "from-env");
    }

    #[test]
    fn an_empty_file_falls_through_to_the_environment() {
        let cookie = resolve(Some(""), Some("from-env")).expect("resolves");
        assert_eq!(cookie, "from-env");
    }

    /// A missing file is a different input from an empty one, and both reach the
    /// environment.
    #[test]
    fn a_missing_file_falls_through_to_the_environment() {
        let cookie = resolve(None, Some("from-env")).expect("resolves");
        assert_eq!(cookie, "from-env");
    }

    /// C20, case 3. `Main.kt:26`: the file value is trimmed before it is returned,
    /// so the surrounding newline `make auth` leaves behind never reaches a header.
    #[test]
    fn the_file_value_is_trimmed() {
        let cookie = resolve(Some("\n  SESSION=abc123  \n"), None).expect("resolves");
        assert_eq!(cookie, "SESSION=abc123");
    }

    /// C20, case 4 — the one a port loses. `Main.kt:34` tests the environment
    /// value for emptiness **without** trimming it, and `:35` returns it as it
    /// came. The asymmetry against the file branch is real behaviour, not a
    /// transcription slip.
    #[test]
    fn the_environment_value_is_not_trimmed() {
        let cookie = resolve(None, Some(" SESSION=abc123 ")).expect("resolves");
        assert_eq!(cookie, " SESSION=abc123 ");
    }

    /// And the corollary: whitespace makes the environment value non-empty, so it
    /// is returned rather than falling through to the error.
    #[test]
    fn a_whitespace_only_environment_value_is_still_a_cookie() {
        let cookie = resolve(None, Some(" ")).expect("resolves");
        assert_eq!(cookie, " ");
    }

    /// `Main.kt:34` tests `isNotEmpty()`, so an empty string is skipped even
    /// though the variable is set.
    #[test]
    fn an_empty_environment_value_is_skipped() {
        let err = resolve(None, Some("")).expect_err("must not resolve");
        assert!(err.to_string().contains("make auth"));
    }

    /// C20, case 5. `Main.kt:39` — the message names the command that fixes it,
    /// and that wording is what the operator acts on.
    #[test]
    fn neither_source_gives_the_make_auth_instruction() {
        let err = resolve(None, None).expect_err("must not resolve");
        assert_eq!(
            err.to_string(),
            "No Dev.Pro session cookie found. Run 'make auth' on your host machine to create one."
        );
    }

    /// Both sources empty is the same failure as neither being present.
    #[test]
    fn an_empty_file_and_an_empty_environment_value_fail_together() {
        assert!(resolve(Some("   "), Some("")).is_err());
    }

    /// A cookie is `name=value` and may carry `=` in the value; nothing here may
    /// split or re-encode it.
    #[test]
    fn the_value_is_returned_verbatim_apart_from_the_file_trim() {
        let raw = "SESSION=eyJhbGciOi==; Path=/; Domain=.dev.pro";
        assert_eq!(resolve(Some(raw), None).unwrap(), raw);
        assert_eq!(resolve(None, Some(raw)).unwrap(), raw);
    }

    /// Interior whitespace is not touched — only the ends, and only for the file.
    #[test]
    fn interior_whitespace_survives_the_trim() {
        assert_eq!(
            resolve(Some("  a=1; b=2  "), None).unwrap(),
            "a=1; b=2"
        );
    }
}
