//! FakeTelegram — in-memory [`TelegramApi`] test double for the Telegram
//! transport tests and any future control-plane tests. NOT compiled into
//! release paths: the module is gated behind
//! `cfg(any(test, feature = "test-util"))`.
//!
//! Outbound calls are recorded (never transmitted) and readable through
//! the getters; inbound updates are queued with [`FakeTelegram::push_update`]
//! and drained by `get_updates`, which can also be scripted to fail first
//! (`fail_next_get_updates`) so tests can drive the poll loop's backoff.

use std::sync::Mutex;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use crate::telegram::send::{MsgRef, SendRequest};
use crate::telegram::{TelegramApi, Update};

/// Chat id the fake "delivers" into when a send carries no reply anchor.
pub const FAKE_CHAT_ID: i64 = 100_000;

#[derive(Default)]
struct Inner {
    sent: Vec<SendRequest>,
    edits: Vec<(MsgRef, SendRequest)>,
    deletes: Vec<MsgRef>,
    answers: Vec<String>,
    queue: Vec<Update>,
    offsets_seen: Vec<i64>,
    pending_failures: usize,
    pending_send_failures: usize,
    pending_delete_failures: usize,
    next_message_id: i32,
    next_update_id: i64,
}

/// Recorded-outbound fake of the Telegram Bot API.
#[derive(Default)]
pub struct FakeTelegram {
    inner: Mutex<Inner>,
}

impl FakeTelegram {
    /// Empty fake: no recorded traffic, no queued updates, no failures.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue an update for the next successful `get_updates`. Updates
    /// without an explicit `update_id` get sequential ids assigned
    /// automatically.
    pub fn push_update(&self, mut update: Update) {
        let mut inner = self.inner.lock().expect("fake lock must be poison-free");
        if update.update_id == 0 {
            inner.next_update_id += 1;
            update.update_id = inner.next_update_id;
        } else {
            inner.next_update_id = inner.next_update_id.max(update.update_id);
        }
        inner.queue.push(update);
    }

    /// Make the next `n` `get_updates` calls fail (simulated outage) so
    /// tests can exercise the poll loop's exponential backoff and prove it
    /// survives.
    pub fn fail_next_get_updates(&self, n: usize) {
        self.inner
            .lock()
            .expect("fake lock must be poison-free")
            .pending_failures = n;
    }

    /// Make the next `n` `send_message` calls fail (simulated delivery
    /// outage) so tests can exercise undeliverable-ask handling; a failed
    /// send records nothing and consumes no message id.
    pub fn fail_next_sends(&self, n: usize) {
        self.inner
            .lock()
            .expect("fake lock must be poison-free")
            .pending_send_failures = n;
    }

    /// Make the next `n` `delete_message` calls fail (e.g. the setup
    /// walkthrough's paste-intake failure-to-delete notice).
    pub fn fail_next_deletes(&self, n: usize) {
        self.inner
            .lock()
            .expect("fake lock must be poison-free")
            .pending_delete_failures = n;
    }

    /// Every outbound send, in call order.
    pub fn sent(&self) -> Vec<SendRequest> {
        self.inner.lock().unwrap().sent.clone()
    }

    /// Every outbound edit as `(target, request)`, in call order.
    pub fn edits(&self) -> Vec<(MsgRef, SendRequest)> {
        self.inner.lock().unwrap().edits.clone()
    }

    /// Every outbound delete, in call order.
    pub fn deletes(&self) -> Vec<MsgRef> {
        self.inner.lock().unwrap().deletes.clone()
    }

    /// Every answered callback-query id, in call order.
    pub fn answered(&self) -> Vec<String> {
        self.inner.lock().unwrap().answers.clone()
    }

    /// The `offset` argument of every `get_updates` call, in call order —
    /// the observable that proves the poll loop advances across deliveries.
    pub fn offsets_seen(&self) -> Vec<i64> {
        self.inner.lock().unwrap().offsets_seen.clone()
    }
}

#[async_trait]
impl TelegramApi for FakeTelegram {
    async fn get_updates(&self, offset: i64) -> Result<Vec<Update>> {
        let mut inner = self.inner.lock().expect("fake lock must be poison-free");
        inner.offsets_seen.push(offset);
        if inner.pending_failures > 0 {
            inner.pending_failures -= 1;
            return Err(anyhow!("fake: simulated telegram outage"));
        }
        Ok(std::mem::take(&mut inner.queue))
    }

    async fn send_message(&self, req: SendRequest) -> Result<MsgRef> {
        let mut inner = self.inner.lock().expect("fake lock must be poison-free");
        if inner.pending_send_failures > 0 {
            inner.pending_send_failures -= 1;
            return Err(anyhow!("fake: simulated telegram outage"));
        }
        inner.sent.push(req.clone());
        inner.next_message_id += 1;
        Ok(MsgRef {
            chat_id: req
                .reply_to
                .map(|m| m.chat_id)
                .unwrap_or(FAKE_CHAT_ID),
            message_id: inner.next_message_id,
        })
    }

    async fn edit_message(&self, to: MsgRef, req: SendRequest) -> Result<MsgRef> {
        let mut inner = self.inner.lock().expect("fake lock must be poison-free");
        inner.edits.push((to, req));
        Ok(to)
    }

    async fn answer_callback(&self, callback_id: &str) -> Result<()> {
        self.inner
            .lock()
            .expect("fake lock must be poison-free")
            .answers
            .push(callback_id.to_string());
        Ok(())
    }

    async fn delete_message(&self, to: MsgRef) -> Result<()> {
        let mut inner = self.inner.lock().expect("fake lock must be poison-free");
        if inner.pending_delete_failures > 0 {
            inner.pending_delete_failures -= 1;
            return Err(anyhow!("fake: simulated telegram outage"));
        }
        inner.deletes.push(to);
        Ok(())
    }
}
