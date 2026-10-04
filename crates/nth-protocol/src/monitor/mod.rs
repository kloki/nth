//! Background monitors: commands the model leaves running whose output
//! comes back to it as notices, between steps or as a turn of their own.
//! The types here are what a monitor says; `registry` keeps the running
//! ones and what the model has not heard yet, and `notice` is the text
//! that carries it, written for the model and read back by the transcript.

mod notice;
mod registry;

use std::{fmt, path::PathBuf};

pub use notice::{NOTICE_LINES, NoticeSummary, split_notices};
pub use registry::{Monitors, log_dir};
use tokio_util::sync::CancellationToken;

/// Numbered from 1 for as long as the front-end runs, so a monitor keeps its
/// number across sessions.
pub type MonitorId = u32;

/// What a monitor reports to the front-end, for its tab.
#[derive(Debug, Clone, PartialEq)]
pub enum MonitorEvent {
    Started {
        id: MonitorId,
        description: String,
        command: String,
        log: PathBuf,
    },
    /// One line of output. Only stdout lines are events for the model.
    Output {
        id: MonitorId,
        line: String,
        stream: Stream,
    },
    Ended {
        id: MonitorId,
        end: MonitorEnd,
        /// How many stdout lines it produced.
        events: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorEnd {
    /// The command exited by itself; `None` when a signal killed it.
    Exited(Option<i32>),
    TimedOut {
        after_ms: u64,
    },
    /// It printed more than the model could take in.
    Flooded,
    Stopped(StoppedBy),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoppedBy {
    Model,
    User,
    /// nth quit, or the session it ran for was left.
    Exit,
}

impl MonitorEnd {
    /// Whether it ended the way a finished command should.
    pub fn is_success(self) -> bool {
        self == MonitorEnd::Exited(Some(0))
    }
}

impl fmt::Display for MonitorEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MonitorEnd::Exited(Some(code)) => write!(f, "exited with code {code}"),
            MonitorEnd::Exited(None) => write!(f, "killed by a signal"),
            MonitorEnd::TimedOut { after_ms } => {
                write!(
                    f,
                    "timed out after {}s, start it again to keep watching",
                    after_ms / 1000
                )
            }
            MonitorEnd::Flooded => {
                write!(
                    f,
                    "stopped: too many events, start it again with a tighter filter"
                )
            }
            MonitorEnd::Stopped(StoppedBy::Model) => write!(f, "stopped with monitor_stop"),
            MonitorEnd::Stopped(StoppedBy::User) => write!(f, "stopped by the user"),
            MonitorEnd::Stopped(StoppedBy::Exit) => write!(f, "stopped: nth exited"),
        }
    }
}

/// What the monitor tool gets for a new monitor.
#[derive(Debug)]
pub struct Registered {
    pub id: MonitorId,
    /// Cancelled when someone stops it; the monitor then asks
    /// [`Monitors::stopped_by`] who.
    pub stop: CancellationToken,
    /// Where it writes every line it reads, stdout and stderr.
    pub log: PathBuf,
}
