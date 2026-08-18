//! The dashboard's log panel.
//!
//! When the TUI is running it replaces the console `fmt` layer — otherwise
//! stray log writes scribble over the rendered frame. The `--log` file layer is
//! untouched, so `--debug 2 --log` still produces the complete file while the
//! panel shows a readable tail.
//!
//! Discrete stream events (underrun, receiver dropped, reconnect succeeded) are
//! already `warn!`/`info!` lines, so the panel gets them for free — they never
//! needed to be duplicated into the stats snapshot.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// How many lines the panel remembers. The panel shows a few dozen at a time;
/// the rest is scrollback for when something scrolls past during a dropout.
pub const DEFAULT_CAPACITY: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// `HH:MM:SS`, UTC — matching the `--log` file so a panel line can be
    /// found in the file.
    pub ts: String,
    pub level: Level,
    pub msg: String,
}

/// A bounded, shared ring of recent log lines.
///
/// Cloning shares the same underlying buffer: the tracing layer holds one
/// clone and the renderer another.
#[derive(Clone)]
pub struct LogBuffer {
    lines: Arc<Mutex<VecDeque<LogLine>>>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::with_capacity(capacity.min(1024)))),
            capacity: capacity.max(1),
        }
    }

    pub fn push(&self, line: LogLine) {
        // A poisoned lock must not take the stream down with it: logging is
        // never worth killing audio for.
        let Ok(mut lines) = self.lines.lock() else {
            return;
        };
        if lines.len() == self.capacity {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// The newest `n` lines, oldest first — the order they are drawn in.
    pub fn tail(&self, n: usize) -> Vec<LogLine> {
        let Ok(lines) = self.lines.lock() else {
            return Vec::new();
        };
        let skip = lines.len().saturating_sub(n);
        lines.iter().skip(skip).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.lines.lock().map(|l| l.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

/// A `tracing` layer that appends every event to a [`LogBuffer`].
///
/// Filtering is left to the caller — attach this with `.with_filter(...)` so
/// `--debug` controls the panel exactly as it controls the console.
pub struct LogLayer {
    buf: LogBuffer,
}

impl LogLayer {
    pub fn new(buf: LogBuffer) -> Self {
        Self { buf }
    }
}

impl<S: Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        self.buf.push(LogLine {
            ts: clock_time(),
            level: *event.metadata().level(),
            msg: visitor.finish(),
        });
    }
}

/// Renders an event's `message` plus its structured fields into one line.
///
/// The message comes first and the fields follow as `key=value`, which is what
/// the `fmt` layer does — so a panel line reads the same as the file line it
/// corresponds to.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl MessageVisitor {
    fn finish(self) -> String {
        match (self.message.is_empty(), self.fields.is_empty()) {
            (true, _) => self.fields,
            (false, true) => self.message,
            (false, false) => format!("{} {}", self.message, self.fields),
        }
    }

    fn record(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
            // Debug on a `&str` message quotes it; the fmt layer shows it bare.
            if self.message.starts_with('"') && self.message.ends_with('"') {
                self.message = self.message[1..self.message.len() - 1].to_string();
            }
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={value:?}", field.name());
        }
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record(field, value);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={value}", field.name());
        }
    }
}

/// Whether console log output is currently suppressed.
///
/// While the dashboard owns the screen, a stray log write would scribble over
/// the frame. Rather than rebuilding the subscriber mid-run, the console layer
/// carries a filter that consults this flag — so the same process can narrate
/// normally before and after the dashboard, and stay silent during it.
static CONSOLE_QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_console_quiet(quiet: bool) {
    CONSOLE_QUIET.store(quiet, std::sync::atomic::Ordering::Relaxed);
}

pub fn console_quiet() -> bool {
    CONSOLE_QUIET.load(std::sync::atomic::Ordering::Relaxed)
}

/// `HH:MM:SS` UTC, without pulling in a date library.
fn clock_time() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day_secs = secs % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        day_secs / 3600,
        (day_secs % 3600) / 60,
        day_secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    fn line(msg: &str) -> LogLine {
        LogLine {
            ts: "12:00:00".into(),
            level: Level::INFO,
            msg: msg.into(),
        }
    }

    #[test]
    fn capacity_is_enforced_and_oldest_is_dropped() {
        let buf = LogBuffer::new(3);
        for i in 0..5 {
            buf.push(line(&format!("line {i}")));
        }
        assert_eq!(buf.len(), 3);
        let msgs: Vec<String> = buf.tail(10).into_iter().map(|l| l.msg).collect();
        assert_eq!(msgs, ["line 2", "line 3", "line 4"]);
    }

    #[test]
    fn tail_returns_newest_last() {
        let buf = LogBuffer::new(10);
        buf.push(line("first"));
        buf.push(line("second"));
        buf.push(line("third"));
        let msgs: Vec<String> = buf.tail(2).into_iter().map(|l| l.msg).collect();
        assert_eq!(msgs, ["second", "third"], "oldest first, newest last");
    }

    #[test]
    fn tail_of_an_empty_buffer_is_empty() {
        assert!(LogBuffer::new(10).tail(5).is_empty());
    }

    #[test]
    fn clones_share_one_buffer() {
        let buf = LogBuffer::new(10);
        let writer = buf.clone();
        writer.push(line("from the clone"));
        assert_eq!(buf.len(), 1, "the layer's clone must feed the renderer's");
    }

    #[test]
    fn layer_captures_level_and_message() {
        let buf = LogBuffer::new(10);
        let subscriber = tracing_subscriber::registry().with(LogLayer::new(buf.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("underrun risk — raising latency");
        });

        let lines = buf.tail(10);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].level, Level::WARN);
        assert_eq!(lines[0].msg, "underrun risk — raising latency");
        assert_eq!(lines[0].ts.len(), 8, "HH:MM:SS");
    }

    #[test]
    fn layer_captures_structured_fields_after_the_message() {
        let buf = LogBuffer::new(10);
        let subscriber = tracing_subscriber::registry().with(LogLayer::new(buf.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(from_ms = 500, to_ms = 750, "raising latency");
        });

        let msg = &buf.tail(1)[0].msg;
        assert!(msg.starts_with("raising latency"), "message leads: {msg}");
        assert!(msg.contains("from_ms=500"), "fields follow: {msg}");
        assert!(msg.contains("to_ms=750"), "fields follow: {msg}");
    }

    #[test]
    fn string_fields_are_not_double_quoted() {
        // `receiver = %r.name` is the project's usual style; a panel reading
        // `receiver="Pool Room"` instead of `receiver=Pool Room` is noise.
        let buf = LogBuffer::new(10);
        let subscriber = tracing_subscriber::registry().with(LogLayer::new(buf.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(receiver = %"Pool Room", "reconnected");
        });
        let msg = &buf.tail(1)[0].msg;
        assert!(msg.contains("receiver=Pool Room"), "got: {msg}");
    }

    #[test]
    fn clock_time_is_well_formed() {
        let t = clock_time();
        assert_eq!(t.len(), 8);
        let parts: Vec<&str> = t.split(':').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].parse::<u32>().unwrap() < 24);
        assert!(parts[1].parse::<u32>().unwrap() < 60);
        assert!(parts[2].parse::<u32>().unwrap() < 60);
    }
}
