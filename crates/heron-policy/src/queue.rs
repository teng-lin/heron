//! Pure-logic speech queue per spec §9.
//!
//! The trait surface in [`crate::SpeechController`] hides whatever
//! the realtime backend exposes — atomic-replace if available,
//! cancel-then-speak otherwise. This module is the *reference model*
//! for the queue semantics: a deterministic state machine that takes
//! [`crate::Priority`]-tagged enqueue calls and produces the
//! [`EnqueueOutcome`] (cancellations + start hint) the controller
//! should drive against TTS.
//!
//! Why have a pure model:
//! - The cancel-then-speak race (spec §9 / Invariant 11) is subtle.
//!   A pure data structure with property tests catches state-bug
//!   regressions that an end-to-end TTS test would miss in the noise.
//! - A backend that can't honor a primitive (`Replace` on a TTS
//!   without atomic_replace) emulates by issuing the same cancellation
//!   set this model produces; downstream logic stays identical.
//! - Tests don't need a tokio runtime or audio plumbing.
//!
//! ## State machine
//!
//! ```text
//! Idle ──enqueue(*)──► Speaking { current, queue: [] }
//!
//! Speaking { current, queue }
//!   ├─enqueue(Append)──► Speaking { current, queue+[new] }
//!   ├─enqueue(Replace)─► Speaking { current=new, queue=[] }
//!   │      and cancels: [current] + queue (atomic, no audible gap)
//!   └─enqueue(Interrupt)─► Speaking { current=new, queue }
//!          and cancels: [current] only
//!
//! finish_current() in Speaking pops queue head into current; goes
//! Idle when both are empty.
//!
//! cancel(id):
//!   - if id == current: same as finish_current (queue head promotes)
//!   - if id in queue: remove that entry; current keeps speaking
//!   - else: no-op (idempotent per the SpeechController trait contract)
//! ```

use std::collections::VecDeque;

use uuid::Uuid;

use crate::{Priority, UtteranceId};

/// One queued or speaking utterance. Held inside the queue; the
/// caller's TTS pipeline keeps any heavier per-utterance state
/// (the actual rendered audio bytes, the response ID from the
/// realtime backend) keyed by `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedUtterance {
    pub id: UtteranceId,
    pub text: String,
}

/// What the controller should DO after [`SpeechQueue::enqueue`]
/// updates the model. The model has already consumed the enqueue;
/// the controller is responsible for firing the cancellations and
/// (if `start_immediately`) kicking off TTS for `new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnqueueOutcome {
    /// Stable ID minted for the enqueued text. Returned to the API
    /// caller as `Result<UtteranceId, _>` from `speak`.
    pub new: UtteranceId,
    /// Utterances the model cancelled as a result of this enqueue.
    /// In priority order (current first). The controller fires
    /// `SpeechEvent::Cancelled { reason: Replaced { by: new } }`
    /// for each.
    pub cancellations: Vec<UtteranceId>,
    /// `true` when `new` should start playing immediately. `false`
    /// means it's queued behind a still-speaking utterance and
    /// should start when [`SpeechQueue::finish_current`] promotes it.
    pub start_immediately: bool,
}

/// Pure-logic speech queue. Drives the model the controller commits
/// against the TTS backend.
#[derive(Debug, Default, Clone)]
pub struct SpeechQueue {
    /// `None` ⇔ Idle; `Some` ⇔ Speaking. Mutually exclusive with
    /// `queue.is_empty() && current.is_none()`.
    current: Option<QueuedUtterance>,
    queue: VecDeque<QueuedUtterance>,
}

impl SpeechQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_idle(&self) -> bool {
        self.current.is_none() && self.queue.is_empty()
    }

    pub fn current(&self) -> Option<&QueuedUtterance> {
        self.current.as_ref()
    }

    pub fn queued(&self) -> impl Iterator<Item = &QueuedUtterance> {
        self.queue.iter()
    }

    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Submit a new utterance under `priority`. Returns the
    /// [`EnqueueOutcome`] the controller commits against the TTS
    /// backend.
    pub fn enqueue(&mut self, text: String, priority: Priority) -> EnqueueOutcome {
        let new_id = UtteranceId(Uuid::now_v7());
        let new_utt = QueuedUtterance { id: new_id, text };

        match priority {
            Priority::Append => self.append(new_utt),
            Priority::Replace => self.replace(new_utt),
            Priority::Interrupt => self.interrupt(new_utt),
        }
    }

    fn append(&mut self, new_utt: QueuedUtterance) -> EnqueueOutcome {
        let new_id = new_utt.id;
        if self.current.is_none() {
            self.current = Some(new_utt);
            EnqueueOutcome {
                new: new_id,
                cancellations: Vec::new(),
                start_immediately: true,
            }
        } else {
            self.queue.push_back(new_utt);
            EnqueueOutcome {
                new: new_id,
                cancellations: Vec::new(),
                start_immediately: false,
            }
        }
    }

    fn replace(&mut self, new_utt: QueuedUtterance) -> EnqueueOutcome {
        let new_id = new_utt.id;
        // Atomic replace per spec §9: cancel current, clear the
        // queue, install new as current — all within one model call.
        // The controller fires a single batch of cancellations so
        // there's no audible gap between the old utterance dying
        // and the new one starting.
        let mut cancellations = Vec::with_capacity(1 + self.queue.len());
        if let Some(old) = self.current.take() {
            cancellations.push(old.id);
        }
        for queued in self.queue.drain(..) {
            cancellations.push(queued.id);
        }
        self.current = Some(new_utt);
        EnqueueOutcome {
            new: new_id,
            cancellations,
            start_immediately: true,
        }
    }

    fn interrupt(&mut self, new_utt: QueuedUtterance) -> EnqueueOutcome {
        let new_id = new_utt.id;
        // Interrupt cancels current only; queue stays. Used for
        // mid-utterance corrections that should still let queued
        // follow-ups play.
        let cancellations = if let Some(old) = self.current.take() {
            vec![old.id]
        } else {
            Vec::new()
        };
        self.current = Some(new_utt);
        EnqueueOutcome {
            new: new_id,
            cancellations,
            start_immediately: true,
        }
    }

    /// The TTS backend reported the current utterance finished
    /// playing. Promote the queue head (if any). Returns the
    /// promoted utterance so the controller can start its TTS.
    pub fn finish_current(&mut self) -> Option<QueuedUtterance> {
        self.current = None;
        if let Some(next) = self.queue.pop_front() {
            self.current = Some(next.clone());
            Some(next)
        } else {
            None
        }
    }

    /// Cancel a specific utterance. Idempotent: returns the
    /// [`CancelOutcome`] describing what happened. Unknown IDs
    /// return `NotFound` — same shape as the trait's
    /// "Idempotent: Ok(()) if utterance already done / cancelled /
    /// unknown" contract.
    pub fn cancel(&mut self, id: UtteranceId) -> CancelOutcome {
        if let Some(current) = &self.current
            && current.id == id
        {
            // Cancelling the current utterance: promote queue head
            // (if any) into the current slot. The controller fires
            // SpeechEvent::Cancelled { id, reason: UserRequested }
            // and then starts the promoted utterance.
            self.current = None;
            let promoted = self.queue.pop_front();
            if let Some(next) = &promoted {
                self.current = Some(next.clone());
            }
            return CancelOutcome::CancelledCurrent { promoted };
        }
        if let Some(pos) = self.queue.iter().position(|u| u.id == id) {
            self.queue.remove(pos);
            return CancelOutcome::CancelledQueued;
        }
        CancelOutcome::NotFound
    }

    /// Spec §9: drop the queue but let current finish.
    pub fn cancel_all_queued(&mut self) -> Vec<UtteranceId> {
        self.queue.drain(..).map(|u| u.id).collect()
    }

    /// Spec §9: panic-stop — cancel current AND clear queue.
    pub fn cancel_current_and_clear(&mut self) -> Vec<UtteranceId> {
        let mut cancelled = Vec::with_capacity(1 + self.queue.len());
        if let Some(old) = self.current.take() {
            cancelled.push(old.id);
        }
        for q in self.queue.drain(..) {
            cancelled.push(q.id);
        }
        cancelled
    }
}

/// Result of [`SpeechQueue::cancel`]. The controller branches on
/// this to decide which `SpeechEvent::Cancelled` to fire and whether
/// to start TTS for a promoted utterance.
#[derive(Debug, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The current utterance was cancelled. `promoted` is the queue
    /// head that took its place, or `None` if the queue was empty
    /// (the queue is now idle).
    CancelledCurrent { promoted: Option<QueuedUtterance> },
    /// A queued (non-current) utterance was removed.
    CancelledQueued,
    /// No utterance with this ID exists. Idempotent caller path.
    NotFound,
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn enqueue(q: &mut SpeechQueue, text: &str, p: Priority) -> EnqueueOutcome {
        q.enqueue(text.to_owned(), p)
    }

    #[test]
    fn empty_queue_starts_first_enqueue_immediately() {
        let mut q = SpeechQueue::new();
        let out = enqueue(&mut q, "hello", Priority::Append);
        assert!(out.start_immediately);
        assert!(out.cancellations.is_empty());
        assert!(!q.is_idle());
        assert_eq!(q.current().expect("current").text, "hello");
    }

    #[test]
    fn append_to_busy_queue_does_not_start_immediately() {
        let mut q = SpeechQueue::new();
        enqueue(&mut q, "first", Priority::Append);
        let out = enqueue(&mut q, "second", Priority::Append);
        assert!(!out.start_immediately, "must wait for first to finish");
        assert!(out.cancellations.is_empty());
        assert_eq!(q.queue_len(), 1);
    }

    #[test]
    fn replace_cancels_current_and_clears_queue() {
        let mut q = SpeechQueue::new();
        let first = enqueue(&mut q, "first", Priority::Append).new;
        let second = enqueue(&mut q, "second", Priority::Append).new;
        let third = enqueue(&mut q, "third", Priority::Append).new;
        // State: current=first, queue=[second, third]

        let out = enqueue(&mut q, "boss", Priority::Replace);
        assert!(out.start_immediately);
        assert_eq!(
            out.cancellations,
            vec![first, second, third],
            "cancellations must be in priority order: current first, then queue"
        );
        assert_eq!(q.current().expect("current").text, "boss");
        assert_eq!(q.queue_len(), 0);
    }

    #[test]
    fn replace_on_idle_queue_acts_like_append() {
        let mut q = SpeechQueue::new();
        let out = enqueue(&mut q, "first", Priority::Replace);
        assert!(out.start_immediately);
        assert!(out.cancellations.is_empty());
        assert_eq!(q.current().expect("current").text, "first");
    }

    #[test]
    fn interrupt_cancels_current_only_keeps_queue() {
        let mut q = SpeechQueue::new();
        let first = enqueue(&mut q, "first", Priority::Append).new;
        let second = enqueue(&mut q, "second", Priority::Append).new;
        let third = enqueue(&mut q, "third", Priority::Append).new;

        let out = enqueue(&mut q, "correction", Priority::Interrupt);
        assert!(out.start_immediately);
        assert_eq!(out.cancellations, vec![first]);
        // second + third stay queued behind correction
        assert_eq!(q.current().expect("current").text, "correction");
        assert_eq!(q.queue_len(), 2);
        let next_ids: Vec<_> = q.queued().map(|u| u.id).collect();
        assert_eq!(next_ids, vec![second, third]);
    }

    #[test]
    fn interrupt_on_idle_queue_acts_like_append() {
        let mut q = SpeechQueue::new();
        let out = enqueue(&mut q, "hi", Priority::Interrupt);
        assert!(out.start_immediately);
        assert!(out.cancellations.is_empty());
    }

    #[test]
    fn finish_current_promotes_queue_head() {
        let mut q = SpeechQueue::new();
        enqueue(&mut q, "first", Priority::Append);
        enqueue(&mut q, "second", Priority::Append);

        let promoted = q.finish_current().expect("queue head promotes");
        assert_eq!(promoted.text, "second");
        assert_eq!(q.current().expect("now speaking").text, "second");
        assert_eq!(q.queue_len(), 0);
    }

    #[test]
    fn finish_current_with_empty_queue_lands_idle() {
        let mut q = SpeechQueue::new();
        enqueue(&mut q, "first", Priority::Append);
        let promoted = q.finish_current();
        assert!(promoted.is_none());
        assert!(q.is_idle());
    }

    #[test]
    fn cancel_unknown_id_is_idempotent_not_found() {
        let mut q = SpeechQueue::new();
        let outcome = q.cancel(UtteranceId(Uuid::now_v7()));
        assert_eq!(outcome, CancelOutcome::NotFound);
        assert!(q.is_idle());
    }

    #[test]
    fn cancel_current_promotes_queue_head() {
        let mut q = SpeechQueue::new();
        let first = enqueue(&mut q, "first", Priority::Append).new;
        enqueue(&mut q, "second", Priority::Append);

        match q.cancel(first) {
            CancelOutcome::CancelledCurrent { promoted } => {
                let promoted = promoted.expect("queue head took the slot");
                assert_eq!(promoted.text, "second");
            }
            other => panic!("expected CancelledCurrent, got {other:?}"),
        }
        assert_eq!(q.current().expect("now speaking").text, "second");
    }

    #[test]
    fn cancel_current_lands_idle_when_queue_empty() {
        let mut q = SpeechQueue::new();
        let only = enqueue(&mut q, "first", Priority::Append).new;
        match q.cancel(only) {
            CancelOutcome::CancelledCurrent { promoted } => assert!(promoted.is_none()),
            other => panic!("expected CancelledCurrent, got {other:?}"),
        }
        assert!(q.is_idle());
    }

    #[test]
    fn cancel_queued_removes_only_that_entry() {
        let mut q = SpeechQueue::new();
        enqueue(&mut q, "first", Priority::Append);
        let second = enqueue(&mut q, "second", Priority::Append).new;
        enqueue(&mut q, "third", Priority::Append);

        let outcome = q.cancel(second);
        assert_eq!(outcome, CancelOutcome::CancelledQueued);
        // current=first still speaking; queue is just [third].
        assert_eq!(q.current().expect("current").text, "first");
        assert_eq!(q.queue_len(), 1);
        assert_eq!(q.queued().next().expect("third").text, "third");
    }

    #[test]
    fn cancel_all_queued_drops_queue_keeps_current() {
        let mut q = SpeechQueue::new();
        enqueue(&mut q, "first", Priority::Append);
        let second = enqueue(&mut q, "second", Priority::Append).new;
        let third = enqueue(&mut q, "third", Priority::Append).new;

        let cancelled = q.cancel_all_queued();
        assert_eq!(cancelled, vec![second, third]);
        assert_eq!(q.current().expect("first survives").text, "first");
        assert_eq!(q.queue_len(), 0);
    }

    #[test]
    fn cancel_current_and_clear_lands_idle() {
        let mut q = SpeechQueue::new();
        let first = enqueue(&mut q, "first", Priority::Append).new;
        let second = enqueue(&mut q, "second", Priority::Append).new;
        let third = enqueue(&mut q, "third", Priority::Append).new;

        let cancelled = q.cancel_current_and_clear();
        assert_eq!(cancelled, vec![first, second, third]);
        assert!(q.is_idle());
    }

    #[test]
    fn cancel_current_and_clear_on_idle_queue_returns_empty() {
        let mut q = SpeechQueue::new();
        assert!(q.cancel_current_and_clear().is_empty());
    }

    #[test]
    fn enqueue_returns_unique_ids() {
        // UUIDv7 is monotonic by minting time; pin that we don't
        // reuse IDs across enqueues even when text repeats.
        let mut q = SpeechQueue::new();
        let a = enqueue(&mut q, "x", Priority::Append).new;
        let b = enqueue(&mut q, "x", Priority::Append).new;
        let c = enqueue(&mut q, "x", Priority::Append).new;
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn replace_emits_cancellations_in_priority_order() {
        // Pin the order: current first, then queue in FIFO order.
        // Controller relies on this to fire SpeechEvent::Cancelled
        // for the current utterance before the queued ones, so the
        // user sees a single transition rather than a flicker.
        let mut q = SpeechQueue::new();
        let first = enqueue(&mut q, "first", Priority::Append).new;
        let second = enqueue(&mut q, "second", Priority::Append).new;
        let third = enqueue(&mut q, "third", Priority::Append).new;
        let fourth = enqueue(&mut q, "fourth", Priority::Append).new;

        let out = enqueue(&mut q, "boss", Priority::Replace);
        assert_eq!(out.cancellations, vec![first, second, third, fourth]);
    }

    #[test]
    fn full_round_trip_three_priorities() {
        // Property-style sanity: append → replace → interrupt → fin.
        let mut q = SpeechQueue::new();
        let first = enqueue(&mut q, "first", Priority::Append).new;
        assert_eq!(q.current().unwrap().id, first);

        let second = enqueue(&mut q, "second", Priority::Append).new;
        assert_eq!(q.queue_len(), 1);

        // Replace cancels both, installs `boss`.
        let boss_out = enqueue(&mut q, "boss", Priority::Replace);
        assert_eq!(boss_out.cancellations, vec![first, second]);
        let boss = boss_out.new;

        // Interrupt cancels boss, installs `correction`.
        let correction_out = enqueue(&mut q, "correction", Priority::Interrupt);
        assert_eq!(correction_out.cancellations, vec![boss]);
        let correction = correction_out.new;

        // Now finish_current with empty queue → idle.
        assert!(q.finish_current().is_none());
        assert!(q.is_idle());

        // Cancel a non-existent ID is still NotFound after Idle.
        let out = q.cancel(UtteranceId(Uuid::now_v7()));
        assert_eq!(out, CancelOutcome::NotFound);
        // For coverage: assert correction was actually installed.
        assert_ne!(boss, correction);
    }

    #[test]
    fn clone_yields_independent_state() {
        let mut a = SpeechQueue::new();
        enqueue(&mut a, "first", Priority::Append);
        enqueue(&mut a, "second", Priority::Append);
        let b = a.clone();

        a.finish_current();
        // a now has current=second, queue=[]
        // b should still have current=first, queue=[second]
        assert_eq!(b.current().unwrap().text, "first");
        assert_eq!(b.queue_len(), 1);
        // While a has advanced.
        assert_eq!(a.current().unwrap().text, "second");
        assert_eq!(a.queue_len(), 0);
    }
}
