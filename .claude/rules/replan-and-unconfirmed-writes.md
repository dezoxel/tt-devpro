---
paths:
  - src/commands/settle.rs
  - src/commands/settle/
  - src/plan/
  - src/service/plan_context.rs
  - src/service/planner.rs
---

# Replan pins and the ledger of unconfirmed writes

## `pinned` is not `edited`

- `PlanLine.pinned` means the title and hours are fixed for the model: meetings, capped overrides, recorded worklogs, and Yurii's edits. `PlanLine.edited` means Yurii changed or added the line in `plan.md`.
- `Settle::day_pins` carries into a replan only lines with `edited`, plus this round's edits. Every other pin is rebuilt from Chrono and DevPro as they are now, so a meeting corrected in Chrono shows its new hours. Do not go back to filtering on `pinned`: that kept stale hours.
- Any new `PlanLine` sets `edited: false` unless it comes from a `plan.md` edit (`edited_line`, the added-row branch of `day_pins`).

## Removed rows stay removed

- `Plan.removed` holds, per day, the `ChronoKey`s Yurii deleted from `plan.md`. The line is gone from `days`, so without this set the next replan of that day would build it from Chrono again.
- A replan writes the set back from `DayPins.removed` for every rebuilt day, and drops the day's entry when the set is empty. A `Plan` built by `plan`, `replay` or the post-`--apply` remainder starts with `BTreeMap::new()`.

## Stored-plan compatibility

`plan.json` from an older build must still load. A new field on `Plan` or `PlanLine` gets `#[serde(default)]`. The state test `a_plan_stored_before_edited_and_removed_existed_loads_with_neither` covers the current two fields; extend it for a new one.

## `unconfirmed.json`

- A write that timed out and was not found in DevPro (or DevPro could not be read to check) is never sent again. `Settle::unconfirmed` records it via `State::record_unconfirmed` with the day's `before_ids` and `written_at` from `Settle.now`, and `--apply` stops.
- The ledger is not part of the plan: `State::clear` removes only `plan.md` and `plan.json`. Keep it that way — a stopped `--apply` drops the plan, and the ledger is the only record of the write.
- `State::unconfirmed` returns an error for a file it cannot read or parse, never an empty list: an empty ledger is what lets a day be planned beside a worklog that landed late.
- Both `plan` and `replan` call `hold_unconfirmed(&days, &portal)` after reading the portal and before `plan_days`, then add the held days to `planned.errors`. A new run that plans days does the same.
- `check_unconfirmed` is pure and takes the clock and zone as arguments. Its rules: an entry on a day DevPro holds at 8.0 h is dropped; an entry on a day not being planned is kept, unless it is older than `SCAN_DAYS` (then dropped) — age never drops an entry whose day is being planned, because an explicit `--from`/`--to` reaches past the scan and `--apply` writes it; a matching new worklog (`is_late_write`: not in `before_ids`, same project id, title and quarters) holds the day and names `tt-devpro api delete-worklog <id>`; no match within `LATE_WRITE_WINDOW` (15 min) holds the day until the window ends; after that the entry is dropped and the day is planned.
- Inside `Settle` the clock is the `now: fn() -> DateTime<Utc>` field, never `Utc::now()` directly, so tests can fix it. Only the constructors in `dispatch` and `replay` pass `Utc::now`.
