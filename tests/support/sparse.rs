//! A loopback-only sparse registry serving real Cargo index records and archives.

use super::index_path;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Clone, Copy)]
pub enum IndexFault {
    Malformed,
    Unavailable,
}

pub struct SparseRegistry {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    faults: Arc<Mutex<BTreeMap<String, IndexFault>>>,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    worker: Option<JoinHandle<()>>,
}

impl SparseRegistry {
    pub fn new(registry: PathBuf) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let faults = Arc::new(Mutex::new(BTreeMap::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let worker = {
            let stop = stop.clone();
            let faults = faults.clone();
            let requests = requests.clone();
            thread::spawn(move || {
                while let Ok((mut stream, peer)) = listener.accept() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    assert!(peer.ip().is_loopback());
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0; 1024];
                    while request.len() < 16 * 1024
                        && !request.windows(4).any(|bytes| bytes == b"\r\n\r\n")
                    {
                        match stream.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(size) => request.extend_from_slice(&buffer[..size]),
                        }
                    }
                    let request = String::from_utf8_lossy(&request);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("");
                    let user_agent = request
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("user-agent")
                                .then(|| value.trim().to_owned())
                        })
                        .unwrap_or_default();
                    requests
                        .lock()
                        .unwrap()
                        .push((path.into(), user_agent.clone()));
                    let fault = (!user_agent.starts_with("cargo/"))
                        .then(|| faults.lock().unwrap().get(path).copied())
                        .flatten();
                    let (status, body) = if let Some(fault) = fault {
                        match fault {
                            IndexFault::Malformed => (200, b"{invalid registry record}\n".to_vec()),
                            IndexFault::Unavailable => (503, b"fixture index unavailable".to_vec()),
                        }
                    } else if path == "/index/config.json" {
                        (
                            200,
                            json!({
                                "dl": format!("http://{address}/crates/{{crate}}/{{version}}/download"),
                                "api": format!("http://{address}"),
                            })
                            .to_string()
                            .into_bytes(),
                        )
                    } else if path.starts_with("/index/") && !path.contains("..") {
                        file_response(registry.join(&path[1..]))
                    } else if let Some(rest) = path.strip_prefix("/crates/") {
                        let pieces: Vec<_> = rest.split('/').collect();
                        if pieces.len() == 3 && pieces[2] == "download" && !rest.contains("..") {
                            file_response(
                                registry.join(format!("{}-{}.crate", pieces[0], pieces[1])),
                            )
                        } else {
                            (404, Vec::new())
                        }
                    } else {
                        (404, Vec::new())
                    };
                    let reason = match status {
                        200 => "OK",
                        503 => "Service Unavailable",
                        _ => "Not Found",
                    };
                    let header = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n\
                         Content-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                        body.len(),
                    );
                    // A client may close a conditional/retried request early.
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(&body);
                }
            })
        };
        Self {
            address,
            stop,
            faults,
            requests,
            worker: Some(worker),
        }
    }

    pub fn index_url(&self) -> String {
        format!("sparse+http://{}/index/", self.address)
    }

    /// Cargo's own client still receives valid records. Other clients receive
    /// the fault, without depending on cargo-floor's particular User-Agent.
    pub fn fault_enumeration(&self, name: &str, fault: IndexFault) {
        let path = index_path(name).to_string_lossy().replace('\\', "/");
        self.faults
            .lock()
            .unwrap()
            .insert(format!("/index/{path}"), fault);
    }

    pub fn requests(&self) -> Vec<(String, String)> {
        self.requests.lock().unwrap().clone()
    }

    pub fn enumerations(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .filter(|(_, agent)| !agent.starts_with("cargo/"))
            .map(|(path, _)| path)
            .collect()
    }
}

fn file_response(path: PathBuf) -> (u16, Vec<u8>) {
    match fs::read(path) {
        Ok(bytes) => (200, bytes),
        Err(_) => (404, Vec::new()),
    }
}

impl Drop for SparseRegistry {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(2));
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join loopback sparse registry");
        }
    }
}
