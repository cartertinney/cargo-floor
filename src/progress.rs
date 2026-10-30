use std::fmt;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub struct Progress {
    quiet: bool,
}

#[derive(Default)]
pub struct Activity {
    worker: Option<(Sender<()>, JoinHandle<()>)>,
}

impl Progress {
    pub fn new(quiet: bool) -> Self {
        Self { quiet }
    }

    pub fn message(&self, message: fmt::Arguments<'_>) {
        if !self.quiet {
            eprintln!("cargo-floor: {message}");
        }
    }

    pub fn activity(&self, message: fmt::Arguments<'_>) -> Activity {
        if self.quiet {
            return Activity::default();
        }
        self.message(message);
        let operation = message.to_string().to_lowercase();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let started = Instant::now();
            while let Err(RecvTimeoutError::Timeout) =
                receiver.recv_timeout(Duration::from_secs(10))
            {
                eprintln!(
                    "cargo-floor: still {operation} ({}s elapsed)",
                    started.elapsed().as_secs()
                );
            }
        });
        Activity {
            worker: Some((sender, worker)),
        }
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        if let Some((sender, worker)) = self.worker.take() {
            drop(sender);
            worker.join().expect("progress reporter panicked");
        }
    }
}
