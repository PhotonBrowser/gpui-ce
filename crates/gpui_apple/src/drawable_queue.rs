use gpui::{BackgroundExecutor, Task};
use std::{
    sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError},
    time::Duration,
};

/// One acquisition in flight, retaining at most one prepared drawable.
pub(crate) struct DrawableQueue<T> {
    generation: u64,
    pending: Option<PendingDrawable<T>>,
}

struct PendingDrawable<T> {
    generation: u64,
    receiver: Receiver<Option<T>>,
    _task: Task<()>,
}

impl<T> Default for DrawableQueue<T> {
    fn default() -> Self {
        Self {
            generation: 0,
            pending: None,
        }
    }
}

impl<T: Send + 'static> DrawableQueue<T> {
    pub(crate) fn request(
        &mut self,
        executor: &BackgroundExecutor,
        acquire: impl FnOnce() -> Option<T> + Send + 'static,
    ) {
        if self.pending.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let task = executor.spawn(async move {
            // The channel has a single producer and an empty slot. Sending never
            // waits for the UI thread; closing the queue drops the result here.
            let _ = sender.send(acquire());
        });
        self.pending = Some(PendingDrawable {
            generation: self.generation,
            receiver,
            _task: task,
        });
    }

    pub(crate) fn take(&mut self) -> Option<T> {
        self.take_within(Duration::ZERO)
    }

    /// Takes the drawable, waiting up to `timeout` for an acquisition still
    /// in flight. A late drawable then costs part of a frame, not a whole one.
    pub(crate) fn take_within(&mut self, timeout: Duration) -> Option<T> {
        let pending = self.pending.as_ref()?;
        let result = if timeout.is_zero() {
            pending.receiver.try_recv().map_err(|error| match error {
                TryRecvError::Empty => RecvTimeoutError::Timeout,
                TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
            })
        } else {
            pending.receiver.recv_timeout(timeout)
        };
        let drawable = match result {
            Ok(drawable) => drawable,
            Err(RecvTimeoutError::Timeout) => return None,
            Err(RecvTimeoutError::Disconnected) => None,
        };
        let pending = self.pending.take().expect("acquisition is pending");
        (pending.generation == self.generation)
            .then_some(drawable)
            .flatten()
    }

    pub(crate) fn invalidate(&mut self) {
        // Let an outstanding acquisition finish before starting another. Rapid
        // resizing must not create a worker or retained drawable per resize.
        self.generation = self.generation.wrapping_add(1);
    }

    pub(crate) fn clear(&mut self) {
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[gpui::test]
    fn taking_is_nonblocking_and_acquisitions_are_coalesced(cx: &mut gpui::TestAppContext) {
        let acquisitions = Arc::new(AtomicUsize::new(0));
        let mut queue = DrawableQueue::default();
        for _ in 0..20 {
            let acquisitions = acquisitions.clone();
            queue.request(&cx.background_executor, move || {
                acquisitions.fetch_add(1, Ordering::Relaxed);
                Some(7)
            });
        }
        assert_eq!(acquisitions.load(Ordering::Relaxed), 0);
        assert_eq!(queue.take(), None);
        cx.run_until_parked();
        assert_eq!(acquisitions.load(Ordering::Relaxed), 1);
        assert_eq!(queue.take(), Some(7));
        assert_eq!(queue.take(), None);
    }

    #[gpui::test]
    fn resize_rejects_stale_drawables_and_allows_a_retry(cx: &mut gpui::TestAppContext) {
        let mut queue = DrawableQueue::default();
        queue.request(&cx.background_executor, || Some(1));
        for _ in 0..20 {
            queue.invalidate();
            queue.request(&cx.background_executor, || {
                panic!("acquisition already pending")
            });
        }
        cx.run_until_parked();
        assert_eq!(queue.take(), None);
        queue.request(&cx.background_executor, || Some(2));
        cx.run_until_parked();
        assert_eq!(queue.take(), Some(2));
    }

    #[gpui::test]
    fn failed_acquisitions_allow_a_retry(cx: &mut gpui::TestAppContext) {
        let mut queue = DrawableQueue::default();
        queue.request(&cx.background_executor, || None::<u8>);
        cx.run_until_parked();
        assert_eq!(queue.take(), None);
        queue.request(&cx.background_executor, || Some(3));
        cx.run_until_parked();
        assert_eq!(queue.take(), Some(3));
        queue.request(&cx.background_executor, || Some(4));
        cx.run_until_parked();
        queue.clear();
        assert_eq!(queue.take(), None);
    }

    #[gpui::test]
    fn closing_releases_the_prepared_resource(cx: &mut gpui::TestAppContext) {
        let resource = Arc::new(());
        let mut queue = DrawableQueue::default();
        for _ in 0..20 {
            let resource = resource.clone();
            queue.request(&cx.background_executor, move || Some(resource));
        }
        cx.run_until_parked();
        assert_eq!(
            Arc::strong_count(&resource),
            2,
            "only one resource is retained"
        );
        queue.clear();
        assert_eq!(
            Arc::strong_count(&resource),
            1,
            "closing releases the resource"
        );
    }
}
