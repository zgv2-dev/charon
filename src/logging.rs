//! Tray apps have no console, so logs go to daily files in %APPDATA%\Pinged\<app name>\data\logs.

use tracing_appender::rolling::{Builder, Rotation};

pub fn init(prefix: &str) {
    let dir = crate::platform::log_dir();
    match Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix(prefix)
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
    {
        Ok(file) => tracing_subscriber::fmt()
            .with_writer(file)
            .with_ansi(false)
            .init(),
        Err(e) => eprintln!("logging disabled: {e}"),
    }
}
