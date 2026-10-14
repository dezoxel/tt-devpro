---
paths:
  - src/commands/settle_period.rs
---

# Billing-period block under the settle plan

`settle_period.rs` is pure: it takes the period, the portal days, the stored plan and the `allocations` and returns lines. The reads live in `settle.rs` (`Settle::period` / `period_block`), which `Settle::plan_then_period` runs only after `plan` or `replan` returned `Outcome::Ok` — also under «Все дни закрыты». `dispatch` sends `--apply` elsewhere before it, so the block never follows a write. The settle tests run `plan_then_period` itself (`Run::Settle`), so keep the ordering there rather than back in `dispatch`, which builds real clients and is not tested.

## Printed, never stored

`plan.md` is the edit surface `--apply` hashes; a line that changes every morning has no place in it. Do not route the block through `plan::render` or `State`. The block reads `plan.json`, so hand edits in `plan.md` count only after `--replan`. Its own failure is one `⚠️ Период не прочитан: …` line on stdout and the run still succeeds — do not turn it into an error that fails `settle`.

## Period

- Boundaries come from `GET /api/contact/ptrPeriods?onDate=<through>` (`TtApiClient::get_ptr_periods`), never from a 1–15 / 16–end rule. Labels read `October 01 - 15, 2026`; `parse_label` also reads a label across two months.
- Several periods can hold one date (a 1–17 and a 1–31 in the same month): `choose` takes the shortest. An unreadable label or no period holding the date is an error, not a skip.
- `through` is the last settleable day: yesterday, or today when the stored `plan.cutoff` reaches today (`last_settleable_day`).

## Days

- Total = the portal's `expectedHours / 8`, rounded.
- Days gone = weekdays not in the local 8-holiday predicate (`holidays.rs`), capped at the total. A holiday the portal observes and the local predicate does not (Columbus, Veterans, Presidents' Day) makes "days gone" one high after it. Known and accepted; do not add holidays to `holidays.rs` for this — the eight are deliberate.

## Hours

- DevPro worklogs whose read-side billability is exactly `"Billable"` (`READ_BILLABLE`). The read side spells the other value `Non-billable`, the write side `NonBillable` — never compare against the write-side enum.
- Plus plan lines not yet in DevPro (`kind != Recorded`), dated in `[period.start, through]`. A `Recorded` line is counted from DevPro only.
- A plan day whose DevPro worklogs are no longer exactly its `Recorded` lines is skipped from the plan side — the same `recorded_ids` / `portal_ids` test `--apply` makes before writing a day. Reuse those helpers; do not keep a second copy of the check here.
- A planned line is named by the `project_short_name` DevPro gives its `project_id`, falling back to `devpro_project` only when DevPro has no worklog on that id. That keeps a project the config spells differently as one line.
- Days in `plan.errors` have no lines; the block prints how many fall in `[period.start, through]` and that their planned hours are missing.

## Allocation and pace

- The code reads no allocation from the portal; `allocations` in the config is the only source. Validated in `config.rs`: `fte` in (0, 1], `from` and `to` required and `from ≤ to`, entries of one project never on the same day (checked pairwise, any order). Several entries per project is how a mid-period FTE change is written. `Allocation::holds` is the one reader of the dates.
- `target` (`settle_period.rs`) counts the allocation by day: each local working day of the period weighs the `fte` in force on it (0 without one). Allocated = `expectedHours` × Σfte / N working days — at one fte throughout that is `fte × expectedHours`, the formula before entries had dates. The weights use the local predicate's days, so a holiday the portal observes and the predicate does not shifts them by one day, the same accepted drift as «Days gone».
- Due = Σfte of the days up to `through` / Σfte of the period: the share of the target the days gone ask for. Do not go back to the share of time gone: on 9 Oct 2026, 28 h of 52 at 0.5 → 0.75 was exactly on plan and read `↗84%` against time.
- No entry holds any day of the period → no target: the project is listed only if it has hours, with «плановой аллокации нет» and where to set it. An entry that ended before the period is not listed.
- Used = hours / allocated. Pace `↗N%` = used / due, rounded, capped at `MAX_PACE` (999), shown only from `MIN_DAYS_FOR_PACE` (2) working days and when due > 0. The statusline's 1/33 guard counts minutes; with whole days it never fires, so one full day of eleven would read 200%.
- «X FTE из Y»: X = hours / (8 × days gone), Y = mean fte over the days gone. Before the first day an entry holds (due = 0) there is no Y, no `┃` and no pace.
- The bar is `BAR_CELLS` (10) cells, `█` used / `░` rest, with `┃` between cells at due, clamped off both ends.
