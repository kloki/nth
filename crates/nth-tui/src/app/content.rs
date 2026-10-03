//! The content panel above the input panel: what you look at. Only the chat
//! for now; later views such as Plan, Diff and Monitor join it as tabs.

/// Which view the content panel shows. Each view's state lives on the app,
/// so it keeps up with the session while another view is shown.
#[derive(Debug)]
pub(super) enum Content {
    Chat,
}
