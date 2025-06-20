use comfy_table::{Attribute as ComfyAttribute, Cell, CellAlignment, Table};
use crossterm::style::Color;
use pueue_lib::{
    Client,
    message::{TaskLogResponse, TaskSelection, *},
    settings::Settings,
    task::{Task, TaskResult, TaskStatus},
};

use super::{OutputStyle, handle_response, selection_from_params};
use crate::internal_prelude::*;

mod json;
mod local;
mod remote;

use json::*;
use local::*;
use remote::*;

/// Print the log output of finished tasks.
/// This may be selected tasks, all tasks of a group or **all** tasks.
#[allow(clippy::too_many_arguments)]
pub async fn print_logs(
    client: &mut Client,
    settings: Settings,
    style: &OutputStyle,
    task_ids: Vec<usize>,
    group: Option<String>,
    all: bool,
    json: bool,
    lines: Option<usize>,
    full: bool,
    timestamps: bool,
) -> Result<()> {
    let lines = determine_log_line_amount(full, &lines);
    let selection = selection_from_params(all, group.clone(), task_ids.clone());

    client
        .send_request(LogRequest {
            tasks: selection.clone(),
            send_logs: !settings.client.read_local_logs,
            lines,
        })
        .await?;

    let response = client.receive_response().await?;

    let Response::Log(task_logs) = response else {
        handle_response(style, response)?;
        return Ok(());
    };

    // Return the server response in json representation.
    if json {
        print_log_json(task_logs, &settings, lines, timestamps);
        return Ok(());
    }

    if task_logs.is_empty() {
        match selection {
            TaskSelection::TaskIds(_) => {
                eprintln!("There are no finished tasks for your specified ids");
                return Ok(());
            }
            TaskSelection::Group(group) => {
                eprintln!("There are no finished tasks for group '{group}'");
                return Ok(());
            }
            TaskSelection::All => {
                eprintln!("There are no finished tasks");
                return Ok(());
            }
        }
    }

    // Iterate over each task and print the respective log.
    let mut task_iter = task_logs.iter().peekable();
    while let Some((_, task_log)) = task_iter.next() {
        print_log(task_log, style, &settings, lines, timestamps);

        // Add a newline if there is another task that's going to be printed.
        if let Some((_, task_log)) = task_iter.peek() {
            if matches!(
                &task_log.task.status,
                TaskStatus::Done { .. } | TaskStatus::Running { .. } | TaskStatus::Paused { .. }
            ) {
                println!();
            }
        }
    }

    Ok(())
}

/// Determine how many lines of output should be printed/returned.
/// `None` implicates that all lines are printed.
///
/// By default, everything is returned for single tasks and only some lines for multiple.
/// `json` is an exception to this, in json mode we always only return some lines
/// (unless otherwise explicitly requested).
///
/// `full` always forces the full log output
/// `lines` force a specific amount of lines
fn determine_log_line_amount(full: bool, lines: &Option<usize>) -> Option<usize> {
    if full {
        None
    } else if let Some(lines) = lines {
        Some(*lines)
    } else {
        // By default, only some lines are shown per task
        Some(15)
    }
}

/// Print the log of a single task.
///
/// message: The message returned by the daemon. This message includes all
///          requested tasks and the tasks' logs, if we don't read local logs.
/// lines: Whether we should reduce the log output of each task to a specific number of lines.
///         `None` implicates that everything should be printed.
///         This is only important, if we read local lines.
fn print_log(
    message: &TaskLogResponse,
    style: &OutputStyle,
    settings: &Settings,
    lines: Option<usize>,
    timestamps: bool,
) {
    let task = &message.task;
    // We only show logs of finished or running tasks.
    if !matches!(
        task.status,
        TaskStatus::Done { .. } | TaskStatus::Running { .. } | TaskStatus::Paused { .. }
    ) {
        return;
    }

    print_task_info(task, style);

    if settings.client.read_local_logs {
        print_local_log(message.task.id, style, settings, lines, timestamps);
    } else if message.output.is_some() {
        print_remote_log(message, style, lines, timestamps);
    } else {
        println!("Logs requested from pueue daemon, but none received. Please report this bug.");
    }
}

/// Print some information about a task, which is displayed on top of the task's log output.
fn print_task_info(task: &Task, style: &OutputStyle) {
    // Print task id and exit code.
    let task_cell = style.styled_cell(
        format!("Task {}: ", task.id),
        None,
        Some(ComfyAttribute::Bold),
    );

    let (exit_status, color) = match &task.status {
        TaskStatus::Paused { .. } => ("paused".into(), Color::White),
        TaskStatus::Running { .. } => ("running".into(), Color::Yellow),
        TaskStatus::Done { result, .. } => match result {
            TaskResult::Success => ("completed successfully".into(), Color::Green),
            TaskResult::Failed(exit_code) => {
                (format!("failed with exit code {exit_code}"), Color::Red)
            }
            TaskResult::FailedToSpawn(_err) => ("Failed to spawn".to_string(), Color::Red),
            TaskResult::Killed => ("killed by system or user".into(), Color::Red),
            TaskResult::Errored => ("some IO error.\n Check daemon log.".into(), Color::Red),
            TaskResult::DependencyFailed => ("dependency failed".into(), Color::Red),
        },
        _ => (task.status.to_string(), Color::White),
    };
    let status_cell = style.styled_cell(exit_status, Some(color), None);

    // The styling of the task number and status is done by a single-row table.
    let mut table = Table::new();
    table.load_preset("││─ └──┘     ─ ┌┐  ");
    table.set_content_arrangement(comfy_table::ContentArrangement::Dynamic);
    table.set_header(vec![task_cell, status_cell]);

    // Explicitly force styling, in case we aren't on a tty, but `--color=always` is set.
    if style.enabled {
        table.enforce_styling();
    }
    eprintln!("{table}");

    // All other information is aligned and styled by using a separate table.
    let mut table = Table::new();
    table.load_preset(comfy_table::presets::NOTHING);
    table.set_content_arrangement(comfy_table::ContentArrangement::Dynamic);

    // Command and path
    table.add_row(vec![
        style.styled_cell("Command:", None, Some(ComfyAttribute::Bold)),
        Cell::new(&task.command),
    ]);
    table.add_row(vec![
        style.styled_cell("Path:", None, Some(ComfyAttribute::Bold)),
        Cell::new(task.path.to_string_lossy()),
    ]);
    if let Some(label) = &task.label {
        table.add_row(vec![
            style.styled_cell("Label:", None, Some(ComfyAttribute::Bold)),
            Cell::new(label),
        ]);
    }

    let (start, end) = task.start_and_end();

    // Start and end time
    if let Some(start) = start {
        table.add_row(vec![
            style.styled_cell("Start:", None, Some(ComfyAttribute::Bold)),
            Cell::new(start.to_rfc2822()),
        ]);
    }
    if let Some(end) = end {
        table.add_row(vec![
            style.styled_cell("End:", None, Some(ComfyAttribute::Bold)),
            Cell::new(end.to_rfc2822()),
        ]);
    }

    // Set the padding of the left column to 0 align the keys to the right
    let first_column = table.column_mut(0).unwrap();
    first_column.set_cell_alignment(CellAlignment::Right);
    first_column.set_padding((0, 0));

    eprintln!("{table}");
}
