//! Runs the compaction stages against the agent's history (SPEC §7).

use super::recovery::{self, ThinkSplitter};
use super::{Agent, AgentEvent, Turn};
use crate::context::compaction::{
    CompactionStage, CompactionState, READ_ONLY_TOOLS, SUMMARY_PROMPT, apply_summary,
    mask_oldest_results, stub_oldest_results, summary_boundary,
};
use crate::provider::{ChatChunk, ChatRequest, FinishReason, Message, ToolCall};
use futures::{StreamExt, pin_mut};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

/// Share of `C` that kept turns may fill, leaving the rest for prefix, summary and new work.
const SUMMARY_KEEP_PERCENT: u64 = 30;

impl Agent {
    fn prompt_tokens(&self) -> u32 {
        self.budget.estimate(self.chat_request(None).chars())
    }

    /// Whichever stages are needed, cheapest first. Forced when the server has shown that the
    /// estimate is wrong, so the target then comes down from the estimate as well.
    pub(super) async fn compact_if_needed(&mut self, turn: &Turn<'_>) {
        let forced = std::mem::take(&mut self.compact_next);
        let before = self.prompt_tokens();
        if !forced && before <= self.budget.compact_at() {
            return;
        }
        let target = match forced {
            true => self.budget.mask_target().min(before / 5 * 3),
            false => self.budget.mask_target(),
        };
        if self.config.masking {
            self.mask(before, target, turn).await;
        }
        let enough = if forced {
            target
        } else {
            self.budget.settle_at()
        };
        if self.prompt_tokens() <= enough {
            return;
        }
        if self.summarise(turn).await || turn.cancel.is_cancelled() {
            return;
        }
        self.stub(turn).await;
    }

    async fn mask(&mut self, before: u32, target: u32, turn: &Turn<'_>) {
        let stage = CompactionStage::Masking;
        turn.compaction(CompactionState::Started(stage)).await;
        let excess = self.budget.chars_for(before.saturating_sub(target));
        let tools = std::sync::Arc::clone(&self.tools);
        let unmasked = self.history.clone();
        let spooled = std::cell::RefCell::new(Vec::new());
        mask_oldest_results(
            &mut self.history,
            self.config.mask_keep_turns,
            excess,
            |text| {
                let path = tools.spool(text)?;
                spooled.borrow_mut().push(path.clone());
                Ok(path)
            },
        );
        self.remember_masked(&unmasked, spooled.into_inner());
        self.finish(stage, before, turn).await;
    }

    /// Records how the model can get each newly masked result back, mirroring the placeholder.
    fn remember_masked(&mut self, unmasked: &[Message], spooled: Vec<PathBuf>) {
        let calls: HashMap<&str, &ToolCall> = unmasked
            .iter()
            .filter_map(|m| match m {
                Message::Assistant { tool_calls, .. } => Some(tool_calls),
                _ => None,
            })
            .flatten()
            .map(|call| (call.id.as_str(), call))
            .collect();
        for (old, new) in unmasked.iter().zip(&self.history) {
            let (
                Message::Tool {
                    tool_call_id,
                    content: before,
                },
                Message::Tool { content: after, .. },
            ) = (old, new)
            else {
                continue;
            };
            let Some(call) = calls.get(tool_call_id.as_str()) else {
                continue;
            };
            if before == after || !READ_ONLY_TOOLS.contains(&call.name.as_str()) {
                continue;
            }
            if let Ok(input) = serde_json::from_str(&call.arguments) {
                self.refetchable.push((call.name.clone(), input));
            }
        }
        for path in spooled {
            let input = serde_json::json!({ "file_path": path.to_string_lossy() });
            self.refetchable.push(("Read".to_owned(), input));
        }
    }

    async fn finish(&self, stage: CompactionStage, before_tokens: u32, turn: &Turn<'_>) {
        let after_tokens = self.prompt_tokens();
        // shortcut: the boundary only reaches events and logs; add it to the session log with T3.
        tracing::info!(?stage, before_tokens, after_tokens, "history compacted");
        turn.compaction(CompactionState::Finished {
            stage,
            before_tokens,
            after_tokens,
        })
        .await;
    }

    /// Returns whether a summary replaced older history and the prompt now fits.
    async fn summarise(&mut self, turn: &Turn<'_>) -> bool {
        let stage = CompactionStage::Summary;
        turn.compaction(CompactionState::Started(stage)).await;
        let before = self.prompt_tokens();
        let keep_chars = self
            .budget
            .chars_for(self.budget.share(SUMMARY_KEEP_PERCENT));
        let boundary = summary_boundary(&self.history, self.config.mask_keep_turns, keep_chars);
        let summary = match boundary {
            Some(boundary) => self.request_summary(boundary, turn.cancel.clone()).await,
            None => Err("nothing to summarise".to_owned()),
        };
        if let (Some(boundary), Ok(summary)) = (boundary, summary.as_ref()) {
            let mut candidate = self.history.clone();
            apply_summary(&mut candidate, boundary, summary);
            let previous = std::mem::replace(&mut self.history, candidate);
            if self.prompt_tokens() <= self.budget.settle_at() {
                self.summarised_messages += previous.len() - self.history.len();
                self.finish(stage, before, turn).await;
                return true;
            }
            self.history = previous;
        }
        tracing::warn!(?summary, "summary not used");
        turn.compaction(CompactionState::Failed(stage)).await;
        false
    }

    /// Asks over the existing prefix and tools, so the server's cache still helps this request.
    async fn request_summary(
        &self,
        boundary: usize,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let mut request: ChatRequest = self.chat_request(None);
        request.messages.truncate(boundary);
        // Two user messages in a row break strict chat templates.
        match request.messages.last_mut() {
            Some(Message::User { content }) => {
                content.push_str("\n\n");
                content.push_str(SUMMARY_PROMPT);
            }
            _ => request.messages.push(Message::User {
                content: SUMMARY_PROMPT.to_owned(),
            }),
        }
        let stream = self
            .provider
            .chat(request, cancel)
            .await
            .map_err(|e| e.to_string())?;
        pin_mut!(stream);
        let profile = &self.config.profile;
        let mut splitter = ThinkSplitter::default();
        let mut summary = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk.map_err(|e| e.to_string())? {
                ChatChunk::Text(delta) if profile.strip_thinking => {
                    summary.push_str(&splitter.push(&delta).text);
                }
                ChatChunk::Text(delta) => summary.push_str(&delta),
                ChatChunk::Finish(FinishReason::Length) => {
                    return Err("the summary ran out of space".to_owned());
                }
                _ => {}
            }
        }
        summary.push_str(&splitter.finish().text);
        // Same cleaning as any reply, so reasoning or a stray text call can't enter history.
        let summary =
            recovery::recover_calls(&summary, Vec::new(), profile.tool_format, "summary").text;
        match summary.trim() {
            "" => Err("the model wrote no summary".to_owned()),
            text => Ok(text.to_owned()),
        }
    }

    async fn stub(&mut self, turn: &Turn<'_>) {
        let stage = CompactionStage::Stubs;
        turn.compaction(CompactionState::Started(stage)).await;
        let before = self.prompt_tokens();
        let excess = before.saturating_sub(self.budget.mask_target());
        let excess = self.budget.chars_for(excess);
        stub_oldest_results(&mut self.history, excess);
        self.finish(stage, before, turn).await;
        turn.warn(
            "Old tool output was removed without a summary, so the model may have lost context."
                .to_owned(),
        )
        .await;
    }
}

impl Turn<'_> {
    async fn compaction(&self, state: CompactionState) {
        self.emit(AgentEvent::Compaction(state)).await;
    }
}
