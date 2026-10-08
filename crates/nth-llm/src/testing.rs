//! What the three protocols' stream tests share: a recorded fixture fed in
//! chunks of any size, cut at a marker, and checked for tool calls.

use nth_protocol::StreamEvent;

/// `input` as the chunks a connection might deliver it in.
pub(crate) fn chunks<E>(input: &[u8], chunk_size: usize) -> Vec<Result<bytes::Bytes, E>> {
    input
        .chunks(chunk_size)
        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
        .collect()
}

/// `fixture` up to where `marker` starts, as a connection cut there would
/// have delivered it.
pub(crate) fn until<'a>(fixture: &'a [u8], marker: &str) -> &'a [u8] {
    let text = std::str::from_utf8(fixture).expect("utf-8 fixture");
    let end = text.find(marker).expect("marker in fixture");
    &fixture[..end]
}

/// Whether no tool call came out: what a stream that ends badly must
/// guarantee, since its calls may be partial.
pub(crate) fn runs_no_tools<E>(events: &[Result<StreamEvent, E>]) -> bool {
    !events
        .iter()
        .any(|e| matches!(e, Ok(StreamEvent::ToolCall(_))))
}
