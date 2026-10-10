//! The three runs against stub servers for the portal and Chrono, a scripted plan model and a
//! state directory of their own. The portal stub serves its responses in order whatever the
//! path, so each test lists them in the order the run asks.

use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;
use crate::api::stub::{STALL, StubServer, json_200, response};
use crate::llm::fake::FakePlanModel;
use crate::plan::fixtures::date;

const INVENIAM: &str = "Inveniam - DevPro - Work";
const AI: &str = "AI Practices - DevPro - Work";

/// Nothing listens here: a run that must not touch a server gets this one, and a request
/// would fail the test with a connection error.
const NOWHERE: &str = "http://127.0.0.1:9";

const CONFIG: &str = r#"
chrono_api: "http://unused"
vault_path: "/vault"
mappings:
  - chrono_project: "Inveniam - DevPro - Work"
    devpro_project: "Inveniam SOW #5"
    billability: "Billable"
  - chrono_project: "AI Practices - DevPro - Work"
    devpro_project: "AI Practices"
    billability: "NonBillable"
fillers:
  - devpro_project: "AI Practices"
    task_title: "AI research"
    billability: "NonBillable"
    min_hours: 0.5
    max_hours: 1.5
"#;

fn monday() -> NaiveDate {
    date(2026, 10, 5)
}

fn today() -> NaiveDate {
    date(2026, 10, 8)
}

fn project_id(name: &str) -> &'static str {
    match name {
        "Inveniam SOW #5" => "id-inveniam",
        "AI Practices" => "id-ai",
        other => panic!("no id for {other}"),
    }
}

// -- canned responses -------------------------------------------------------

fn user() -> String {
    json_200(&json!({"uniqueId": "me", "fullName": "Y", "email": "y@dev.pro"}).to_string())
}

fn assigned() -> String {
    json_200(
        &json!({"uniqueId": "me", "projects": [
            {"uniqueId": "id-inveniam", "shortName": "Inveniam SOW #5"},
            {"uniqueId": "id-ai", "shortName": "AI Practices"}
        ]})
        .to_string(),
    )
}

fn worklog(id: &str, title: &str, project: &str, hours: f64) -> Value {
    json!({
        "uniqueId": id,
        "projectUniqueId": project_id(project),
        "projectShortName": project,
        "taskTitle": title,
        "billability": "NonBillable",
        "loggedHours": hours,
        "isDeletable": true
    })
}

fn view(days: &[(NaiveDate, Vec<Value>)]) -> String {
    let details: Vec<Value> = days
        .iter()
        .map(|(day, worklogs)| {
            let logged: f64 = worklogs
                .iter()
                .map(|w| w["loggedHours"].as_f64().unwrap())
                .sum();
            json!({
                "date": format!("{day}T00:00:00"),
                "loggedHours": logged,
                "expectedHours": 8.0,
                "worklogsDetails": worklogs
            })
        })
        .collect();
    json_200(
        &json!({"totalLoggedHours": 0.0, "totalExpectedHours": 0.0, "pageList": [{
            "contactUniqueId": "me", "fullName": "Y", "loggedHours": 0.0, "expectedHours": 0.0,
            "detailsByDates": details
        }]})
        .to_string(),
    )
}

fn empty_view() -> String {
    view(&[])
}

/// Monday after a full write: the meeting and the stretched work line.
fn monday_written() -> String {
    view(&[(
        monday(),
        vec![
            worklog("w1", "Weekly sync", "Inveniam SOW #5", 1.0),
            worklog("w2", "AI cost model draft", "AI Practices", 7.0),
        ],
    )])
}

fn entry(id: i64, day: NaiveDate, hh: u32, project: &str, description: &str, hours: f64) -> Value {
    json!({
        "id": id,
        "description": description,
        "start_time": format!("{day}T{hh:02}") + ":00:00Z",
        "duration": (hours * 3600.0) as i64,
        "project": {"id": 1, "name": project, "color": "#000"}
    })
}

fn chrono(entries: &[Value]) -> String {
    json_200(&Value::Array(entries.to_vec()).to_string())
}

fn monday_entries() -> Vec<Value> {
    vec![
        entry(1, monday(), 9, INVENIAM, "Weekly sync", 1.0),
        entry(2, monday(), 11, AI, "Draft the cost model", 1.5),
    ]
}

/// The model's answer for Monday: the one free line stretched to `hours`.
fn answer(hours: f64) -> Value {
    json!({"days": [{
        "date": "2026-10-05",
        "lines": [{"key": "c1", "title": "AI cost model draft", "hours": hours, "needs_detail": false}],
        "extra": []
    }]})
}

fn is_meeting(aggregate: &DayProjectAggregate) -> bool {
    aggregate
        .descriptions
        .first()
        .is_some_and(|d| d == "Weekly sync")
}

// -- the harness ------------------------------------------------------------

#[derive(Default)]
struct Recorder {
    out: Vec<String>,
    err: Vec<String>,
}

impl Console for Recorder {
    fn out(&mut self, line: &str) {
        self.out.push(line.to_string());
    }
    fn err(&mut self, line: &str) {
        self.err.push(line.to_string());
    }
}

impl Recorder {
    fn err_text(&self) -> String {
        self.err.join("\n")
    }
    fn out_text(&self) -> String {
        self.out.join("\n")
    }
}

enum Run {
    Plan(SettleArgs),
    Replan,
    Apply,
    Period,
    /// What `dispatch` runs for `settle` and `--replan`: the plan, then the period block.
    Settle(SettleArgs),
}

struct Harness {
    dir: TempDir,
    config: Config,
    now: fn() -> DateTime<Utc>,
}

/// The clock of every run: 09:00 UTC on [`today`].
fn nine_am() -> DateTime<Utc> {
    "2026-10-08T09:00:00Z".parse().unwrap()
}

impl Harness {
    fn new() -> Self {
        Self {
            dir: TempDir::new().unwrap(),
            config: crate::config::parse(CONFIG).unwrap(),
            now: nine_am,
        }
    }

    fn state(&self) -> State {
        State::at(self.dir.path())
    }

    async fn run(
        &self,
        run: Run,
        portal: &str,
        chrono: &str,
        model: &FakePlanModel,
    ) -> (Result<Outcome>, Recorder) {
        self.run_with_timeout(run, portal, chrono, model, Duration::from_secs(5))
            .await
    }

    async fn run_with_timeout(
        &self,
        run: Run,
        portal: &str,
        chrono: &str,
        model: &FakePlanModel,
        timeout: Duration,
    ) -> (Result<Outcome>, Recorder) {
        let portal = TtApiClient::with_base_url("cookie", portal, timeout, timeout).unwrap();
        let chrono = ChronoClient::with_timeouts(chrono, timeout, timeout).unwrap();
        let state = self.state();
        let settle = Settle {
            config: &self.config,
            chrono: &chrono,
            portal: &portal,
            state: &state,
            zone: &Utc,
            today: today(),
            now: self.now,
        };
        let planning = Planning { model, is_meeting };
        let mut io = Recorder::default();
        let result = match run {
            Run::Plan(args) => settle.plan(&args, &planning, &mut io).await,
            Run::Replan => settle.replan(&planning, &mut io).await,
            Run::Apply => settle.apply(&mut io).await,
            Run::Period => {
                settle.period(&mut io).await;
                Ok(Outcome::Ok)
            }
            Run::Settle(args) => settle.plan_then_period(&args, &planning, &mut io).await,
        };
        (result, io)
    }

    /// Plans Monday alone with `--prefix Б`, the state every replan and apply test starts
    /// from: Б1 the meeting at 1.0, Б2 the work line stretched to 7.0.
    async fn planned_monday(&self) {
        let portal = StubServer::start(vec![user(), empty_view(), empty_view(), assigned()]);
        let chrono_stub = StubServer::start(vec![chrono(&monday_entries())]);
        let model = FakePlanModel::new(vec![Ok(answer(7.0))]);
        let (result, _) = self
            .run(
                Run::Plan(monday_args()),
                &portal.base_url,
                &chrono_stub.base_url,
                &model,
            )
            .await;
        assert_eq!(result.unwrap(), Outcome::Ok);
        portal.requests();
        chrono_stub.requests();
    }

    fn md(&self) -> String {
        std::fs::read_to_string(self.state().md_path()).unwrap()
    }

    fn write_md(&self, text: &str) {
        std::fs::write(self.state().md_path(), text).unwrap();
    }
}

fn monday_args() -> SettleArgs {
    SettleArgs {
        from: Some(monday()),
        to: Some(monday()),
        prefix: Some("Б".to_string()),
        ..SettleArgs::default()
    }
}

fn summary(day: &DayPlan) -> Vec<(u32, LineKind, &str, Quarters, bool)> {
    day.lines
        .iter()
        .map(|l| (l.addr, l.kind, l.title.as_str(), l.quarters, l.pinned))
        .collect()
}

fn posts(requests: &[crate::api::stub::CapturedRequest]) -> Vec<Value> {
    requests
        .iter()
        .filter(|r| r.method == "POST")
        .map(|r| serde_json::from_str(&r.body).unwrap())
        .collect()
}

// -- arguments --------------------------------------------------------------

#[test]
fn replan_and_apply_refuse_every_option_that_shapes_a_new_plan() {
    let args = SettleArgs {
        from: Some(monday()),
        prefix: Some("Б".to_string()),
        replan: true,
        apply: true,
        ..SettleArgs::default()
    };
    assert_eq!(
        conflicts(&args),
        vec![
            "option --apply cannot be used with --replan",
            "option --from cannot be used with --apply",
            "option --prefix cannot be used with --apply",
        ]
    );
    let replan = SettleArgs {
        to: Some(monday()),
        include_today: true,
        replan: true,
        ..SettleArgs::default()
    };
    assert_eq!(
        conflicts(&replan),
        vec![
            "option --to cannot be used with --replan",
            "option --include-today cannot be used with --replan",
        ]
    );
    assert!(conflicts(&monday_args()).is_empty());
}

// -- plan -------------------------------------------------------------------

#[tokio::test]
async fn an_explicit_range_plans_its_days_and_stores_exactly_what_it_prints() {
    let h = Harness::new();
    let portal = StubServer::start(vec![user(), empty_view(), empty_view(), assigned()]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries())]);
    let model = FakePlanModel::new(vec![Ok(answer(7.0))]);

    let (result, io) = h
        .run(
            Run::Plan(monday_args()),
            &portal.base_url,
            &chrono_stub.base_url,
            &model,
        )
        .await;

    assert_eq!(result.unwrap(), Outcome::Ok);
    let targets: Vec<String> = portal.requests().into_iter().map(|r| r.target).collect();
    assert!(targets[0].contains("/contact/currentUser"), "{targets:?}");
    assert!(targets[1].contains("period=2026-09-01"), "{targets:?}");
    assert!(targets[2].contains("period=2026-10-01"), "{targets:?}");
    assert!(targets[3].contains("dateFrom=2026-10-05"), "{targets:?}");
    let chrono_target = &chrono_stub.requests()[0].target;
    assert!(
        chrono_target.contains("start_date=2026-09-28")
            && chrono_target.contains("end_date=2026-10-06"),
        "{chrono_target}"
    );
    assert_eq!(model.calls().len(), 1);

    let loaded = h.state().load().unwrap();
    assert!(loaded.md_unchanged());
    assert_eq!(io.out, vec![loaded.md.clone()]);
    assert!(loaded.md.contains("| Б2 |"), "{}", loaded.md);
    let plan = loaded.plan;
    assert_eq!((plan.prefix.as_str(), plan.cutoff), ("Б", monday()));
    assert_eq!(
        summary(&plan.days[0]),
        vec![
            (1, LineKind::Meeting, "Weekly sync", 4, true),
            (2, LineKind::Work, "AI cost model draft", 28, false),
        ]
    );
    assert_eq!(plan.days[0].lines[1].project_id, "id-ai");
}

#[tokio::test]
async fn the_scan_offers_final_unfilled_working_days_and_says_what_it_held_back() {
    let h = Harness::new();
    let tuesday = date(2026, 10, 6);
    let mut entries = monday_entries();
    entries.push(entry(3, date(2026, 10, 3), 9, AI, "Weekend reading", 2.0));
    entries.push(entry(4, tuesday, 9, AI, "Draft the cost model", 3.0));
    entries.push(entry(5, today(), 9, AI, "Draft the cost model", 1.0));
    let tuesday_full = view(&[(
        tuesday,
        vec![worklog("w9", "Cost model", "AI Practices", 8.0)],
    )]);
    // The scan starts 45 days back (24 Aug) and the titles 30 days before that: July on.
    let portal = StubServer::start(vec![
        user(),
        empty_view(),
        empty_view(),
        empty_view(),
        tuesday_full,
        assigned(),
    ]);
    let chrono_stub = StubServer::start(vec![chrono(&entries)]);
    let model = FakePlanModel::new(vec![Ok(answer(7.0))]);

    let args = SettleArgs::default();
    let (result, io) = h
        .run(
            Run::Plan(args),
            &portal.base_url,
            &chrono_stub.base_url,
            &model,
        )
        .await;

    assert_eq!(result.unwrap(), Outcome::Ok);
    let targets: Vec<String> = portal.requests().into_iter().map(|r| r.target).collect();
    assert!(targets[1].contains("period=2026-07-01"), "{targets:?}");
    assert!(targets[5].contains("dateFrom=2026-10-05"), "{targets:?}");
    let chrono_target = &chrono_stub.requests()[0].target;
    assert!(
        chrono_target.contains("end_date=2026-10-08"),
        "{chrono_target}"
    );
    assert!(
        io.err_text()
            .contains("Skipped (hours not final yet): 2026-10-08 (today)"),
        "{}",
        io.err_text()
    );
    let plan = h.state().load().unwrap().plan;
    let dates: Vec<NaiveDate> = plan.days.iter().map(|d| d.date).collect();
    assert_eq!(dates, vec![monday()]);
    assert_eq!(plan.cutoff, date(2026, 10, 7));
    assert_eq!(plan.prefix, "");
}

#[tokio::test]
async fn an_empty_scan_that_held_today_back_does_not_claim_every_day_is_closed() {
    let h = Harness::new();
    let portal = StubServer::start(vec![
        user(),
        empty_view(),
        empty_view(),
        empty_view(),
        empty_view(),
    ]);
    let chrono_stub = StubServer::start(vec![chrono(&[entry(1, today(), 9, AI, "Notes", 1.0)])]);
    let model = FakePlanModel::new(vec![]);

    let (result, io) = h
        .run(
            Run::Plan(SettleArgs::default()),
            &portal.base_url,
            &chrono_stub.base_url,
            &model,
        )
        .await;

    assert_eq!(result.unwrap(), Outcome::Ok);
    portal.requests();
    assert_eq!(
        io.out,
        vec![
            "Nothing to settle yet. Held back: 2026-10-08 (today). Use --include-today to settle \
             today anyway."
        ]
    );
    assert!(h.state().load().unwrap().md_unchanged());
}

#[tokio::test]
async fn an_explicit_working_day_without_chrono_work_is_an_error_not_a_gap() {
    let h = Harness::new();
    let tuesday = date(2026, 10, 6);
    let portal = StubServer::start(vec![
        user(),
        empty_view(),
        empty_view(),
        assigned(),
        assigned(),
    ]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries())]);
    let model = FakePlanModel::new(vec![Ok(answer(7.0))]);
    let args = SettleArgs {
        to: Some(tuesday),
        ..monday_args()
    };

    let (result, io) = h
        .run(
            Run::Plan(args),
            &portal.base_url,
            &chrono_stub.base_url,
            &model,
        )
        .await;

    assert_eq!(result.unwrap(), Outcome::Ok);
    portal.requests();
    let plan = h.state().load().unwrap().plan;
    assert_eq!(plan.days.len(), 1);
    assert_eq!(plan.errors.len(), 1);
    assert_eq!(plan.errors[0].date, tuesday);
    assert!(
        io.out_text().contains("не спланирован"),
        "{}",
        io.out_text()
    );
}

// -- replan -----------------------------------------------------------------

#[tokio::test]
async fn a_rewritten_title_is_pinned_and_a_full_day_needs_no_model() {
    let h = Harness::new();
    h.planned_monday().await;
    h.write_md(
        &h.md()
            .replace("AI cost model draft", "AI cost model outline"),
    );

    let portal = StubServer::start(vec![user(), assigned(), empty_view(), empty_view()]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries())]);
    let model = FakePlanModel::new(vec![]);
    let (result, io) = h
        .run(Run::Replan, &portal.base_url, &chrono_stub.base_url, &model)
        .await;

    assert_eq!(result.unwrap(), Outcome::Ok, "{}", io.err_text());
    portal.requests();
    assert!(model.calls().is_empty());
    let loaded = h.state().load().unwrap();
    assert!(loaded.md_unchanged());
    assert_eq!(
        summary(&loaded.plan.days[0]),
        vec![
            (1, LineKind::Meeting, "Weekly sync", 4, true),
            (2, LineKind::Work, "AI cost model outline", 28, true),
        ]
    );
}

#[tokio::test]
async fn shortened_hours_are_pinned_and_the_model_fills_the_rest() {
    let h = Harness::new();
    h.planned_monday().await;
    h.write_md(&h.md().replace("1.5 → **7.0**", "1.5 → 6.5"));

    let portal = StubServer::start(vec![user(), assigned(), empty_view(), empty_view()]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries())]);
    let filler = json!({"days": [{"date": "2026-10-05", "lines": [], "extra": [
        {"kind": "filler", "candidate_id": "f1", "title": "AI tooling research", "hours": 0.5}
    ]}]});
    let model = FakePlanModel::new(vec![Ok(filler)]);
    let (result, io) = h
        .run(Run::Replan, &portal.base_url, &chrono_stub.base_url, &model)
        .await;

    assert_eq!(result.unwrap(), Outcome::Ok, "{}", io.err_text());
    portal.requests();
    assert_eq!(model.calls().len(), 1);
    let day = &h.state().load().unwrap().plan.days[0];
    assert_eq!(day.total_quarters(), DAY_QUARTERS);
    let work = day.lines.iter().find(|l| l.kind == LineKind::Work).unwrap();
    assert_eq!((work.quarters, work.pinned), (26, true));
    let added = day
        .lines
        .iter()
        .find(|l| l.kind == LineKind::Filler)
        .unwrap();
    assert_eq!(
        (added.title.as_str(), added.quarters),
        ("AI tooling research", 2)
    );
}

#[tokio::test]
async fn a_project_pair_nobody_configured_is_refused_with_its_line() {
    let h = Harness::new();
    h.planned_monday().await;
    let md = h.md();
    h.write_md(&md.replace("| AI Practices |", "| 💵 AI Practices |"));

    let portal = StubServer::start(vec![user(), assigned()]);
    let model = FakePlanModel::new(vec![]);
    let (result, _) = h.run(Run::Replan, &portal.base_url, NOWHERE, &model).await;

    let message = format!("{:#}", result.unwrap_err());
    assert!(
        message.contains("AI Practices as Billable is in none of"),
        "{message}"
    );
    assert!(message.contains("plan.md line"), "{message}");
    portal.requests();
    // Refused before anything was stored: the edited file is still there to fix.
    assert_eq!(
        h.md(),
        md.replace("| AI Practices |", "| 💵 AI Practices |")
    );
}

/// Б2 edited to 7.5 h beside the 1.0 h meeting pins 8.5 h. The day cannot be planned around
/// the edit, and storing it as a day error would drop the edit with it: the replan stops
/// and plan.md keeps the edit to correct.
#[tokio::test]
async fn an_edited_day_that_cannot_be_planned_stops_the_replan_and_keeps_the_edits() {
    let h = Harness::new();
    h.planned_monday().await;
    let edited = h.md().replace("1.5 → **7.0**", "1.5 → 7.5");
    h.write_md(&edited);

    let portal = StubServer::start(vec![user(), assigned(), empty_view(), empty_view()]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries())]);
    let model = FakePlanModel::new(vec![]);
    let (result, _) = h
        .run(Run::Replan, &portal.base_url, &chrono_stub.base_url, &model)
        .await;

    let message = format!("{:#}", result.unwrap_err());
    assert!(message.contains("cannot be planned"), "{message}");
    assert!(message.contains("8.5"), "{message}");
    portal.requests();
    assert!(model.calls().is_empty());
    assert_eq!(h.md(), edited);
}

#[tokio::test]
async fn a_plan_without_edits_is_shown_again_without_a_request() {
    let h = Harness::new();
    h.planned_monday().await;
    let md = h.md();

    let model = FakePlanModel::new(vec![]);
    let (result, io) = h.run(Run::Replan, NOWHERE, NOWHERE, &model).await;

    assert_eq!(result.unwrap(), Outcome::Ok);
    assert!(io.err_text().contains("no edits"), "{}", io.err_text());
    assert_eq!(io.out, vec![md]);
}

/// Monday's entries with the meeting as Chrono has it now.
fn monday_entries_with_meeting(hours: f64) -> Vec<Value> {
    vec![
        entry(1, monday(), 9, INVENIAM, "Weekly sync", hours),
        entry(2, monday(), 11, AI, "Draft the cost model", 1.5),
    ]
}

/// The meeting is pinned by the context, not by Yurii: a replan takes its hours from Chrono
/// as it is now. Б2 shortened to 6.5 beside the corrected 1.5 h meeting is the whole day, so
/// the stale 1.0 would leave a gap the model is not there to fill.
#[tokio::test]
async fn a_meeting_corrected_in_chrono_reaches_the_replanned_day() {
    let h = Harness::new();
    h.planned_monday().await;
    h.write_md(&h.md().replace("1.5 → **7.0**", "1.5 → 6.5"));

    let portal = StubServer::start(vec![user(), assigned(), empty_view(), empty_view()]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries_with_meeting(1.5))]);
    let model = FakePlanModel::new(vec![]);
    let (result, io) = h
        .run(Run::Replan, &portal.base_url, &chrono_stub.base_url, &model)
        .await;

    assert_eq!(result.unwrap(), Outcome::Ok, "{}", io.err_text());
    portal.requests();
    assert!(model.calls().is_empty());
    let day = &h.state().load().unwrap().plan.days[0];
    let lines: Vec<(LineKind, Quarters, bool)> = day
        .lines
        .iter()
        .map(|l| (l.kind, l.quarters, l.edited))
        .collect();
    assert_eq!(
        lines,
        vec![(LineKind::Meeting, 6, false), (LineKind::Work, 26, true)]
    );
}

/// The meeting row Yurii edited to 0.75 stays at 0.75 two rounds on, after Chrono moved the
/// meeting to 1.5 h and another row was edited.
#[tokio::test]
async fn a_meeting_yurii_edited_keeps_his_hours_across_a_replan() {
    let h = Harness::new();
    h.planned_monday().await;
    let md = h.md();
    assert_eq!(md.matches("| 1.0 |").count(), 1, "{md}");
    h.write_md(&md.replace("| 1.0 |", "| 0.75 |"));

    let portal = StubServer::start(vec![user(), assigned(), empty_view(), empty_view()]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries())]);
    let model = FakePlanModel::new(vec![Ok(answer(7.25))]);
    let (first, io) = h
        .run(Run::Replan, &portal.base_url, &chrono_stub.base_url, &model)
        .await;
    assert_eq!(first.unwrap(), Outcome::Ok, "{}", io.err_text());
    portal.requests();

    h.write_md(
        &h.md()
            .replace("AI cost model draft", "AI cost model outline"),
    );
    let portal = StubServer::start(vec![user(), assigned(), empty_view(), empty_view()]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries_with_meeting(1.5))]);
    let model = FakePlanModel::new(vec![]);
    let (second, io) = h
        .run(Run::Replan, &portal.base_url, &chrono_stub.base_url, &model)
        .await;

    assert_eq!(second.unwrap(), Outcome::Ok, "{}", io.err_text());
    portal.requests();
    assert!(model.calls().is_empty());
    assert_eq!(
        summary(&h.state().load().unwrap().plan.days[0]),
        vec![
            (1, LineKind::Meeting, "Weekly sync", 3, true),
            (2, LineKind::Work, "AI cost model outline", 29, true),
        ]
    );
}

/// A removed Chrono row stays removed when a later round replans its day for another edit:
/// the plan keeps the removal, not only the round that made it.
#[tokio::test]
async fn a_removed_chrono_row_stays_removed_in_later_rounds() {
    let h = Harness::new();
    let mut entries = monday_entries();
    entries.push(entry(3, monday(), 14, AI, "Review the pipeline", 1.0));
    let portal = StubServer::start(vec![user(), empty_view(), empty_view(), assigned()]);
    let chrono_stub = StubServer::start(vec![chrono(&entries)]);
    let two_lines = json!({"days": [{"date": "2026-10-05", "lines": [
        {"key": "c1", "title": "AI cost model draft", "hours": 3.5, "needs_detail": false},
        {"key": "c2", "title": "Pipeline review", "hours": 3.5, "needs_detail": false}
    ], "extra": []}]});
    let model = FakePlanModel::new(vec![Ok(two_lines)]);
    let (planned, io) = h
        .run(
            Run::Plan(monday_args()),
            &portal.base_url,
            &chrono_stub.base_url,
            &model,
        )
        .await;
    assert_eq!(planned.unwrap(), Outcome::Ok, "{}", io.err_text());
    portal.requests();

    let md = h.md();
    let removed: Vec<&str> = md
        .lines()
        .filter(|line| !line.contains("Review the pipeline"))
        .collect();
    assert_eq!(removed.len() + 1, md.lines().count(), "{md}");
    h.write_md(&(removed.join("\n") + "\n"));
    let portal = StubServer::start(vec![user(), assigned(), empty_view(), empty_view()]);
    let chrono_stub = StubServer::start(vec![chrono(&entries)]);
    let model = FakePlanModel::new(vec![Ok(answer(7.0))]);
    let (first, io) = h
        .run(Run::Replan, &portal.base_url, &chrono_stub.base_url, &model)
        .await;
    assert_eq!(first.unwrap(), Outcome::Ok, "{}", io.err_text());
    portal.requests();

    h.write_md(
        &h.md()
            .replace("AI cost model draft", "AI cost model outline"),
    );
    let portal = StubServer::start(vec![user(), assigned(), empty_view(), empty_view()]);
    let chrono_stub = StubServer::start(vec![chrono(&entries)]);
    let model = FakePlanModel::new(vec![]);
    let (second, io) = h
        .run(Run::Replan, &portal.base_url, &chrono_stub.base_url, &model)
        .await;

    assert_eq!(second.unwrap(), Outcome::Ok, "{}", io.err_text());
    portal.requests();
    assert!(model.calls().is_empty());
    let plan = h.state().load().unwrap().plan;
    assert_eq!(
        summary(&plan.days[0]),
        vec![
            (1, LineKind::Meeting, "Weekly sync", 4, true),
            (2, LineKind::Work, "AI cost model outline", 28, true),
        ]
    );
    let kept: Vec<&str> = plan.removed[&monday()]
        .iter()
        .map(|key| key.description.as_str())
        .collect();
    assert_eq!(kept, vec!["Review the pipeline"]);
}

// -- apply ------------------------------------------------------------------

#[tokio::test]
async fn apply_refuses_a_plan_edited_after_it_was_shown() {
    let h = Harness::new();
    h.planned_monday().await;
    h.write_md(&format!("{}\n", h.md()));

    let model = FakePlanModel::new(vec![]);
    let (result, _) = h.run(Run::Apply, NOWHERE, NOWHERE, &model).await;

    let message = format!("{:#}", result.unwrap_err());
    assert!(message.contains("changed after it was shown"), "{message}");
}

#[tokio::test]
async fn apply_creates_every_line_checks_devpro_and_drops_the_plan() {
    let h = Harness::new();
    h.planned_monday().await;
    let ok = json_200("{}");
    let portal = StubServer::start(vec![
        user(),
        assigned(),
        empty_view(),
        ok.clone(),
        ok,
        monday_written(),
    ]);

    let model = FakePlanModel::new(vec![]);
    let (result, io) = h.run(Run::Apply, &portal.base_url, NOWHERE, &model).await;

    assert_eq!(result.unwrap(), Outcome::Ok, "{}", io.err_text());
    assert_eq!(
        io.out,
        vec!["\u{2713} Пн 5 октября — 8.0 h in DevPro, 2 line(s) written"]
    );
    let bodies = posts(&portal.requests());
    assert_eq!(
        bodies[0],
        json!({
            "worklogDate": "2026-10-05",
            "projectUniqueId": "id-inveniam",
            "taskTitle": "Weekly sync",
            "billability": "Billable",
            "duration": 1.0,
            "description": null,
            "overtime": null,
            "expenseType": "None",
            "pif": null,
            "googleCalendarEventId": null
        })
    );
    assert_eq!(bodies[1]["taskTitle"], "AI cost model draft");
    assert_eq!(bodies[1]["duration"], 7.0);
    assert!(!h.state().md_path().exists());
}

#[tokio::test]
async fn a_day_devpro_changed_since_the_plan_is_not_written() {
    let h = Harness::new();
    h.planned_monday().await;
    let someone = view(&[(
        monday(),
        vec![worklog("w5", "Typed in the portal", "AI Practices", 2.0)],
    )]);
    let portal = StubServer::start(vec![user(), assigned(), someone.clone(), someone]);

    let model = FakePlanModel::new(vec![]);
    let (result, io) = h.run(Run::Apply, &portal.base_url, NOWHERE, &model).await;

    assert_eq!(result.unwrap(), Outcome::Failed);
    assert!(posts(&portal.requests()).is_empty());
    assert!(
        io.out_text()
            .contains("DevPro changed after the plan was shown"),
        "{}",
        io.out_text()
    );
}

#[tokio::test]
async fn apply_refuses_titles_still_marked_for_detail() {
    let h = Harness::new();
    h.planned_monday().await;
    let state = h.state();
    let mut plan = state.load().unwrap().plan;
    plan.days[0].lines[1].needs_detail = true;
    state.save(&plan, &render(&plan)).unwrap();

    let model = FakePlanModel::new(vec![]);
    let (result, _) = h.run(Run::Apply, NOWHERE, NOWHERE, &model).await;

    let message = format!("{:#}", result.unwrap_err());
    assert!(message.contains("Б2"), "{message}");
}

fn half_written_monday() -> String {
    view(&[(
        monday(),
        vec![worklog("w1", "Weekly sync", "Inveniam SOW #5", 1.0)],
    )])
}

fn deletes(requests: &[crate::api::stub::CapturedRequest]) -> Vec<String> {
    requests
        .iter()
        .filter(|r| r.method == "DELETE")
        .map(|r| r.target.clone())
        .collect()
}

#[tokio::test]
async fn a_failed_write_undoes_its_day_stops_the_run_and_drops_the_plan() {
    let h = Harness::new();
    h.planned_monday().await;
    let portal = StubServer::start(vec![
        user(),
        assigned(),
        empty_view(),
        json_200("{}"),
        response(500, "Internal Server Error", "text/plain", "boom"),
        half_written_monday(),
        json_200("{}"),
        empty_view(),
    ]);

    let model = FakePlanModel::new(vec![]);
    let (result, io) = h.run(Run::Apply, &portal.base_url, NOWHERE, &model).await;

    assert_eq!(result.unwrap(), Outcome::Failed);
    let requests = portal.requests();
    assert_eq!(posts(&requests).len(), 2);
    assert_eq!(deletes(&requests), vec!["/worklog/w1"]);
    let err = io.err_text();
    assert!(err.contains("Б2"), "{err}");
    assert!(err.contains("1 line(s) this run had written"), "{err}");
    assert!(err.contains("Writing stopped"), "{err}");
    assert!(!err.contains("Weekly sync"), "{err}");
    assert!(!h.state().md_path().exists());
}

#[tokio::test]
async fn an_undo_that_fails_names_each_line_left_and_its_command() {
    let h = Harness::new();
    h.planned_monday().await;
    let portal = StubServer::start(vec![
        user(),
        assigned(),
        empty_view(),
        json_200("{}"),
        response(500, "Internal Server Error", "text/plain", "boom"),
        half_written_monday(),
        response(500, "Internal Server Error", "text/plain", "boom"),
        half_written_monday(),
    ]);

    let model = FakePlanModel::new(vec![]);
    let (result, io) = h.run(Run::Apply, &portal.base_url, NOWHERE, &model).await;

    assert_eq!(result.unwrap(), Outcome::Failed);
    let err = io.err_text();
    assert!(err.contains("`tt-devpro api delete-worklog w1`"), "{err}");
    assert!(err.contains("a second time"), "{err}");
    assert!(
        err.contains("Weekly sync (1.0 h, Inveniam SOW #5)"),
        "{err}"
    );
    assert!(!h.state().md_path().exists());
}

#[tokio::test]
async fn a_timed_out_write_that_landed_counts_as_written() {
    let h = Harness::new();
    h.planned_monday().await;
    let portal = StubServer::start(vec![
        user(),
        assigned(),
        empty_view(),
        json_200("{}"),
        STALL.to_string(),
        monday_written(),
        monday_written(),
    ]);

    let model = FakePlanModel::new(vec![]);
    let (result, io) = h
        .run_with_timeout(
            Run::Apply,
            &portal.base_url,
            NOWHERE,
            &model,
            Duration::from_millis(500),
        )
        .await;

    assert_eq!(result.unwrap(), Outcome::Ok, "{}", io.err_text());
    assert_eq!(posts(&portal.requests()).len(), 2);
    assert!(
        io.err_text()
            .contains("timed out, but the line is in DevPro"),
        "{}",
        io.err_text()
    );
    assert!(
        io.out_text().contains("2 line(s) written"),
        "{}",
        io.out_text()
    );
}

#[tokio::test]
async fn a_timed_out_write_that_did_not_land_is_not_sent_again_and_is_recorded() {
    let h = Harness::new();
    h.planned_monday().await;
    let portal = StubServer::start(vec![
        user(),
        assigned(),
        empty_view(),
        json_200("{}"),
        STALL.to_string(),
        half_written_monday(),
        half_written_monday(),
        json_200("{}"),
        empty_view(),
    ]);

    let model = FakePlanModel::new(vec![]);
    let (result, io) = h
        .run_with_timeout(
            Run::Apply,
            &portal.base_url,
            NOWHERE,
            &model,
            Duration::from_millis(500),
        )
        .await;

    assert_eq!(result.unwrap(), Outcome::Failed);
    let requests = portal.requests();
    assert_eq!(posts(&requests).len(), 2);
    assert_eq!(deletes(&requests), vec!["/worklog/w1"]);
    assert!(
        io.err_text().contains("not sent again"),
        "{}",
        io.err_text()
    );
    assert!(
        io.err_text().contains("The next settle checks for it"),
        "{}",
        io.err_text()
    );
    assert!(!h.state().md_path().exists());
    // The plan is dropped; the write that may still land is not.
    assert_eq!(
        h.state().unconfirmed().unwrap(),
        vec![unconfirmed_on(monday(), 0)]
    );
}

// -- writes that timed out --------------------------------------------------

/// The Б2 write of [`planned_monday`] as the ledger keeps it after a timeout, sent
/// `minutes_ago` before [`nine_am`].
fn unconfirmed_on(date: NaiveDate, minutes_ago: i64) -> UnconfirmedWrite {
    UnconfirmedWrite {
        date,
        project_id: "id-ai".to_string(),
        devpro_project: "AI Practices".to_string(),
        title: "AI cost model draft".to_string(),
        quarters: 28,
        before_ids: Vec::new(),
        written_at: nine_am() - TimeDelta::minutes(minutes_ago),
    }
}

fn ten_am() -> DateTime<Utc> {
    nine_am() + TimeDelta::hours(1)
}

/// Monday after the rollback, with the timed-out Б2 write landed late as w7.
fn monday_with_the_late_write() -> String {
    view(&[(
        monday(),
        vec![worklog("w7", "AI cost model draft", "AI Practices", 7.0)],
    )])
}

#[tokio::test]
async fn a_late_landing_write_is_named_by_the_next_settle_and_its_day_is_not_planned() {
    let h = Harness::new();
    let tuesday = date(2026, 10, 6);
    let ledger = vec![unconfirmed_on(monday(), 60), unconfirmed_on(tuesday, 60)];
    h.state().save_unconfirmed(&ledger).unwrap();
    let portal = StubServer::start(vec![user(), empty_view(), monday_with_the_late_write()]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries())]);
    let model = FakePlanModel::new(vec![]);

    let (result, io) = h
        .run(
            Run::Plan(monday_args()),
            &portal.base_url,
            &chrono_stub.base_url,
            &model,
        )
        .await;

    assert_eq!(result.unwrap(), Outcome::Ok, "{}", io.err_text());
    let requests = portal.requests();
    assert_eq!(requests.len(), 3, "the held day asks for no assignments");
    assert!(requests.iter().all(|r| r.method == "GET"));
    assert!(model.calls().is_empty());
    let plan = h.state().load().unwrap().plan;
    assert!(plan.days.is_empty());
    let message = &plan.errors[0].message;
    assert_eq!(plan.errors[0].date, monday());
    assert!(
        message.contains("`tt-devpro api delete-worklog w7`"),
        "{message}"
    );
    assert!(
        message.contains("«AI cost model draft» (7.0 ч, AI Practices)"),
        "{message}"
    );
    let shown = io.out_text();
    assert!(shown.contains("не спланирован"), "{shown}");
    assert!(shown.contains("delete-worklog w7"), "{shown}");
    // Monday stays held until w7 is gone; Tuesday was not planned, so it waits its turn.
    assert_eq!(h.state().unconfirmed().unwrap(), ledger);
}

#[tokio::test]
async fn an_unconfirmed_write_within_its_window_holds_the_day() {
    let h = Harness::new();
    h.state()
        .record_unconfirmed(unconfirmed_on(monday(), 5))
        .unwrap();
    let portal = StubServer::start(vec![user(), empty_view(), empty_view()]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries())]);
    let model = FakePlanModel::new(vec![]);

    let (result, io) = h
        .run(
            Run::Plan(monday_args()),
            &portal.base_url,
            &chrono_stub.base_url,
            &model,
        )
        .await;

    assert_eq!(result.unwrap(), Outcome::Ok, "{}", io.err_text());
    portal.requests();
    assert!(model.calls().is_empty());
    let plan = h.state().load().unwrap().plan;
    assert!(plan.days.is_empty());
    let message = &plan.errors[0].message;
    assert!(message.contains("ещё может дойти"), "{message}");
    assert!(message.contains("после 09:10"), "{message}");
    assert_eq!(h.state().unconfirmed().unwrap().len(), 1);
}

#[tokio::test]
async fn an_expired_unconfirmed_write_is_dropped_and_the_day_planned() {
    let h = Harness::new();
    let ledger = vec![
        unconfirmed_on(monday(), 16),
        // Older than any scan reaches: nothing will check it again.
        unconfirmed_on(date(2026, 8, 1), 60),
    ];
    h.state().save_unconfirmed(&ledger).unwrap();
    let portal = StubServer::start(vec![user(), empty_view(), empty_view(), assigned()]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries())]);
    let model = FakePlanModel::new(vec![Ok(answer(7.0))]);

    let (result, io) = h
        .run(
            Run::Plan(monday_args()),
            &portal.base_url,
            &chrono_stub.base_url,
            &model,
        )
        .await;

    assert_eq!(result.unwrap(), Outcome::Ok, "{}", io.err_text());
    portal.requests();
    let plan = h.state().load().unwrap().plan;
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert_eq!(plan.days[0].total_quarters(), DAY_QUARTERS);
    assert!(!h.state().unconfirmed_path().exists());
}

/// An explicit range reaches further back than the scan, and `--apply` writes whatever range
/// it was shown: a write that timed out on such a day and landed holds the day like any other.
#[test]
fn an_unconfirmed_write_older_than_the_scan_is_still_checked_on_a_day_being_planned() {
    let old = date(2026, 8, 3);
    let late: WorklogDetail =
        serde_json::from_value(worklog("w9", "AI cost model draft", "AI Practices", 7.0)).unwrap();
    let portal = vec![PortalDay {
        date: old,
        logged_hours: 7.0,
        worklogs: vec![late],
    }];
    let ledger = vec![unconfirmed_on(old, 2)];

    let (kept, held) = check_unconfirmed(
        &ledger,
        &[old],
        &portal,
        nine_am(),
        nine_am().date_naive(),
        &Utc,
    );

    assert_eq!(kept, ledger);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].date, old);
    assert!(
        held[0]
            .message
            .contains("`tt-devpro api delete-worklog w9`"),
        "{}",
        held[0].message
    );
}

/// Yurii kept the late worklog and closed the day around it in the portal: nothing is left to
/// plan there, so the entry has done its work.
#[tokio::test]
async fn an_unconfirmed_write_on_a_day_devpro_holds_at_eight_hours_is_dropped() {
    let h = Harness::new();
    h.state()
        .record_unconfirmed(unconfirmed_on(monday(), 60))
        .unwrap();
    let full = view(&[(
        monday(),
        vec![
            worklog("w7", "AI cost model draft", "AI Practices", 7.0),
            worklog("w8", "Weekly sync", "Inveniam SOW #5", 1.0),
        ],
    )]);
    let portal = StubServer::start(vec![user(), empty_view(), full, assigned()]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries())]);
    let model = FakePlanModel::new(vec![]);

    let (result, io) = h
        .run(
            Run::Plan(monday_args()),
            &portal.base_url,
            &chrono_stub.base_url,
            &model,
        )
        .await;

    assert_eq!(result.unwrap(), Outcome::Ok, "{}", io.err_text());
    portal.requests();
    let plan = h.state().load().unwrap().plan;
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert_eq!(plan.closed, vec![monday()]);
    assert!(!h.state().unconfirmed_path().exists());
}

/// A day held by the ledger is a day error, so `--replan` checks it again; once the worklog
/// is gone and the window has passed, the day is planned.
#[tokio::test]
async fn a_held_day_is_planned_by_the_replan_once_the_write_cannot_land() {
    let mut h = Harness::new();
    h.state()
        .record_unconfirmed(unconfirmed_on(monday(), 5))
        .unwrap();
    let portal = StubServer::start(vec![user(), empty_view(), empty_view()]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries())]);
    let model = FakePlanModel::new(vec![]);
    let (held, _) = h
        .run(
            Run::Plan(monday_args()),
            &portal.base_url,
            &chrono_stub.base_url,
            &model,
        )
        .await;
    assert_eq!(held.unwrap(), Outcome::Ok);
    portal.requests();
    assert_eq!(h.state().load().unwrap().plan.errors.len(), 1);

    h.now = ten_am;
    let portal = StubServer::start(vec![user(), assigned(), empty_view(), empty_view()]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries())]);
    let model = FakePlanModel::new(vec![Ok(answer(7.0))]);
    let (result, io) = h
        .run(Run::Replan, &portal.base_url, &chrono_stub.base_url, &model)
        .await;

    assert_eq!(result.unwrap(), Outcome::Ok, "{}", io.err_text());
    portal.requests();
    let plan = h.state().load().unwrap().plan;
    assert!(plan.errors.is_empty(), "{:?}", plan.errors);
    assert_eq!(plan.days[0].total_quarters(), DAY_QUARTERS);
    assert!(!h.state().unconfirmed_path().exists());
}

#[tokio::test]
async fn days_in_error_are_not_written_and_stay_for_the_next_replan() {
    let h = Harness::new();
    let tuesday = date(2026, 10, 6);
    let portal = StubServer::start(vec![
        user(),
        empty_view(),
        empty_view(),
        assigned(),
        assigned(),
    ]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries())]);
    let model = FakePlanModel::new(vec![Ok(answer(7.0))]);
    let args = SettleArgs {
        to: Some(tuesday),
        ..monday_args()
    };
    let (planned, _) = h
        .run(
            Run::Plan(args),
            &portal.base_url,
            &chrono_stub.base_url,
            &model,
        )
        .await;
    assert_eq!(planned.unwrap(), Outcome::Ok);
    portal.requests();

    let ok = json_200("{}");
    let portal = StubServer::start(vec![
        user(),
        assigned(),
        empty_view(),
        ok.clone(),
        ok,
        monday_written(),
    ]);
    let (result, io) = h.run(Run::Apply, &portal.base_url, NOWHERE, &model).await;

    assert_eq!(result.unwrap(), Outcome::Ok, "{}", io.err_text());
    assert_eq!(posts(&portal.requests()).len(), 2);
    assert!(
        io.out_text().contains("kept for --replan"),
        "{}",
        io.out_text()
    );
    let left = h.state().load().unwrap();
    assert!(left.md_unchanged());
    assert!(left.plan.days.is_empty());
    assert_eq!(left.plan.errors[0].date, tuesday);
}

#[tokio::test]
async fn apply_with_only_errors_left_writes_nothing() {
    let h = Harness::new();
    let state = h.state();
    let plan = Plan {
        prefix: "Б".to_string(),
        cutoff: monday(),
        days: Vec::new(),
        errors: vec![DayError {
            date: monday(),
            message: "unmapped".to_string(),
        }],
        closed: Vec::new(),
        removed: BTreeMap::new(),
    };
    state.save(&plan, &render(&plan)).unwrap();

    let model = FakePlanModel::new(vec![]);
    let (result, io) = h.run(Run::Apply, NOWHERE, NOWHERE, &model).await;

    assert_eq!(result.unwrap(), Outcome::Ok);
    assert!(
        io.out_text().starts_with("Nothing to apply"),
        "{}",
        io.out_text()
    );
}

// -- the billing-period block ----------------------------------------------

fn ptr_periods() -> String {
    json_200(
        &json!([
            {"ptrPeriod": "October 01 - 15, 2026", "expectedHours": 88, "loggedHours": 14.75,
             "isShowExpectedHours": true, "differenceHours": -73.25},
            {"ptrPeriod": "October 16 - 31, 2026", "expectedHours": 88, "loggedHours": 0.0,
             "isShowExpectedHours": true, "differenceHours": -88.0}
        ])
        .to_string(),
    )
}

/// The read side spells billability `Billable` / `Non-billable`.
fn read_worklog(project: &str, billability: &str, hours: f64) -> Value {
    let mut worklog = worklog("r", "Earlier work", project, hours);
    worklog["billability"] = json!(billability);
    worklog
}

fn october_so_far() -> String {
    view(&[
        (
            date(2026, 10, 1),
            vec![read_worklog("Inveniam SOW #5", "Billable", 6.75)],
        ),
        (
            date(2026, 10, 2),
            vec![read_worklog("AI Practices", "Non-billable", 8.0)],
        ),
    ])
}

fn allocate_inveniam(h: &mut Harness) {
    h.config.allocations = vec![crate::config::Allocation {
        devpro_project: "Inveniam SOW #5".to_string(),
        fte: 0.5,
    }];
}

/// Monday's plan holds the billable meeting (1.0 h) not yet in DevPro; with 6.75 h already
/// recorded on the 1st that is 7.75 h of 44 by Wednesday, 5 of 11 days gone.
#[tokio::test]
async fn the_period_block_counts_devpro_and_the_plan_against_the_allocation() {
    let mut h = Harness::new();
    allocate_inveniam(&mut h);
    h.planned_monday().await;

    let portal = StubServer::start(vec![ptr_periods(), october_so_far()]);
    let model = FakePlanModel::new(vec![]);
    let (result, io) = h.run(Run::Period, &portal.base_url, NOWHERE, &model).await;

    assert_eq!(result.unwrap(), Outcome::Ok);
    assert_eq!(
        io.out,
        vec![
            String::new(),
            "**Период 1–15 октября** — прошло 5 из 11 рабочих дней (45%), по Ср 7 октября"
                .to_string(),
            "- 💵 Inveniam SOW #5 — 7.75 из 44.0 ч (18%) █░░░░┃░░░░░ ↗39% · 0.19 FTE из 0.5 · \
             из них в плане 1.0 ч"
                .to_string(),
        ]
    );
    let requests = portal.requests();
    assert_eq!(requests[0].target, "/contact/ptrPeriods?onDate=2026-10-07");
    assert!(
        requests[1]
            .target
            .starts_with("/timeTracking/normalView?period=2026-10-01"),
        "{}",
        requests[1].target
    );
}

/// «Все дни закрыты» is still a morning the period has moved on.
#[tokio::test]
async fn the_period_block_follows_a_plan_with_every_day_closed() {
    let h = Harness::new();
    let plan = Plan {
        prefix: "Б".to_string(),
        cutoff: date(2026, 10, 7),
        days: Vec::new(),
        errors: Vec::new(),
        closed: Vec::new(),
        removed: BTreeMap::new(),
    };
    h.state().save(&plan, &render(&plan)).unwrap();

    let portal = StubServer::start(vec![ptr_periods(), october_so_far()]);
    let model = FakePlanModel::new(vec![]);
    let (result, io) = h.run(Run::Period, &portal.base_url, NOWHERE, &model).await;

    assert_eq!(result.unwrap(), Outcome::Ok);
    assert_eq!(
        io.out[2],
        "- 💵 Inveniam SOW #5 — 6.75 ч · 0.17 FTE · плановой аллокации нет: allocations в \
         ~/.config/tt-devpro/config.yaml"
    );
}

/// The plan is already shown and stored when the block is read, so a portal failure costs
/// the block and nothing else.
#[tokio::test]
async fn a_failed_period_read_is_one_warning_and_the_run_succeeds() {
    let h = Harness::new();
    h.planned_monday().await;

    let portal = StubServer::start(vec![response(
        500,
        "Internal Server Error",
        "text/plain",
        "boom",
    )]);
    let model = FakePlanModel::new(vec![]);
    let (result, io) = h.run(Run::Period, &portal.base_url, NOWHERE, &model).await;

    assert_eq!(result.unwrap(), Outcome::Ok);
    assert_eq!(io.out.len(), 2, "{}", io.out_text());
    assert_eq!(io.out[0], "");
    assert!(
        io.out[1].starts_with("⚠️ Период не прочитан: "),
        "{}",
        io.out[1]
    );
}

/// The block's place in the run: `settle` shows the plan, stores it, and only then prints
/// the block under it — on stdout, never into `plan.md`.
#[tokio::test]
async fn settle_prints_the_period_block_under_the_plan_it_shows() {
    let h = Harness::new();
    let portal = StubServer::start(vec![
        user(),
        empty_view(),
        empty_view(),
        assigned(),
        ptr_periods(),
        october_so_far(),
    ]);
    let chrono_stub = StubServer::start(vec![chrono(&monday_entries())]);
    let model = FakePlanModel::new(vec![Ok(answer(7.0))]);
    let (result, io) = h
        .run(
            Run::Settle(monday_args()),
            &portal.base_url,
            &chrono_stub.base_url,
            &model,
        )
        .await;

    assert_eq!(result.unwrap(), Outcome::Ok, "{}", io.err_text());
    let header = io
        .out
        .iter()
        .position(|line| line.starts_with("**Период 1–15 октября**"))
        .unwrap_or_else(|| panic!("no period header in:\n{}", io.out_text()));
    let md = h.md();
    assert_eq!(io.out[..header - 1].join("\n"), md);
    assert_eq!(io.out[header - 1], "");
    assert!(!md.contains("Период"), "{md}");
    assert!(
        io.out[header + 1].starts_with("- 💵 Inveniam SOW #5 — 7.75 ч"),
        "{}",
        io.out[header + 1]
    );
}

/// A run that fails shows no plan, so no block follows it and the portal is not asked.
#[tokio::test]
async fn a_failed_replan_prints_no_period_block() {
    let h = Harness::new();
    let args = SettleArgs {
        replan: true,
        ..SettleArgs::default()
    };

    let model = FakePlanModel::new(vec![]);
    let (result, io) = h.run(Run::Settle(args), NOWHERE, NOWHERE, &model).await;

    assert!(result.is_err());
    assert!(!io.out_text().contains("Период"), "{}", io.out_text());
}

/// `--apply` writes and reads DevPro back; the period reads are never among its requests.
#[tokio::test]
async fn apply_prints_no_period_block() {
    let h = Harness::new();
    h.planned_monday().await;
    let ok = json_200("{}");
    let portal = StubServer::start(vec![
        user(),
        assigned(),
        empty_view(),
        ok.clone(),
        ok,
        monday_written(),
    ]);

    let model = FakePlanModel::new(vec![]);
    let (result, io) = h.run(Run::Apply, &portal.base_url, NOWHERE, &model).await;

    assert_eq!(result.unwrap(), Outcome::Ok, "{}", io.err_text());
    assert!(!io.out_text().contains("Период"), "{}", io.out_text());
    assert!(
        portal
            .requests()
            .iter()
            .all(|request| !request.target.contains("ptrPeriods"))
    );
}
