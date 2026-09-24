//! herdr-beads - a beads (bd) board for herdr: List / Table / Kanban over your
//! bd issues, docked as a side panel or floating as a popup.

mod app;
mod bd;
mod form;
mod input;
mod keys;
mod model;
mod selftest;
mod ui;
mod views;

use crate::app::App;
use crate::model::{Mode, Scope};
use anyhow::Result;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
};
use ratatui::Terminal;
use std::io;
use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Opt-in latency log. `touch "$(herdr plugin config-dir herdr-beads)/trace"`
/// turns it on for panes opened afterwards; lines land in `latency.log` beside
/// the marker. With no marker this is one cached lookup and a return.
pub fn trace(what: &str, took: Duration) {
    static LOG: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let Some(path) = LOG.get_or_init(|| {
        let dir = std::path::PathBuf::from(std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")?);
        dir.join("trace").exists().then(|| dir.join("latency.log"))
    }) else {
        return;
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let line = format!(
        "{}.{:03} pid={} {:>8.1}ms {what}\n",
        now.as_secs(),
        now.subsec_millis(),
        std::process::id(),
        took.as_secs_f64() * 1000.0
    );
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

fn parse_args() -> (Mode, Scope, bool) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut mode = Mode::Popup;
    let mut scope = Scope::Repo;
    let mut selftest = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mode" => {
                if let Some(v) = args.get(i + 1) {
                    mode = if v == "dock" { Mode::Dock } else { Mode::Popup };
                    i += 1;
                }
            }
            "--dock" => mode = Mode::Dock,
            "--popup" | "--board" => mode = Mode::Popup,
            "--global" => scope = Scope::Global,
            "--selftest" => selftest = true,
            _ => {}
        }
        i += 1;
    }
    (mode, scope, selftest)
}

/// Resolve the repo the board should scope to. herdr runs the pane in the
/// plugin dir (no `.beads`), but injects HERDR_SOCKET_PATH / HERDR_WORKSPACE_ID
/// / HERDR_PANE_ID / HERDR_BIN_PATH - so we ask `herdr pane list` for the
/// focused pane's cwd in our workspace (same trick herdr-flist uses).
fn resolve_repo_cwd() -> Option<String> {
    let me = std::env::var("HERDR_PANE_ID").unwrap_or_default();
    let ws = std::env::var("HERDR_WORKSPACE_ID").unwrap_or_default();
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
    let out = std::process::Command::new(&bin)
        .arg("pane")
        .arg("list")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let panes = v.get("result")?.get("panes")?.as_array()?;

    let others: Vec<&serde_json::Value> = panes
        .iter()
        .filter(|p| p.get("pane_id").and_then(|x| x.as_str()).unwrap_or("") != me)
        .collect();
    let cwd = |p: &serde_json::Value| p.get("cwd").and_then(|x| x.as_str()).map(String::from);
    let in_ws = |p: &serde_json::Value| {
        !ws.is_empty() && p.get("workspace_id").and_then(|x| x.as_str()) == Some(ws.as_str())
    };
    let focused = |p: &serde_json::Value| p.get("focused").and_then(|x| x.as_bool()) == Some(true);
    let has_beads = |c: &str| std::path::Path::new(c).join(".beads").exists();

    // Preference order: a pane with a real .beads dir wins (focused first, my
    // workspace next), then any focused pane, then anything.
    let mut candidates: Vec<String> = Vec::new();
    for p in &others {
        if let Some(c) = cwd(p) {
            let score = (has_beads(&c) as u8) * 4 + (focused(p) as u8) * 2 + (in_ws(p) as u8);
            candidates.push(format!("{score:02}\u{1}{c}"));
        }
    }
    candidates.sort();
    candidates
        .last()
        .and_then(|s| s.split('\u{1}').nth(1))
        .map(String::from)
}

fn apply_working_dir() {
    // 1) explicit override from the launcher
    if let Ok(dir) = std::env::var("HERDR_BEADS_CWD") {
        if !dir.is_empty() {
            let _ = std::env::set_current_dir(&dir);
        }
    }
    // 2) if still no .beads here, ask herdr for the workspace's repo
    if !std::path::Path::new(".beads").exists() {
        if let Some(dir) = resolve_repo_cwd() {
            let _ = std::env::set_current_dir(&dir);
        }
    }
}

fn main() -> Result<()> {
    let started = Instant::now();
    let (mode, scope, selftest) = parse_args();

    apply_working_dir();

    if selftest {
        return selftest::run(scope);
    }

    // Tag the pane so the launcher can find (and toggle) it via `herdr pane list`.
    let pane_title = match mode {
        Mode::Dock => "herdr-beads-dock",
        Mode::Popup => "herdr-beads-board",
    };
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        SetTitle(pane_title)
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;

    let res = run_app(&mut terminal, App::new(mode, scope), started);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    res
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, mut app: App, started: Instant) -> Result<()> {
    // Traced span: key press (or process start) -> the frame that shows its result.
    let mut pending = Some((started, "startup (first frame)".to_string()));
    let mut dirty = true;
    loop {
        // Redraw only when an input or a bd reply changed something. The 50ms
        // poll is how often finished bd calls get picked up while idle.
        dirty |= app.apply_replies();
        if dirty {
            terminal.draw(|f| ui::render(f, &mut app))?;
            dirty = false;
            if let Some((t, what)) = pending.take() {
                trace(&what, t.elapsed());
            }
        }
        if app.should_quit {
            break;
        }
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                pending = Some((Instant::now(), format!("key {:?}", k.code)));
                app.error_shown = false; // seen: the next status message may replace it
                keys::handle_key(&mut app, k)
            }
            Event::Mouse(m) => keys::handle_mouse(&mut app, m),
            _ => {}
        }
        dirty = true;
        if app.should_quit {
            break;
        }
    }
    // Queued writes (a verdict, a close) still have to reach bd before the process exits.
    app.status_msg = "finishing bd writes…".into();
    terminal.draw(|f| ui::render(f, &mut app))?;
    app.finish();
    Ok(())
}
