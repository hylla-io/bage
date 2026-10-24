//! Opt-in metadata for client requests; payloads never enter the trace.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// One protocol hop of a client request.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestTraceHop {
    /// The client is sending the request.
    Send,
    /// The server answered the request.
    Reply,
    /// The request's response bound expired.
    Timeout,
    /// The server published its health and quiescence state.
    Status,
    /// The client stopped awaiting the request without a response or timeout.
    Discarded,
}

/// Correlation metadata without request or response payloads.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestTraceEvent {
    /// Caller-declared session label.
    pub session: String,
    /// The client's protocol request id, absent for status notifications.
    pub request_id: Option<u64>,
    /// Protocol method.
    pub method: String,
    /// Document URI or hierarchy item URI, when present.
    pub path: Option<String>,
    /// Zero-based line, when present.
    pub line: Option<u64>,
    /// Zero-based UTF-16 character, when present.
    pub character: Option<u64>,
    /// The observed protocol hop.
    pub hop: RequestTraceHop,
    /// Wall-clock observation time.
    pub observed_at: std::time::SystemTime,
    /// Server health for a status observation, when present.
    pub health: Option<String>,
    /// Server quiescence for a status observation, when present.
    pub quiescent: Option<bool>,
}

impl std::fmt::Display for RequestTraceEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "session {:?}, request {:?}, {:?}",
            self.session, self.request_id, self.hop
        )?;
        if let Some(path) = &self.path {
            write!(f, ", {path}")?;
        }
        if let Some(line) = self.line {
            write!(f, ", line {line}")?;
        }
        if let Some(character) = self.character {
            write!(f, ", character {character}")?;
        }
        Ok(())
    }
}

/// A caller-owned destination. Implementations must return promptly and not panic.
/// Reply and Status callbacks run on the reader thread: a slow sink delays delivery.
/// Never call back into the client or its pool from a callback.
/// Request ids restart per client; use a unique session label for each spawn.
/// Reply means the reader observed a response, not that the caller received it
/// before its deadline. A Reply callback may complete after the caller times out.
pub trait RequestTraceSink: Send + Sync {
    /// Receives metadata only, never source text or response content.
    fn record(&self, event: &RequestTraceEvent);
}

#[derive(Default)]
pub(crate) struct RequestTrace {
    session: String,
    sink: Option<Arc<dyn RequestTraceSink>>,
    pending: HashMap<u64, RequestTraceEvent>,
}

impl RequestTrace {
    pub(crate) fn configure(
        &mut self,
        session: String,
        sink: Option<Arc<dyn RequestTraceSink>>,
    ) -> Vec<(Arc<dyn RequestTraceSink>, RequestTraceEvent)> {
        let discarded = self.clear();
        self.session = session;
        self.sink = sink;
        discarded
    }

    pub(crate) fn send(
        &mut self,
        id: u64,
        method: &str,
        params: &Value,
    ) -> Option<(Arc<dyn RequestTraceSink>, RequestTraceEvent)> {
        let sink = self.sink.clone()?;
        let position = params
            .get("position")
            .or_else(|| params.pointer("/item/selectionRange/start"));
        let event = RequestTraceEvent {
            session: self.session.clone(),
            request_id: Some(id),
            method: method.to_owned(),
            path: params
                .pointer("/textDocument/uri")
                .or_else(|| params.pointer("/item/uri"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            line: position.and_then(|p| p.get("line")).and_then(Value::as_u64),
            character: position
                .and_then(|p| p.get("character"))
                .and_then(Value::as_u64),
            hop: RequestTraceHop::Send,
            observed_at: std::time::SystemTime::now(),
            health: None,
            quiescent: None,
        };
        self.pending.insert(id, event.clone());
        Some((sink, event))
    }

    pub(crate) fn status(
        &self,
        method: &str,
        params: Option<&Value>,
    ) -> Option<(Arc<dyn RequestTraceSink>, RequestTraceEvent)> {
        if method != "experimental/serverStatus" {
            return None;
        }
        let sink = self.sink.clone()?;
        let params = params?;
        Some((
            sink,
            RequestTraceEvent {
                session: self.session.clone(),
                request_id: None,
                method: method.to_owned(),
                path: None,
                line: None,
                character: None,
                hop: RequestTraceHop::Status,
                observed_at: std::time::SystemTime::now(),
                health: params
                    .get("health")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                quiescent: params.get("quiescent").and_then(Value::as_bool),
            },
        ))
    }

    pub(crate) fn discard(
        &mut self,
        id: u64,
    ) -> Option<(Arc<dyn RequestTraceSink>, RequestTraceEvent)> {
        self.end(id, RequestTraceHop::Discarded)
    }

    pub(crate) fn clear(&mut self) -> Vec<(Arc<dyn RequestTraceSink>, RequestTraceEvent)> {
        let ids: Vec<_> = self.pending.keys().copied().collect();
        ids.into_iter().filter_map(|id| self.discard(id)).collect()
    }

    pub(crate) fn end(
        &mut self,
        id: u64,
        hop: RequestTraceHop,
    ) -> Option<(Arc<dyn RequestTraceSink>, RequestTraceEvent)> {
        let mut event = self.pending.remove(&id)?;
        event.hop = hop;
        event.observed_at = std::time::SystemTime::now();
        Some((self.sink.clone()?, event))
    }
}

pub(crate) fn emit(event: Option<(Arc<dyn RequestTraceSink>, RequestTraceEvent)>) {
    if let Some((sink, event)) = event {
        sink.record(&event);
    }
}

pub(crate) type SharedTrace = Arc<Mutex<RequestTrace>>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Sink;
    impl RequestTraceSink for Sink {
        fn record(&self, _: &RequestTraceEvent) {}
    }

    #[test]
    fn request_trace_every_send_ends_when_reconfigured() {
        #[derive(Default)]
        struct Events(Mutex<Vec<RequestTraceEvent>>);
        impl RequestTraceSink for Events {
            fn record(&self, event: &RequestTraceEvent) {
                self.0.lock().unwrap().push(event.clone());
            }
        }
        let events = Arc::new(Events::default());
        let mut trace = RequestTrace::default();
        trace.configure("old-session".into(), Some(events.clone()));
        emit(trace.send(7, "initialize", &json!({})));
        for event in trace.configure(String::new(), None) {
            emit(Some(event));
        }
        let events = events.0.lock().unwrap();
        assert_eq!(
            events.len(),
            2,
            "every Send needs one terminal event: {events:?}"
        );
        assert_eq!(events[0].request_id, events[1].request_id);
        assert_eq!(events[1].hop, RequestTraceHop::Discarded);
    }

    #[test]
    fn request_trace_discard_and_clear_release_only_the_owned_entries() {
        let mut trace = RequestTrace::default();
        trace.configure("cleanup".into(), Some(Arc::new(Sink)));
        for id in [1, 2, 3] {
            trace.send(id, "initialize", &json!({})).unwrap();
        }
        trace.discard(2);
        assert!(trace.end(2, RequestTraceHop::Reply).is_none());
        assert!(trace.end(1, RequestTraceHop::Reply).is_some());
        assert_eq!(trace.pending.len(), 1);
        trace.clear();
        assert!(trace.pending.is_empty());
        assert!(trace.end(3, RequestTraceHop::Reply).is_none());
        assert!(trace.send(4, "initialize", &json!({})).is_some());
    }

    #[test]
    fn request_trace_switch_stops_new_capture_by_the_old_sink() {
        #[derive(Default)]
        struct Events(Mutex<Vec<RequestTraceEvent>>);
        impl RequestTraceSink for Events {
            fn record(&self, event: &RequestTraceEvent) {
                self.0.lock().unwrap().push(event.clone());
            }
        }
        let old = Arc::new(Events::default());
        let new = Arc::new(Events::default());
        let mut trace = RequestTrace::default();
        trace.configure("old".into(), Some(old.clone()));
        emit(trace.send(1, "initialize", &json!({})));
        let captured = trace.end(1, RequestTraceHop::Reply);
        trace.configure("new".into(), Some(new.clone()));
        emit(captured);
        let old_count = old.0.lock().unwrap().len();
        assert_eq!(old_count, 2);
        emit(trace.send(2, "initialize", &json!({})));
        emit(trace.end(2, RequestTraceHop::Reply));
        trace.configure(String::new(), None);
        emit(trace.send(3, "initialize", &json!({})));
        assert_eq!(old.0.lock().unwrap().len(), old_count);
        assert_eq!(new.0.lock().unwrap().len(), 2);
    }

    #[test]
    fn request_trace_hierarchy_metadata_survives_reversed_replies() {
        let mut trace = RequestTrace::default();
        trace.configure("hierarchy".into(), Some(Arc::new(Sink)));
        for (id, line, character) in [(41, 7, 11), (42, 19, 23)] {
            let (_, event) = trace
                .send(
                    id,
                    "callHierarchy/outgoingCalls",
                    &json!({
                        "item": {
                            "uri": format!("file:///work/{id}.rs"),
                            "selectionRange": {"start": {"line": line, "character": character}},
                            "name": "source_canary",
                            "data": {"private": "payload_canary"}
                        }
                    }),
                )
                .unwrap();
            assert_eq!(event.hop, RequestTraceHop::Send);
        }
        for (id, line, character) in [(42, 19, 23), (41, 7, 11)] {
            let (_, event) = trace.end(id, RequestTraceHop::Reply).unwrap();
            assert_eq!(event.request_id, Some(id));
            assert_eq!(event.path, Some(format!("file:///work/{id}.rs")));
            assert_eq!((event.line, event.character), (Some(line), Some(character)));
            assert_eq!(event.session, "hierarchy");
            assert_eq!(event.hop, RequestTraceHop::Reply);
            let rendered = format!("{event:?}");
            assert!(!rendered.contains("source_canary"));
            assert!(!rendered.contains("payload_canary"));
            assert!(trace.end(id, RequestTraceHop::Reply).is_none());
        }
        assert!(trace.pending.is_empty());
        trace.configure(String::new(), None);
        assert!(trace.send(43, "initialize", &json!({})).is_none());
        assert!(trace.pending.is_empty());
    }
}
