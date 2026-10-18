//! `nth usage`: what a saved session spent, the latest by default, or with
//! `--all` what every saved session spent per day and model. Prices are
//! list-price estimates at models.dev's rates, when the providers' listing
//! answers in time.

use std::{
    collections::BTreeMap,
    io::{IsTerminal, Write},
    time::{Duration, SystemTime},
};

use anyhow::Result;
use jiff::{Timestamp, civil::Date, tz::TimeZone};
use nth_icons::icons;
use nth_protocol::{Cost, Listing, Provider};
use nth_session::{
    Price, Session, Spend, Store, Total,
    usage::{self, Row, ToolUse, padded, row, short, short_duration},
};
use owo_colors::OwoColorize;
use serde_json::{Value, json};

use crate::{config::Config, providers};

/// How long a price waits for the model listing.
const LISTING_WAIT: Duration = Duration::from_secs(3);

/// The providers' listing, for prices; `None` when it is slow or fails,
/// which costs only the prices.
pub async fn listing(provider: &dyn Provider) -> Option<Listing> {
    tokio::time::timeout(LISTING_WAIT, provider.models())
        .await
        .ok()?
        .ok()
}

pub async fn run(session: Option<String>, all: bool, json: bool, config: Config) -> Result<()> {
    let store = Store::open()?;
    // No provider configured costs the prices, not the command.
    let listing = match providers(&config) {
        Ok(providers) => listing(&providers).await,
        Err(_) => None,
    };
    let cost = |model: &str| listing.as_ref()?.cost_of(model);
    let mut out = std::io::stdout().lock();
    let json = json || !out.is_terminal();
    if all {
        let spending = store.spending().await?;
        let spends: Vec<&Spend> = spending.iter().flat_map(|s| s.usage.spends()).collect();
        let days = per_day(&spends);
        return match json {
            true => days_json(&mut out, &days, &cost),
            false => days_text(&mut out, &days, &cost),
        };
    }
    let session = crate::session::pick(&store, session.as_deref()).await?;
    match json {
        true => writeln!(out, "{}", session_json(&session, &cost))?,
        false => session_text(&mut out, &session, &cost)?,
    }
    Ok(())
}

fn session_text(
    out: &mut impl Write,
    session: &Session,
    cost: &dyn Fn(&str) -> Option<Cost>,
) -> Result<()> {
    let title = session.title().unwrap_or_default();
    writeln!(
        out,
        "{} {}  {}",
        icons().to.cyan().bold(),
        title.bold(),
        session.id.to_string().dimmed()
    )?;
    let ledger = &session.usage;
    if ledger.total().steps == 0 {
        writeln!(out, "  {}", "nothing spent in this session".dimmed())?;
        return Ok(());
    }
    table(out, None, &[row("all", ledger.total(), ledger.price(cost))])?;
    let models: Vec<_> = ledger
        .by_model()
        .into_iter()
        .map(|(model, total)| row(model, total, ledger.price_of(model, cost)))
        .collect();
    table(out, Some("per model"), &models)?;
    let turns: Vec<_> = usage::turns(ledger)
        .map(|(who, spend)| row(&who, spend.into(), spend.price(cost)))
        .collect();
    table(out, Some("per turn"), &turns)?;
    if let Some(saved) = ledger.saved(cost) {
        writeln!(out)?;
        writeln!(
            out,
            "  {} {}",
            format!("${saved:.2}").green(),
            "saved by caching".dimmed()
        )?;
    }
    note(out)
}

fn session_json(session: &Session, cost: &dyn Fn(&str) -> Option<Cost>) -> Value {
    let ledger = &session.usage;
    let models: Vec<Value> = ledger
        .by_model()
        .into_iter()
        .map(|(model, total)| {
            let mut line = counts(total, ledger.price_of(model, cost));
            line["model"] = json!(model);
            line
        })
        .collect();
    let turns: Vec<Value> = usage::turns(ledger)
        .map(|(_, spend)| {
            let mut line = counts(spend.into(), spend.price(cost));
            line["model"] = json!(spend.model);
            line["agent"] = json!(spend.agent);
            line["at"] = json!(timestamp(spend.at));
            line
        })
        .collect();
    json!({
        "id": session.id,
        "title": session.title(),
        "total": counts(ledger.total(), ledger.price(cost)),
        "saved": ledger.saved(cost),
        "models": models,
        "turns": turns,
        "tools": tools_value(session.usage.spends()),
    })
}

/// Spends per day, newest first, and per model within each day.
type Days<'a> = Vec<(Date, BTreeMap<&'a str, Vec<&'a Spend>>)>;

fn per_day<'a>(spends: &[&'a Spend]) -> Days<'a> {
    let mut days: BTreeMap<Date, BTreeMap<&str, Vec<&Spend>>> = BTreeMap::new();
    for spend in spends.iter().filter(|spend| spend.steps > 0) {
        let Some(day) = day(spend.at) else { continue };
        days.entry(day)
            .or_default()
            .entry(&spend.model)
            .or_default()
            .push(spend);
    }
    days.into_iter().rev().collect()
}

fn days_text(out: &mut impl Write, days: &Days, cost: &dyn Fn(&str) -> Option<Cost>) -> Result<()> {
    if days.is_empty() {
        writeln!(
            out,
            "{} {}",
            icons().to.cyan().bold(),
            "nothing spent yet".dimmed()
        )?;
        return Ok(());
    }
    let spends: Vec<&Spend> = days
        .iter()
        .flat_map(|(_, models)| models.values())
        .flatten()
        .copied()
        .collect();
    let mut all: BTreeMap<&str, Vec<&Spend>> = BTreeMap::new();
    for (_, models) in days {
        for (model, spends) in models {
            all.entry(model).or_default().extend(spends);
        }
    }
    let all_rows: Vec<_> = all
        .iter()
        .map(|(model, spends)| {
            let total = usage::total(spends.iter().copied());
            row(
                model,
                total,
                usage::price_spends(spends.iter().copied(), cost),
            )
        })
        .collect();
    table(out, Some("all days"), &all_rows)?;
    for (day, models) in days {
        let rows: Vec<_> = models
            .iter()
            .map(|(model, spends)| {
                let total = usage::total(spends.iter().copied());
                row(
                    model,
                    total,
                    usage::price_spends(spends.iter().copied(), cost),
                )
            })
            .collect();
        writeln!(out)?;
        let total = usage::total(models.values().flatten().copied());
        let day_price = usage::price_spends(models.values().flatten().copied(), cost);
        let head = row(&day.to_string(), total, day_price);
        writeln!(out, "{}", paint(&[head]).concat().bold())?;
        for line in paint(&rows) {
            writeln!(out, "  {line}")?;
        }
    }
    tools_text(out, &usage::tools(spends.iter().copied()))?;
    let saved: f64 = spends.iter().filter_map(|spend| spend.saved(cost)).sum();
    if saved > 0.0 {
        writeln!(out)?;
        writeln!(
            out,
            "  {} {}",
            format!("${saved:.2}").green(),
            "saved by caching".dimmed()
        )?;
    }
    note(out)
}

fn days_json(out: &mut impl Write, days: &Days, cost: &dyn Fn(&str) -> Option<Cost>) -> Result<()> {
    for (day, models) in days {
        for (model, spends) in models {
            let total = usage::total(spends.iter().copied());
            let mut line = counts(total, usage::price_spends(spends.iter().copied(), cost));
            line["day"] = json!(day.to_string());
            line["model"] = json!(model);
            writeln!(out, "{line}")?;
        }
    }
    let spends: Vec<&Spend> = days
        .iter()
        .flat_map(|(_, models)| models.values())
        .flatten()
        .copied()
        .collect();
    for (name, use_of) in usage::tools(spends.iter().copied()) {
        writeln!(
            out,
            "{}",
            json!({
                "tool": name,
                "calls": use_of.calls,
                "failed": use_of.failed,
                "duration_ms": use_of.duration.as_millis() as u64,
                "output": use_of.output,
            })
        )?;
    }
    Ok(())
}

/// The tool calls' totals as a table of rows: calls, failures, the mean
/// time and the output returned, all time.
fn tools_text(out: &mut impl Write, tools: &BTreeMap<&str, ToolUse>) -> Result<()> {
    if tools.is_empty() {
        return Ok(());
    }
    writeln!(out)?;
    writeln!(out, "{}", "tools".bold())?;
    let width = tools
        .keys()
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(0);
    for (name, use_of) in tools {
        let mean = use_of
            .duration
            .checked_div(use_of.calls)
            .unwrap_or_default();
        let failed = match use_of.failed {
            0 => use_of.failed.to_string().dimmed().to_string(),
            n => n.to_string().yellow().to_string(),
        };
        writeln!(
            out,
            "  {}  {} {}  {} {}  {} avg  {} out",
            format!("{name:<width$}").blue(),
            use_of.calls,
            "calls".dimmed(),
            failed,
            "failed".dimmed(),
            short_duration(mean).dimmed(),
            short(use_of.output),
        )?;
    }
    Ok(())
}

/// A titled block of rows, indented under its title.
fn table(out: &mut impl Write, title: Option<&str>, rows: &[Row]) -> Result<()> {
    if let Some(title) = title {
        writeln!(out)?;
        writeln!(out, "{}", title.bold())?;
    }
    for line in paint(rows) {
        writeln!(out, "  {line}")?;
    }
    Ok(())
}

/// Rows in columns, coloured as the TUI's usage tab: models blue,
/// agents cyan, turn numbers and steps dim, the cache share green from
/// half on and yellow under it, the write and think shares plain, prices
/// magenta.
fn paint(rows: &[Row]) -> Vec<String> {
    padded(rows)
        .iter()
        .map(|row| {
            let who: Vec<String> = row[0]
                .split(' ')
                .map(|word| match word {
                    // A day heading reads as a date, not a model.
                    "" | "all" => word.to_string(),
                    _ if word.starts_with(|c: char| c.is_ascii_digit()) => word.to_string(),
                    _ if word.starts_with('#') => word.dimmed().to_string(),
                    _ if word.starts_with('@') => word.cyan().to_string(),
                    _ => word.blue().to_string(),
                })
                .collect();
            let share = |cell: &str| -> String {
                let share = cell.trim_start().split('%').next();
                match share.and_then(|n| n.parse::<u32>().ok()) {
                    Some(share) if share >= 50 => cell.green().to_string(),
                    Some(_) => cell.yellow().to_string(),
                    None => cell.dimmed().to_string(),
                }
            };
            let cells = [
                who.join(" "),
                row[1].dimmed().to_string(),
                row[2].clone(),
                share(&row[3]),
                row[4].clone(),
                row[5].clone(),
                row[6].clone(),
                row[7].magenta().to_string(),
            ];
            let used = if row[7].trim().is_empty() { 7 } else { 8 };
            cells[..used].join("  ")
        })
        .collect()
}

fn note(out: &mut impl Write) -> Result<()> {
    writeln!(out)?;
    let note = "prices are list-price estimates from models.dev, whatever your plan bills";
    writeln!(out, "{}", note.dimmed())?;
    Ok(())
}

/// The counts as JSON, with the price when known.
fn counts(total: Total, price: Option<Price>) -> Value {
    let tokens = total.tokens;
    json!({
        "steps": total.steps,
        "input": tokens.input,
        "output": tokens.output,
        "reasoning": tokens.reasoning,
        "cache_read": tokens.cache_read,
        "cache_write": tokens.cache_write,
        "price": price.map(|p| json!({ "dollars": p.dollars, "partial": p.partial })),
    })
}

/// `spends`' tool calls as JSON objects, name and totals.
fn tools_value<'a>(spends: impl IntoIterator<Item = &'a Spend>) -> Value {
    Value::Array(
        usage::tools(spends)
            .into_iter()
            .map(|(name, use_of)| {
                json!({
                    "name": name,
                    "calls": use_of.calls,
                    "failed": use_of.failed,
                    "duration_ms": use_of.duration.as_millis() as u64,
                    "output": use_of.output,
                })
            })
            .collect(),
    )
}

/// The local day `at` fell on.
fn day(at: SystemTime) -> Option<Date> {
    let at = Timestamp::try_from(at).ok()?;
    Some(at.to_zoned(TimeZone::system()).date())
}

fn timestamp(at: SystemTime) -> Option<String> {
    Timestamp::try_from(at).ok().map(|at| at.to_string())
}

#[cfg(test)]
mod tests {
    use nth_protocol::Usage;

    use super::*;

    fn spend(model: &str, agent: Option<&str>, at: SystemTime, input: u64) -> Spend {
        Spend {
            model: model.into(),
            agent: agent.map(String::from),
            steps: 1,
            tokens: Usage {
                input,
                output: 1,
                cache_read: Some(input / 2),
                cache_write: None,
                reasoning: None,
            },
            at,
            cost: None,
            tools: Default::default(),
        }
    }

    fn at(day: &str) -> SystemTime {
        let date: Date = day.parse().expect("a date");
        let zoned = date.to_zoned(TimeZone::system()).expect("in range");
        SystemTime::from(zoned.timestamp()) + Duration::from_secs(12 * 3600)
    }

    #[test]
    fn days_newest_first_with_their_models() {
        let spends = [
            spend("glm", None, at("2026-10-09"), 100),
            spend("glm", None, at("2026-10-10"), 10),
            spend("kimi", Some("explore"), at("2026-10-10"), 20),
            spend("glm", None, at("2026-10-10"), 30),
        ];
        let refs: Vec<&Spend> = spends.iter().collect();
        let days = per_day(&refs);

        let shape: Vec<(String, Vec<(&str, u64)>)> = days
            .iter()
            .map(|(day, models)| {
                let models = models
                    .iter()
                    .map(|(m, spends)| (*m, usage::total(spends.iter().copied()).tokens.input));
                (day.to_string(), models.collect())
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("2026-10-10".into(), vec![("glm", 40), ("kimi", 20)]),
                ("2026-10-09".into(), vec![("glm", 100)]),
            ]
        );
    }

    #[test]
    fn tools_are_listed_all_time() {
        let mut mine = spend("glm", None, at("2026-10-10"), 10);
        mine.tool(
            "read",
            &nth_protocol::Message::ToolResult {
                call_id: "1".into(),
                content: "hi".into(),
                images: Vec::new(),
            },
            Duration::from_millis(50),
        );
        let value = tools_value([&mine]);
        assert_eq!(value[0]["name"], "read");
        assert_eq!(value[0]["calls"], 1);
        assert_eq!(value[0]["duration_ms"], 50);
    }

    #[test]
    fn a_session_as_json_has_its_total_models_and_turns() {
        let mut session = Session::new("glm", "/repo".into());
        session
            .usage
            .record(spend("glm", None, at("2026-10-10"), 1_000));
        session
            .usage
            .record(spend("kimi", Some("explore"), at("2026-10-10"), 10));
        let cost = |model: &str| {
            (model == "glm").then_some(Cost {
                input: 1.0,
                output: 0.0,
                cache_read: Some(0.0),
                cache_write: None,
            })
        };

        let line = session_json(&session, &cost);
        assert_eq!(line["total"]["steps"], 2);
        assert_eq!(line["total"]["input"], 1_010);
        assert_eq!(line["total"]["cache_read"], 505);
        assert_eq!(line["total"]["price"]["partial"], true);
        assert_eq!(line["models"][1]["model"], "kimi");
        assert_eq!(line["models"][1]["price"], Value::Null);
        assert_eq!(line["turns"][1]["agent"], "explore");
        assert!(
            line["turns"][0]["at"]
                .as_str()
                .is_some_and(|at| at.starts_with("2026-10-10"))
        );
    }
}
