use std::io::{self, BufRead, BufReader, Write};

use chrono::Local;
use crossterm::style::{Attribute, Color};
use pueue_lib::message::TaskLogResponse;
use snap::read::FrameDecoder;

use super::OutputStyle;
use crate::internal_prelude::*;

/// Prints log output received from the daemon.
pub fn print_remote_log(
    task_log: &TaskLogResponse,
    style: &OutputStyle,
    lines: Option<usize>,
    timestamps: bool,
) {
    if let Some(bytes) = task_log.output.as_ref() {
        if !bytes.is_empty() {
            // Add a hint if we should limit the output to X lines **and** there are actually more
            // lines than that given limit.
            let mut line_info = String::new();
            if !task_log.output_complete {
                line_info = lines.map_or(String::new(), |lines| format!(" (last {lines} lines)"));
            }

            // Print a newline between the task information and the first output.
            let header = style.style_text("output:", Some(Color::Green), Some(Attribute::Bold));
            println!("\n{header}{line_info}");

            if let Err(err) = decompress_and_print_remote_log(bytes, timestamps) {
                eprintln!("Error while parsing stdout: {err}");
            }
        }
    }
}

/// We cannot easily stream log output from the client to the daemon (yet).
/// Right now, the output is compressed in the daemon and sent as a single payload to the
/// client. In here, we take that payload, decompress it and stream it it directly to stdout.
fn decompress_and_print_remote_log(bytes: &[u8], timestamps: bool) -> Result<()> {
    let mut decompressor = FrameDecoder::new(bytes);

    if timestamps {
        let reader = BufReader::new(decompressor);
        let stdout = io::stdout();
        let mut write = stdout.lock();

        for line_result in reader.lines() {
            match line_result {
                Ok(line) => {
                    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                    writeln!(write, "[{}] {}", timestamp, line)?;
                }
                Err(err) => {
                    eprintln!("Failed reading line from decompressed log: {err}");
                    break;
                }
            }
        }
    } else {
        let stdout = io::stdout();
        let mut write = stdout.lock();
        io::copy(&mut decompressor, &mut write)?;
    }

    Ok(())
}
