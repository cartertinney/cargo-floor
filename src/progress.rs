use std::fmt;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Quiet,
    Compact,
    Verbose,
}

pub struct Progress {
    mode: Mode,
}

#[derive(Default)]
pub struct Activity {
    worker: Option<(Sender<()>, JoinHandle<()>)>,
}

pub struct Task {
    mode: Mode,
    label: String,
    activity: Activity,
    finished: bool,
}

impl Progress {
    pub fn new(quiet: bool, verbose: bool) -> Self {
        Self {
            mode: if quiet {
                Mode::Quiet
            } else if verbose {
                Mode::Verbose
            } else {
                Mode::Compact
            },
        }
    }

    pub fn message(&self, message: fmt::Arguments<'_>) {
        if self.mode == Mode::Verbose {
            eprintln!("cargo-floor: {message}");
        }
    }

    pub fn activity(&self, message: fmt::Arguments<'_>) -> Activity {
        if self.mode != Mode::Verbose {
            return Activity::default();
        }
        self.message(message);
        let operation = message.to_string().to_lowercase();
        Activity::start(move |elapsed| {
            eprintln!(
                "cargo-floor: still {operation} ({}s elapsed)",
                elapsed.as_secs()
            );
        })
    }

    pub fn task(&self, label: fmt::Arguments<'_>) -> Task {
        let label = label.to_string();
        let activity = match self.mode {
            Mode::Quiet => Activity::default(),
            Mode::Compact => {
                eprint!("{label}");
                Activity::start(|_| eprint!("."))
            }
            Mode::Verbose => {
                eprintln!("cargo-floor: {label}");
                Activity::default()
            }
        };
        Task {
            mode: self.mode,
            label,
            activity,
            finished: false,
        }
    }
}

impl Task {
    pub fn advance(&self) {
        if self.mode == Mode::Compact {
            eprint!(".");
        }
    }

    pub fn finish(mut self, result: fmt::Arguments<'_>) {
        drop(std::mem::take(&mut self.activity));
        self.finished = true;
        match self.mode {
            Mode::Quiet => {}
            Mode::Compact => eprintln!(" {result}"),
            Mode::Verbose => eprintln!("cargo-floor: {}: {result}", self.label),
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        drop(std::mem::take(&mut self.activity));
        if !self.finished {
            match self.mode {
                Mode::Quiet => {}
                Mode::Compact => eprintln!(" failed"),
                Mode::Verbose => eprintln!("cargo-floor: {}: failed", self.label),
            }
        }
    }
}

impl Activity {
    fn start(tick: impl Fn(Duration) + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let started = Instant::now();
            while let Err(RecvTimeoutError::Timeout) =
                receiver.recv_timeout(Duration::from_secs(10))
            {
                tick(started.elapsed());
            }
        });
        Self {
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
