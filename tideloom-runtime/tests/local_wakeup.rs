use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Barrier;
use std::time::Duration;

use futures::FutureExt;
use futures::StreamExt;
use tideloom_runtime::BusError;
use tideloom_runtime::Hint;
use tideloom_runtime::InstanceId;
use tideloom_runtime::LocalWakeup;
use tideloom_runtime::Offset;
use tideloom_runtime::Signal;
use tideloom_runtime::SignalStream;
use tideloom_runtime::Wakeup;

fn bus(capacity: usize) -> LocalWakeup {
    LocalWakeup::new(NonZeroUsize::new(capacity).unwrap())
}

async fn next(stream: &mut SignalStream) -> Signal {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the watcher should not miss a notification")
        .expect("the stream should remain open")
        .expect("the offset should still be retained")
}

#[tokio::test]
async fn a_watch_replays_only_later_signals_then_continues_live() {
    let bus = bus(4);
    let instance = InstanceId::new();
    let first = bus.notify(instance.clone(), Hint::Timer).await.unwrap();
    let second = bus.notify(instance.clone(), Hint::Other).await.unwrap();
    assert_eq!(first, Offset(1));
    assert_eq!(second, Offset(2));

    let mut stream = bus.watch(first).await.unwrap();
    assert_eq!(
        next(&mut stream).await,
        Signal {
            instance: instance.clone(),
            hint: Hint::Other,
            offset: second
        }
    );
    assert!(stream.next().now_or_never().is_none());
    let third = bus
        .notify(instance.clone(), Hint::Event("landed".into()))
        .await
        .unwrap();
    assert_eq!(
        next(&mut stream).await,
        Signal {
            instance,
            hint: Hint::Event("landed".into()),
            offset: third
        }
    );
    assert!(stream.next().now_or_never().is_none());
}

#[tokio::test]
async fn every_waiting_worker_receives_the_same_live_signals() {
    let bus = bus(2);
    let mut first = bus.watch(Offset(0)).await.unwrap();
    let mut second = bus.watch(Offset(0)).await.unwrap();
    for expected in 1..=8 {
        let (ready, mut waiting) = tokio::sync::mpsc::channel(2);
        let [first_worker, second_worker] = [first, second].map(|mut stream| {
            let ready = ready.clone();
            tokio::spawn(async move {
                assert!(stream.next().now_or_never().is_none());
                ready.try_send(()).unwrap();
                let signal = next(&mut stream).await;
                (stream, signal)
            })
        });
        // Each worker registers its own waker before announcing readiness.
        waiting.recv().await.unwrap();
        waiting.recv().await.unwrap();
        let instance = InstanceId::new();
        let offset = bus.notify(instance.clone(), Hint::Other).await.unwrap();
        let signal = Signal {
            instance,
            hint: Hint::Other,
            offset: Offset(expected),
        };
        assert_eq!(offset, signal.offset);
        let (first_stream, first_signal) = first_worker.await.unwrap();
        let (second_stream, second_signal) = second_worker.await.unwrap();
        assert_eq!(first_signal, signal);
        assert_eq!(second_signal, signal);
        first = first_stream;
        second = second_stream;
    }
}

#[tokio::test]
async fn eviction_expires_only_cursors_that_would_miss_a_signal() {
    for capacity in [1, 2] {
        let bus = bus(capacity);
        let instance = InstanceId::new();
        for _ in 0..=capacity {
            bus.notify(instance.clone(), Hint::Timer).await.unwrap();
        }
        assert!(matches!(
            bus.watch(Offset(0)).await,
            Err(BusError::OffsetExpired(Offset(0)))
        ));
        // Offset 1 was evicted, but a consumer that already consumed it
        // can still replay every subsequent signal.
        let mut stream = bus.watch(Offset(1)).await.unwrap();
        for expected in 2..=capacity as u64 + 1 {
            assert_eq!(next(&mut stream).await.offset, Offset(expected));
        }
        let mut tip = bus.watch(Offset(capacity as u64 + 1)).await.unwrap();
        assert!(tip.next().now_or_never().is_none());
    }
}

#[tokio::test]
async fn a_lagging_live_watcher_reports_expiration_then_ends() {
    let bus = bus(1);
    let mut stream = bus.watch(Offset(0)).await.unwrap();
    assert!(stream.next().now_or_never().is_none());
    for _ in 0..2 {
        bus.notify(InstanceId::new(), Hint::Other).await.unwrap();
    }
    assert!(matches!(
        stream.next().await,
        Some(Err(BusError::OffsetExpired(Offset(0))))
    ));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn committed_offsets_advance_monotonically_without_consuming_the_log() {
    let bus = bus(4);
    assert_eq!(bus.committed().await.unwrap(), Offset(0));
    for _ in 0..4 {
        bus.notify(InstanceId::new(), Hint::Other).await.unwrap();
    }
    // Exercise the backend contract directly; production acknowledgements
    // go through Engine::commit after the store apply succeeds.
    for offset in [3, 3, 1, 0] {
        bus.commit_offset(Offset(offset)).await.unwrap();
        assert_eq!(bus.committed().await.unwrap(), Offset(3));
    }
    let mut resumed = bus.watch(bus.committed().await.unwrap()).await.unwrap();
    assert_eq!(next(&mut resumed).await.offset, Offset(4));
    let mut independent = bus.watch(Offset(0)).await.unwrap();
    assert_eq!(next(&mut independent).await.offset, Offset(1));
    bus.commit_offset(Offset(4)).await.unwrap();
    assert_eq!(bus.committed().await.unwrap(), Offset(4));
}

#[tokio::test]
async fn concurrent_publishers_get_unique_offsets_in_log_order() {
    let bus = Arc::new(bus(128));
    let barrier = Arc::new(Barrier::new(8));
    let mut published = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|publisher| {
                let bus = Arc::clone(&bus);
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .build()
                        .unwrap();
                    barrier.wait();
                    runtime.block_on(async {
                        let mut signals: Vec<Signal> = Vec::new();
                        for sequence in 0..16 {
                            let instance = InstanceId::new();
                            let hint = Hint::Event(format!("{publisher}:{sequence}"));
                            let offset = bus.notify(instance.clone(), hint.clone()).await.unwrap();
                            if let Some(previous) = signals.last() {
                                assert!(previous.offset < offset);
                            }
                            signals.push(Signal {
                                instance,
                                hint,
                                offset,
                            });
                        }
                        signals
                    })
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    published.sort_by_key(|signal| signal.offset);
    let mut stream = bus.watch(Offset(0)).await.unwrap();
    for (index, signal) in published.into_iter().enumerate() {
        assert_eq!(signal.offset, Offset(index as u64 + 1));
        assert_eq!(next(&mut stream).await, signal);
    }
    assert!(stream.next().now_or_never().is_none());
}
