//! Prompts typed while a turn runs, held until the turn ends cleanly (T14.2).

/// The prompts waiting to be sent, in order. A turn that ends in any way but `end_turn`, or a
/// denied permission, pauses the queue instead of dropping it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Queue {
    prompts: Vec<String>,
    paused: bool,
}

impl Queue {
    pub fn push(&mut self, prompt: String) {
        self.prompts.push(prompt);
    }

    /// Drops the prompt at `index`; an index past the end changes nothing. Emptying the queue
    /// also clears the pause, since there is nothing left to resume.
    pub fn remove(&mut self, index: usize) -> Option<String> {
        let removed = (index < self.prompts.len()).then(|| self.prompts.remove(index));
        if self.prompts.is_empty() {
            self.paused = false;
        }
        removed
    }

    pub fn prompts(&self) -> &[String] {
        &self.prompts
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Holds the queue until the owner resumes it. An empty queue has nothing to hold.
    pub fn pause(&mut self) {
        if !self.prompts.is_empty() {
            self.paused = true;
        }
    }

    /// Called when a turn ends with `reason`. Only `end_turn` sends the next prompt; any other
    /// reason pauses the queue.
    pub fn turn_ended(&mut self, reason: &str) -> Option<String> {
        if reason != "end_turn" {
            self.pause();
            return None;
        }
        if self.paused || self.prompts.is_empty() {
            return None;
        }
        Some(self.prompts.remove(0))
    }

    /// Clears the pause and returns the first prompt to send now. The caller sends it only while
    /// idle, so the rest waits for the next clean turn end.
    pub fn resume(&mut self) -> Option<String> {
        self.paused = false;
        (!self.prompts.is_empty()).then(|| self.prompts.remove(0))
    }
}

#[cfg(test)]
mod tests {
    use super::Queue;

    fn queued(prompts: &[&str]) -> Queue {
        let mut queue = Queue::default();
        for prompt in prompts {
            queue.push((*prompt).to_owned());
        }
        queue
    }

    #[test]
    fn push_keeps_prompts_in_arrival_order() {
        let queue = queued(&["first", "second"]);
        assert_eq!(queue.prompts(), ["first", "second"]);
    }

    #[test]
    fn remove_drops_only_the_chosen_prompt() {
        let mut queue = queued(&["a", "b", "c"]);
        assert_eq!(queue.remove(1).as_deref(), Some("b"));
        assert_eq!(queue.prompts(), ["a", "c"]);
    }

    #[test]
    fn remove_ignores_an_index_past_the_end() {
        let mut queue = queued(&["a"]);
        assert_eq!(queue.remove(5), None);
        assert_eq!(queue.prompts(), ["a"]);
    }

    #[test]
    fn end_turn_sends_the_first_queued_prompt() {
        let mut queue = queued(&["a", "b"]);
        assert_eq!(queue.turn_ended("end_turn").as_deref(), Some("a"));
        assert_eq!(queue.prompts(), ["b"]);
    }

    #[test]
    fn end_turn_with_nothing_queued_sends_nothing() {
        assert_eq!(Queue::default().turn_ended("end_turn"), None);
    }

    #[test]
    fn cancel_error_and_refusal_pause_without_dropping() {
        for reason in ["cancelled", "error: boom", "refusal"] {
            let mut queue = queued(&["a", "b"]);
            assert_eq!(queue.turn_ended(reason), None, "{reason}");
            assert!(queue.is_paused(), "{reason}");
            assert_eq!(queue.prompts(), ["a", "b"], "{reason}");
        }
    }

    #[test]
    fn a_failed_turn_with_nothing_queued_does_not_pause() {
        let mut queue = Queue::default();
        queue.turn_ended("cancelled");
        assert!(!queue.is_paused());
    }

    #[test]
    fn a_paused_queue_waits_for_resume_even_after_end_turn() {
        let mut queue = queued(&["a"]);
        queue.turn_ended("cancelled");
        assert_eq!(queue.turn_ended("end_turn"), None);
        assert_eq!(queue.prompts(), ["a"]);
    }

    #[test]
    fn denied_permission_pauses_the_queue() {
        let mut queue = queued(&["a"]);
        queue.pause();
        assert!(queue.is_paused());
    }

    #[test]
    fn resume_clears_the_pause_and_returns_the_first_prompt() {
        let mut queue = queued(&["a", "b"]);
        queue.turn_ended("cancelled");
        assert_eq!(queue.resume().as_deref(), Some("a"));
        assert!(!queue.is_paused());
        assert_eq!(queue.prompts(), ["b"]);
    }

    #[test]
    fn removing_the_last_prompt_clears_the_pause() {
        let mut queue = queued(&["a"]);
        queue.turn_ended("cancelled");
        queue.remove(0);
        assert!(!queue.is_paused());
    }
}
