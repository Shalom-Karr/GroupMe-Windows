// Hide the console window in release; keep it in debug for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    groupme_desktop_lib::run()
}
