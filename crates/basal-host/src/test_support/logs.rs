//! Process-wide tracing registration with independently scoped test sinks.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread::{self, ThreadId};

type Events = Mutex<Vec<(tracing::Level, String)>>;

#[derive(Default)]
struct Routes {
    threads: HashMap<ThreadId, Vec<Weak<Events>>>,
    all_threads: Vec<Weak<Events>>,
}

#[derive(Clone, Default)]
struct CaptureSubscriber(Arc<Mutex<Routes>>);

static SUBSCRIBER: OnceLock<CaptureSubscriber> = OnceLock::new();

fn subscriber() -> &'static CaptureSubscriber {
    SUBSCRIBER.get_or_init(|| {
        let subscriber = CaptureSubscriber::default();
        tracing::subscriber::set_global_default(subscriber.clone())
            .expect("test log subscriber is installed once");
        subscriber
    })
}

/// Captures events on the creating thread until dropped, without changing its
/// tracing dispatcher. Other test threads cannot write to this sink.
pub struct CapturedLogs(pub Arc<Events>, Option<ThreadId>);

impl CapturedLogs {
    pub fn new() -> Self {
        Self::register(Some(thread::current().id()))
    }

    /// Also observes background threads, for tests checking that secrets never
    /// appear in logs regardless of which runtime thread emits them.
    pub fn all_threads() -> Self {
        Self::register(None)
    }

    fn register(thread: Option<ThreadId>) -> Self {
        let events = Arc::new(Events::default());
        let mut routes = subscriber().0.lock().unwrap();
        let sinks = match thread {
            Some(id) => routes.threads.entry(id).or_default(),
            None => &mut routes.all_threads,
        };
        sinks.push(Arc::downgrade(&events));
        Self(events, thread)
    }
}

impl Default for CapturedLogs {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CapturedLogs {
    fn drop(&mut self) {
        let mut routes = subscriber().0.lock().unwrap();
        let sinks = match self.1 {
            Some(id) => routes.threads.get_mut(&id).unwrap(),
            None => &mut routes.all_threads,
        };
        sinks.retain(|sink| !sink.ptr_eq(&Arc::downgrade(&self.0)));
        if let Some(id) = self.1
            && sinks.is_empty()
        {
            routes.threads.remove(&id);
        }
    }
}

impl tracing::Subscriber for CaptureSubscriber {
    fn register_callsite(
        &self,
        _: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        // Interest belongs to the process, not to the currently active sinks.
        // A callsite first hit by an uncaptured thread must stay enabled for
        // later tests, even while scoped dispatchers are registered or dropped.
        tracing::subscriber::Interest::always()
    }
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
        Some(tracing::metadata::LevelFilter::TRACE)
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let sinks: Vec<_> = {
            let routes = self.0.lock().unwrap();
            routes
                .threads
                .get(&thread::current().id())
                .into_iter()
                .flatten()
                .chain(&routes.all_threads)
                .filter_map(Weak::upgrade)
                .collect()
        };
        if sinks.is_empty() {
            return;
        }
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={value:?};", field.name()));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.record_debug(field, &value);
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        for sink in sinks {
            sink.lock()
                .unwrap()
                .push((*event.metadata().level(), fields.0.clone()));
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_are_thread_local_and_removed_on_drop() {
        let logs = CapturedLogs::new();
        tracing::warn!("main capture");
        thread::spawn(|| {
            let other = CapturedLogs::new();
            tracing::warn!("other capture");
            assert_eq!(other.0.lock().unwrap().len(), 1);
            assert!(other.0.lock().unwrap()[0].1.contains("other capture"));
        })
        .join()
        .unwrap();
        let events = logs.0.clone();
        drop(logs);
        tracing::warn!("after capture");
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].1.contains("main capture"));
    }

    #[test]
    fn all_thread_capture_includes_background_events() {
        let logs = CapturedLogs::all_threads();
        thread::spawn(|| tracing::debug!("background capture marker"))
            .join()
            .unwrap();
        assert!(logs.0.lock().unwrap().iter().any(|(level, fields)| {
            *level == tracing::Level::DEBUG && fields.contains("background capture marker")
        }));
    }
}
