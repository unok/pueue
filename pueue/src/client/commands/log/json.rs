use std::{
    collections::{BTreeMap, HashMap},
    io::Read,
};

use chrono::Local;
use pueue_lib::{
    log::{get_log_file_handle, read_last_lines},
    message::TaskLogResponse,
    settings::Settings,
    task::Task,
};
use serde::{Deserialize, Serialize};
use snap::read::FrameDecoder;

/// This is the output struct used for
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TaskLog {
    pub task: Task,
    pub output: String,
}

/// Print some log output in JSON serialized form.
///
/// If the log isn't read from the disk but rather received from the daemon, we have to
/// convert the received [TaskLogResponse] into a proper JSON serializable format.
/// Output in [TaskLogResponse], is usually compressed, so we need to decompress it first.
pub fn print_log_json(
    task_log_messages: BTreeMap<usize, TaskLogResponse>,
    settings: &Settings,
    lines: Option<usize>,
    timestamps: bool,
) {
    let mut tasks: BTreeMap<usize, Task> = BTreeMap::new();
    let mut task_log: BTreeMap<usize, String> = BTreeMap::new();
    for (id, message) in task_log_messages {
        tasks.insert(id, message.task);

        if settings.client.read_local_logs {
            let output = get_local_log(settings, id, lines, timestamps);
            task_log.insert(id, output);
        } else {
            let output = get_remote_log(message.output, timestamps);
            task_log.insert(id, output);
        }
    }

    // Now assemble the final struct that will be returned
    let mut json = BTreeMap::new();
    for (id, mut task) in tasks {
        let (id, output) = task_log.remove_entry(&id).unwrap();

        task.envs = HashMap::new();
        json.insert(id, TaskLog { task, output });
    }

    println!("{}", serde_json::to_string(&json).unwrap());
}

/// Read logs directly from local files for a specific task.
fn get_local_log(settings: &Settings, id: usize, lines: Option<usize>, timestamps: bool) -> String {
    let mut file = match get_log_file_handle(id, &settings.shared.pueue_directory()) {
        Ok(file) => file,
        Err(err) => {
            return format!("(Pueue error) Failed to get log file handle: {err}");
        }
    };

    // Only return the last few lines.
    if let Some(lines) = lines {
        let content = read_last_lines(&mut file, lines);
        if timestamps {
            return add_timestamps_to_string(&content);
        }
        return content;
    }

    // Read the whole local log output.
    let mut output = String::new();
    if let Err(error) = file.read_to_string(&mut output) {
        return format!("(Pueue error) Failed to read local log output file: {error:?}");
    };

    if timestamps {
        add_timestamps_to_string(&output)
    } else {
        output
    }
}

/// Read logs from from compressed remote logs.
/// If logs don't exist, an empty string will be returned.
fn get_remote_log(output_bytes: Option<Vec<u8>>, timestamps: bool) -> String {
    let Some(bytes) = output_bytes else {
        return String::new();
    };

    let mut decoder = FrameDecoder::new(&bytes[..]);
    let mut output = String::new();
    if let Err(error) = decoder.read_to_string(&mut output) {
        return format!("(Pueue error) Failed to decompress remote log output: {error:?}");
    }

    if timestamps {
        add_timestamps_to_string(&output)
    } else {
        output
    }
}

/// Add timestamps to each line of the given string content.
fn add_timestamps_to_string(content: &str) -> String {
    content
        .lines()
        .map(|line| {
            let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
            format!("[{}] {}", timestamp, line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}
