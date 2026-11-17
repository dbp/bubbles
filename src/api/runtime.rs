//! Vendored from upstream `rust/src/lib.rs`: the tokio `RUNTIME` and
//! `init_logger` that the api subset (`do_first_time_init`, internal spawns)
//! depends on. The Android branch is dropped — this is a desktop client.
//!
//! Note: the crate already has `crate::runtime` (the tokio runtime backing the
//! GTK bridge). This second runtime mirrors upstream so the vendored api code
//! is a faithful copy; you can later unify the two if you like.

use std::path::Path;
use std::sync::LazyLock;

use flexi_logger::{opt_format, Age, Cleanup, Criterion, FileSpec, Logger, Naming, WriteMode};
use log::info;

#[allow(dead_code)]
pub static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    info!("creating runner");
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("tokio-rustpush")
        .enable_all()
        .build()
        .unwrap()
});

/// What the log file records: everything at debug, except two vendored
/// rustpush modules that write message content. `imessage::aps_client` logs
/// every received message with its sender and text at info, and
/// `imessage::messages` logs outgoing message bodies at debug and warn and
/// attachment names at warn. Both keep their more severe lines.
const FILE_LOG_SPEC: &str =
    "debug, rustpush::imessage::aps_client=warn, rustpush::imessage::messages=error";

pub fn init_logger(path: &Path) {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "debug");
    }
    let system = pretty_env_logger::formatted_builder().build();

    let (logger, _) = Logger::try_with_str(FILE_LOG_SPEC)
        .expect("No logger?")
        .log_to_file(
            FileSpec::default()
                .directory(path.join("logs"))
                .suppress_timestamp(),
        )
        .append()
        .format(opt_format)
        .cleanup_in_background_thread(false)
        .rotate(
            Criterion::AgeOrSize(Age::Day, 1024 * 1024 * 10 /* 10 MB */),
            Naming::Numbers,
            Cleanup::KeepLogFiles(1),
        )
        .write_mode(WriteMode::BufferAndFlush)
        .build()
        .unwrap();

    multi_log::MultiLogger::init(vec![Box::new(system), logger], log::Level::Trace)
        .expect("No init?");
}

#[cfg(test)]
mod tests {
    use super::FILE_LOG_SPEC;
    use flexi_logger::LogSpecification;
    use log::Level;

    /// The two rustpush modules that log message content are cut down to
    /// their warn/error lines; the rest of rustpush and bubbles still log at
    /// debug.
    #[test]
    fn file_log_spec_silences_content_logging_rustpush_modules() {
        let spec = LogSpecification::parse(FILE_LOG_SPEC).expect("spec parses");

        let aps_client = "rustpush::imessage::aps_client";
        assert!(!spec.enabled(Level::Info, aps_client));
        assert!(spec.enabled(Level::Warn, aps_client));

        let messages = "rustpush::imessage::messages";
        assert!(!spec.enabled(Level::Debug, messages));
        assert!(!spec.enabled(Level::Warn, messages));
        assert!(spec.enabled(Level::Error, messages));

        assert!(spec.enabled(Level::Debug, "rustpush::aps"));
        assert!(spec.enabled(Level::Debug, "rustpush::imessage::cloud_messages"));
        assert!(spec.enabled(Level::Debug, "bubbles::protocol::rustpush_backend"));
    }
}
