use jiff::Timestamp;
use tokio::sync::{mpsc, oneshot};

/// A turn the model asked for later: the prompt it left itself, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wakeup {
    pub at: Timestamp,
    pub prompt: String,
    pub reason: String,
}

/// What a tool asks of the front-end's alarm: one wakeup pending at most,
/// so a new one replaces the old.
#[derive(Debug)]
pub enum WakeupRequest {
    Set(Wakeup),
    Cancel,
}

/// A request and where the front-end answers with the wakeup it dropped.
pub type AlarmRequest = (WakeupRequest, oneshot::Sender<Option<Wakeup>>);

/// Lets a tool ask the front-end for a turn later, as
/// [`Screen`](crate::Screen) lets it switch panels. The default alarm is
/// nowhere, as in a headless run: nothing would start the turn.
#[derive(Debug, Clone, Default)]
pub struct Alarm(Option<mpsc::Sender<AlarmRequest>>);

impl Alarm {
    pub fn new(front_end: mpsc::Sender<AlarmRequest>) -> Self {
        Self(Some(front_end))
    }

    /// Whether a front-end would wake the model, which is what the tool
    /// checks before promising it a turn.
    pub fn reaches_front_end(&self) -> bool {
        self.0.is_some()
    }

    /// Sets `wakeup`, returning the one it replaced.
    pub async fn set(&self, wakeup: Wakeup) -> Result<Option<Wakeup>, Gone> {
        self.ask(WakeupRequest::Set(wakeup)).await
    }

    /// Drops the pending wakeup, returning it.
    pub async fn cancel(&self) -> Result<Option<Wakeup>, Gone> {
        self.ask(WakeupRequest::Cancel).await
    }

    async fn ask(&self, request: WakeupRequest) -> Result<Option<Wakeup>, Gone> {
        let front_end = self.0.as_ref().ok_or(Gone)?;
        let (reply, answer) = oneshot::channel();
        front_end.send((request, reply)).await.map_err(|_| Gone)?;
        answer.await.map_err(|_| Gone)
    }
}

/// No front-end answered the alarm, so nothing would start the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gone;

impl std::fmt::Display for Gone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the front-end is gone; nothing would wake you")
    }
}

impl std::error::Error for Gone {}

/// A moment as the model reads it: the date as the system prompt writes
/// it, then the time and the zone.
pub fn clock(at: &jiff::Zoned) -> String {
    at.strftime("%a %b %d %Y %H:%M:%S %Z").to_string()
}
