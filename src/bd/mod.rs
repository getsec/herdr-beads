//! The bd bridge: every call shells out to the `bd` CLI with an argv VECTOR
//! (never a shell string) so task titles/notes/reasons can never be injected.
//!
//! herdr launches plugins with a minimal PATH, so `bd` is resolved against the
//! common Homebrew/usr locations before falling back to PATH.

pub mod types;

use crate::model::Scope;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{channel, Receiver, Sender};
use types::Bead;

fn resolve_bd() -> String {
    for c in [
        "/opt/homebrew/bin/bd",
        "/usr/local/bin/bd",
        "/usr/bin/bd",
        "/home/linuxbrew/.linuxbrew/bin/bd",
    ] {
        if Path::new(c).exists() {
            return c.to_string();
        }
    }
    "bd".to_string()
}

/// Run a bd subcommand, returning stdout. Errors carry bd's stderr.
/// A Command in its own process group, so closing the board's pane mid-write can't hang it up
/// halfway (`bd human respond` comments, then closes).
pub fn command(program: &str) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(program);
    cmd.process_group(0);
    cmd
}

pub fn run(scope: Scope, args: &[&str]) -> Result<String> {
    let mut cmd = command(&resolve_bd());
    // Ensure Homebrew is on PATH even under herdr's minimal launch environment.
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", format!("/opt/homebrew/bin:/usr/local/bin:{path}"));
    }
    if scope == Scope::Global {
        cmd.arg("--global");
        // bd's --global needs shared-server mode; opt in (harmless if unavailable).
        cmd.env("BEADS_DOLT_SHARED_SERVER", "1");
    }
    cmd.args(args);
    let t = std::time::Instant::now();
    let out = cmd
        .output()
        .with_context(|| format!("failed to spawn bd {}", args.join(" ")))?;
    crate::trace(&format!("  bd {}", args.first().unwrap_or(&"")), t.elapsed());
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("bd {}: {}", args.join(" "), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn parse_list(s: &str) -> Result<Vec<Bead>> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(s).context("parsing bd --json output")
}

// ---------------------------------------------------------------- reads

/// The full board: `bd list`, plus closed issues when asked. Each bd call costs
/// ~0.5s on a big repo, so closed comes from one `--all` call rather than a
/// second list. `--all` also returns pinned, which the default list hides, so
/// drop it to keep the board the same as the default list plus closed.
pub fn load(scope: Scope, include_closed: bool) -> Result<Vec<Bead>> {
    if !include_closed {
        return parse_list(&run(scope, &["list", "--json"])?);
    }
    let mut beads = parse_list(&run(scope, &["list", "--all", "--json"])?)?;
    beads.retain(|b| b.status != "pinned");
    Ok(beads)
}

pub fn show(scope: Scope, id: &str) -> Result<Option<Bead>> {
    let v = parse_list(&run(scope, &["show", id, "--json"])?)?;
    Ok(v.into_iter().next())
}

// ---------------------------------------------------------------- writes

pub fn set_status(scope: Scope, id: &str, status: &str) -> Result<()> {
    run(scope, &["update", id, "-s", status]).map(|_| ())
}

pub fn claim(scope: Scope, id: &str) -> Result<()> {
    run(scope, &["update", id, "--claim"]).map(|_| ())
}

pub fn close(scope: Scope, id: &str, reason: &str) -> Result<()> {
    run(scope, &["close", id, "-r", reason]).map(|_| ())
}

pub fn set_priority(scope: Scope, id: &str, priority: u8) -> Result<()> {
    let p = priority.to_string();
    run(scope, &["priority", id, &p]).map(|_| ())
}

pub fn add_note(scope: Scope, id: &str, note: &str) -> Result<()> {
    run(scope, &["note", id, note]).map(|_| ())
}

pub fn add_comment(scope: Scope, id: &str, text: &str) -> Result<()> {
    run(scope, &["comment", id, text]).map(|_| ())
}

/// Answer a human-queue bead: bd adds the text as a comment and closes it.
pub fn human_respond(scope: Scope, id: &str, text: &str) -> Result<()> {
    run(scope, &["human", "respond", id, "-r", text]).map(|_| ())
}

/// The answer text for a verify item; tools/review.sh reads `pass` and `fail: <notes>`.
pub fn answer(pass: bool, notes: &str) -> String {
    if pass {
        "pass".into()
    } else {
        format!("fail: {}", notes.trim())
    }
}

/// The repo's review command (`bd config set custom.herdr-beads.review …`), split into argv;
/// empty when it isn't set.
pub fn review_command(scope: Scope) -> Result<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(&run(
        scope,
        &["config", "get", "custom.herdr-beads.review", "--json"],
    )?)?;
    Ok(v["value"]
        .as_str()
        .unwrap_or("")
        .split_whitespace()
        .map(String::from)
        .collect())
}

pub struct NewBead<'a> {
    pub title: &'a str,
    pub issue_type: &'a str,
    pub priority: u8,
    pub description: &'a str,
    pub assignee: &'a str,
    pub parent: &'a str,
    pub labels: &'a str,
    pub deferred: bool,
}

/// Create a bead from a fully-specified form; returns the new id.
pub fn create(scope: Scope, nb: &NewBead) -> Result<String> {
    let p = nb.priority.to_string();
    // --description is mandatory by convention; seed from title if blank.
    let desc = if nb.description.is_empty() {
        nb.title
    } else {
        nb.description
    };
    let mut args: Vec<&str> = vec![
        "create",
        nb.title,
        "-t",
        nb.issue_type,
        "-p",
        &p,
        "--description",
        desc,
        "--silent",
    ];
    if !nb.assignee.is_empty() {
        args.push("-a");
        args.push(nb.assignee);
    }
    if !nb.parent.is_empty() {
        args.push("--parent");
        args.push(nb.parent);
    }
    if !nb.labels.is_empty() {
        args.push("-l");
        args.push(nb.labels);
    }
    let id = run(scope, &args)?.trim().to_string();
    if nb.deferred && !id.is_empty() {
        let _ = set_status(scope, &id, "deferred"); // best-effort → backlog
    }
    Ok(id)
}

/// Update an existing bead's core fields from an edited form. Optional fields
/// (description/assignee/parent/labels) are only written when non-empty so an
/// untouched field never wipes existing data. Status is left alone unless the
/// backlog toggle is on.
pub fn update_bead(scope: Scope, id: &str, nb: &NewBead) -> Result<()> {
    let p = nb.priority.to_string();
    let mut args: Vec<&str> = vec![
        "update",
        id,
        "--title",
        nb.title,
        "-t",
        nb.issue_type,
        "-p",
        &p,
    ];
    if !nb.description.is_empty() {
        args.push("--description");
        args.push(nb.description);
    }
    if !nb.assignee.is_empty() {
        args.push("-a");
        args.push(nb.assignee);
    }
    if !nb.parent.is_empty() {
        args.push("--parent");
        args.push(nb.parent);
    }
    if !nb.labels.is_empty() {
        args.push("--set-labels");
        args.push(nb.labels);
    }
    run(scope, &args)?;
    if nb.deferred {
        let _ = set_status(scope, id, "deferred");
    }
    Ok(())
}

// ---------------------------------------------------------------- worker

/// A bd call for the background worker. bd costs ~0.5s a call on a big repo,
/// so the UI never waits on one: it queues a job and applies the reply later.
pub enum Job {
    Load { gen: u64, scope: Scope, closed: bool },
    Show { scope: Scope, id: String, seq: u64 },
    /// Read the repo's review command (see `review_command`).
    ReviewCmd { scope: Scope },
    /// A write. Ok carries the status message to show.
    Write(Box<dyn FnOnce() -> Result<String> + Send>),
}

pub enum Reply {
    Loaded { gen: u64, beads: Result<Vec<Bead>> },
    Shown { id: String, bead: Bead, seq: u64 },
    Wrote(Result<String>),
    ReviewCmd(std::result::Result<Vec<String>, String>),
    /// A launch (L) exited; Err carries its last output line.
    Launched(std::result::Result<(), String>),
}

/// One thread runs every job in order, so a write always lands before the
/// reload queued after it. Of the jobs waiting in the queue, only the newest
/// load and the newest show run: the older ones are already stale.
pub fn spawn_worker() -> (Sender<Job>, Sender<Reply>, Receiver<Reply>, std::thread::JoinHandle<()>) {
    let (jobs, queue) = channel::<Job>();
    let (replies, inbox) = channel();
    let launches = replies.clone();
    let worker = std::thread::spawn(move || {
        while let Ok(first) = queue.recv() {
            let mut batch = vec![first];
            batch.extend(queue.try_iter());
            let skip = superseded(&batch);
            for (job, skip) in batch.into_iter().zip(skip) {
                if skip {
                    continue;
                }
                let reply = match job {
                    Job::Load { gen, scope, closed } => Reply::Loaded {
                        gen,
                        beads: load(scope, closed),
                    },
                    Job::Show { scope, id, seq } => match show(scope, &id) {
                        Ok(Some(bead)) => Reply::Shown { id, bead, seq },
                        _ => continue,
                    },
                    Job::ReviewCmd { scope } => {
                        Reply::ReviewCmd(review_command(scope).map_err(|e| e.to_string()))
                    }
                    Job::Write(f) => Reply::Wrote(f()),
                };
                if replies.send(reply).is_err() {
                    return; // the UI is gone
                }
            }
        }
    });
    (jobs, launches, inbox, worker)
}

/// Which jobs in a batch to skip: every load but the last, every show but the
/// last. Writes always run.
fn superseded(batch: &[Job]) -> Vec<bool> {
    let last_load = batch.iter().rposition(|j| matches!(j, Job::Load { .. }));
    let last_show = batch.iter().rposition(|j| matches!(j, Job::Show { .. }));
    (0..batch.len())
        .map(|i| match batch[i] {
            Job::Load { .. } => Some(i) != last_load,
            Job::Show { .. } => Some(i) != last_show,
            Job::ReviewCmd { .. } | Job::Write(_) => false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_skips_stale_loads_and_shows_but_never_writes() {
        let load = |gen| Job::Load { gen, scope: Scope::Repo, closed: false };
        let show = |id: &str| Job::Show { scope: Scope::Repo, id: id.into(), seq: 0 };
        let write = || Job::Write(Box::new(|| Ok(String::new())));
        let batch = vec![load(1), show("a"), write(), load(2), show("b"), write(), show("c")];
        assert_eq!(
            superseded(&batch),
            [true, true, false, false, true, false, false],
            "the last load still follows the first write, so it sees it"
        );
    }

    #[test]
    fn parses_list_fixture() {
        let s = include_str!("../../tests/fixtures/list.json");
        let beads = parse_list(s).expect("list.json parses");
        assert!(!beads.is_empty(), "fixture should have beads");
        let b = &beads[0];
        assert!(!b.id.is_empty());
        assert!(!b.status.is_empty());
        assert!(b.priority <= 4);
    }

    #[test]
    fn parses_show_fixture_with_dependencies() {
        let s = include_str!("../../tests/fixtures/show.json");
        let beads = parse_list(s).expect("show.json parses");
        assert_eq!(beads.len().min(1), 1);
        for d in &beads[0].dependencies {
            // In `show`, deps are expanded issues (id/title); either identifies them.
            assert!(d.other_id().is_some() || d.title.is_some());
        }
    }

    #[test]
    fn empty_and_whitespace_parse_to_empty() {
        assert!(parse_list("").unwrap().is_empty());
        assert!(parse_list("   \n  ").unwrap().is_empty());
    }

    #[test]
    fn human_queue_is_open_beads_labelled_human() {
        let s = r#"[
            {"id":"a","status":"open","labels":["human","keyboard"]},
            {"id":"b","status":"closed","labels":["human"]},
            {"id":"c","status":"open","labels":["humane"]},
            {"id":"d","status":"open"}
        ]"#;
        let waiting: Vec<_> = parse_list(s).unwrap().into_iter().filter(|b| b.needs_human()).map(|b| b.id).collect();
        assert_eq!(waiting, ["a"]);
    }

    #[test]
    fn verify_items_are_open_human_beads_labelled_verify() {
        let s = r#"[
            {"id":"v","status":"open","labels":["human","verify"]},
            {"id":"q","status":"open","labels":["human"]},
            {"id":"done","status":"closed","labels":["human","verify"]}
        ]"#;
        let v: Vec<_> = parse_list(s).unwrap().into_iter().filter(|b| b.is_verify()).map(|b| b.id).collect();
        assert_eq!(v, ["v"]);
    }

    #[test]
    fn answers_match_what_review_sh_reads() {
        assert_eq!(answer(true, "ignored"), "pass");
        assert_eq!(answer(false, "  too quiet "), "fail: too quiet");
    }

    #[test]
    fn tolerates_unknown_fields() {
        let s = r#"[{"id":"x-1","title":"t","status":"open","priority":2,"issue_type":"task","surprise_field":123}]"#;
        let beads = parse_list(s).unwrap();
        assert_eq!(beads[0].id, "x-1");
        assert_eq!(beads[0].assignee(), "-");
    }
}
