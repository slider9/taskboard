//! A terminal's line: the tasks waiting their turn in one terminal behind the task it's on. When the
//! owner asks for something separate in a long conversation, the agent splits it off into its own task:
//! queued in this terminal after the current one (`tb task new … --here --next`), or switched to now,
//! the current one waiting here to resume (`--here --now`, `tb switch`). Once the terminal's task is
//! done, fails or parks, the terminal moves on to the next ready task in its line by itself.
//!
//! A task in a line is `queued` with `pickup = 'manual'` and no `session_id`, so nothing else starts
//! it; `line_session` names the terminal and `line_pos` orders the line. One that was started before
//! (`started_at`) waits to resume; one that wasn't is queued here.

use serde_json::{json, Value};

use crate::app::App;
use crate::util::*;
use crate::board::MIDNA;
use crate::{board, deliver, devices, fields, handoff, hours, locks, p, waitsfor};

pub const ADDED: &[(&str, &str, &str)] = &[("tasks", "line_session", "TEXT"), ("tasks", "line_pos", "INT")];
pub const SCHEMA: &str = "CREATE INDEX IF NOT EXISTS tasks_line ON tasks(line_session);";

/// A terminal gone this long gives its line back to the board even while its task might still restart.
const GONE_RELEASE_SECS: f64 = 24.0 * 3600.0;

pub enum Place {
    Front,
    Back,
}

/// Terminal `sid`'s line, first to go first.
pub fn of(app: &App, sid: &str) -> Result<Vec<Row>> {
    app.db.q("SELECT * FROM tasks WHERE line_session = ? AND status = 'queued' ORDER BY line_pos, id", p![sid])
}

/// Whether terminal `sid` is on a task or has one waiting in its line.
pub fn busy(app: &App, sid: &str) -> Result<bool> {
    Ok(board::task_for_session(app, Some(sid))?.is_some() || !of(app, sid)?.is_empty())
}

pub fn kind(t: &Row) -> &'static str {
    if has(t.s("started_at")) {
        "resume"
    } else {
        "queued"
    }
}

fn label(t: &Row, name: &str) -> String {
    match kind(t) {
        "resume" => format!("To resume in {name}"),
        _ => format!("Queued in {name}"),
    }
}

fn name_of(app: &App, sid: &str) -> String {
    board::session_name(app, Some(sid), None)
}

/// The card's `line`: where the task waits its turn, or null.
pub fn card(app: &App, t: &Row) -> Result<Value> {
    let Some(sid) = t.s("line_session").filter(|s| !s.is_empty()) else { return Ok(Value::Null) };
    if t.s("status") != Some("queued") {
        return Ok(Value::Null);
    }
    let line = of(app, sid)?;
    let pos = line.iter().position(|x| x.id() == t.id()).map(|i| i + 1).unwrap_or(0);
    let after = board::task_for_session(app, Some(sid))?.map(|a| json!(rf("task", a.id()))).unwrap_or(Value::Null);
    let name = name_of(app, sid);
    Ok(json!({"session": sid, "name": name, "kind": kind(t), "pos": pos, "label": label(t, &name), "after": after}))
}

/// The line's entries for a session view.
pub fn entries(app: &App, sid: &str) -> Result<Vec<Value>> {
    Ok(of(app, sid)?.iter().map(|t| json!({"ref": rf("task", t.id()), "id": t.id(), "title": t.v("title"), "kind": kind(t)})).collect())
}

/// Puts `t` in terminal `sid`'s line, at its front or back. It waits there; nothing else starts it.
pub fn enqueue(app: &App, t: &Row, sid: &str, place: Place) -> Result<()> {
    let ends = app.db.q1("SELECT MIN(line_pos) AS lo, MAX(line_pos) AS hi FROM tasks WHERE line_session = ? AND status = 'queued' AND id != ?", p![sid, t.id()])?;
    let pos = match (place, ends) {
        (Place::Back, Some(e)) => e.i("hi").map(|h| h + 1).unwrap_or(0),
        (Place::Front, Some(e)) => e.i("lo").map(|l| l - 1).unwrap_or(0),
        (_, None) => 0,
    };
    board::update_task(
        app,
        t.id(),
        fields!["line_session" => sid, "line_pos" => pos, "pickup" => "manual", "pickup_session" => null, "start_job" => null],
    )
}

/// Takes terminal `sid` off `t` and puts it at the front of the line, to resume once the terminal's
/// next task is done.
pub fn shelve(app: &App, t: &Row, sid: &str, who: &str, text: &str) -> Result<()> {
    board::update_task(
        app,
        t.id(),
        fields!["status" => "queued", "session_id" => null, "start_job" => null, "needs_reason" => null,
                "question" => null, "answered_at" => null, "lost" => 0, "latest" => text],
    )?;
    deliver::drop(app, t.id(), "its terminal moved to another task")?;
    enqueue(app, &board::get_task(app, t.id())?, sid, Place::Front)?;
    board::log_event(app, t.id(), who, "status", text)?;
    Ok(())
}

/// Takes `t` out of whatever line it's in.
pub fn leave(app: &App, t: &Row) -> Result<()> {
    if t.s("line_session").is_some() {
        board::update_task(app, t.id(), fields!["line_session" => null, "line_pos" => null])?;
    }
    Ok(())
}

/// Takes `t` out of its line and back to the board, waiting for Start.
pub fn drop(app: &App, t: &Row, who: &str, why: &str) -> Result<()> {
    leave(app, t)?;
    board::update_task(app, t.id(), fields!["latest" => why])?;
    board::log_event(app, t.id(), who, "status", why)?;
    Ok(())
}

/// Whether the clock holds `t` back in its line, the same as any other start: outside the work hours
/// (unless its goal was started now until they open) or with the usage used up. It's delivered
/// once they open or the usage resets; the owner's Start still runs it now.
pub fn held_by_clock(app: &App, t: &Row) -> Result<bool> {
    Ok(!hours::goal_open(app, board::find_goal(app, t.i("goal_id"))?.as_ref()))
}

/// The first task in terminal `sid`'s line that nothing holds back (the clock, what it waits for, a
/// lock, devices).
fn next_ready(app: &App, sid: &str) -> Result<Option<Row>> {
    for t in of(app, sid)? {
        if held_by_clock(app, &t)? {
            continue;
        }
        if waitsfor::blocker(app, &t)?.is_none() && locks::blocker(app, &t)?.is_none() && devices::blocker(app, &t)?.is_none() {
            return Ok(Some(t));
        }
    }
    Ok(None)
}

/// A terminal moved on to a task that needs a worktree slot the board claims for it first. Claiming
/// checks the slot out with git (seconds, more on a big repository), so a move made inside a
/// transaction leaves the claim and the text that brings the agent on to `finish`, once it's over.
pub struct Later {
    sid: String,
    task: i64,
    why: String,
    resuming: bool,
}

/// Where a move left its terminal: on its next task with the text that brings the agent onto it, or
/// on a task whose slot is claimed first.
pub enum Moved {
    Now(Row, String),
    Later(Later),
}

/// Moves terminal `sid`, on no task now, on to the next ready task in its line. The task and the text
/// that brings the agent onto it (`why` first: "T12 is done"), its handoff included. A task on the
/// slot lane holding no slot gets one first, so it never starts in the slot its terminal sits in;
/// inside a transaction that's `Later`.
pub fn advance(app: &App, sid: &str, claude: Option<&str>, why: &str) -> Result<Option<Moved>> {
    if board::task_for_session(app, Some(sid))?.is_some() {
        return Ok(None);
    }
    let Some(s) = board::get_session(app, Some(sid))? else { return Ok(None) };
    if s.s("status") == Some("gone") {
        return Ok(None);
    }
    let Some(next) = next_ready(app, sid)? else { return Ok(None) };
    let resuming = kind(&next) == "resume";
    leave(app, &next)?;
    let claude = claude.or(s.s("claude_session_id"));
    if board::claim(app, &next, sid, claude, None, "Next in line", false)?.is_err() {
        return Ok(None);
    }
    let next = board::get_task(app, next.id())?;
    devices::lend(app, &next)?;
    waitsfor::started(app, &next)?;
    let next = board::get_task(app, next.id())?;
    app.db.x("UPDATE sessions SET ctx_task = ?, ctx_version = ? WHERE id = ?", p![next.id(), next.v("ctx_version"), sid])?;
    let later = Later { sid: sid.to_string(), task: next.id(), why: why.to_string(), resuming };
    if needs_slot(app, &next)? {
        if app.db.held_here() {
            return Ok(Some(Moved::Later(later)));
        }
        claim_slot(app, next.id());
    }
    let text = move_text(app, &later)?;
    Ok(Some(Moved::Now(board::get_task(app, next.id())?, text)))
}

/// Whether `t` works in a worktree slot and holds none, nor a worktree of its own from before the pool.
fn needs_slot(app: &App, t: &Row) -> Result<bool> {
    if t.s("pickup") == Some("attach") || crate::slots::held(app, t)?.is_some() || crate::worktrees::made(app, t)?.is_some() {
        return Ok(false);
    }
    Ok(crate::worktrees::slot_repo(app, t)?.is_some())
}

/// Claims task `id`'s worktree slot. When it can't (the pool is full, git failed), the move's text
/// tells the agent to claim one.
fn claim_slot(app: &App, id: i64) {
    if let Err(e) = board::get_task(app, id).and_then(|t| crate::worktrees::ensure(app, &t)) {
        app.info(format!("lines: {} moved on without a worktree slot: {}", rf("task", id), e.message));
    }
}

/// The text that brings the agent onto the task a move left its terminal on: why, where it works and
/// its handoff.
fn move_text(app: &App, l: &Later) -> Result<String> {
    let s = board::get_session(app, Some(&l.sid))?.unwrap_or_default();
    let next = board::get_task(app, l.task)?;
    let r = rf("task", next.id());
    let how = if l.resuming { "picks up where it left off on" } else { "moves on to" };
    let place = where_to_work(app, &s, &next)?.map(|w| format!("\n\n{w}")).unwrap_or_default();
    Ok(format!(
        "{} {}; this terminal {how} {r} “{}”, next in its line.{place}\n\n{}",
        handoff::marker(next.id()),
        l.why,
        next.st("title"),
        handoff::build(app, next.id())?
    ))
}

/// A `Later` move once its transaction is over: claims the task's slot, then the text that brings the
/// agent onto it. None when the terminal has gone on to another task meanwhile.
pub fn finish(app: &App, l: &Later) -> Result<Option<String>> {
    claim_slot(app, l.task);
    if board::task_for_session(app, Some(&l.sid))?.map(|t| t.id()) != Some(l.task) {
        return Ok(None);
    }
    move_text(app, l).map(Some)
}

/// Where `next` works when its terminal isn't there. A terminal in a worktree slot `next` doesn't hold
/// sits in the slot its last task used, which the board gives to other tasks, so switching its branch
/// there takes another task's worktree from under it. Its own slot, else a slot it claims (slot lane),
/// else the project's shared checkout. None when the terminal is already where it works.
fn where_to_work(app: &App, s: &Row, next: &Row) -> Result<Option<String>> {
    let cwd = s.s("cwd").filter(|c| !c.is_empty()).or(s.s("project_path"));
    let r = rf("task", next.id());
    let Some(slot) = crate::worktrees::root_of(cwd).filter(|p| crate::slots::is_slot(p)) else {
        let Some(own) = crate::slots::held(app, next)?.map(|o| o.st("path")) else { return Ok(None) };
        return Ok(Some(format!("{r} works in its own worktree slot: cd \"{own}\" and work there.")));
    };
    if let Some(own) = crate::slots::held(app, next)? {
        let path = own.st("path");
        if crate::util::path_key(&path) == crate::util::path_key(&slot) {
            return Ok(None);
        }
        return Ok(Some(format!("This terminal is in {slot}, a worktree slot {r} doesn't hold. {r} has its own: cd \"{path}\" and work there.")));
    }
    let left = format!(
        "This terminal is still in {slot}, the worktree slot its last task used. It isn't {r}'s: the board gives it to \
         other tasks, so don't switch its branch or work there."
    );
    let shared = crate::worktrees::repo_of(app, next)?.filter(|p| !crate::slots::is_slot(p) && std::path::Path::new(p).is_dir());
    let tb = board::tb_cmd(app);
    Ok(Some(match (crate::worklane::pick(app, next)?.as_str(), shared) {
        ("slot", _) | (_, None) => format!("{left} {r} works in a slot of its own: run {tb} slot claim, then cd to the folder it prints and work there."),
        (_, Some(repo)) => format!("{left} {r} works in the project's shared checkout: cd \"{repo}\" and work there."),
    }))
}

/// `advance`, its text sent to the terminal (by its next hook, or typed in once it's idle). A task
/// whose slot is claimed first has its text sent once the claim is done, outside the transaction.
pub fn deliver_next(app: &App, sid: &str, why: &str) -> Result<Option<Row>> {
    match advance(app, sid, None, why)? {
        None => Ok(None),
        Some(Moved::Now(next, text)) => {
            deliver::add(app, "line_next", &text, next.id(), None, None)?;
            Ok(Some(next))
        }
        Some(Moved::Later(l)) => {
            let next = board::get_task(app, l.task)?;
            app.defer(Box::new(move |app: &App| {
                let sent = finish(app, &l).and_then(|text| match text {
                    Some(text) => app.db.tx(|| deliver::add(app, "line_next", &text, l.task, None, None).map(|_| ())),
                    None => Ok(()),
                });
                if let Err(e) = sent {
                    app.info(format!("lines: couldn't send {} its next task: {}", rf("task", l.task), e.message));
                }
            }));
            Ok(Some(next))
        }
    }
}

/// One line naming terminal `sid`'s line, or "" when it's empty.
pub fn summary(app: &App, sid: &str) -> Result<String> {
    let line = of(app, sid)?;
    if line.is_empty() {
        return Ok(String::new());
    }
    let items: Vec<String> = line
        .iter()
        .map(|t| format!("{} “{}” {}", rf("task", t.id()), t.st("title"), if kind(t) == "resume" { "to resume" } else { "queued" }))
        .collect();
    Ok(format!("Line in this terminal: {}.", items.join(", ")))
}

/// The note on each of the owner's prompts while the terminal is on `t`: is this ask part of it?
pub fn split_check(app: &App, t: &Row, sid: &str) -> Result<String> {
    let r = rf("task", t.id());
    let mut text = format!(
        "[task-board] You're on {r} “{}”. If this ask is separate work, split it off before you start on it: \
         tb task new \"<title>\" --detail \"<the ask>\" --est <len> --here --next (queue it after {r}), or --here --now \
         (switch to it; {r} waits here to resume).",
        t.st("title")
    );
    let line = of(app, sid)?;
    if !line.is_empty() {
        let refs: Vec<String> = line.iter().map(|x| rf("task", x.id())).collect();
        text.push_str(&format!(" Already in line here: {}.", refs.join(", ")));
    }
    Ok(text)
}

/// When `t` is claimed in terminal `sid` and it was the last task of a terminal that's gone, that
/// terminal's line comes along.
pub fn follow(app: &App, t: &Row, sid: &str) -> Result<()> {
    for s in app.db.q("SELECT id FROM sessions WHERE status = 'gone' AND last_task = ? AND id != ?", p![t.id(), sid])? {
        let from = s.st("id");
        if of(app, &from)?.is_empty() {
            continue;
        }
        let start = app.db.q1("SELECT MAX(line_pos) AS hi FROM tasks WHERE line_session = ?", p![sid])?.and_then(|r| r.i("hi")).map(|h| h + 1).unwrap_or(0);
        for (pos, x) in (start..).zip(of(app, &from)?) {
            board::update_task(app, x.id(), fields!["line_session" => sid, "line_pos" => pos])?;
        }
    }
    Ok(())
}

/// Gives terminal `sid`'s line back to the board: each task waits for Start.
pub fn release(app: &App, sid: &str) -> Result<()> {
    let name = name_of(app, sid);
    for t in of(app, sid)? {
        drop(app, &t, MIDNA, &format!("Its terminal {name} closed at {}; press Start to carry on", local_clock(None)))?;
    }
    Ok(())
}

/// Keeps lines moving: a live terminal on no task moves on once something in its line is ready, and a
/// gone terminal's line goes back to the board unless its task is set to restart and take the line along.
pub fn tick(app: &App) -> Result<()> {
    let sids = app.db.q("SELECT DISTINCT line_session AS sid FROM tasks WHERE line_session IS NOT NULL AND status = 'queued'", p![])?;
    for row in sids {
        let sid = row.st("sid");
        let Some(s) = board::get_session(app, Some(&sid))? else {
            release(app, &sid)?;
            continue;
        };
        if s.s("status") != Some("gone") {
            if board::task_for_session(app, Some(&sid))?.is_none() {
                deliver_next(app, &sid, "Something in this terminal's line is ready")?;
            }
            continue;
        }
        let last = board::find_task(app, s.i("last_task"))?;
        let restarting = last
            .as_ref()
            .map(|t| match t.s("status") {
                Some("needs") => true,
                Some("queued") => t.s("pickup") != Some("manual"),
                Some("working") => true,
                _ => false,
            })
            .unwrap_or(false);
        if !restarting || age_secs(s.s("gone_at")).unwrap_or(0.0) > GONE_RELEASE_SECS {
            release(app, &sid)?;
        }
    }
    Ok(())
}
