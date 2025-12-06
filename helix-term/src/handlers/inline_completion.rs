use std::sync::Arc;

use arc_swap::ArcSwap;
use helix_core::{syntax::config::LanguageServerFeature, text_annotations::Overlay, Range};
use helix_event::{cancelable_future, register_hook, send_blocking, TaskController};
use helix_lsp::{lsp, util::lsp_range_to_range};
use helix_view::{
    document::{InlineCompletion, Mode},
    events::{DocumentDidChange, SelectionDidChange},
    handlers::{inline_completion::InlineCompletionEvent, Handlers},
};
use tokio::time::Instant;

use crate::events::OnModeSwitch;
use crate::{config::Config, job};

pub(super) struct InlineCompletionHandler {
    config: Arc<ArcSwap<Config>>,
    task_controller: TaskController,
}

impl InlineCompletionHandler {
    pub fn new(config: Arc<ArcSwap<Config>>) -> Self {
        Self {
            config,
            task_controller: TaskController::new(),
        }
    }
}

impl helix_event::AsyncHook for InlineCompletionHandler {
    type Event = InlineCompletionEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        _old_timeout: Option<Instant>,
    ) -> Option<Instant> {
        match event {
            InlineCompletionEvent::Trigger(trigger_kind) => match trigger_kind {
                lsp::InlineCompletionTriggerKind::Invoked => {
                    self.finish_debounce();
                    None
                }
                _ => Some(Instant::now() + self.config.load().editor.inline_completion_timeout),
            },
            InlineCompletionEvent::Cancel => {
                self.task_controller.cancel();
                None
            }
        }
    }

    fn finish_debounce(&mut self) {
        let handle = self.task_controller.restart();
        job::dispatch_blocking(move |editor, _| {
            request_inline_completion(editor, handle);
        });
    }
}

/// Request inline completion from LSP servers.
fn request_inline_completion(editor: &mut helix_view::Editor, handle: helix_event::TaskHandle) {
    let (view, doc) = current!(editor);
    let tab_width = doc.tab_width();

    for ls in doc.language_servers_with_feature(LanguageServerFeature::InlineCompletion) {
        let pos = doc.position(view.id, ls.offset_encoding());
        let context = lsp::InlineCompletionContext {
            trigger_kind: lsp::InlineCompletionTriggerKind::Automatic,
            selected_completion_info: None,
        };
        let Some(fut) = ls.inline_completion(doc.identifier(), pos, context, None) else {
            continue;
        };

        let offset_encoding = ls.offset_encoding();
        let handle = handle.clone();
        let request = async move {
            let Ok(Some(resp)) = fut.await else { return };
            let items = match resp {
                lsp::InlineCompletionResponse::Array(v) => v,
                lsp::InlineCompletionResponse::List(l) => l.items,
            };
            if items.is_empty() {
                return;
            }

            job::dispatch(move |editor, _| {
                let (view, doc) = current!(editor);
                let text = doc.text();
                let cursor = doc.selection(view.id).primary().cursor(text.slice(..));

                let completions: Vec<_> = items
                    .into_iter()
                    .filter_map(|item| {
                        let replace_range = item
                            .range
                            .and_then(|r| lsp_range_to_range(text, r, offset_encoding))
                            .unwrap_or_else(|| Range::point(cursor));

                        if !replace_range.contains_range(&Range::point(cursor)) {
                            return None;
                        }

                        let skip = cursor.saturating_sub(replace_range.from());
                        let ghost_text: String = item.insert_text.chars().skip(skip).collect();

                        if ghost_text.is_empty() {
                            return None;
                        }

                        let at_eol = text.get_char(cursor).is_none_or(|c| c == '\n');

                        let tab_spaces: String = " ".repeat(tab_width);
                        let mut lines: Vec<String> = ghost_text
                            .split('\n')
                            .map(|line| line.replace('\t', &tab_spaces))
                            .collect();

                        let first_line = lines.remove(0);

                        let line_end = text.line_to_char(text.char_to_line(cursor) + 1);
                        let rest_of_line: String = text
                            .slice(cursor..line_end)
                            .chars()
                            .take_while(|c| *c != '\n')
                            .collect();

                        let (overlays, overflow_text, eol_ghost_text, additional_lines) = if at_eol
                        {
                            let eol_text = if !first_line.is_empty() {
                                Some(first_line)
                            } else {
                                None
                            };
                            (Vec::new(), None, eol_text, lines)
                        } else {
                            let is_multiline = !lines.is_empty();
                            let after_cursor: String = rest_of_line.chars().skip(1).collect();

                            let mut display_first_line = first_line.clone();
                            if !is_multiline {
                                for suffix_len in (1..=after_cursor.len()).rev() {
                                    if let Some(suffix) = after_cursor.get(..suffix_len) {
                                        if display_first_line.ends_with(suffix) {
                                            display_first_line
                                                .truncate(display_first_line.len() - suffix.len());
                                            break;
                                        }
                                    }
                                }
                            }

                            let preview = if is_multiline {
                                display_first_line
                            } else {
                                format!("{}{}", display_first_line, after_cursor)
                            };

                            let mut overlays = Vec::new();
                            for (i, preview_char) in preview.chars().enumerate() {
                                if i >= rest_of_line.chars().count() {
                                    break;
                                }
                                overlays.push(Overlay::new(cursor + i, preview_char.to_string()));
                            }

                            let overflow: String =
                                preview.chars().skip(rest_of_line.chars().count()).collect();
                            let overflow_text =
                                if !overflow.is_empty() { Some(overflow) } else { None };

                            let additional_lines = if is_multiline {
                                let mut result = lines;
                                let ghost_contains_rest = first_line.contains(&rest_of_line)
                                    || result.iter().any(|l| l.contains(&rest_of_line));
                                if !ghost_contains_rest {
                                    if let Some(last) = result.last_mut() {
                                        last.push_str(&rest_of_line);
                                    }
                                }
                                result
                            } else {
                                lines
                            };

                            (overlays, overflow_text, None, additional_lines)
                        };

                        Some(InlineCompletion {
                            ghost_text,
                            replace_range,
                            cursor_char_idx: cursor,
                            overlays,
                            overflow_text,
                            eol_ghost_text,
                            additional_lines,
                        })
                    })
                    .collect();

                for completion in completions {
                    doc.inline_completions.push(completion);
                }

                doc.inline_completions
                    .rebuild_overlays(&mut doc.inline_completion_overlays);
            })
            .await;
        };
        tokio::spawn(cancelable_future(request, handle));
    }
}

pub(super) fn register_hooks(handlers: &Handlers) {
    let tx = handlers.inline_completions.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        event.doc.inline_completions.take_and_clear();
        event.doc.inline_completion_overlays.clear();
        if event.ghost_transaction {
            return Ok(());
        }
        send_blocking(&tx, InlineCompletionEvent::Cancel);
        if event.doc.config.load().inline_completion_auto_trigger {
            send_blocking(&tx, InlineCompletionEvent::Trigger(lsp::InlineCompletionTriggerKind::Automatic));
        }
        Ok(())
    });

    let tx = handlers.inline_completions.clone();
    register_hook!(move |event: &mut SelectionDidChange<'_>| {
        event.doc.inline_completions.take_and_clear();
        event.doc.inline_completion_overlays.clear();
        send_blocking(&tx, InlineCompletionEvent::Cancel);
        Ok(())
    });

    let tx = handlers.inline_completions.clone();
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        if event.old_mode == Mode::Insert && event.new_mode != Mode::Insert {
            let (_, doc) = current!(event.cx.editor);
            doc.inline_completions.take_and_clear();
            doc.inline_completion_overlays.clear();
            send_blocking(&tx, InlineCompletionEvent::Cancel);
        }
        Ok(())
    });
}
