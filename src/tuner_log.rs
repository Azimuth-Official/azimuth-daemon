//! Handling of the tuner driver's log records ("[R82xx] PLL not locked!"): they are
//! counted but never printed. Kept in its own file, with no tests, so the
//! `logging_bridge` integration test (its own process, because it installs the
//! process-global subscriber) can include exactly this code.

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Filter, SubscriberExt};
use tracing_subscriber::{fmt, reload, EnvFilter, Layer, Registry};

/// Target of the rtl-sdr-rs tuner driver, whose `log` records carry the
/// "[R82xx] PLL not locked!" line (and chatty per-tune frequency math).
pub const TUNER_LOG_TARGET: &str = "rtl_sdr_rs::tuners::r82xx";
/// Lets those records reach the (silent) counting layer. The output layers drop
/// them again (`QuietTuner`), so nothing from this target is ever printed.
const TUNER_DIRECTIVE: &str = "rtl_sdr_rs::tuners::r82xx=info";

pub fn with_tuner_directive(directive: &str) -> String {
    format!("{directive},{TUNER_DIRECTIVE}")
}

/// What we need from an event to recognise tuner-driver records. Records that
/// came through the `log` bridge carry the original target in `log.target`.
#[derive(Default)]
struct EventFacts {
    message: String,
    log_target: Option<String>,
}

impl Visit for EventFacts {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "log.target" => self.log_target = Some(value.to_string()),
            "message" => self.message = value.to_string(),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

fn facts(event: &Event<'_>) -> Option<EventFacts> {
    let mut f = EventFacts::default();
    event.record(&mut f);
    let target = f.log_target.as_deref().unwrap_or_else(|| event.metadata().target());
    target.starts_with(TUNER_LOG_TARGET).then_some(f)
}

/// Counts "PLL not locked" records and writes nothing.
struct PllCounter;

impl<S: Subscriber> Layer<S> for PllCounter {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if let Some(f) = facts(event) {
            if f.message.contains("PLL not locked") {
                crate::sdr::PLL_LOCK_FAILS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

/// Output filter: drops tuner-driver records so they never reach stdout or the
/// log file at the default level (they are only counted).
struct QuietTuner;

impl<S: Subscriber> Filter<S> for QuietTuner {
    fn enabled(&self, _meta: &tracing::Metadata<'_>, _cx: &Context<'_, S>) -> bool {
        true
    }

    fn event_enabled(&self, event: &Event<'_>, _cx: &Context<'_, S>) -> bool {
        facts(event).is_none()
    }
}

/// The subscriber stack: runtime filter, silent PLL counter, then the stdout and
/// file outputs (both blind to tuner-driver records).
pub fn assemble<W1, W2>(
    filter_layer: reload::Layer<EnvFilter, Registry>,
    stdout: W1,
    file: W2,
) -> impl Subscriber + Send + Sync
where
    W1: for<'a> fmt::MakeWriter<'a> + Send + Sync + 'static,
    W2: for<'a> fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    Registry::default()
        .with(filter_layer)
        .with(PllCounter)
        .with(fmt::layer().with_writer(stdout).with_target(true).with_filter(QuietTuner))
        .with(fmt::layer().with_writer(file).with_ansi(false).with_filter(QuietTuner))
}

