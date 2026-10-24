//! Request traces expose correlation metadata, never source text.

use std::sync::{Arc, Mutex};

use bage::lsp::{Client, LspPool, RequestTraceEvent, RequestTraceSink};

#[derive(Default)]
struct Events(Mutex<Vec<RequestTraceEvent>>);

impl RequestTraceSink for Events {
    fn record(&self, event: &RequestTraceEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

#[test]
fn request_trace_factory_is_caller_owned() {
    let events = Arc::new(Events::default());
    let sink = events.clone();
    let pool = LspPool::from_spawn(
        Box::new(move || {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let socket = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (server, _) = listener.accept().unwrap();
            std::thread::spawn(move || {
                use std::io::{BufRead, Read, Write};
                let mut writer = server.try_clone().unwrap();
                let mut reader = std::io::BufReader::new(server);
                loop {
                    let mut length = None;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap() == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                        if let Some(value) = line.strip_prefix("Content-Length: ") {
                            length = Some(value.trim().parse::<usize>().unwrap());
                        }
                    }
                    let mut body = vec![0; length.expect("frame length")];
                    reader.read_exact(&mut body).unwrap();
                    let message: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    if message["method"] == "exit" {
                        return;
                    }
                    if let Some(id) = message.get("id") {
                        let result = if message["method"] == "initialize" {
                            serde_json::json!({"capabilities": {}})
                        } else {
                            serde_json::json!([])
                        };
                        let reply = serde_json::json!({"jsonrpc":"2.0", "id":id, "result":result})
                            .to_string();
                        write!(writer, "Content-Length: {}\r\n\r\n{}", reply.len(), reply).unwrap();
                        writer.flush().unwrap();
                    }
                }
            });
            let mut client = Client::from_conn(socket.try_clone().unwrap(), socket);
            client.set_request_trace("factory-session".to_owned(), Some(sink.clone()));
            Ok(client)
        }),
        std::time::Duration::from_secs(60),
        1,
    );
    assert!(pool.is_empty());
    pool.with_client(std::path::Path::new("/work"), "rust", |client| {
        client
            .definition("/work/main.rs", "fn main() {}", 0, 3)
            .map(|_| ())
    })
    .expect("public request through caller-owned factory");
    pool.shutdown();
    let events = events.0.lock().unwrap();
    for method in ["initialize", "textDocument/definition", "shutdown"] {
        let pair: Vec<_> = events
            .iter()
            .filter(|event| event.method == method)
            .collect();
        assert_eq!(pair.len(), 2, "{events:?}");
        assert_eq!(pair[0].hop, bage::lsp::RequestTraceHop::Send);
        assert_eq!(pair[1].hop, bage::lsp::RequestTraceHop::Reply);
        assert_eq!(pair[0].request_id, pair[1].request_id);
    }
}

#[test]
fn request_trace_is_explicitly_opted_in() {
    let events = Arc::new(Events::default());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let socket = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    let peer = std::thread::spawn(move || {
        use std::io::{BufRead, Read, Write};
        let mut writer = server.try_clone().unwrap();
        let mut reader = std::io::BufReader::new(server);
        loop {
            let mut length = None;
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.strip_prefix("Content-Length: ") {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let mut body = vec![0; length.expect("frame length")];
            reader.read_exact(&mut body).unwrap();
            let message: serde_json::Value = serde_json::from_slice(&body).unwrap();
            if let Some(id) = message.get("id") {
                assert_eq!(message["method"], "textDocument/definition");
                let reply = serde_json::json!({"jsonrpc":"2.0", "id":id, "result":[]}).to_string();
                write!(writer, "Content-Length: {}\r\n\r\n{}", reply.len(), reply).unwrap();
                writer.flush().unwrap();
                return;
            }
        }
    });
    let mut client = Client::from_conn(socket.try_clone().unwrap(), socket);
    client.set_request_trace("test-session".to_owned(), Some(events.clone()));
    client.set_request_trace("test-session".to_owned(), None);
    client
        .definition("/work/main.rs", "fn main() {}", 0, 3)
        .expect("request succeeds with tracing disabled");
    peer.join().unwrap();
    assert!(events.0.lock().unwrap().is_empty());
}
