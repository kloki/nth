use futures::{FutureExt, future::BoxFuture};
use jiff::{SignedDuration, Timestamp, Zoned, civil};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec, Wakeup, clock};
use serde::Deserialize;
use serde_json::json;

/// The furthest ahead a wakeup may be: nth is a session you sit at, not a
/// scheduler.
const HORIZON: SignedDuration = SignedDuration::from_hours(24);

/// Asks the front-end for a turn later, with a prompt the model leaves
/// itself.
pub struct ScheduleWakeup;

#[derive(Deserialize)]
struct Args {
    prompt: Option<String>,
    reason: Option<String>,
    delay_seconds: Option<i64>,
    at: Option<String>,
    #[serde(default)]
    cancel: bool,
}

impl Tool for ScheduleWakeup {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "schedule_wakeup",
            description: include_str!("description.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "What you get when you wake: what to check and what to do with it" },
                    "reason": { "type": "string", "description": "A few words on why, shown to the user while it is pending" },
                    "delay_seconds": { "type": "integer", "minimum": 1, "maximum": 86400, "description": "Wake this many seconds from now" },
                    "at": { "type": "string", "description": "Wake at this local time of day (HH:MM, next occurrence) or date and time (2026-10-11T15:00)" },
                    "cancel": { "type": "boolean", "description": "Drop the pending wakeup instead of scheduling one" }
                }
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            let args: Args = crate::parse_args(args)?;
            if !ctx.alarm.reaches_front_end() {
                return Err(
                    "there is no front-end to wake you in this session; nothing would start the turn"
                        .into(),
                );
            }
            let now = Zoned::now();
            if args.cancel {
                let dropped = ctx.alarm.cancel().await.ok_or(GONE)?;
                return Ok(match dropped {
                    Some(old) => format!("Cancelled the wakeup {}.", describe(&old, &now)),
                    None => "No wakeup was pending.".into(),
                });
            }
            let at = target(args.delay_seconds, args.at.as_deref(), &now)?;
            let prompt = required(args.prompt, "prompt")?;
            let reason = required(args.reason, "reason")?;
            let wakeup = Wakeup { at, prompt, reason };
            let set = format!(
                "Wakeup set for {} (in {}; now {}).",
                clock(&at.to_zoned(now.time_zone().clone())),
                span(at.duration_since(now.timestamp())),
                clock(&now),
            );
            Ok(match ctx.alarm.set(wakeup).await.ok_or(GONE)? {
                Some(old) => format!("{set} It replaced the wakeup {}.", describe(&old, &now)),
                None => set,
            })
        }
        .boxed()
    }
}

const GONE: &str = "the front-end is gone; nothing would wake you";

fn required(value: Option<String>, name: &str) -> Result<String, String> {
    value
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("`{name}` is required to schedule a wakeup"))
}

/// When to wake, from a delay or a time: a bare time of day is its next
/// occurrence, a date and time without an offset is local.
fn target(delay: Option<i64>, at: Option<&str>, now: &Zoned) -> Result<Timestamp, String> {
    let target = match (delay, at) {
        (Some(_), Some(_)) => return Err("pass `delay_seconds` or `at`, not both".into()),
        (None, None) => return Err("pass `delay_seconds` or `at`".into()),
        (Some(seconds), None) => {
            if !(1..=HORIZON.as_secs()).contains(&seconds) {
                return Err(format!(
                    "`delay_seconds` must be from 1 to {}",
                    HORIZON.as_secs()
                ));
            }
            now.timestamp() + SignedDuration::from_secs(seconds)
        }
        (None, Some(at)) => at_time(at.trim(), now)?,
    };
    let ahead = target.duration_since(now.timestamp());
    if ahead <= SignedDuration::ZERO {
        return Err(format!(
            "{at:?} has passed; it is now {}",
            clock(now),
            at = at.unwrap_or_default()
        ));
    }
    if ahead > HORIZON {
        return Err(format!(
            "a wakeup must be within 24 hours; it is now {}",
            clock(now)
        ));
    }
    Ok(target)
}

fn at_time(at: &str, now: &Zoned) -> Result<Timestamp, String> {
    let tz = now.time_zone().clone();
    let local = |datetime: civil::DateTime| {
        datetime
            .to_zoned(tz.clone())
            .map(|z| z.timestamp())
            .map_err(|e| format!("cannot place {at:?} in the local timezone: {e}"))
    };
    // A time parses out of a full datetime too, so the full forms go first.
    if let Ok(timestamp) = at.parse::<Timestamp>() {
        return Ok(timestamp);
    }
    if let Ok(datetime) = at.parse::<civil::DateTime>() {
        return local(datetime);
    }
    let Ok(time) = at.parse::<civil::Time>() else {
        return Err(format!(
            "cannot read {at:?} as a time: use HH:MM or a date and time such as 2026-10-11T15:00"
        ));
    };
    let today = local(now.date().to_datetime(time))?;
    if today > now.timestamp() {
        return Ok(today);
    }
    let tomorrow = now.date().tomorrow().map_err(|e| e.to_string())?;
    local(tomorrow.to_datetime(time))
}

/// A pending wakeup in a result: when, and why.
fn describe(wakeup: &Wakeup, now: &Zoned) -> String {
    format!(
        "for {} ({})",
        clock(&wakeup.at.to_zoned(now.time_zone().clone())),
        wakeup.reason
    )
}

/// How long until a wakeup, to the second: `1h 12m`, `5m 30s`, `45s`.
fn span(duration: SignedDuration) -> String {
    let total = duration.as_secs().max(0);
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    match (hours, minutes, seconds) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::{Alarm, AlarmRequest, WakeupRequest};
    use tokio::sync::mpsc;

    use super::*;

    fn now() -> Zoned {
        "2026-10-11T13:48:00+02:00[Europe/Amsterdam]"
            .parse()
            .unwrap()
    }

    fn at(text: &str) -> Timestamp {
        text.parse::<Zoned>().unwrap().timestamp()
    }

    #[test]
    fn a_delay_counts_from_now() {
        assert_eq!(
            target(Some(90), None, &now()),
            Ok(at("2026-10-11T13:49:30+02:00[Europe/Amsterdam]"))
        );
    }

    #[test]
    fn a_time_of_day_is_its_next_occurrence() {
        assert_eq!(
            target(None, Some("15:00"), &now()),
            Ok(at("2026-10-11T15:00:00+02:00[Europe/Amsterdam]")),
            "later today"
        );
        assert_eq!(
            target(None, Some("09:30:15"), &now()),
            Ok(at("2026-10-12T09:30:15+02:00[Europe/Amsterdam]")),
            "passed, so tomorrow"
        );
    }

    #[test]
    fn a_date_and_time_is_local_unless_it_has_an_offset() {
        assert_eq!(
            target(None, Some("2026-10-11T15:00"), &now()),
            Ok(at("2026-10-11T15:00:00+02:00[Europe/Amsterdam]"))
        );
        assert_eq!(
            target(None, Some("2026-10-11T15:00:00Z"), &now()),
            Ok(at("2026-10-11T17:00:00+02:00[Europe/Amsterdam]"))
        );
    }

    #[test]
    fn rejects_the_past_far_ahead_and_unclear_asks() {
        let err = |delay, at| target(delay, at, &now()).unwrap_err();
        assert!(err(None, Some("2026-10-11T12:00")).contains("has passed"));
        assert!(err(None, Some("2026-10-13T12:00")).contains("within 24 hours"));
        assert!(err(Some(0), None).contains("from 1 to 86400"));
        assert!(err(Some(86401), None).contains("from 1 to 86400"));
        assert!(err(Some(60), Some("15:00")).contains("not both"));
        assert!(err(None, None).contains("pass `delay_seconds` or `at`"));
        assert!(err(None, Some("teatime")).contains("cannot read"));
    }

    #[test]
    fn spans_read_to_the_second() {
        let span = |s| span(SignedDuration::from_secs(s));
        assert_eq!(span(45), "45s");
        assert_eq!(span(300), "5m");
        assert_eq!(span(330), "5m 30s");
        assert_eq!(span(7200), "2h");
        assert_eq!(span(4320), "1h 12m");
    }

    #[tokio::test]
    async fn tells_the_model_when_nothing_would_wake_it() {
        let out = ScheduleWakeup
            .call(
                json!({ "prompt": "look", "reason": "ci", "delay_seconds": 60 }),
                &ToolContext::new(".".into()),
            )
            .await;

        assert!(out.expect_err("headless").contains("no front-end"));
    }

    /// An alarm whose front-end keeps one wakeup, as the TUI does.
    fn alarm() -> Alarm {
        let (tx, mut rx) = mpsc::channel::<AlarmRequest>(1);
        tokio::spawn(async move {
            let mut pending = None;
            while let Some((request, reply)) = rx.recv().await {
                let old = match request {
                    WakeupRequest::Set(wakeup) => pending.replace(wakeup),
                    WakeupRequest::Cancel => pending.take(),
                };
                let _ = reply.send(old);
            }
        });
        Alarm::new(tx)
    }

    #[tokio::test]
    async fn a_new_wakeup_replaces_the_pending_one() {
        let ctx = ToolContext {
            alarm: alarm(),
            ..ToolContext::new(".".into())
        };
        let ask = |args| ScheduleWakeup.call(args, &ctx);

        let first = ask(json!({ "prompt": "look", "reason": "ci", "delay_seconds": 600 }))
            .await
            .unwrap();
        assert!(first.starts_with("Wakeup set for "), "{first}");
        assert!(first.contains("(in 10m; now "), "{first}");
        let second = ask(json!({ "prompt": "look", "reason": "deploy", "delay_seconds": 60 }))
            .await
            .unwrap();
        assert!(second.contains("It replaced the wakeup for "), "{second}");
        assert!(second.ends_with("(ci)."), "{second}");
        let cancelled = ask(json!({ "cancel": true })).await.unwrap();
        assert!(
            cancelled.starts_with("Cancelled the wakeup for "),
            "{cancelled}"
        );
        assert_eq!(
            ask(json!({ "cancel": true })).await,
            Ok("No wakeup was pending.".into())
        );
        assert!(
            ask(json!({ "delay_seconds": 60 }))
                .await
                .unwrap_err()
                .contains("`prompt` is required")
        );
    }
}
