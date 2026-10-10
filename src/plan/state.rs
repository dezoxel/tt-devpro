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
//!
//! Beside them, `unconfirmed.json` lists the writes `--apply` sent that timed out and were not
//! in DevPro when it looked. Such a write may still land after the run took its day back, and
//! nothing else would remember it. The ledger is not part of the plan: dropping the plan, which
//! a stopped `--apply` does, leaves it in place for the next `settle` to check.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Plan, Quarters};

const MD_FILE: &str = "plan.md";
const JSON_FILE: &str = "plan.json";
const UNCONFIRMED_FILE: &str = "unconfirmed.json";

/// A write that timed out and was not in DevPro when `--apply` looked: the worklog it would
/// make, and the ids the day held before that run, so a later read can tell it apart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnconfirmedWrite {
    pub date: NaiveDate,
    pub project_id: String,
    pub devpro_project: String,
    pub title: String,
    pub quarters: Quarters,
    pub before_ids: Vec<String>,
    pub written_at: DateTime<Utc>,
}

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

    pub fn unconfirmed_path(&self) -> PathBuf {
        self.dir.join(UNCONFIRMED_FILE)
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

    /// Removes both plan files; a missing one is not an error. The ledger of unconfirmed
    /// writes stays.
    pub fn clear(&self) -> Result<()> {
        for path in [self.json_path(), self.md_path()] {
            remove_if_present(&path)?;
        }
        Ok(())
    }

    /// The unconfirmed writes on record. No file is an empty ledger; a file that cannot be
    /// read is an error and never an empty ledger, because an empty one is exactly what lets
    /// a day be planned beside a worklog that landed late.
    pub fn unconfirmed(&self) -> Result<Vec<UnconfirmedWrite>> {
        let path = self.unconfirmed_path();
        let json = match std::fs::read_to_string(&path) {
            Ok(json) => json,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        };
        serde_json::from_str(&json).with_context(|| format!("parsing {}", path.display()))
    }

    /// Replaces the ledger; an empty one removes the file.
    pub fn save_unconfirmed(&self, writes: &[UnconfirmedWrite]) -> Result<()> {
        let path = self.unconfirmed_path();
        if writes.is_empty() {
            return remove_if_present(&path);
        }
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let json = serde_json::to_string_pretty(writes).context("serialising the ledger")?;
        write_atomically(&path, &json)
    }

    pub fn record_unconfirmed(&self, write: UnconfirmedWrite) -> Result<()> {
        let mut writes = self.unconfirmed()?;
        writes.push(write);
        self.save_unconfirmed(&writes)
    }
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
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

    fn unconfirmed(title: &str) -> UnconfirmedWrite {
        UnconfirmedWrite {
            date: crate::plan::fixtures::date(2026, 10, 5),
            project_id: "id-ai".to_string(),
            devpro_project: "AI Practices".to_string(),
            title: title.to_string(),
            quarters: 28,
            before_ids: vec!["w0".to_string()],
            written_at: "2026-10-08T09:00:00Z".parse().unwrap(),
        }
    }

    #[test]
    fn the_ledger_is_recorded_into_a_missing_dir_and_survives_clearing_the_plan() {
        let dir = TempDir::new().unwrap();
        let state = State::at(dir.path().join("tt-devpro"));
        assert!(state.unconfirmed().unwrap().is_empty());

        state.record_unconfirmed(unconfirmed("a")).unwrap();
        state.save(&plan(), "x").unwrap();
        state.record_unconfirmed(unconfirmed("b")).unwrap();
        state.clear().unwrap();

        assert_eq!(
            state.unconfirmed().unwrap(),
            vec![unconfirmed("a"), unconfirmed("b")]
        );
        state.save_unconfirmed(&[]).unwrap();
        assert!(!state.unconfirmed_path().exists());
        state.save_unconfirmed(&[]).unwrap();
    }

    #[test]
    fn a_ledger_that_cannot_be_parsed_is_an_error_naming_its_file() {
        let dir = TempDir::new().unwrap();
        let state = State::at(dir.path());
        std::fs::write(state.unconfirmed_path(), "{not json").unwrap();
        let error = format!("{:#}", state.unconfirmed().unwrap_err());
        assert!(error.contains("unconfirmed.json"), "{error}");
        assert!(state.record_unconfirmed(unconfirmed("a")).is_err());
    }

    #[test]
    fn a_plan_stored_before_edited_and_removed_existed_loads_with_neither() {
        let dir = TempDir::new().unwrap();
        let state = State::at(dir.path());
        state.save(&plan(), "x").unwrap();
        let json = std::fs::read_to_string(state.json_path()).unwrap();
        assert!(!json.contains("\"removed\""), "{json}");
        assert!(json.contains("\"edited\": false,"), "{json}");
        std::fs::write(state.json_path(), json.replace("\"edited\": false,", "")).unwrap();
        let loaded = state.load().unwrap().plan;
        assert_eq!(loaded, plan());
        assert!(loaded.removed.is_empty());
    }

    #[test]
    fn the_hash_is_lowercase_hex_sha256() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
