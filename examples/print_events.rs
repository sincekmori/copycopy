//! Minimal usage: print every capture to stdout.
//!
//! Run with `cargo run --example print_events`, then copy something and press
//! Ctrl+C+C (Windows) / Cmd+C+C (macOS). A plain single copy keeps working.
//! Quit with Ctrl+C in this terminal.

use copycopy::{block_forever, start, Captured, Config};

fn main() {
    println!("copycopy: copy something, then press Ctrl/Cmd + C + C. (Ctrl+C here to quit.)");

    let _capture = start(Config::default(), |event| {
        let summary = match &event.content {
            Captured::Text { text } => format!("text ({} chars)", text.chars().count()),
            Captured::Image { width, height, png } => {
                format!("image {width}x{height} ({} PNG bytes)", png.len())
            }
            Captured::RichText { format, plain, .. } => {
                format!(
                    "rich_text {format:?} ({} plain chars)",
                    plain.chars().count()
                )
            }
            Captured::Files { paths } => format!("files [{}]", paths.join(", ")),
            Captured::Empty => "empty".to_string(),
        };
        println!(
            "[{}] {} / {} :: {}",
            event.timestamp_ms, event.app_name, event.window_title, summary
        );
    })
    .expect("failed to start capture");

    block_forever();
}
