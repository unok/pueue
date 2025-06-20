use std::{
    io::{self, Write},
    time::Duration,
};

use chrono::Local;
use pueue_lib::{
    Client, Response, Settings,
    log::{get_log_file_handle, get_log_path, seek_to_last_lines},
    message::{StreamRequest, TaskSelection},
};
use tokio::time::sleep;

use crate::{
    client::{
        commands::{get_state, get_task},
        display_helper::print_error,
        style::OutputStyle,
    },
    internal_prelude::*,
};

/// Wrapper around following logic.
///
/// Log files may be read directly on the local machine, but they may also be streamed via the
/// daemon in case they're somewhere inaccessible or on a remote machine.
pub async fn follow(
    client: &mut Client,
    settings: Settings,
    style: &OutputStyle,
    task_id: Option<usize>,
    lines: Option<usize>,
    timestamps: bool,
) -> Result<()> {
    // If we're supposed to read the log files from the local system, we don't have to
    // do any communication with the daemon.
    // Thereby we handle this in a separate function.
    if settings.client.read_local_logs {
        local_follow(client, settings, task_id, lines, timestamps).await?;
        return Ok(());
    }

    remote_follow(client, style, task_id, lines, timestamps).await
}

/// Request the daemon to stream log files for some tasks.
///
/// This receives log output until the connection goes away or is explicitly closed by the daemon
/// once the task finishes.
pub async fn remote_follow(
    client: &mut Client,
    style: &OutputStyle,
    task_id: Option<usize>,
    lines: Option<usize>,
    timestamps: bool,
) -> Result<()> {
    let task_ids = task_id.map(|id| vec![id]).unwrap_or_default();

    // Request the log stream.
    client
        .send_request(StreamRequest {
            tasks: TaskSelection::TaskIds(task_ids),
            lines,
        })
        .await?;

    // Receive the stream until the connection is closed, breaks or another failure appears.
    loop {
        let response = client.receive_response().await?;
        match response {
            Response::Stream(response) => {
                for (_, text) in response.logs {
                    if timestamps {
                        // Split text into lines and add timestamp to each line
                        for line in text.lines() {
                            let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                            println!("[{}] {}", timestamp, line);
                        }
                        // Handle the case where text doesn't end with a newline
                        if !text.ends_with('\n') && !text.is_empty() {
                            io::stdout().flush().unwrap();
                        }
                    } else {
                        print!("{text}");
                        io::stdout().flush().unwrap();
                    }
                }
                continue;
            }
            Response::Close => break,
            Response::Failure(text) => {
                print_error(style, &text);
                std::process::exit(1);
            }
            _ => error!("Received unhandled response message: {response:?}"),
        }
    }

    Ok(())
}

/// This function reads a log file from the filesystem and streams it to `stdout`.
/// This is the default behavior of `pueue`'s log reading logic, which is only possible
/// if `pueued` runs on the same environment.
///
/// `pueue follow` can be called without a `task_id`, in which case we check whether there's a
/// single running task. If that's the case, we default to it.
/// If there are multiple tasks, the user has to specify which task they want to follow.
pub async fn local_follow(
    client: &mut Client,
    settings: Settings,
    task_id: Option<usize>,
    lines: Option<usize>,
    timestamps: bool,
) -> Result<()> {
    let task_id = match task_id {
        Some(task_id) => task_id,
        None => {
            // The user didn't provide a task id.
            // Check whether we can find a single running task to follow.
            let state = get_state(client).await?;
            let running_ids: Vec<_> = state
                .tasks
                .iter()
                .filter_map(|(&id, t)| if t.is_running() { Some(id) } else { None })
                .collect();

            match running_ids.len() {
                0 => {
                    bail!("There are no running tasks.");
                }
                1 => running_ids[0],
                _ => {
                    let running_ids = running_ids
                        .iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    bail!(
                        "Multiple tasks are running, please select one of the following: {running_ids}",
                    );
                }
            }
        }
    };

    follow_local_task_logs(client, settings, task_id, lines, timestamps).await?;

    Ok(())
}

/// Follow the log output of running task.
///
/// If no task is specified, this will check for the following cases:
///
/// - No running task: Wait until the task starts running.
/// - Single running task: Follow the output of that task.
/// - Multiple running tasks: Print out the list of possible tasks to follow.
pub async fn follow_local_task_logs(
    client: &mut Client,
    settings: Settings,
    task_id: usize,
    lines: Option<usize>,
    timestamps: bool,
) -> Result<()> {
    let pueue_directory = &settings.shared.pueue_directory();
    // It might be that the task is not yet running.
    // Ensure that it exists and is started.
    loop {
        let Some(task) = get_task(client, task_id).await? else {
            eprintln!("Pueue: The task to be followed doesn't exist.");
            std::process::exit(1);
        };
        // Task started up, we can start to follow.
        if task.is_running() || task.is_done() {
            break;
        }
        sleep(Duration::from_millis(1000)).await;
    }

    let mut handle = match get_log_file_handle(task_id, pueue_directory) {
        Ok(stdout) => stdout,
        Err(err) => {
            eprintln!("Failed to get log file handles: {err}");
            return Ok(());
        }
    };
    let path = get_log_path(task_id, pueue_directory);

    // Stdout handle to directly stream log file output to `io::stdout`.
    // This prevents us from allocating any large amounts of memory.
    let mut stdout = io::stdout();

    // If `lines` is passed as an option, we only want to show the last `X` lines.
    // To achieve this, we seek the file handle to the start of the `Xth` line
    // from the end of the file.
    // The loop following this section will then only copy those last lines to stdout.
    if let Some(lines) = lines {
        if let Err(err) = seek_to_last_lines(&mut handle, lines) {
            eprintln!("Error seeking to last lines from log: {err}");
        }
    }

    // The interval at which the task log is checked and streamed to stdout.
    let log_check_interval = 250;

    // We check in regular intervals whether the task finished.
    // This is something we don't want to do in every loop, as we have to communicate with
    // the daemon. That's why we only do it now and then.
    let task_check_interval = log_check_interval * 2;
    let mut last_check = 0;

    // Store incomplete line buffer for timestamps mode
    let mut incomplete_line = String::new();

    loop {
        // Check whether the file still exists. Exit if it doesn't.
        if !path.exists() {
            eprintln!("Pueue: Log file has gone away. Has the task been removed?");
            return Ok(());
        }

        // Read and output the next chunk of text
        if timestamps {
            // Read new data into a buffer
            let mut buffer = Vec::new();
            if let Err(err) = io::copy(&mut handle, &mut buffer) {
                eprintln!("Pueue: Error while reading file: {err}");
                return Ok(());
            }

            if !buffer.is_empty() {
                // Convert to string and combine with any incomplete line from previous iteration
                let new_text = String::from_utf8_lossy(&buffer);
                let full_text = format!("{}{}", incomplete_line, new_text);

                // Split into lines
                let mut lines: Vec<&str> = full_text.lines().collect();

                // Check if the text ends with a newline
                let ends_with_newline = full_text.ends_with('\n');

                // If it doesn't end with newline, the last line is incomplete
                if !ends_with_newline && !lines.is_empty() {
                    incomplete_line = lines.pop().unwrap().to_string();
                } else {
                    incomplete_line.clear();
                }

                // Print complete lines with timestamps
                for line in lines {
                    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                    println!("[{}] {}", timestamp, line);
                }

                if let Err(err) = stdout.flush() {
                    eprintln!("Pueue: Error while flushing stdout: {err}");
                    return Ok(());
                }
            }
        } else {
            // Original behavior - use io::copy
            if let Err(err) = io::copy(&mut handle, &mut stdout) {
                eprintln!("Pueue: Error while reading file: {err}");
                return Ok(());
            }
            // Flush the stdout buffer to actually print the output.
            if let Err(err) = stdout.flush() {
                eprintln!("Pueue: Error while flushing stdout: {err}");
                return Ok(());
            }
        }

        // Check every `task_check_interval` whether the task:
        // 1. Still exist
        // 2. Is still running
        //
        // In case either is not, exit.
        if (last_check % task_check_interval) == 0 {
            let Some(task) = get_task(client, task_id).await? else {
                eprintln!("Pueue: The followed task has been removed.");
                std::process::exit(1);
            };
            // Task exited by itself. We can stop following.
            if !task.is_running() {
                return Ok(());
            }
        }

        last_check += log_check_interval;
        let timeout = Duration::from_millis(log_check_interval);
        sleep(timeout).await;
    }
}
