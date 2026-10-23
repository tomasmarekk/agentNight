#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod classify;
mod config;
mod detect;
mod logs;
mod monitor;
mod net;
mod policy;
mod power;
mod procs;

fn main() {
    if std::env::args().any(|a| a == "--scan") {
        monitor::print_scan();
        return;
    }
    app::run();
}
