//! `thunder dl` — command-line download client.
//!
//! Talks to the running thunder frontend (default `http://127.0.0.1:5055`)
//! over the reverse-engineered `drive/v1` REST API. See [`crate::client`].

use crate::client::{Resolved, ThunderClient};
use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand};
use serde_json::Value;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

#[derive(Args, Clone)]
pub struct DlConfig {
    /// Thunder server address
    #[clap(long, env = "THUNDER_HOST", default_value = "http://127.0.0.1:5055", global = true)]
    host: String,
    /// Panel password (only if the server was started with --auth-password)
    #[clap(long, env = "THUNDER_AUTH_PASS", global = true)]
    password: Option<String>,
    /// Output raw JSON instead of formatted tables
    #[clap(long, global = true)]
    json: bool,
    #[clap(subcommand)]
    command: DlCommand,
}

#[derive(Subcommand, Clone)]
pub enum DlCommand {
    /// Add a download from a magnet link, http(s) URL, or .torrent file
    Add(AddArgs),
    /// Resolve a link and print its file list without downloading
    Resolve { url: String },
    /// List download tasks
    List(ListArgs),
    /// Pause one or more tasks
    Pause { ids: Vec<String> },
    /// Resume one or more tasks
    Resume { ids: Vec<String> },
    /// Remove one or more tasks
    Rm(RmArgs),
    /// Escape hatch: send a raw request to any API path and print the response
    Raw(RawArgs),
}

#[derive(Args, Clone)]
pub struct AddArgs {
    /// Magnet link, http(s) URL, or path to a .torrent file
    source: String,
    /// Task/folder name (defaults to the resolved name)
    #[clap(short, long)]
    name: Option<String>,
    /// Target cloud folder id ("" = default root)
    #[clap(short = 'p', long, default_value = "")]
    parent_folder_id: String,
    /// Download all files without prompting
    #[clap(long)]
    all: bool,
    /// Comma-separated file indices to download (e.g. 0,2,5)
    #[clap(long, value_delimiter = ',')]
    pick: Vec<i64>,
}

#[derive(Args, Clone)]
pub struct ListArgs {
    /// Only show active (pending/running) tasks
    #[clap(short, long)]
    active: bool,
    /// Max tasks to fetch
    #[clap(short, long, default_value = "100")]
    limit: u32,
}

#[derive(Args, Clone)]
pub struct RmArgs {
    ids: Vec<String>,
    /// Also delete the downloaded files from disk
    #[clap(long)]
    delete_files: bool,
}

#[derive(Args, Clone)]
pub struct RawArgs {
    /// HTTP method (GET, POST, PATCH, DELETE)
    method: String,
    /// API path relative to the CGI prefix, e.g. drive/v1/tasks?space=user%23download
    path: String,
    /// JSON request body
    #[clap(short, long)]
    body: Option<String>,
}

pub fn run(cfg: DlConfig) -> Result<()> {
    let client = ThunderClient::new(&cfg.host, cfg.password.as_deref())
        .context("failed to create client")?;

    match cfg.command.clone() {
        DlCommand::Add(a) => cmd_add(&client, &cfg, a),
        DlCommand::Resolve { url } => cmd_resolve(&client, &cfg, &url),
        DlCommand::List(a) => cmd_list(&client, &cfg, a),
        DlCommand::Pause { ids } => cmd_state(&client, &ids, true),
        DlCommand::Resume { ids } => cmd_state(&client, &ids, false),
        DlCommand::Rm(a) => cmd_rm(&client, a),
        DlCommand::Raw(a) => cmd_raw(&client, &cfg, a),
    }
}

/// A source is a magnet/URL passed straight through, or a .torrent file whose
/// contents we... currently cannot upload (needs a live-confirmed upload
/// route). For now, magnets and URLs work directly.
fn source_to_url(source: &str) -> Result<String> {
    if source.starts_with("magnet:") || source.starts_with("http://") || source.starts_with("https://")
        || source.starts_with("ed2k://") || source.starts_with("ftp://")
    {
        return Ok(source.to_string());
    }
    let p = PathBuf::from(source);
    if p.exists() && p.extension().map(|e| e == "torrent").unwrap_or(false) {
        return Err(anyhow!(
            "uploading .torrent files is not yet wired up (needs a confirmed upload route).\n\
             Workaround: use the magnet link instead, or drop the .torrent into the panel once to confirm the route."
        ));
    }
    Err(anyhow!("unrecognized source: {source} (expected magnet:, http(s)://, or a .torrent path)"))
}

fn cmd_add(client: &ThunderClient, cfg: &DlConfig, a: AddArgs) -> Result<()> {
    let url = source_to_url(&a.source)?;
    let resolved = client.resolve(&url).context("failed to resolve link")?;

    if cfg.json {
        print_resolved_json(&resolved);
    } else {
        print_resolved_table(&resolved);
    }

    // Decide which file indices to download.
    let indices: Vec<i64> = if a.all || resolved.files.len() <= 1 {
        Vec::new() // empty = all
    } else if !a.pick.is_empty() {
        a.pick.clone()
    } else if std::io::stdin().is_terminal() {
        prompt_pick(&resolved)?
    } else {
        // Non-interactive and no selection given: default to all.
        eprintln!("(no --pick/--all and not a TTY: downloading all files)");
        Vec::new()
    };

    let name = a.name.as_deref();

    let id = client
        .add(&url, &resolved, name, &a.parent_folder_id, &indices)
        .context("failed to create task")?;

    if id.is_empty() {
        println!("Task created.");
    } else {
        println!("Task created: {id}");
    }
    Ok(())
}

fn cmd_resolve(client: &ThunderClient, cfg: &DlConfig, source: &str) -> Result<()> {
    let url = source_to_url(source)?;
    let resolved = client.resolve(&url)?;
    if cfg.json {
        print_resolved_json(&resolved);
    } else {
        print_resolved_table(&resolved);
    }
    Ok(())
}

fn cmd_list(client: &ThunderClient, cfg: &DlConfig, a: ListArgs) -> Result<()> {
    let tasks = client.list(a.active, a.limit)?;
    if cfg.json {
        let arr: Vec<Value> = tasks
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id, "name": t.name, "size": t.file_size,
                    "progress": t.progress, "phase": t.phase_label(),
                    "speed": t.speed, "message": t.message,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&Value::Array(arr))?);
        return Ok(());
    }

    if tasks.is_empty() {
        println!("No tasks.");
        return Ok(());
    }
    println!(
        "{:<20} {:>4}%  {:<9} {:>10}  NAME",
        "ID", "PROG", "PHASE", "SPEED"
    );
    for t in &tasks {
        let id_short = if t.id.len() > 20 { &t.id[..20] } else { &t.id };
        println!(
            "{:<20} {:>4}  {:<9} {:>10}  {}",
            id_short,
            t.progress,
            t.phase_label(),
            human_size(t.speed).map(|s| format!("{s}/s")).unwrap_or_else(|| "-".into()),
            t.name
        );
        if !t.message.is_empty() {
            println!("  └ {}", t.message);
        }
    }
    Ok(())
}

fn cmd_state(client: &ThunderClient, ids: &[String], pause: bool) -> Result<()> {
    if ids.is_empty() {
        return Err(anyhow!("no task ids given"));
    }
    for id in ids {
        let r = if pause { client.pause(id) } else { client.resume(id) };
        match r {
            Ok(()) => println!("{}: {}", if pause { "paused" } else { "resumed" }, id),
            Err(e) => eprintln!("failed on {id}: {e}"),
        }
    }
    Ok(())
}

fn cmd_rm(client: &ThunderClient, a: RmArgs) -> Result<()> {
    if a.ids.is_empty() {
        return Err(anyhow!("no task ids given"));
    }
    client.remove(&a.ids, a.delete_files)?;
    println!("Removed {} task(s).", a.ids.len());
    Ok(())
}

fn cmd_raw(client: &ThunderClient, cfg: &DlConfig, a: RawArgs) -> Result<()> {
    let body: Option<Value> = match a.body {
        Some(ref b) => Some(serde_json::from_str(b).context("--body is not valid JSON")?),
        None => None,
    };
    let v = client.request(&a.method.to_uppercase(), &a.path, body.as_ref())?;
    if cfg.json {
        println!("{}", serde_json::to_string(&v)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&v)?);
    }
    Ok(())
}

// ---- output helpers -----------------------------------------------------

fn print_resolved_table(r: &Resolved) {
    println!("Resolved: {}", if r.name.is_empty() { "(unnamed)" } else { &r.name });
    if let Some(s) = human_size(r.total_size) {
        println!("Total: {s}  ({} file(s))", r.files.len());
    }
    if r.files.len() > 1 {
        println!("{:>4}  {:>10}  NAME", "IDX", "SIZE");
        for f in &r.files {
            println!(
                "{:>4}  {:>10}  {}",
                f.index,
                human_size(f.size).unwrap_or_else(|| "-".into()),
                f.name
            );
        }
    }
}

fn print_resolved_json(r: &Resolved) {
    let files: Vec<Value> = r
        .files
        .iter()
        .map(|f| serde_json::json!({"index": f.index, "name": f.name, "size": f.size, "is_dir": f.is_dir}))
        .collect();
    let v = serde_json::json!({
        "list_id": r.list_id, "name": r.name, "total_size": r.total_size, "files": files,
    });
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}

/// Interactive multi-select prompt for which files to download.
fn prompt_pick(r: &Resolved) -> Result<Vec<i64>> {
    print!(
        "Select files to download [Enter=all {} file(s), e.g. 0,2,5]: ",
        r.files.len()
    );
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim();
    if line.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for part in line.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        out.push(
            part.parse::<i64>()
                .with_context(|| format!("invalid index: {part}"))?,
        );
    }
    Ok(out)
}

/// Format bytes as a human-readable size; None for non-positive.
fn human_size(bytes: i64) -> Option<String> {
    if bytes <= 0 {
        return None;
    }
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    Some(if unit == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    })
}
