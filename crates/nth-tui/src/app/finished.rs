//! What the auxiliary background jobs hand back, and what the app does with
//! each. A job that panicked is a bug worth seeing, not worth quitting
//! over: leaving `App::run` would drop a running turn unsaved.

use std::path::PathBuf;

use nth_protocol::{BoxError, Listing};
use nth_session::{Summary, ToolUse, store};
use tokio::task::JoinError;

use super::{App, input::Input, session_picker};
use crate::diagnostics::Lookup;

/// A finished job: everything the loop waits on but the turn, the editor
/// and the curio host, which have handlers of their own.
pub(super) enum Finished {
    Indexed(Result<Vec<String>, JoinError>),
    Git(crate::git::Loaded),
    LlmsListed(Result<Result<Listing, BoxError>, JoinError>),
    Connected(Result<Result<nth_auth::Account, nth_auth::Error>, JoinError>),
    SessionsListed(Result<Result<Vec<Summary>, store::Error>, JoinError>),
    SessionLoaded(Result<anyhow::Result<session_picker::Loaded>, JoinError>),
    HistorySaved(Result<std::io::Result<()>, JoinError>),
    SessionSaved(Result<Result<(), store::Error>, JoinError>),
    LlmUsageSaved(Result<std::io::Result<()>, JoinError>),
    PlanRead(Result<Option<String>, JoinError>),
    WorkRead(Result<(PathBuf, Option<nth_work::Plan>), JoinError>),
    Diagnosed(Result<Lookup, JoinError>),
    ToolsFound(Result<Result<Vec<(String, ToolUse)>, store::Error>, JoinError>),
}

impl App {
    /// Takes in what a job found. One that failed still starts the run
    /// asked for while it ran, so a queued save or read is not lost to it.
    pub(super) fn job_finished(&mut self, finished: Finished) {
        match finished {
            Finished::Indexed(Ok(files)) => self.indexed(files),
            Finished::Indexed(Err(e)) => {
                self.job_failed("listing files", e);
                if self.indexing.take_again() {
                    self.index_files();
                }
            }
            Finished::Git(loaded) => self.git_finished(loaded),
            Finished::LlmsListed(Ok(llms)) => self.llms_listed(llms),
            Finished::LlmsListed(Err(e)) => {
                let failed = self.job_failed("listing LLMs", e);
                // An open picker would wait on it for good.
                self.llms_listed(Err(failed.into()));
            }
            Finished::Connected(Ok(account)) => self.connected(account),
            Finished::Connected(Err(e)) => {
                self.job_failed("signing in", e);
                if matches!(self.input, Input::Connect(_)) {
                    self.close_connect();
                }
            }
            Finished::SessionsListed(Ok(sessions)) => self.sessions_listed(sessions),
            Finished::SessionsListed(Err(e)) => {
                let failed = self.job_failed("listing sessions", e);
                if let Input::SessionPicker(picker) = &mut self.input {
                    picker.load(Err(failed));
                }
            }
            Finished::SessionLoaded(Ok(session)) => self.session_loaded(session),
            Finished::SessionLoaded(Err(e)) => {
                self.job_failed("loading the session", e);
            }
            Finished::HistorySaved(Ok(saved)) => self.history_saved(saved),
            Finished::HistorySaved(Err(e)) => {
                self.job_failed("saving prompt history", e);
                if self.history_saving.take_again() {
                    self.save_history();
                }
            }
            Finished::SessionSaved(Ok(saved)) => self.session_saved(saved),
            Finished::SessionSaved(Err(e)) => {
                self.job_failed("saving the session", e);
                self.save_ended();
            }
            Finished::LlmUsageSaved(Ok(saved)) => self.llm_usage_saved(saved),
            Finished::LlmUsageSaved(Err(e)) => {
                self.job_failed("saving model usage", e);
                if self.llm_usage_saving.take_again() {
                    self.save_llm_usage();
                }
            }
            Finished::PlanRead(Ok(plan)) => self.plan_read(plan),
            Finished::PlanRead(Err(e)) => {
                self.job_failed("reading the plan", e);
                if self.plan.reading.take_again() {
                    self.plan.read();
                } else {
                    // A plan-mode turn's notification waits on this read.
                    self.notify_plan_read();
                }
            }
            Finished::WorkRead(Ok(plan)) => self.work_read(plan),
            Finished::WorkRead(Err(e)) => {
                self.job_failed("reading the work plan", e);
                if self.work.reading.take_again() {
                    self.work.read();
                }
            }
            Finished::Diagnosed(Ok(lookup)) => self.diagnostics.found(lookup),
            Finished::Diagnosed(Err(e)) => {
                self.job_failed("looking up servers, formatters and programs", e);
            }
            Finished::ToolsFound(Ok(tools)) => self.tools_found(tools),
            Finished::ToolsFound(Err(e)) => {
                self.job_failed("reading tool usage", e);
            }
        }
    }

    /// Says in the chat that a job panicked, leaving what it would have
    /// filled as it was. Returns the message, for a view waiting on the
    /// job to show too.
    pub(super) fn job_failed(&mut self, what: &str, error: JoinError) -> String {
        let failed = format!("{what} failed: {error}");
        self.chat.transcript.push_error(failed.clone());
        failed
    }
}
