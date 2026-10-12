use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream;
use tokio::sync::Mutex;
use tokio::sync::Notify;

use crate::BusError;
use crate::Hint;
use crate::InstanceId;
use crate::Offset;
use crate::Signal;
use crate::SignalStream;
use crate::Wakeup;

/// A bounded, replayable in-process signal log shared by all workers.
///
/// Offsets start at one; [`Offset::default()`] watches from the beginning.
/// Signals and the committed offset live only as long as this bus. Watchers
/// retain the bus and receive every signal independently, not as a queue.
/// A watcher that falls behind the retained log yields
/// [`BusError::OffsetExpired`] once and then ends.
pub struct LocalWakeup {
    inner: Arc<Inner>,
}

struct Inner {
    capacity: NonZeroUsize,
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Default)]
struct State {
    signals: VecDeque<Signal>,
    latest: Offset,
    committed: Offset,
}

impl State {
    fn next(&self, from: Offset) -> Result<Option<Signal>, BusError> {
        let retained_from = self
            .signals
            .front()
            .map_or(self.latest, |signal| Offset(signal.offset.0 - 1));
        if from < retained_from {
            return Err(BusError::OffsetExpired(from));
        }
        if from >= self.latest {
            return Ok(None);
        }
        // The difference is less than signals.len(), so it fits usize.
        Ok(self
            .signals
            .get((from.0 - retained_from.0) as usize)
            .cloned())
    }
}

impl LocalWakeup {
    /// Create an empty log retaining at most `capacity` signals.
    #[must_use]
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            inner: Arc::new(Inner {
                capacity,
                state: Mutex::new(State::default()),
                changed: Notify::new(),
            }),
        }
    }
}

#[async_trait]
impl Wakeup for LocalWakeup {
    async fn notify(&self, instance: InstanceId, hint: Hint) -> Result<Offset, BusError> {
        let mut state = self.inner.state.lock().await;
        let offset = Offset(
            state
                .latest
                .0
                .checked_add(1)
                .ok_or_else(|| BusError::Backend("signal offset exhausted".into()))?,
        );
        if state.signals.len() == self.inner.capacity.get() {
            state.signals.pop_front();
        }
        state.signals.push_back(Signal {
            instance,
            hint,
            offset,
        });
        state.latest = offset;
        drop(state);
        self.inner.changed.notify_waiters();
        Ok(offset)
    }

    async fn watch(&self, from: Offset) -> Result<SignalStream, BusError> {
        self.inner.state.lock().await.next(from)?;
        let inner = Arc::clone(&self.inner);
        Ok(Box::pin(stream::unfold(
            Some((inner, from)),
            |cursor| async move {
                let (inner, mut from) = cursor?;
                loop {
                    // Register before checking the log so a concurrent append
                    // cannot land between an empty check and going to sleep.
                    let changed = inner.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    let next = inner.state.lock().await.next(from);
                    match next {
                        Ok(Some(signal)) => {
                            from = signal.offset;
                            return Some((Ok(signal), Some((Arc::clone(&inner), from))));
                        }
                        Err(error) => return Some((Err(error), None)),
                        Ok(None) => changed.await,
                    }
                }
            },
        )))
    }

    async fn commit_offset(&self, offset: Offset) -> Result<(), BusError> {
        let mut state = self.inner.state.lock().await;
        state.committed = state.committed.max(offset);
        Ok(())
    }

    async fn committed(&self) -> Result<Offset, BusError> {
        Ok(self.inner.state.lock().await.committed)
    }
}
