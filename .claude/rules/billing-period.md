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

- The code reads no allocation from the portal; `allocations` in the config is the only source. `fte` is validated in `config.rs`: in (0, 1], one entry per project. Without one, the line shows hours and FTE and says where to set it.
- Used = hours / (`fte` × `expectedHours`). Pace `↗N%` = used / share of the period gone, rounded, capped at `MAX_PACE` (999), shown only from `MIN_DAYS_FOR_PACE` (2) working days. The statusline's 1/33 guard counts minutes; with whole days it never fires, so one full day of eleven would read 200%.
- The bar is `BAR_CELLS` (10) cells, `█` used / `░` rest, with `┃` between cells where the period's clock stands, clamped off both ends.
