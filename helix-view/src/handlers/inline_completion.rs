use helix_lsp::lsp;

#[derive(Debug)]
pub enum InlineCompletionEvent {
    /// Trigger inline completion (automatic or manual)
    Trigger(lsp::InlineCompletionTriggerKind),
    /// Cancel any pending inline completion request
    Cancel,
}
