//! The model's half of a plan: titles and hours for the free lines, and the extra lines that
//! fill each day to 8 h.
//!
//! One call covers every day that needs the model, so it sees the neighbouring days and words
//! the same recurring topic differently on each. The answer is checked here, in code, against
//! every rule the arithmetic depends on; a broken answer gets one retry with the list of what
//! it broke, and then the run stops. Nothing is rounded or trimmed into shape: a figure the
//! model did not choose and Yurii did not see would end up in DevPro.

use std::collections::HashSet;

use anyhow::{Result, bail};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::llm::{ModelCall, PlanModel};
use crate::plan::render::hours;
use crate::plan::{
    DAY_QUARTERS, DayPlan, LineKind, PlanLine, Quarters, hours_to_quarters, quarters_to_hours,
};
use crate::service::plan_context::{
    Candidate, CandidateSource, Context, DayContext, is_plain_ascii,
};

/// The longest title accepted; a worklog title is a line, not a description.
pub const MAX_TITLE_CHARS: usize = 80;

const RULES: &str = r#"You plan the worklogs of one software architect for his employer's time-tracking portal (DevPro), from his own time tracker (Chrono). Every day must come to exactly 8.0 hours. Pinned lines of a day are already fixed and shown for context only; you answer for the day's "lines" and add "extra" lines.

TITLES
- These worklogs are legal records. They are an addendum to the contract, payment rests on them, and they are the first thing read if anyone asks what the work was. Never write anything that could be used against the author or his employer: no internal judgments of the client or its people, no blame, risks, disputes, negotiation positions, defensive preparation, complaints, or "what to reply to X". Such work gets a neutral professional name: "Thinking how to answer John's gaslighting" becomes "Strategy alignment".
- Apart from that, stay concrete and close to the Chrono description. Keep the specifics that say what was done: systems, tools, repositories, tickets, documents (AWS, MBO, swe-ai-in-sdlc-tools, AI-792). Tidy the wording and drop filler such as "this week", but do not make it vaguer: "Get the Inveniam AWS account confirmed and put MFA on it" stays about that, not "Configure cloud account access".
- English, one line, at most 80 characters. Translate a description in another language.
- People's names are fine where neutral (a colleague's PR reviewed, a candidate evaluated); leave a name out where it would carry a judgment.
- Never copy a title from recent_devpro_titles. Recurring work gets a fresh wording with the same meaning every day.
- A vague description ("Call follow-up", "Review") is expanded from the lines around it in the day's timeline. If nothing there explains it, still write your best title and set needs_detail to true.
- Plain ASCII only.

HOURS
- Every hours value is a multiple of 0.25 and at least 0.25.
- A day's lines and extra lines together come to exactly its hours_to_fill.
- Find the day's main work: the line that is its substantive work (design, analysis, building, writing, review), usually the one with the most chrono_hours. Communication, admin, inbox processing and short preparation are never the main work.
- Every other line is stretched only as far as it plausibly took, roughly within 2x its chrono_hours; a two-minute admin note stays small.
- The main work line takes everything else, however large that makes it: 0.58 h of chrono can become 5.5 h.
- Only when the day has no substantive work line at all, add one extra line of kind "main" instead: the main topic of the last days, picked from the candidates with source "history" (usually the one with the most recent_hours). At most one main line per day.
- An extra line's title names its candidate's topic, never the work of another candidate or another project.
- Lines of kind "borrow" (other history candidates) and "filler" (filler candidates, each within its max_hours) are a last resort, for a day with no substantive work line and no history candidate to carry it. Borrow and filler lines of a day together stay within max_synthetic_hours.
- If the lines alone already exceed hours_to_fill, shrink them in proportion and add no extra lines.

ANSWER
- One entry per day of the input, with its date as given.
- Every key of the day's lines exactly once.
- Each extra line names its candidate by candidate_id; use a candidate at most once per day.

INPUT
"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    days: Vec<DayAnswer>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DayAnswer {
    date: String,
    lines: Vec<LineAnswer>,
    extra: Vec<ExtraAnswer>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LineAnswer {
    key: String,
    title: String,
    hours: f64,
    needs_detail: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExtraKind {
    Main,
    Borrow,
    Filler,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtraAnswer {
    kind: ExtraKind,
    candidate_id: String,
    title: String,
    hours: f64,
}

/// Plans every day of `context` that needs the model and returns them with the days that did
/// not, unnumbered.
pub async fn plan<M: PlanModel>(
    model: &M,
    context: &Context,
    max_synthetic_hours: f64,
) -> Result<Vec<DayPlan>> {
    let mut days = context.ready.clone();
    if context.days.is_empty() {
        return Ok(days);
    }
    let max_synthetic = (max_synthetic_hours * 4.0).floor() as Quarters;
    let prompt = prompt(context, max_synthetic_hours)?;
    let schema = schema().to_string();

    let first = model
        .complete(&ModelCall {
            prompt: prompt.clone(),
            schema: schema.clone(),
        })
        .await?;
    let violations = match validate(context, &first, max_synthetic) {
        Ok(planned) => {
            days.extend(planned);
            return Ok(days);
        }
        Err(violations) => violations,
    };

    let retry = format!(
        "{prompt}\n\nYOUR PREVIOUS ANSWER\n{first}\n\nIt broke these rules; answer again in \
         full, keeping what was right:\n- {}",
        violations.join("\n- ")
    );
    let second = model
        .complete(&ModelCall {
            prompt: retry,
            schema,
        })
        .await?;
    match validate(context, &second, max_synthetic) {
        Ok(planned) => {
            days.extend(planned);
            Ok(days)
        }
        Err(violations) => bail!(
            "The plan model broke the plan rules twice. The second answer:\n- {}",
            violations.join("\n- ")
        ),
    }
}

/// The rules text followed by the input as JSON.
pub fn prompt(context: &Context, max_synthetic_hours: f64) -> Result<String> {
    let input = json!({
        "max_synthetic_hours": max_synthetic_hours,
        "recent_devpro_titles": context.recent_titles,
        "days": context.days.iter().map(day_input).collect::<Vec<_>>(),
    });
    Ok(format!("{RULES}{}", serde_json::to_string_pretty(&input)?))
}

#[derive(Serialize)]
struct CandidateInput<'a> {
    id: &'a str,
    source: CandidateSource,
    topic: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    recent_hours: Option<f64>,
    devpro_project: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_hours: Option<f64>,
}

fn day_input(day: &DayContext) -> Value {
    let free_quarters = DAY_QUARTERS - day.pinned_quarters();
    json!({
        "date": day.date.to_string(),
        "weekday": day.date.format("%A").to_string(),
        "hours_to_fill": quarters_to_hours(free_quarters),
        "pinned": day.pinned.iter().map(|line| json!({
            "title": line.title,
            "devpro_project": line.devpro_project,
            "hours": quarters_to_hours(line.quarters),
        })).collect::<Vec<_>>(),
        "lines": day.free.iter().map(|line| json!({
            "key": line.key,
            "chrono_project": line.chrono.project,
            "description": line.chrono.description,
            "chrono_hours": line.chrono_hours,
            "entries": line.entry_count,
            "devpro_project": line.devpro_project,
        })).collect::<Vec<_>>(),
        "timeline": day.timeline,
        "candidates": day.candidates.iter().map(|c| CandidateInput {
            id: &c.id,
            source: c.source,
            topic: &c.topic,
            recent_hours: c.recent_hours,
            devpro_project: &c.devpro_project,
            max_hours: c.max_quarters.map(quarters_to_hours),
        }).collect::<Vec<_>>(),
    })
}

/// The JSON schema the answer is held to by `claude -p --json-schema`.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["days"],
        "properties": {
            "days": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["date", "lines", "extra"],
                    "properties": {
                        "date": {"type": "string"},
                        "lines": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["key", "title", "hours", "needs_detail"],
                                "properties": {
                                    "key": {"type": "string"},
                                    "title": {"type": "string"},
                                    "hours": {"type": "number"},
                                    "needs_detail": {"type": "boolean"}
                                }
                            }
                        },
                        "extra": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["kind", "candidate_id", "title", "hours"],
                                "properties": {
                                    "kind": {"type": "string", "enum": ["main", "borrow", "filler"]},
                                    "candidate_id": {"type": "string"},
                                    "title": {"type": "string"},
                                    "hours": {"type": "number"}
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

/// The planned days, or every rule the answer broke.
fn validate(
    context: &Context,
    answer: &Value,
    max_synthetic: Quarters,
) -> Result<Vec<DayPlan>, Vec<String>> {
    let answer: Answer = serde_json::from_value(answer.clone())
        .map_err(|error| vec![format!("the answer does not match the schema: {error}")])?;
    let mut violations = Vec::new();
    let mut planned = Vec::new();
    let mut answered: HashSet<NaiveDate> = HashSet::new();

    for day_answer in &answer.days {
        let Some(day) = context
            .days
            .iter()
            .find(|day| day.date.to_string() == day_answer.date)
        else {
            violations.push(format!("{}: not a day of the input", day_answer.date));
            continue;
        };
        if !answered.insert(day.date) {
            violations.push(format!("{}: answered more than once", day.date));
            continue;
        }
        let before = violations.len();
        let lines = check_day(day, day_answer, max_synthetic, &mut violations);
        if violations.len() == before {
            planned.push(lines);
        }
    }
    for day in &context.days {
        if !answered.contains(&day.date) {
            violations.push(format!("{}: missing from the answer", day.date));
        }
    }
    if violations.is_empty() {
        Ok(planned)
    } else {
        Err(violations)
    }
}

fn check_day(
    day: &DayContext,
    answer: &DayAnswer,
    max_synthetic: Quarters,
    violations: &mut Vec<String>,
) -> DayPlan {
    let date = day.date;
    let mut lines: Vec<PlanLine> = day.pinned.clone();

    let mut seen: HashSet<&str> = HashSet::new();
    for line in &answer.lines {
        let Some(free) = day.free.iter().find(|free| free.key == line.key) else {
            violations.push(format!(
                "{date}: line key '{}' is not in the input",
                line.key
            ));
            continue;
        };
        if !seen.insert(&line.key) {
            violations.push(format!(
                "{date}: line key '{}' answered more than once",
                line.key
            ));
            continue;
        }
        let what = format!("{date}: line {}", line.key);
        check_title(&what, &line.title, violations);
        let Some(quarters) = check_hours(&what, line.hours, violations) else {
            continue;
        };
        lines.push(
            free.clone()
                .into_plan_line(line.title.clone(), line.needs_detail, quarters),
        );
    }
    for free in &day.free {
        if !seen.contains(free.key.as_str()) {
            violations.push(format!("{date}: line key '{}' is missing", free.key));
        }
    }

    let mut used: HashSet<&str> = HashSet::new();
    let mut mains = 0;
    let mut synthetic: Quarters = 0;
    for extra in &answer.extra {
        let what = format!("{date}: extra line '{}'", extra.title);
        let Some(candidate) = day.candidates.iter().find(|c| c.id == extra.candidate_id) else {
            violations.push(format!(
                "{what}: candidate '{}' is not in the input",
                extra.candidate_id
            ));
            continue;
        };
        if !used.insert(&extra.candidate_id) {
            violations.push(format!("{what}: candidate '{}' used twice", candidate.id));
        }
        check_title(&what, &extra.title, violations);
        let Some(quarters) = check_hours(&what, extra.hours, violations) else {
            continue;
        };
        let kind = match (extra.kind, candidate.source) {
            (ExtraKind::Main, CandidateSource::History) => {
                mains += 1;
                LineKind::Main
            }
            (ExtraKind::Borrow, CandidateSource::History) => {
                synthetic += quarters;
                LineKind::Borrow
            }
            (ExtraKind::Filler, CandidateSource::Filler) => {
                synthetic += quarters;
                if candidate.max_quarters.is_some_and(|max| quarters > max) {
                    violations.push(format!(
                        "{what}: {} h is past the filler's max_hours {}",
                        hours(quarters_to_hours(quarters)),
                        hours(quarters_to_hours(candidate.max_quarters.unwrap_or(0)))
                    ));
                }
                LineKind::Filler
            }
            (kind, source) => {
                violations.push(format!(
                    "{what}: kind {kind:?} cannot use a {source:?} candidate"
                ));
                continue;
            }
        };
        lines.push(extra_line(candidate, kind, &extra.title, quarters));
    }
    if mains > 1 {
        violations.push(format!("{date}: {mains} main lines, at most one"));
    }
    if synthetic > max_synthetic {
        violations.push(format!(
            "{date}: borrow and filler lines come to {} h, past max_synthetic_hours {}",
            hours(quarters_to_hours(synthetic)),
            hours(quarters_to_hours(max_synthetic))
        ));
    }

    let total: Quarters = lines.iter().map(|line| line.quarters).sum();
    if total != DAY_QUARTERS {
        violations.push(format!(
            "{date}: the day comes to {} h with the pinned lines, not 8.0 (hours_to_fill was {})",
            hours(quarters_to_hours(total)),
            hours(quarters_to_hours(DAY_QUARTERS - day.pinned_quarters()))
        ));
    }
    DayPlan { date, lines }
}

fn check_title(what: &str, title: &str, violations: &mut Vec<String>) {
    if !is_plain_ascii(title) {
        violations.push(format!("{what}: the title must be non-empty plain ASCII"));
    } else if title.chars().count() > MAX_TITLE_CHARS {
        violations.push(format!(
            "{what}: the title is longer than {MAX_TITLE_CHARS} characters"
        ));
    }
}

fn check_hours(what: &str, value: f64, violations: &mut Vec<String>) -> Option<Quarters> {
    match hours_to_quarters(value) {
        Some(quarters) if quarters >= 1 => Some(quarters),
        _ => {
            violations.push(format!(
                "{what}: {value} h is not a multiple of 0.25 of at least 0.25"
            ));
            None
        }
    }
}

fn extra_line(candidate: &Candidate, kind: LineKind, title: &str, quarters: Quarters) -> PlanLine {
    PlanLine {
        addr: 0,
        kind,
        chrono: None,
        entry_count: 0,
        chrono_hours: None,
        title: title.to_string(),
        needs_detail: false,
        devpro_project: candidate.devpro_project.clone(),
        project_id: candidate.project_id.clone(),
        billability: candidate.billability.clone(),
        quarters,
        pinned: false,
        worklog_id: None,
        candidate_id: Some(candidate.id.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::fake::FakePlanModel;
    use crate::plan::fixtures::{date, line};
    use crate::plan::{BILLABLE, ChronoKey, NON_BILLABLE};
    use crate::service::plan_context::{ChronoLine, RecentTitle};
    use anyhow::anyhow;

    fn monday() -> DayContext {
        DayContext {
            date: date(2026, 10, 5),
            pinned: vec![line(
                LineKind::Meeting,
                "D3 - Agentic DQ - Sync",
                "Inveniam SOW #5",
                true,
                4,
            )],
            free: vec![ChronoLine {
                key: "c1".to_string(),
                chrono: ChronoKey {
                    project: "Inveniam Measurabl - Presales - DevPro - Work".to_string(),
                    description: "Map every Connect issue to its owner".to_string(),
                },
                entry_count: 2,
                chrono_hours: 1.25,
                devpro_project: "Inveniam SOW #5".to_string(),
                project_id: "id-Inveniam SOW #5".to_string(),
                billability: BILLABLE.to_string(),
            }],
            candidates: vec![
                Candidate {
                    id: "h1".to_string(),
                    source: CandidateSource::History,
                    topic: "Cost framework".to_string(),
                    recent_hours: Some(9.0),
                    devpro_project: "AI Practices".to_string(),
                    project_id: "id-AI Practices".to_string(),
                    billability: NON_BILLABLE.to_string(),
                    max_quarters: None,
                },
                Candidate {
                    id: "h2".to_string(),
                    source: CandidateSource::History,
                    topic: "Connect mapping".to_string(),
                    recent_hours: Some(4.0),
                    devpro_project: "Inveniam SOW #5".to_string(),
                    project_id: "id-Inveniam SOW #5".to_string(),
                    billability: BILLABLE.to_string(),
                    max_quarters: None,
                },
                Candidate {
                    id: "f1".to_string(),
                    source: CandidateSource::Filler,
                    topic: "AI research".to_string(),
                    recent_hours: None,
                    devpro_project: "AI Practices".to_string(),
                    project_id: "id-AI Practices".to_string(),
                    billability: NON_BILLABLE.to_string(),
                    max_quarters: Some(6),
                },
            ],
            timeline: vec![],
        }
    }

    fn context() -> Context {
        Context {
            days: vec![monday()],
            recent_titles: vec![RecentTitle {
                title: "AI cost governance framework".to_string(),
                devpro_project: "AI Practices".to_string(),
                last_date: date(2026, 10, 2),
                days: 3,
            }],
            ..Context::default()
        }
    }

    /// A valid answer for [`monday`]: 1.0 pinned, 4.0 stretched work, 3.0 main.
    fn good() -> Value {
        json!({"days": [{
            "date": "2026-10-05",
            "lines": [{"key": "c1", "title": "Connect issue ownership mapping", "hours": 4.0, "needs_detail": false}],
            "extra": [{"kind": "main", "candidate_id": "h1", "title": "AI spend governance model", "hours": 3.0}]
        }]})
    }

    fn violations_of(answer: Value) -> Vec<String> {
        validate(&context(), &answer, 16).expect_err("the answer breaks a rule")
    }

    #[tokio::test]
    async fn a_valid_answer_becomes_the_day_with_its_pinned_lines() {
        let model = FakePlanModel::new(vec![Ok(good())]);
        let days = plan(&model, &context(), 4.0).await.unwrap();
        assert_eq!(model.calls().len(), 1);
        let lines: Vec<(LineKind, &str, Quarters, Option<&str>)> = days[0]
            .lines
            .iter()
            .map(|l| {
                (
                    l.kind,
                    l.title.as_str(),
                    l.quarters,
                    l.candidate_id.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            lines,
            vec![
                (LineKind::Meeting, "D3 - Agentic DQ - Sync", 4, None),
                (LineKind::Work, "Connect issue ownership mapping", 16, None),
                (LineKind::Main, "AI spend governance model", 12, Some("h1")),
            ]
        );
        let work = &days[0].lines[1];
        assert_eq!(work.chrono_hours, Some(1.25));
        assert_eq!(work.entry_count, 2);
        assert_eq!(work.devpro_project, "Inveniam SOW #5");
        assert_eq!(days[0].lines[2].project_id, "id-AI Practices");
    }

    #[tokio::test]
    async fn a_broken_answer_is_retried_once_with_what_it_broke() {
        let mut broken = good();
        broken["days"][0]["lines"][0]["hours"] = json!(3.0);
        let model = FakePlanModel::new(vec![Ok(broken), Ok(good())]);
        let days = plan(&model, &context(), 4.0).await.unwrap();
        assert_eq!(days[0].total_quarters(), DAY_QUARTERS);
        let calls = model.calls();
        assert_eq!(calls.len(), 2);
        assert!(
            calls[1].prompt.contains("the day comes to 7.0 h"),
            "{}",
            calls[1].prompt
        );
        assert!(calls[1].prompt.starts_with(&calls[0].prompt));
    }

    #[tokio::test]
    async fn a_second_broken_answer_stops_the_run() {
        let mut broken = good();
        broken["days"][0]["lines"][0]["title"] = json!("Созвон");
        let model = FakePlanModel::new(vec![Ok(broken.clone()), Ok(broken)]);
        let error = plan(&model, &context(), 4.0).await.unwrap_err().to_string();
        assert!(error.contains("broke the plan rules twice"), "{error}");
        assert!(error.contains("plain ASCII"), "{error}");
    }

    #[tokio::test]
    async fn a_model_failure_is_the_run_failure() {
        let model = FakePlanModel::new(vec![Err(anyhow!("auth expired"))]);
        let error = plan(&model, &context(), 4.0).await.unwrap_err().to_string();
        assert_eq!(error, "auth expired");
    }

    #[tokio::test]
    async fn days_that_need_no_model_skip_the_call() {
        let ready = DayPlan {
            date: date(2026, 10, 6),
            lines: vec![line(
                LineKind::Meeting,
                "Offsite",
                "AI Practices",
                false,
                32,
            )],
        };
        let context = Context {
            ready: vec![ready.clone()],
            ..Context::default()
        };
        let model = FakePlanModel::new(vec![]);
        assert_eq!(plan(&model, &context, 4.0).await.unwrap(), vec![ready]);
        assert!(model.calls().is_empty());
    }

    #[test]
    fn the_prompt_carries_the_rules_and_the_day_input() {
        let prompt = prompt(&context(), 4.0).unwrap();
        assert!(prompt.starts_with("You plan the worklogs"));
        let input: Value = serde_json::from_str(&prompt[RULES.len()..]).unwrap();
        let day = &input["days"][0];
        assert_eq!(day["hours_to_fill"], json!(7.0));
        assert_eq!(day["weekday"], json!("Monday"));
        assert_eq!(day["lines"][0]["key"], json!("c1"));
        assert_eq!(day["candidates"][2]["max_hours"], json!(1.5));
        assert!(day["candidates"][0].get("max_hours").is_none());
        assert_eq!(input["recent_devpro_titles"][0]["days"], json!(3));
    }

    #[test]
    fn every_line_key_must_be_answered_exactly_once() {
        let mut answer = good();
        answer["days"][0]["lines"] = json!([]);
        answer["days"][0]["extra"][0]["hours"] = json!(7.0);
        assert!(
            violations_of(answer)
                .iter()
                .any(|v| v.contains("'c1' is missing"))
        );

        let mut answer = good();
        answer["days"][0]["lines"][0]["key"] = json!("c9");
        assert!(
            violations_of(answer)
                .iter()
                .any(|v| v.contains("'c9' is not in the input"))
        );
    }

    #[test]
    fn hours_must_sit_on_the_grid_and_be_positive() {
        for hours in [json!(4.1), json!(0.0)] {
            let mut answer = good();
            answer["days"][0]["lines"][0]["hours"] = hours;
            assert!(
                violations_of(answer)
                    .iter()
                    .any(|v| v.contains("not a multiple of 0.25"))
            );
        }
    }

    #[test]
    fn a_candidate_must_exist_and_match_its_kind() {
        let mut answer = good();
        answer["days"][0]["extra"][0]["candidate_id"] = json!("h9");
        assert!(
            violations_of(answer)
                .iter()
                .any(|v| v.contains("'h9' is not in the input"))
        );

        let mut answer = good();
        answer["days"][0]["extra"][0]["candidate_id"] = json!("f1");
        answer["days"][0]["extra"][0]["hours"] = json!(1.0);
        answer["days"][0]["lines"][0]["hours"] = json!(6.0);
        assert!(
            violations_of(answer)
                .iter()
                .any(|v| v.contains("kind Main cannot use a Filler"))
        );
    }

    #[test]
    fn one_main_line_a_day_and_each_candidate_once() {
        let mut answer = good();
        answer["days"][0]["extra"] = json!([
            {"kind": "main", "candidate_id": "h1", "title": "AI spend model", "hours": 1.5},
            {"kind": "main", "candidate_id": "h2", "title": "Connect ownership", "hours": 1.5}
        ]);
        assert!(
            violations_of(answer)
                .iter()
                .any(|v| v.contains("2 main lines"))
        );

        let mut answer = good();
        answer["days"][0]["extra"] = json!([
            {"kind": "main", "candidate_id": "h1", "title": "AI spend model", "hours": 1.5},
            {"kind": "borrow", "candidate_id": "h1", "title": "AI spend review", "hours": 1.5}
        ]);
        assert!(
            violations_of(answer)
                .iter()
                .any(|v| v.contains("used twice"))
        );
    }

    #[test]
    fn fillers_keep_their_own_cap_and_the_synthetic_cap() {
        let mut answer = good();
        answer["days"][0]["extra"] = json!([
            {"kind": "main", "candidate_id": "h1", "title": "AI spend model", "hours": 1.0},
            {"kind": "filler", "candidate_id": "f1", "title": "AI tooling research", "hours": 2.0}
        ]);
        assert!(
            violations_of(answer)
                .iter()
                .any(|v| v.contains("past the filler's max_hours"))
        );

        let mut answer = good();
        answer["days"][0]["extra"] = json!([
            {"kind": "borrow", "candidate_id": "h2", "title": "Connect ownership", "hours": 3.0}
        ]);
        let violations = validate(&context(), &answer, 8).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.contains("past max_synthetic_hours 2.0")),
            "{violations:?}"
        );
    }

    #[test]
    fn titles_are_plain_ascii_and_short() {
        let mut answer = good();
        answer["days"][0]["extra"][0]["title"] = json!("x".repeat(MAX_TITLE_CHARS + 1));
        assert!(
            violations_of(answer)
                .iter()
                .any(|v| v.contains("longer than"))
        );
    }

    #[test]
    fn every_day_must_be_answered_once_and_no_other() {
        let mut answer = good();
        answer["days"][0]["date"] = json!("2026-10-06");
        let violations = violations_of(answer);
        assert!(
            violations
                .iter()
                .any(|v| v.contains("2026-10-06: not a day of the input"))
        );
        assert!(
            violations
                .iter()
                .any(|v| v.contains("2026-10-05: missing from the answer"))
        );

        let answer = json!({"days": [good()["days"][0], good()["days"][0]]});
        assert!(
            violations_of(answer)
                .iter()
                .any(|v| v.contains("answered more than once"))
        );
    }

    #[test]
    fn an_answer_off_the_schema_is_a_violation_not_a_crash() {
        let violations = violations_of(json!({"days": "none"}));
        assert!(violations[0].starts_with("the answer does not match the schema"));
    }

    #[test]
    fn needs_detail_travels_to_the_plan_line() {
        let mut answer = good();
        answer["days"][0]["lines"][0]["needs_detail"] = json!(true);
        let days = validate(&context(), &answer, 16).unwrap();
        assert!(days[0].lines[1].needs_detail);
    }
}
