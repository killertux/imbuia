use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use std::io::stdout;
use std::path::PathBuf;

mod app;
mod client;
mod commands;
mod config;
mod git;
mod github;
mod input;
mod ipc;
mod layout;
mod reducer;
mod render;
mod runtime;
mod session;
mod supervisor;
mod theme;

mod updater;

fn main() -> Result<()> {
    // Tiny side flag used by `updater::install_update` to check the
    // freshly-installed binary's IPC protocol version. Must short-circuit
    // before any TUI / tokio setup.
    if std::env::args().any(|a| a == "--print-protocol-version") {
        println!("{}", ipc::PROTOCOL_VERSION);
        return Ok(());
    }
    if std::env::args().any(|a| a == "--supervisor") {
        // Supervisor: no TUI, no raw mode, no tokio runtime — just a sync
        // accept loop on the Unix socket.
        return supervisor::run();
    }
    run_client()
}

#[tokio::main(flavor = "multi_thread")]
async fn run_client() -> Result<()> {
    init_tracing();
    install_panic_hook();
    enable_raw_mode()?;
    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;

    let result = runtime::run().await;

    let _ = execute!(
        stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    result
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(
            stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        original(info);
    }));
}

fn init_tracing() {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    let log_dir = cache.join("imbuia");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("imbuia.log");
    let Ok(file) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)
    else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .with_target(false)
        .try_init();
}
