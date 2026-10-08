//! The plan between runs: `plan.md` as shown, `plan.json` as built, and the hash tying them.
//!
//! Both live in `dirs::state_dir()/tt-devpro/`. `plan.md` is the text Yurii was shown and the
//! file the agent edits in place. `plan.json` holds the structured plan — worklog ids, Chrono
//! keys, dates, everything the table does not show — and the SHA-256 of the `plan.md` that was
//! printed. `--apply` writes only when `plan.md` still hashes to that value, which is what
//! makes "nothing is written that was not shown" a property of the code rather than of the
//! agent's discipline.
//!
//! One plan at a time: a new `settle` replaces both files. This is one person's tool.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::Plan;

const MD_FILE: &str = "plan.md";
const JSON_FILE: &str = "plan.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Stored {
    plan: Plan,
    shown_sha256: String,
}

/// What a later `--replan` or `--apply` reads back.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub plan: Plan,
    pub shown_sha256: String,
    /// `plan.md` as it is now, possibly edited.
    pub md: String,
}

impl Loaded {
    pub fn md_unchanged(&self) -> bool {
        sha256_hex(&self.md) == self.shown_sha256
    }
}

pub struct State {
    dir: PathBuf,
}

impl State {
    /// The state directory of this machine. There is no fallback location: on a platform
    /// with no state directory (macOS) the run stops and says so, rather than leaving the
    /// plan somewhere the next run would not look.
    pub fn locate() -> Result<Self> {
        let base = dirs::state_dir().ok_or_else(|| {
            anyhow!(
                "This platform has no state directory (XDG_STATE_HOME / ~/.local/state), so \
                 there is nowhere to keep the plan between settle and --apply"
            )
        })?;
        Ok(Self::at(base.join("tt-devpro")))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn md_path(&self) -> PathBuf {
        self.dir.join(MD_FILE)
    }

    fn json_path(&self) -> PathBuf {
        self.dir.join(JSON_FILE)
    }

    /// Stores the plan and the text shown for it. `plan.json` goes first: a crash between the
    /// two leaves a new hash beside an old `plan.md`, which `--apply` refuses, rather than an
    /// old hash beside a new text, which it might not.
    pub fn save(&self, plan: &Plan, shown: &str) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let stored = Stored {
            plan: plan.clone(),
            shown_sha256: sha256_hex(shown),
        };
        let json = serde_json::to_string_pretty(&stored).context("serialising the plan")?;
        write_atomically(&self.json_path(), &json)?;
        write_atomically(&self.md_path(), shown)
    }

    pub fn load(&self) -> Result<Loaded> {
        let json_path = self.json_path();
        if !json_path.exists() {
            return Err(anyhow!(
                "There is no plan to work on ({} does not exist). Run `tt-devpro settle` first.",
                json_path.display()
            ));
        }
        let json = std::fs::read_to_string(&json_path)
            .with_context(|| format!("reading {}", json_path.display()))?;
        let stored: Stored = serde_json::from_str(&json)
            .with_context(|| format!("parsing {}", json_path.display()))?;
        let md_path = self.md_path();
        let md = std::fs::read_to_string(&md_path)
            .with_context(|| format!("reading {}", md_path.display()))?;
        Ok(Loaded {
            plan: stored.plan,
            shown_sha256: stored.shown_sha256,
            md,
        })
    }

    /// Removes both files; a missing one is not an error.
    pub fn clear(&self) -> Result<()> {
        for path in [self.json_path(), self.md_path()] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("removing {}", path.display()));
                }
            }
        }
        Ok(())
    }
}

pub fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Writes beside the target and renames over it, so a reader sees the old file or the new
/// one and never half of either.
fn write_atomically(path: &Path, content: &str) -> Result<()> {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, content).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::fixtures::plan;
    use tempfile::TempDir;

    #[test]
    fn a_saved_plan_loads_back_with_its_text_unchanged() {
        let dir = TempDir::new().unwrap();
        let state = State::at(dir.path().join("tt-devpro"));
        state.save(&plan(), "shown text").unwrap();

        let loaded = state.load().unwrap();
        assert_eq!(loaded.plan, plan());
        assert_eq!(loaded.md, "shown text");
        assert!(loaded.md_unchanged());
    }

    #[test]
    fn an_edited_plan_md_no_longer_matches_its_hash() {
        let dir = TempDir::new().unwrap();
        let state = State::at(dir.path());
        state.save(&plan(), "shown text").unwrap();
        std::fs::write(state.md_path(), "edited text").unwrap();
        assert!(!state.load().unwrap().md_unchanged());
    }

    #[test]
    fn loading_with_nothing_saved_says_to_settle_first() {
        let dir = TempDir::new().unwrap();
        let error = State::at(dir.path()).load().unwrap_err().to_string();
        assert!(error.contains("Run `tt-devpro settle` first"), "{error}");
    }

    #[test]
    fn clearing_removes_both_files_and_tolerates_a_missing_one() {
        let dir = TempDir::new().unwrap();
        let state = State::at(dir.path());
        state.save(&plan(), "x").unwrap();
        state.clear().unwrap();
        state.clear().unwrap();
        assert!(state.load().is_err());
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn the_hash_is_lowercase_hex_sha256() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
