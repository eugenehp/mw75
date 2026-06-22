//! Centralised logging setup with optional, redirectable output.
//!
//! Every binary calls [`init`] (or [`init_with`]) instead of configuring
//! `env_logger` directly, so logging behaves consistently and can be changed
//! at runtime — without recompiling — via environment variables:
//!
//! * `MW75_LOG` / `RUST_LOG` — level filter (e.g. `info`, `mw75=debug`,
//!   `off`). `MW75_LOG` takes precedence; if neither is set the caller's
//!   `default_level` is used. Set it to `off` to silence all logging.
//! * `MW75_LOG_FILE` — path to append logs to. When set it overrides the
//!   caller's [`LogTarget`], so any binary can be redirected to a file with
//!   `MW75_LOG_FILE=/tmp/mw75.log`.
//!
//! For programmatic control (e.g. a socket or in-memory buffer), pass a
//! [`LogTarget::Pipe`] to [`init_with`].

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use env_logger::{Builder, Target};

/// Destination for log output.
pub enum LogTarget {
    /// Standard error — the default for most binaries.
    Stderr,
    /// Standard output.
    Stdout,
    /// Append to a file at the given path.
    File(PathBuf),
    /// Any custom writer: a pipe, socket, in-memory buffer, etc.
    Pipe(Box<dyn Write + Send + 'static>),
}

/// Initialise logging to stderr (or `MW75_LOG_FILE`, if set).
///
/// `default_level` is the filter used when neither `MW75_LOG` nor `RUST_LOG`
/// is set, e.g. `"info"` or `"warn"`. Safe to call once at startup.
pub fn init(default_level: &str) {
    init_with(default_level, LogTarget::Stderr);
}

/// Initialise logging, writing to `target` unless `MW75_LOG_FILE` overrides it.
///
/// Level resolution: `MW75_LOG`, then `RUST_LOG`, then `default_level`.
/// Calling this more than once is harmless — later calls are ignored.
pub fn init_with(default_level: &str, target: LogTarget) {
    let level = std::env::var("MW75_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| default_level.to_string());

    // MW75_LOG_FILE redirects any binary's output to a file at runtime.
    let target = match std::env::var("MW75_LOG_FILE") {
        Ok(path) if !path.is_empty() => LogTarget::File(path.into()),
        _ => target,
    };

    let mut builder = Builder::new();
    builder.parse_filters(&level);

    match target {
        LogTarget::Stderr => {
            builder.target(Target::Stderr);
        }
        LogTarget::Stdout => {
            builder.target(Target::Stdout);
        }
        LogTarget::File(path) => match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                builder.target(Target::Pipe(Box::new(file)));
            }
            Err(e) => {
                eprintln!("mw75: cannot open log file {path:?}: {e} — logging to stderr");
                builder.target(Target::Stderr);
            }
        },
        LogTarget::Pipe(writer) => {
            builder.target(Target::Pipe(writer));
        }
    }

    // try_init avoids panicking if logging was already initialised.
    let _ = builder.try_init();
}
