//! A minimal stderr logger for the command-line tools, configured by an
//! environment variable (`error`, `warn`, `info`, `debug`, `trace`).

use log::{Level, Log, Metadata, Record};

struct StderrLogger {
    level: Level,
}

impl Log for StderrLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!(
                "[{:<5} {}] {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

/// Parses a level name; unknown names mean "off".
pub fn parse_level(s: &str) -> Option<Level> {
    match s.to_ascii_lowercase().as_str() {
        "error" => Some(Level::Error),
        "warn" => Some(Level::Warn),
        "info" => Some(Level::Info),
        "debug" => Some(Level::Debug),
        "trace" => Some(Level::Trace),
        _ => None,
    }
}

/// Installs the logger at the level named by environment variable `var`.
/// Does nothing if the variable is unset/invalid or a logger already exists.
pub fn init_from_env(var: &str) {
    let Some(level) = std::env::var(var).ok().as_deref().and_then(parse_level) else {
        return;
    };
    let logger = Box::leak(Box::new(StderrLogger { level }));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level.to_level_filter());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse() {
        assert_eq!(parse_level("INFO"), Some(Level::Info));
        assert_eq!(parse_level("warn"), Some(Level::Warn));
        assert_eq!(parse_level("error"), Some(Level::Error));
        assert_eq!(parse_level("debug"), Some(Level::Debug));
        assert_eq!(parse_level("trace"), Some(Level::Trace));
        assert_eq!(parse_level("loud"), None);
        let l = StderrLogger { level: Level::Warn };
        assert!(l.enabled(&Metadata::builder().level(Level::Error).build()));
        assert!(!l.enabled(&Metadata::builder().level(Level::Info).build()));
        l.log(
            &Record::builder()
                .level(Level::Info)
                .args(format_args!("hidden"))
                .build(),
        );
        l.flush();
        init_from_env("LSMKV_TEST_UNSET_VARIABLE");
    }
}
