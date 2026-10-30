//! Real build scripts notify the test over TCP and wait for an explicit release.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct BuildGate {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    ready: mpsc::Receiver<TcpStream>,
    connection: Option<TcpStream>,
    worker: Option<JoinHandle<()>>,
}

impl BuildGate {
    pub fn new() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, ready) = mpsc::channel();
        let worker = {
            let stop = stop.clone();
            thread::spawn(move || {
                while let Ok((mut stream, _)) = listener.accept() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut message = [0; 1];
                    if stream.read_exact(&mut message).is_ok() && message == *b"R" {
                        let _ = sender.send(stream);
                        break;
                    }
                }
            })
        };
        Self {
            address,
            stop,
            ready,
            connection: None,
            worker: Some(worker),
        }
    }

    pub fn address(&self) -> String {
        self.address.to_string()
    }

    pub fn wait_ready(&mut self) {
        self.connection = Some(
            self.ready
                .recv_timeout(Duration::from_secs(45))
                .expect("real Cargo build script must reach the synchronization gate"),
        );
    }

    pub fn release(&mut self) {
        if let Some(mut connection) = self.connection.take() {
            let _ = connection.write_all(b"G");
        }
    }

    pub fn script() -> &'static str {
        r#"
fn main() {
    use std::io::{Read, Write};
    println!("cargo:rerun-if-env-changed=FLOOR_TEST_GATE");
    if let (Ok(address), Ok(marker)) = (
        std::env::var("FLOOR_TEST_GATE"),
        std::env::var("FLOOR_TEST_GATE_MARKER"),
    ) {
        if std::fs::OpenOptions::new().write(true).create_new(true).open(marker).is_ok() {
            let mut socket = std::net::TcpStream::connect(address).unwrap();
            socket.set_read_timeout(Some(std::time::Duration::from_secs(45))).unwrap();
            socket.write_all(b"R").unwrap();
            let mut release = [0; 1];
            socket.read_exact(&mut release).unwrap();
            assert_eq!(release, *b"G");
        }
    }
}
"#
    }
}

impl Drop for BuildGate {
    fn drop(&mut self) {
        self.release();
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(2));
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .expect("join build-script synchronization gate");
        }
    }
}
