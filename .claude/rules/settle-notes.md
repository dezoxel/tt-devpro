---
paths:
  - src/commands/settle.rs
  - src/commands/settle_window.rs
  - src/plan/render.rs
  - src/llm/claude_cli.rs
---

# Signs and day names in settle's notes

The notes are read right under the plan, in the agent's markdown chat, so they follow the plan's conventions rather than the Kotlin bytes.

- **⏳ `NOT_YET` (`settle_window.rs`) — something that is not final yet and needs no action:** a day held back by the cutoff, a range past the last completed day, a write DevPro did not confirm but that is already there, a model retry. Use the constant, never a literal. U+23F3 is an emoji by default and takes no U+FE0F.
- **⚠️ — a day not planned, not written, or left needing a manual step.** Always `\u{26A0}\u{FE0F}`, as every other ⚠️ line in `settle.rs` and `render.rs` is written; the project-id fallback warning was the one bare U+26A0 and was fixed.
- **Never U+2139 (ℹ).** It renders as a lowercase «i» in the terminal font, and «i Skipped» read as a typo.
- ⏳ notes are Russian. Name a day with `plan::render::day_label` («Пт 9 октября»), the label the plan shows it under, not an ISO date. A flag goes in backticks (`` `--include-today` ``), since the note is pasted into markdown.
- The empty-scan message is `render::ALL_CLOSED`, and it is printed only when nothing was held back (`nothing_to_settle_message`); otherwise it is «Планировать пока нечего.» followed by the held-back sentence.
- A held-back day is reported only if `is_workday`: a weekend or a holiday is never planned by any flag, so an `--include-today` hint about a Saturday was advice that could not be followed. `split_by_finality` itself stays unfiltered; the filter is in `Settle`, before the note.
- An unconfirmed write that turns out to be in DevPro gets one ⏳ wording for every cause (timeout, 5xx, lost connection, non-200). The cause stays in the stop texts, where it changes what happens next.
