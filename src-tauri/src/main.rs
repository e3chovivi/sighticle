// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // 首参是子命令（screenshot/record/help）→ CLI 无头模式，不建任何窗口；
    // 否则照常启动 GUI（托盘 + 快捷键）。
    let args: Vec<String> = std::env::args().skip(1).collect();
    if light_capture_lib::cli::wants_cli(&args) {
        std::process::exit(light_capture_lib::cli::run(&args));
    }
    light_capture_lib::run()
}
