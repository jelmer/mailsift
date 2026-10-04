//! In-memory DAV server for sink tests. It remembers what is PUT and
//! serves it back on GET with an ETag, honours `If-Match` and
//! `If-None-Match: *` on PUT, and answers every PROPFIND with the three
//! collection URLs the CalDAV sink discovers.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::runtime::{Handle, Runtime};

use super::webdav::WebdavSink;

const MULTISTATUS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/</D:href>
    <D:propstat>
      <D:prop>
        <D:current-user-principal><D:href>/principal/</D:href></D:current-user-principal>
        <C:schedule-inbox-URL><D:href>/inbox/</D:href></C:schedule-inbox-URL>
        <C:schedule-default-calendar-URL><D:href>/calendar/</D:href></C:schedule-default-calendar-URL>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

#[derive(Default)]
struct State {
    /// Each resource's body and the ETag it currently has.
    resources: BTreeMap<String, (Vec<u8>, String)>,
    /// Bodies to store right after the next GET of their path.
    overtaking: BTreeMap<String, Vec<u8>>,
    requests: Vec<String>,
    writes: u32,
}

impl State {
    fn store(&mut self, path: String, body: Vec<u8>) -> bool {
        self.writes += 1;
        let etag = format!("\"v{}\"", self.writes);
        self.resources.insert(path, (body, etag)).is_some()
    }
}

pub struct FakeDav {
    /// `http://127.0.0.1:<port>/`.
    pub base_url: String,
    state: Arc<Mutex<State>>,
}

impl FakeDav {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}/", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        // Detached: the thread ends with the test process.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                serve(stream.unwrap(), &shared);
            }
        });
        Self { base_url, state }
    }

    /// A WebDAV sink rooted at this server.
    pub fn webdav_sink(&self) -> WebdavSink {
        WebdavSink::new(
            self.base_url.clone(),
            Some("u".into()),
            Some("p".into()),
            runtime_handle(),
        )
        .unwrap()
    }

    /// The resource stored at `path` (e.g. `/calendar/x.ics`).
    pub fn resource(&self, path: &str) -> Option<Vec<u8>> {
        let state = self.state.lock().unwrap();
        state.resources.get(path).map(|(body, _)| body.clone())
    }

    pub fn put(&self, path: &str, body: &[u8]) {
        let mut state = self.state.lock().unwrap();
        state.store(path.to_string(), body.to_vec());
    }

    /// Have another writer store `body` at `path` just after the next
    /// GET of it: in between a sink reading the resource and writing
    /// it back.
    pub fn overtake_after_next_get(&self, path: &str, body: &[u8]) {
        let mut state = self.state.lock().unwrap();
        state.overtaking.insert(path.to_string(), body.to_vec());
    }

    /// Every request served so far, as `"<METHOD> <path>"`.
    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }
}

/// Handle of a runtime that outlives every sink built in a test.
pub fn runtime_handle() -> Handle {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().unwrap()).handle().clone()
}

fn serve(stream: TcpStream, state: &Mutex<State>) {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap().to_string();
    let path = parts.next().unwrap().to_string();

    let mut content_length = 0;
    let mut if_match = None;
    let mut if_none_match = false;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.parse().unwrap(),
            "if-match" => if_match = Some(value.to_string()),
            "if-none-match" => if_none_match = value == "*",
            _ => {}
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).unwrap();

    let (status, etag, response_body) = {
        let mut state = state.lock().unwrap();
        state.requests.push(format!("{method} {path}"));
        match method.as_str() {
            "GET" => {
                let response = match state.resources.get(&path) {
                    Some((body, etag)) => ("200 OK", Some(etag.clone()), body.clone()),
                    None => ("404 Not Found", None, Vec::new()),
                };
                if let Some(body) = state.overtaking.remove(&path) {
                    state.store(path, body);
                }
                response
            }
            "PUT" => {
                let current = state.resources.get(&path).map(|(_, etag)| etag);
                let precondition_holds = match (&if_match, if_none_match) {
                    (Some(wanted), _) => current == Some(wanted),
                    (None, true) => current.is_none(),
                    (None, false) => true,
                };
                if !precondition_holds {
                    ("412 Precondition Failed", None, Vec::new())
                } else if state.store(path, body) {
                    ("204 No Content", None, Vec::new())
                } else {
                    ("201 Created", None, Vec::new())
                }
            }
            "PROPFIND" => ("207 Multi-Status", None, MULTISTATUS.as_bytes().to_vec()),
            other => panic!("fake DAV server got unexpected {other}"),
        }
    };

    let etag = etag
        .map(|etag| format!("ETag: {etag}\r\n"))
        .unwrap_or_default();
    let mut stream = reader.into_inner();
    write!(
        stream,
        "HTTP/1.1 {status}\r\n{etag}Content-Length: {}\r\nConnection: close\r\n\r\n",
        response_body.len()
    )
    .unwrap();
    stream.write_all(&response_body).unwrap();
}
