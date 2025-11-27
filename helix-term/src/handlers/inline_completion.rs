use std::sync::Arc;

use arc_swap::ArcSwap;
use helix_core::syntax::config::LanguageServerFeature;
use helix_core::text_annotations::InlineAnnotation;
use helix_event::{register_hook, send_blocking};
use helix_lsp::{lsp, util::lsp_range_to_range};
use helix_view::{
    document::{InlineCompletion, InlineCompletionItem, Mode},
    events::DocumentDidChange,
    handlers::Handlers,
};
use tokio::time::Instant;

use crate::{config::Config, job};

pub(super) struct InlineCompletionHandler {
    config: Arc<ArcSwap<Config>>,
}

impl InlineCompletionHandler {
    pub fn new(config: Arc<ArcSwap<Config>>) -> Self {
        Self { config }
    }
}

impl helix_event::AsyncHook for InlineCompletionHandler {
    type Event = ();

    fn handle_event(&mut self, _: Self::Event, _: Option<Instant>) -> Option<Instant> {
        Some(Instant::now() + self.config.load().editor.inline_completion_timeout)
    }

    fn finish_debounce(&mut self) {
        job::dispatch_blocking(move |editor, _| {
            // User may have left insert mode before debounce fired
            if editor.mode != Mode::Insert {
                return;
            }
            let (view, doc) = current!(editor);
            let cursor = doc
                .selection(view.id)
                .primary()
                .cursor(doc.text().slice(..));

            let Some(ls) = doc
                .language_servers_with_feature(LanguageServerFeature::InlineCompletion)
                .next()
            else {
                return;
            };

            let pos = doc.position(view.id, ls.offset_encoding());
            let doc_id = doc.id();
            let context = lsp::InlineCompletionContext {
                trigger_kind: lsp::InlineCompletionTriggerKind::Automatic,
                selected_completion_info: None,
            };
            let Some(fut) = ls.inline_completion(doc.identifier(), pos, context, None) else {
                return;
            };

            let offset_encoding = ls.offset_encoding();
            tokio::spawn(async move {
                let Ok(Some(resp)) = fut.await else { return };
                let items = match resp {
                    lsp::InlineCompletionResponse::Array(v) => v,
                    lsp::InlineCompletionResponse::List(l) => l.items,
                };
                if items.is_empty() {
                    return;
                }

                job::dispatch(move |editor, _| {
                    // User may have left insert mode while request was in flight
                    if editor.mode != Mode::Insert {
                        return;
                    }
                    let Some(doc) = editor.documents.get_mut(&doc_id) else {
                        return;
                    };
                    let text = doc.text();

                    let completion_items: Vec<InlineCompletionItem> = items
                        .into_iter()
                        .filter_map(|item| {
                            let replace_range = item
                                .range
                                .and_then(|r| lsp_range_to_range(text, r, offset_encoding));

                            let offset = replace_range.map_or(0, |r| {
                                let typed_len = cursor.saturating_sub(r.from());
                                let Some(typed_slice) = text.get_slice(r.from()..cursor) else {
                                    return 0;
                                };
                                let typed_text: String = typed_slice.into();
                                let prefix = item.insert_text.get(..typed_len).unwrap_or_default();
                                if typed_text == prefix {
                                    typed_len
                                } else {
                                    0
                                }
                            });

                            let display_text = item.insert_text.get(offset..)?;
                            if display_text.is_empty() {
                                return None;
                            }

                            Some(InlineCompletionItem {
                                annotation: InlineAnnotation::new(cursor, display_text),
                                insert_text: item.insert_text,
                                replace_range,
                            })
                        })
                        .collect();

                    doc.inline_completion = InlineCompletion::new(completion_items);
                })
                .await;
            });
        });
    }
}

pub(super) fn register_hooks(handlers: &Handlers) {
    let tx = handlers.inline_completions.clone();

    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        // Clear stale completion: it was computed for the previous document state
        event.doc.inline_completion = None;
        // Ignore changes caused by a preview being displayed
        if event.ghost_transaction {
            return Ok(());
        }

        send_blocking(&tx, ());
        Ok(())
    });
}
