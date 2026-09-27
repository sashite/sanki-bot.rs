// SPDX-License-Identifier: Apache-2.0
//! The engine's process (SEI §5; ADR-0045 §3 *Launch*): launched from the
//! configured argument vector without a shell, in the configured working
//! directory, with an environment that is empty but for the configured
//! variables, with the three standard descriptors piped and nothing else
//! inherited, in its own process group. Its standard output is read
//! continuously into events; its standard error is drained continuously and
//! logged at `debug` under the host's own bounds.
//!
//! The engine's **priority** is the deployment's to lower (SEI §5: only the
//! deployment confines the process): setting it from here would take an
//! `unsafe` call, which this crate forbids.
//!
//! The process is ended by closing its input; a process still alive two
//! seconds later has its **process group** killed. An engine that exits on
//! its own, emits a fatal error, or breaks the execution model is a failure
//! the runtime reads from [`Engine::recv`].

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::wire::{self, Event, Kind, MAX_LINE_BYTES};
use super::EngineFailure;

/// How long a closed engine gets to exit before its group is killed.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Standard-error lines logged per second; the rest are dropped.
const STDERR_LINES_PER_SECOND: u32 = 50;

/// The bytes of a standard-error line kept for the log.
const STDERR_LINE_BYTES: usize = 1024;

/// How to launch an engine (ADR-0045 §4 `[engine]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// The program, as a path resolved by the configuration.
    pub command: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// Its working directory.
    pub cwd: PathBuf,
    /// The only variables it receives.
    pub env: Vec<(String, String)>,
}

/// What the reader task delivers: a raw line with its receipt instant —
/// parsed by [`Engine::recv`], so that an engine talking while nobody
/// listens costs the host at most a few raw lines of memory.
#[derive(Debug)]
enum Incoming {
    Line(Instant, Vec<u8>),
    Violation(String),
    Closed,
}

/// Raw lines queued between the reader and `recv`. Bounded by
/// `RAW_LINES × MAX_LINE_BYTES` of host memory per engine; a conforming
/// engine is silent between requests, so the bound only ever holds back a
/// misbehaving one.
const RAW_LINES: usize = 8;

/// How long a request line may take to be written before the engine is
/// held to have stopped reading.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// A running engine.
#[derive(Debug)]
pub struct Engine {
    child: Child,
    /// The process group, killed on every end of the session.
    pgid: Option<nix::unistd::Pid>,
    stdin: Option<ChildStdin>,
    incoming: mpsc::Receiver<Incoming>,
    next_id: u64,
    /// The requests awaiting their terminal event.
    open: BTreeSet<u64>,
    /// Set once the engine can no longer be spoken to.
    dead: Option<EngineFailure>,
    reader: JoinHandle<()>,
    drain: JoinHandle<()>,
    started: Instant,
    /// When the last event returned by `recv` was read from the pipe.
    last_receipt: Instant,
}

impl Engine {
    /// Launches the engine. Nothing is sent yet.
    ///
    /// # Errors
    ///
    /// [`EngineFailure::Launch`] when the process cannot be started or its
    /// pipes taken.
    pub fn launch(launch: &Launch) -> Result<Self, EngineFailure> {
        let mut command = Command::new(&launch.command);
        command
            .args(&launch.args)
            .current_dir(&launch.cwd)
            .env_clear()
            .envs(launch.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| EngineFailure::Launch(format!("{}: {e}", launch.command.display())))?;
        let pgid = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .map(nix::unistd::Pid::from_raw);
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (Some(stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) else {
            return Err(EngineFailure::Launch(
                "the standard streams were not piped".to_owned(),
            ));
        };

        let (tx, incoming) = mpsc::channel(RAW_LINES);
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::with_capacity(64 * 1024, stdout);
            loop {
                let mut buffer = Vec::new();
                // Bounded read: a line beyond the bound is a violation, and
                // the rest of the stream is not read.
                let read = (&mut lines)
                    .take(MAX_LINE_BYTES as u64)
                    .read_until(b'\n', &mut buffer)
                    .await;
                match read {
                    Ok(0) => {
                        let _ = tx.send(Incoming::Closed).await;
                        return;
                    }
                    Ok(_) if buffer.last() != Some(&b'\n') && buffer.len() >= MAX_LINE_BYTES => {
                        let _ = tx
                            .send(Incoming::Violation(format!(
                                "a line longer than {MAX_LINE_BYTES} bytes"
                            )))
                            .await;
                        return;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        let _ = tx.send(Incoming::Closed).await;
                        return;
                    }
                }
                if buffer.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                if tx
                    .send(Incoming::Line(Instant::now(), buffer))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        let drain = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr);
            let mut buffer = Vec::with_capacity(1024);
            let mut second = Instant::now();
            let mut in_second: u32 = 0;
            loop {
                buffer.clear();
                match (&mut lines)
                    .take(STDERR_LINE_BYTES as u64)
                    .read_until(b'\n', &mut buffer)
                    .await
                {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                // A longer line is truncated: the rest is discarded up to
                // its newline.
                if buffer.last() != Some(&b'\n') && buffer.len() >= STDERR_LINE_BYTES {
                    let mut rest = Vec::new();
                    loop {
                        rest.clear();
                        match (&mut lines)
                            .take(STDERR_LINE_BYTES as u64)
                            .read_until(b'\n', &mut rest)
                            .await
                        {
                            Ok(0) | Err(_) => return,
                            Ok(_) if rest.last() == Some(&b'\n') => break,
                            Ok(_) => {}
                        }
                    }
                    buffer.extend_from_slice("…".as_bytes());
                }
                if second.elapsed() >= Duration::from_secs(1) {
                    second = Instant::now();
                    in_second = 0;
                }
                in_second = in_second.saturating_add(1);
                if in_second > STDERR_LINES_PER_SECOND {
                    continue;
                }
                let text = String::from_utf8_lossy(&buffer);
                tracing::debug!(engine_stderr = %wire::escape(text.trim_end()));
            }
        });

        let started = Instant::now();
        Ok(Self {
            child,
            pgid,
            stdin: Some(stdin),
            incoming,
            next_id: 1,
            open: BTreeSet::new(),
            dead: None,
            reader,
            drain,
            started,
            last_receipt: started,
        })
    }

    /// When the process was launched.
    #[must_use]
    pub const fn started(&self) -> Instant {
        self.started
    }

    /// Whether the engine can still be spoken to.
    #[must_use]
    pub const fn is_alive(&self) -> bool {
        self.dead.is_none()
    }

    /// The failure the engine is in, when it is.
    #[must_use]
    pub const fn failure(&self) -> Option<&EngineFailure> {
        self.dead.as_ref()
    }

    /// When the event `recv` last returned was read from the engine's
    /// output — before the host got round to reading it, so that a late
    /// consumer never mistakes its own delay for the engine's.
    #[must_use]
    pub const fn last_receipt(&self) -> Instant {
        self.last_receipt
    }

    /// Sends a request and returns its `id`.
    ///
    /// # Errors
    ///
    /// The failure the engine is already in, [`EngineFailure::Exited`]
    /// when its input can no longer be written, or
    /// [`EngineFailure::Unresponsive`] when the engine stopped reading it.
    pub async fn send(
        &mut self,
        op: &str,
        fields: Map<String, Value>,
    ) -> Result<u64, EngineFailure> {
        if let Some(failure) = &self.dead {
            return Err(failure.clone());
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let line = wire::request_line(id, op, fields);
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(self.fail(EngineFailure::Exited));
        };
        let written = tokio::time::timeout(WRITE_TIMEOUT, async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await
        })
        .await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(self.fail(EngineFailure::Exited)),
            Err(_) => return Err(self.fail(EngineFailure::Unresponsive)),
        }
        self.open.insert(id);
        Ok(id)
    }

    /// The next event, or `None` when `until` passes first.
    ///
    /// # Errors
    ///
    /// An unexpected end of the session, a fatal error, or a violation of
    /// the execution model (a line that is not an event, an event attached
    /// to no open request, a second terminal event). After an error the
    /// engine is dead.
    ///
    /// A line already read from the pipe is returned even when `until` has
    /// passed, so that a host late to read never mistakes its delay for the
    /// engine's silence.
    pub async fn recv(&mut self, until: Instant) -> Result<Option<Event>, EngineFailure> {
        if let Some(failure) = &self.dead {
            return Err(failure.clone());
        }
        loop {
            let message = match self.incoming.try_recv() {
                Ok(message) => message,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(self.fail(EngineFailure::Exited))
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    let now = Instant::now();
                    if now >= until {
                        return Ok(None);
                    }
                    match tokio::time::timeout(
                        until.saturating_duration_since(now),
                        self.incoming.recv(),
                    )
                    .await
                    {
                        Err(_) => return Ok(None),
                        Ok(None) => return Err(self.fail(EngineFailure::Exited)),
                        Ok(Some(message)) => message,
                    }
                }
            };
            match message {
                Incoming::Closed => return Err(self.fail(EngineFailure::Exited)),
                Incoming::Violation(reason) => {
                    return Err(self.fail(EngineFailure::Violation(reason)))
                }
                Incoming::Line(received, bytes) => {
                    let Ok(text) = std::str::from_utf8(&bytes) else {
                        return Err(self.fail(EngineFailure::Violation(
                            "a line that is not UTF-8".to_owned(),
                        )));
                    };
                    let event = match wire::read_event(text.trim_end_matches(['\n', '\r'])) {
                        Ok(event) => event,
                        Err(reason) => return Err(self.fail(EngineFailure::Violation(reason))),
                    };
                    // An `ev` this host does not know is ignored whatever it
                    // is attached to (SEI §6.4, §9.5).
                    if matches!(event.kind, Kind::Unknown) {
                        continue;
                    }
                    let Some(re) = event.re else {
                        return match event.kind {
                            Kind::Error(error) => Err(self.fail(EngineFailure::Fatal(error))),
                            // A `done` or an `info` with no `re`: nothing in the
                            // protocol; tolerated as unknown.
                            _ => continue,
                        };
                    };
                    if !self.open.contains(&re) {
                        return Err(self.fail(EngineFailure::Violation(format!(
                            "an event attached to request {re}, which is not pending"
                        ))));
                    }
                    if event.is_terminal() {
                        self.open.remove(&re);
                    }
                    self.last_receipt = received;
                    return Ok(Some(event));
                }
            }
        }
    }

    /// Whether request `id` still awaits its terminal event.
    #[must_use]
    pub fn is_open(&self, id: u64) -> bool {
        self.open.contains(&id)
    }

    /// Marks the engine dead with `failure` — the first failure is the one
    /// kept — ends its process group, and returns the failure. What the
    /// runtime calls on an engine failure of its own finding (an overrun, a
    /// refused `best`).
    pub fn fail(&mut self, failure: EngineFailure) -> EngineFailure {
        if self.dead.is_none() {
            self.dead = Some(failure.clone());
        }
        self.kill();
        failure
    }

    /// Kills the engine's process group at once. The engine is not marked
    /// dead by this alone; see [`Engine::fail`].
    pub fn kill(&mut self) {
        self.stdin.take();
        if let Some(pgid) = self.pgid {
            let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
        }
        let _ = self.child.start_kill();
    }

    /// Ends the session as SEI §5 prescribes: the input is closed, the
    /// engine gets two seconds to exit, then its process group is killed —
    /// killed in every case, for the helpers an engine may have started.
    pub async fn close(mut self) {
        self.stdin.take();
        if tokio::time::timeout(EXIT_GRACE, self.child.wait())
            .await
            .is_err()
        {
            self.kill();
            let _ = self.child.wait().await;
        }
        self.kill();
        self.reader.abort();
        self.drain.abort();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // `kill_on_drop` ends the process; the group may hold children of
        // the engine's own, which the signal reaches.
        self.kill();
        self.reader.abort();
        self.drain.abort();
    }
}
