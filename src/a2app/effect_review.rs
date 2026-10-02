//! Pause an immutable worker operation until its foreground user answers.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex, atomic::{AtomicUsize, Ordering}};
use a2app_core::information_flow::{self as flow, EffectReview};
use makepad_widgets::SignalToUI;
use tokio::sync::oneshot;

const MAX_PENDING: usize = 16;
static PENDING: LazyLock<Mutex<VecDeque<WorkerReview>>> = LazyLock::new(Default::default);
static OUTSTANDING: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
pub(super) static TEST_LOCK: Mutex<()> = Mutex::new(());

pub(super) struct WorkerReview {
    pub review: EffectReview,
    pub allow_once: bool,
    answer: Option<oneshot::Sender<Result<(), String>>>,
}

impl WorkerReview {
    pub fn is_cancelled(&self) -> bool {
        self.answer.as_ref().is_none_or(oneshot::Sender::is_closed)
    }

    pub fn approve(&self, approve: impl FnOnce(&EffectReview) -> Result<(), String>) -> Result<(), String> {
        if self.is_cancelled() { return Err("This permission request was cancelled.".into()); }
        approve(&self.review)
    }

    pub fn finish(mut self, result: Result<(), String>) {
        if result.is_err() { let _ = flow::cancel_effect(self.review.id); }
        if let Some(answer) = self.answer.take()
            && answer.send(result).is_err()
        { let _ = flow::cancel_effect(self.review.id); }
    }
}

impl Drop for WorkerReview {
    fn drop(&mut self) {
        if self.answer.is_some() { let _ = flow::cancel_effect(self.review.id); }
        OUTSTANDING.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) fn take_pending() -> Vec<WorkerReview> {
    PENDING.lock().unwrap().drain(..).filter(|worker| !worker.is_cancelled()).collect()
}

struct NotifyCancellation(bool);

impl Drop for NotifyCancellation {
    fn drop(&mut self) {
        if self.0 { SignalToUI::set_ui_signal(); }
    }
}

/// No network work is performed while waiting; callers revalidate permissions
/// and consume the exact approval immediately before their actual effect.
pub(super) async fn request(review: EffectReview, allow_once: bool) -> Result<(), String> {
    if review.allowed { return Ok(()); }
    if OUTSTANDING.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count|
        if count < MAX_PENDING { Some(count + 1) } else { None }).is_err()
    {
        let _ = flow::cancel_effect(review.id);
        return Err("Too many operations are waiting for permission.".into());
    }
    let (answer, receiver) = oneshot::channel();
    let worker = WorkerReview { review, allow_once, answer: Some(answer) };
    {
        let mut pending = PENDING.lock().unwrap();
        pending.push_back(worker);
    }
    SignalToUI::set_ui_signal();
    let mut cancellation = NotifyCancellation(true);
    let result = receiver.await.unwrap_or_else(|_| Err("This permission request was cancelled.".into()));
    cancellation.0 = false;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{future::Future, pin::Pin, task::{Context, Poll, Waker}};
    use flow::{ContextId, Influence, Recipient};

    fn review(index: u64) -> EffectReview {
        EffectReview { id: u64::MAX - index, context: ContextId::App { account: "queue-test".into(), app: "test".into(), room: None },
            epoch: 1, recipient: Some(Recipient::Clipboard), action: None, sources: Default::default(), denied_sources: Default::default(),
            influences: [Influence::Unknown].into(), payload: format!("{{\"text\":\"request {index}\"}}").into(), allowed: false }
    }

    fn poll<T>(future: Pin<&mut impl Future<Output = T>>) -> Poll<T> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    #[test]
    fn immutable_worker_waits_for_answer_and_then_resumes() {
        let _guard = TEST_LOCK.lock().unwrap();
        let capture = review(1);
        let mut future = Box::pin(request(capture.clone(), true));
        assert!(poll(future.as_mut()).is_pending());
        let mut waiting = take_pending();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].review, capture);
        assert!(waiting[0].allow_once);
        assert!(!waiting[0].is_cancelled());
        assert_eq!(OUTSTANDING.load(Ordering::Acquire), 1);
        waiting.pop().unwrap().finish(Ok(()));
        assert!(matches!(poll(future.as_mut()), Poll::Ready(Ok(()))));
        assert_eq!(OUTSTANDING.load(Ordering::Acquire), 0);
        let mut worker = Box::pin(request(review(4), true));
        assert!(poll(worker.as_mut()).is_pending());
        let mut showing = take_pending();
        drop(worker);
        assert!(showing[0].is_cancelled());
        showing.pop().unwrap().finish(Ok(()));
        assert_eq!(OUTSTANDING.load(Ordering::Acquire), 0);
    }

    #[test]
    fn cancelled_workers_and_dismissed_dialogs_release_capacity() {
        let _guard = TEST_LOCK.lock().unwrap();
        let mut cancelled = Box::pin(request(review(2), false));
        assert!(poll(cancelled.as_mut()).is_pending());
        drop(cancelled);
        assert!(take_pending().is_empty());
        assert_eq!(OUTSTANDING.load(Ordering::Acquire), 0);
        let mut dismissed = Box::pin(request(review(3), false));
        assert!(poll(dismissed.as_mut()).is_pending());
        let waiting = take_pending();
        drop(waiting);
        assert!(matches!(poll(dismissed.as_mut()), Poll::Ready(Err(error)) if error.contains("cancelled")));
        assert_eq!(OUTSTANDING.load(Ordering::Acquire), 0);
    }

    #[test]
    fn cancelled_ui_owned_worker_cannot_create_session_sharing_or_action_grants() {
        let _guard = TEST_LOCK.lock().unwrap();
        struct TestRoot(std::path::PathBuf);
        impl Drop for TestRoot {
            fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
        }
        let directory = TestRoot(std::env::temp_dir().join(format!("robrix-cancelled-effect-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())));
        let mut registry = flow::Registry::open(&directory.0).unwrap();
        let context = review(300).context;
        registry.register_context(&context).unwrap();
        registry.add_sources(&context, [flow::Source::Account { account: context.account().into() }]).unwrap();
        registry.add_influences(&context, [Influence::Unknown]).unwrap();
        let epoch = registry.context_epoch(&context).unwrap();
        let capture = registry.prepare_effect_for_activation(&context, epoch, Some(&Recipient::Clipboard),
            Some(&flow::SensitiveAction { kind: "device.clipboard.write".into(), target: "clipboard".into() }),
            &serde_json::json!({"text":"cancelled private fixture"})).unwrap();
        let mut future = Box::pin(request(review(300), true));
        assert!(poll(future.as_mut()).is_pending());
        let worker = take_pending().pop().unwrap();
        drop(future);
        let approved = worker.approve(|_| registry.approve_effect_session(&capture, flow::SharingDuration::RobrixSession));
        assert!(approved.is_err());
        assert!(registry.sharing_grants().unwrap().is_empty());
        assert!(registry.authorities().unwrap().is_empty());
        worker.finish(Err("Cancelled".into()));
        assert_eq!(OUTSTANDING.load(Ordering::Acquire), 0);
    }

    #[test]
    fn limit_counts_reviews_already_owned_by_the_ui_and_allowed_work_needs_no_slot() {
        let _guard = TEST_LOCK.lock().unwrap();
        let mut futures = (0..MAX_PENDING).map(|index| Box::pin(request(review(100 + index as u64), true))).collect::<Vec<_>>();
        for future in &mut futures { assert!(poll(future.as_mut()).is_pending()); }
        let waiting = take_pending();
        assert_eq!(waiting.len(), MAX_PENDING);
        assert!(PENDING.lock().unwrap().is_empty());
        let mut overflow = Box::pin(request(review(200), true));
        assert!(matches!(poll(overflow.as_mut()), Poll::Ready(Err(error)) if error.contains("Too many")));
        let mut approved = review(201); approved.allowed = true;
        let mut allowed = Box::pin(request(approved, true));
        assert!(matches!(poll(allowed.as_mut()), Poll::Ready(Ok(()))));
        for worker in waiting { worker.finish(Err("User declined".into())); }
        for future in &mut futures { assert!(matches!(poll(future.as_mut()), Poll::Ready(Err(error)) if error == "User declined")); }
        assert_eq!(OUTSTANDING.load(Ordering::Acquire), 0);
    }
}
