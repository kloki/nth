use std::time::Duration;

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec, Wakeup, parse_args};
use serde::Deserialize;
use serde_json::{Value, json};

pub struct ScheduleWakeup;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    duration_ms: u64,
}

impl Tool for ScheduleWakeup {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "schedule_wakeup",
            description: "Schedule a one-shot wakeup for the main session after duration_ms milliseconds (at most one hour), as a failsafe while waiting for background work. This call returns immediately and replaces any pending wakeup. It lasts through the rest of this turn; if a later turn starts before it expires (the user's prompt or another notice waking you), the wakeup is cancelled, so schedule another if still needed. The timer exists only while this session is open. Not available headless or to subagents.".into(),
            parameters: json!({"type":"object", "additionalProperties":false, "properties":{"duration_ms":{"type":"integer", "minimum":1, "maximum":Wakeup::MAX.as_millis()}}, "required":["duration_ms"]}),
        }
    }

    fn call<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> BoxFuture<'a, ToolResult> {
        async move {
            if ctx.is_subagent {
                return Err("wakeups are available only to the main session".into());
            }
            if !ctx.wakeup.enabled() {
                return Err(
                    "there is no front-end to wake this session; wait for the work instead".into(),
                );
            }
            let args: Args = parse_args(args)?;
            ctx.wakeup
                .schedule(Duration::from_millis(args.duration_ms))?;
            Ok(format!("Wakeup scheduled in {} ms.", args.duration_ms))
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn schedules_replaces_and_rejects_invalid_calls() {
        let mut ctx = ToolContext::new(".".into());
        assert!(
            ScheduleWakeup
                .call(json!({"duration_ms": 10}), &ctx)
                .await
                .is_err()
        );
        ctx.wakeup = Wakeup::new();
        let mut changes = ctx.wakeup.subscribe().unwrap();
        ScheduleWakeup
            .call(json!({"duration_ms": 10000}), &ctx)
            .await
            .unwrap();
        let first = *changes.borrow_and_update();
        ScheduleWakeup
            .call(json!({"duration_ms": 20000}), &ctx)
            .await
            .unwrap();
        let second = *changes.borrow_and_update();
        assert!(second > first);
        for args in [
            json!({"duration_ms": 0}),
            json!({"duration_ms": Wakeup::MAX.as_millis() + 1}),
            json!({"duration_ms": -1}),
            json!({}),
            json!({"duration_ms": 1, "extra": true}),
        ] {
            assert!(ScheduleWakeup.call(args, &ctx).await.is_err());
        }
        ctx.is_subagent = true;
        assert!(
            ScheduleWakeup
                .call(json!({"duration_ms": 1}), &ctx)
                .await
                .is_err()
        );
        assert_eq!(*changes.borrow(), second);
    }
}
