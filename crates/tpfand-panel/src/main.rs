//! tpfand-panel — tray icon (StatusNotifierItem) + egui control window.
//!
//!   tpfand-panel            run the tray icon (autostart this)
//!   tpfand-panel --window   open the control window directly

mod tray;
mod window;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: tpfand-panel [--window]");
        return;
    }
    if args.iter().any(|a| a == "--window") {
        if let Err(e) = window::run() {
            eprintln!("tpfand-panel: {e}");
            std::process::exit(1);
        }
    } else {
        tray::run();
    }
}
