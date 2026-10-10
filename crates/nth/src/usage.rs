//! `nth usage`: what a saved session spent, the latest by default, or with
//! `--all` what every saved session spent per day and model. Prices are
//! list-price estimates at models.dev's rates, when the providers' listing
//! answers in time.

use std::{
    collections::BTreeMap,
    io::{IsTerminal, Write},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use jiff::{Timestamp, civil::Date, tz::TimeZone};
use nth_protocol::{Cost, Listing, Provider};
use nth_session::{
    Price, Session, Spend, Store, Total,
    usage::{self, padded, row},
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
    let session = pick(&store, session.as_deref()).await?;
    match json {
        true => writeln!(out, "{}", session_json(&session, &cost))?,
        false => session_text(&mut out, &session, &cost)?,
    }
    Ok(())
}

/// The saved session whose id starts with `id`, or the latest one.
async fn pick(store: &Store, id: Option<&str>) -> Result<Session> {
    let sessions = store.list().await?;
    let summary = match id {
        None => sessions.first().context("no saved sessions")?,
        Some(id) => {
            let mut found = sessions.iter().filter(|s| s.id.to_string().starts_with(id));
            match (found.next(), found.next()) {
                (Some(summary), None) => summary,
                (None, _) => bail!("no saved session starts with {id}"),
                (Some(_), Some(_)) => bail!("more than one saved session starts with {id}"),
            }
        }
    };
    Ok(store.load(summary.id).await?)
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
        "→".cyan().bold(),
        title.bold(),
        session.id.to_string().dimmed()
    )?;
    let ledger = &session.usage;
    if ledger.total().steps == 0 {
        writeln!(out, "  {}", "nothing spent in this session".dimmed())?;
        return Ok(());
    }
    let price = |model: &str, total: Total| usage::price([(model, total)], cost);
    table(out, None, &[row("all", ledger.total(), ledger.price(cost))])?;
    let models: Vec<_> = ledger
        .by_model()
        .into_iter()
        .map(|(model, total)| row(model, total, price(model, total)))
        .collect();
    table(out, Some("per model"), &models)?;
    let turns: Vec<_> = usage::turns(ledger)
        .map(|(who, spend)| row(&who, spend.into(), price(&spend.model, spend.into())))
        .collect();
    table(out, Some("per turn"), &turns)?;
    note(out)
}

fn session_json(session: &Session, cost: &dyn Fn(&str) -> Option<Cost>) -> Value {
    let ledger = &session.usage;
    let price = |model: &str, total: Total| usage::price([(model, total)], cost);
    let models: Vec<Value> = ledger
        .by_model()
        .into_iter()
        .map(|(model, total)| {
            let mut line = counts(total, price(model, total));
            line["model"] = json!(model);
            line
        })
        .collect();
    let turns: Vec<Value> = usage::turns(ledger)
        .map(|(_, spend)| {
            let mut line = counts(spend.into(), price(&spend.model, spend.into()));
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
        "models": models,
        "turns": turns,
    })
}

/// Totals per day, newest first, and per model within each day.
type Days<'a> = Vec<(Date, BTreeMap<&'a str, Total>)>;

fn per_day<'a>(spends: &[&'a Spend]) -> Days<'a> {
    let mut days: BTreeMap<Date, BTreeMap<&str, Total>> = BTreeMap::new();
    for spend in spends.iter().filter(|spend| spend.steps > 0) {
        let Some(day) = day(spend.at) else { continue };
        days.entry(day)
            .or_default()
            .entry(&spend.model)
            .or_default()
            .add(spend);
    }
    days.into_iter().rev().collect()
}

fn days_text(out: &mut impl Write, days: &Days, cost: &dyn Fn(&str) -> Option<Cost>) -> Result<()> {
    if days.is_empty() {
        writeln!(
            out,
            "{} {}",
            "→".cyan().bold(),
            "nothing spent yet".dimmed()
        )?;
        return Ok(());
    }
    let price = |model: &str, total: Total| usage::price([(model, total)], cost);
    let mut all: BTreeMap<&str, Total> = BTreeMap::new();
    for (_, models) in days {
        for (model, total) in models {
            *all.entry(model).or_default() += *total;
        }
    }
    let all_rows: Vec<_> = all
        .iter()
        .map(|(model, total)| row(model, *total, price(model, *total)))
        .collect();
    table(out, Some("all days"), &all_rows)?;
    for (day, models) in days {
        let rows: Vec<_> = models
            .iter()
            .map(|(model, total)| row(model, *total, price(model, *total)))
            .collect();
        writeln!(out)?;
        let total: Total = models.values().sum();
        let day_price = usage::price(models.iter().map(|(m, t)| (*m, *t)), cost);
        let head = row(&day.to_string(), total, day_price);
        writeln!(out, "{}", paint(&[head]).concat().bold())?;
        for line in paint(&rows) {
            writeln!(out, "  {line}")?;
        }
    }
    note(out)
}

fn days_json(out: &mut impl Write, days: &Days, cost: &dyn Fn(&str) -> Option<Cost>) -> Result<()> {
    for (day, models) in days {
        for (model, total) in models {
            let mut line = counts(*total, usage::price([(*model, *total)], cost));
            line["day"] = json!(day.to_string());
            line["model"] = json!(model);
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

/// A titled block of rows, indented under its title.
fn table(out: &mut impl Write, title: Option<&str>, rows: &[[String; 6]]) -> Result<()> {
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
/// half on and yellow under it, prices magenta.
fn paint(rows: &[[String; 6]]) -> Vec<String> {
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
            let share = row[3].trim_start().split('%').next();
            let cached = match share.and_then(|n| n.parse::<u32>().ok()) {
                Some(share) if share >= 50 => row[3].green().to_string(),
                Some(_) => row[3].yellow().to_string(),
                None => row[3].dimmed().to_string(),
            };
            let cells = [
                who.join(" "),
                row[1].dimmed().to_string(),
                row[2].clone(),
                cached,
                row[4].clone(),
                row[5].magenta().to_string(),
            ];
            let used = if row[5].trim().is_empty() { 5 } else { 6 };
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
        "cache_read": tokens.cache_read,
        "cache_write": tokens.cache_write,
        "price": price.map(|p| json!({ "dollars": p.dollars, "partial": p.partial })),
    })
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
            },
            at,
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
                let models = models.iter().map(|(m, t)| (*m, t.tokens.input));
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
