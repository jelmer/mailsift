//! In-memory DAV server for sink tests. It remembers what is PUT and
//! serves it back on GET, and answers every PROPFIND with the three
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
    resources: BTreeMap<String, Vec<u8>>,
    requests: Vec<String>,
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
        self.state.lock().unwrap().resources.get(path).cloned()
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
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).unwrap();

    let (status, response_body) = {
        let mut state = state.lock().unwrap();
        state.requests.push(format!("{method} {path}"));
        match method.as_str() {
            "GET" => match state.resources.get(&path) {
                Some(body) => ("200 OK", body.clone()),
                None => ("404 Not Found", Vec::new()),
            },
            "PUT" => match state.resources.insert(path, body) {
                Some(_) => ("204 No Content", Vec::new()),
                None => ("201 Created", Vec::new()),
            },
            "PROPFIND" => ("207 Multi-Status", MULTISTATUS.as_bytes().to_vec()),
            other => panic!("fake DAV server got unexpected {other}"),
        }
    };

    let mut stream = reader.into_inner();
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response_body.len()
    )
    .unwrap();
    stream.write_all(&response_body).unwrap();
}
