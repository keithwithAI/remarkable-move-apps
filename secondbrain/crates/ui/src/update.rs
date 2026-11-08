//! The pure update step: `(model, event, last hitmap) → effects`. No I/O here — the binary
//! interprets [`Effect`]s (enqueue to the action queue, spawn a fetch, present a frame).

use crate::defer::defer_target;
use crate::model::{AppModel, Dirty, Effect, Flash, Overlay, QuickAdd, UiEvent};
use crate::view::{bottom_bar_rect, hit_test, quickadd_input_region, sheet_region, HitMap, Rect, Target};
use actionqueue::Action;
use chrono::Duration;
use render::ui::KbLayer;
use render::Canvas;

const FLASH_SECS: i64 = 5;

pub fn update(model: &mut AppModel, event: UiEvent, hits: &HitMap, canvas: Canvas) -> Vec<Effect> {
    match event {
        UiEvent::Tap { x, y } => {
            let target = hit_test(hits, x, y).map(|(_, t)| t.clone());
            match target {
                Some(t) => on_tap(model, t, canvas),
                None => Vec::new(),
            }
        }
        UiEvent::DataRefreshed(planner) => {
            model.planner = planner;
            model.fetching = false;
            // A new window can drop the viewed date (e.g. after midnight) — clamp back in.
            let (from, to) = model.planner.window;
            model.view_date = model.view_date.clamp(from, to);
            // Keep the "… N more" scroll inside the refreshed day (don't yank to the top mid-read).
            let n = model.planner.day(model.view_date).map(|d| d.events.len()).unwrap_or(0);
            model.day_scroll = model.day_scroll.min(n.saturating_sub(1));
            vec![Effect::Redraw(Dirty::Full)]
        }
        UiEvent::FetchFailed(msg) => {
            model.fetching = false;
            flash(model, if msg.is_empty() { "Offline — showing cached data" } else { msg.as_str() });
            vec![Effect::Redraw(region(bottom_bar_rect(canvas)))]
        }
        UiEvent::UpdateChecked(msg) => {
            flash(model, &msg);
            vec![Effect::Redraw(region(bottom_bar_rect(canvas)))]
        }
        UiEvent::QueueChanged(n) => {
            let changed = model.pending != n;
            model.pending = n;
            if changed {
                vec![Effect::Redraw(region(bottom_bar_rect(canvas)))]
            } else {
                Vec::new()
            }
        }
        UiEvent::Tick { now } => {
            let mut effects = Vec::new();
            model.now = now;
            if model.flash.as_ref().is_some_and(|f| now >= f.expires_at) {
                model.flash = None;
                effects.push(Effect::Redraw(region(bottom_bar_rect(canvas))));
            }
            let today = now.date_naive();
            if today != model.today {
                // Midnight rollover: the whole window shifts; refetch and follow to the new day.
                model.today = today;
                model.view_date = today;
                model.fetching = true;
                effects.push(Effect::RequestFetch);
                effects.push(Effect::Redraw(Dirty::Full));
            }
            effects
        }
    }
}

fn on_tap(model: &mut AppModel, target: Target, canvas: Canvas) -> Vec<Effect> {
    match target {
        Target::PrevDay => page(model, -1),
        Target::NextDay => page(model, 1),
        Target::JumpToday => {
            if model.view_date != model.today {
                model.view_date = model.today;
                model.day_scroll = 0;
                vec![Effect::Redraw(Dirty::Full)]
            } else {
                Vec::new()
            }
        }
        Target::Refresh => {
            if model.fetching {
                return Vec::new();
            }
            model.fetching = true;
            vec![Effect::RequestFetch, Effect::Redraw(region(bottom_bar_rect(canvas)))]
        }
        Target::Exit => vec![Effect::Exit],
        Target::Checkbox { task_id } => complete_task(model, &task_id),
        Target::TaskRow { task_id } => {
            let Some(block) = model.find_block(&task_id) else {
                return Vec::new();
            };
            let title = block.title.clone();
            // Defer only when the Todoist metadata confirms it's safe: unknown or recurring
            // tasks keep their recurrence/due untouched — completing is always available.
            let can_defer = model.find_task(&task_id).map(|t| !t.is_recurring).unwrap_or(false);
            // The card shows the current priority chip; fall back to the block's own colour-derived
            // priority when metadata is missing (P4 when neither knows).
            let priority = model
                .find_task(&task_id)
                .map(|t| t.priority)
                .or_else(|| model.find_block(&task_id).and_then(|b| b.priority(model.find_task(&task_id))))
                .unwrap_or(1);
            model.overlay = Some(Overlay::TaskCard { task_id, title, can_defer, priority });
            // The sheet rises over the bottom band only — the list above it is untouched.
            vec![Effect::Redraw(region(sheet_region(canvas)))]
        }
        Target::CardComplete => {
            let Some(Overlay::TaskCard { task_id, .. }) = model.overlay.take() else {
                return Vec::new();
            };
            complete_task(model, &task_id)
        }
        Target::CardDelete => {
            let Some(Overlay::TaskCard { task_id, .. }) = model.overlay.take() else {
                return Vec::new();
            };
            delete_task(model, &task_id)
        }
        Target::EventRow { calendar_id, event_id, title } => {
            model.overlay = Some(Overlay::EventCard { calendar_id, event_id, title });
            // Like the task card: the sheet rises over the bottom band only.
            vec![Effect::Redraw(region(sheet_region(canvas)))]
        }
        Target::CardDeleteEvent => {
            let Some(Overlay::EventCard { calendar_id, event_id, .. }) = model.overlay.take() else {
                return Vec::new();
            };
            model.remove_event(&event_id);
            model.pending += 1;
            flash(model, "Deleted");
            vec![
                Effect::Enqueue(Action::DeleteEvent { calendar_id, event_id }),
                Effect::Redraw(Dirty::Full),
            ]
        }
        Target::MoreEvents { skip } => {
            model.day_scroll = skip;
            vec![Effect::Redraw(Dirty::Full)]
        }
        Target::EarlierEvents => {
            model.day_scroll = 0;
            vec![Effect::Redraw(Dirty::Full)]
        }
        Target::DeferChoice(choice) => {
            let Some(Overlay::TaskCard { task_id, can_defer, .. }) = model.overlay.clone() else {
                return Vec::new();
            };
            if !can_defer {
                return Vec::new(); // no such button is rendered; belt and braces
            }
            let base = model.today.max(model.view_date);
            let due = defer_target(base, choice);
            // The calendar block STAYS — Diarized owns placement and moves it on its next
            // reschedule. Only the task metadata moves optimistically.
            model.move_task(&task_id, due);
            model.overlay = None;
            model.pending += 1;
            flash(model, &format!("Deferred to {} — Diarized will reschedule", due.format("%a %-d %b")));
            vec![
                Effect::Enqueue(Action::Defer { task_id, due }),
                Effect::Redraw(Dirty::Full),
            ]
        }
        Target::DismissOverlay => {
            if model.overlay.take().is_some() {
                // Closing the sheet only repaints the bottom band back to the list + action bar.
                vec![Effect::Redraw(region(sheet_region(canvas)))]
            } else {
                Vec::new()
            }
        }
        Target::QuickAdd => {
            model.overlay = Some(Overlay::QuickAdd(QuickAdd::new()));
            vec![Effect::Redraw(region(sheet_region(canvas)))]
        }
        Target::Key(c) => {
            if let Some(Overlay::QuickAdd(qa)) = &mut model.overlay {
                qa.text.push(c);
                let was_shift = qa.shift;
                qa.shift = false; // one-shot shift, like a phone keyboard
                // a shifted key also flips the keyboard back to lowercase → repaint the whole sheet
                let r = if was_shift { sheet_region(canvas) } else { quickadd_input_region(canvas) };
                vec![Effect::Redraw(region(r))]
            } else {
                Vec::new()
            }
        }
        Target::KbBackspace => {
            if let Some(Overlay::QuickAdd(qa)) = &mut model.overlay {
                qa.text.pop();
                vec![Effect::Redraw(region(quickadd_input_region(canvas)))]
            } else {
                Vec::new()
            }
        }
        Target::KbShift => {
            if let Some(Overlay::QuickAdd(qa)) = &mut model.overlay {
                qa.shift = !qa.shift;
                vec![Effect::Redraw(region(sheet_region(canvas)))]
            } else {
                Vec::new()
            }
        }
        Target::KbLayerToggle => {
            if let Some(Overlay::QuickAdd(qa)) = &mut model.overlay {
                qa.layer = match qa.layer {
                    KbLayer::Letters => KbLayer::Symbols,
                    KbLayer::Symbols => KbLayer::Letters,
                };
                vec![Effect::Redraw(region(sheet_region(canvas)))]
            } else {
                Vec::new()
            }
        }
        Target::QuickAddDue(due) => {
            if let Some(Overlay::QuickAdd(qa)) = &mut model.overlay {
                qa.due = due;
                vec![Effect::Redraw(region(quickadd_input_region(canvas)))]
            } else {
                Vec::new()
            }
        }
        Target::QuickAddPrio(p) => {
            if let Some(Overlay::QuickAdd(qa)) = &mut model.overlay {
                qa.priority = p;
                vec![Effect::Redraw(region(quickadd_input_region(canvas)))]
            } else {
                Vec::new()
            }
        }
        Target::CardPriority(p) => {
            let Some(Overlay::TaskCard { task_id, .. }) = model.overlay.take() else {
                return Vec::new();
            };
            // Optimistic retint in the live state; the queue's overlay keeps it across refetches.
            model.set_task_priority(&task_id, p.api());
            model.pending += 1;
            flash(model, &format!("Priority → {}", p.label()));
            vec![
                Effect::Enqueue(Action::SetPriority { task_id, priority: p.api() }),
                Effect::Redraw(Dirty::Full),
            ]
        }
        Target::QuickAddSubmit => quickadd_submit(model, canvas),
        Target::CardBody => Vec::new(),
    }
}

/// Commit a quick-add: enqueue a Todoist create for the typed text (empty = no-op, sheet stays),
/// close the sheet, and flash a confirmation. The task syncs to Todoist; the calendar-only day view
/// won't show it unless Diarized later schedules a block for it.
fn quickadd_submit(model: &mut AppModel, canvas: Canvas) -> Vec<Effect> {
    let (content, due, priority) = match &model.overlay {
        Some(Overlay::QuickAdd(qa)) => (
            qa.text.trim().to_string(),
            qa.due.due_string().map(String::from),
            qa.priority,
        ),
        _ => return Vec::new(),
    };
    if content.is_empty() {
        return Vec::new();
    }
    model.overlay = None;
    let temp_id = format!("tmp-{}", model.quickadd_seq);
    model.quickadd_seq += 1;
    model.pending += 1;
    flash(model, &format!("Added: {content}"));
    // Only send a priority when it's not Todoist's default — keeps the create body minimal.
    let priority = (priority != crate::model::Prio::P4).then(|| priority.api());
    vec![
        Effect::Enqueue(Action::Create { temp_id, content, due, priority }),
        Effect::Redraw(region(sheet_region(canvas))),
    ]
}

/// Optimistic complete: hide the block(s) + metadata, queue the Todoist close **and** a calendar
/// delete per block.
fn complete_task(model: &mut AppModel, task_id: &str) -> Vec<Effect> {
    finish_task(model, task_id, Action::Complete { task_id: task_id.to_string() }, "Completed")
}

/// Optimistic delete: same shape as complete, but the Todoist write removes the task entirely
/// (ending recurrence) rather than closing this occurrence.
fn delete_task(model: &mut AppModel, task_id: &str) -> Vec<Effect> {
    finish_task(model, task_id, Action::Delete { task_id: task_id.to_string() }, "Deleted")
}

/// Take a task off the day view for good: the Todoist write, plus one `DeleteEvent` for each
/// calendar block it owns.
///
/// **Deleting the block ourselves is what makes this feel instant.** The day view's task rows *are*
/// Diarized's calendar events, and `actionqueue::Queue::apply_overlay` only masks a completed
/// block while its action is still queued. The Todoist close lands within seconds, the mask lifts,
/// and the next fetch used to bring the block straight back — it only really goes when Diarized
/// notices and deletes it, which is its next sync run (minutes, or an hour off-peak). Queueing the
/// calendar delete alongside removes it at the source; the two actions sit on different dedupe
/// channels, so the mask holds until both have landed, and by then the event is genuinely gone.
///
/// The two systems agree rather than fight: Diarized treats a block the user deleted as a
/// completion signal, which is what we just told Todoist anyway.
fn finish_task(model: &mut AppModel, task_id: &str, write: Action, flash_text: &str) -> Vec<Effect> {
    if model.find_block(task_id).is_none() && model.find_task(task_id).is_none() {
        return Vec::new();
    }
    // Collect the block handles BEFORE removing the task — `remove_task` drops the events too.
    let blocks = model.block_refs(task_id);
    model.remove_task(task_id);
    model.pending += 1 + blocks.len(); // optimistic; QueueChanged corrects it after the enqueues
    flash(model, flash_text);
    // Todoist first: it is the source of truth, and the calendar block is a view of it.
    let mut effects = vec![Effect::Enqueue(write)];
    effects.extend(
        blocks
            .into_iter()
            .map(|(calendar_id, event_id)| Effect::Enqueue(Action::DeleteEvent { calendar_id, event_id })),
    );
    effects.push(Effect::Redraw(Dirty::Full));
    effects
}

fn page(model: &mut AppModel, days: i64) -> Vec<Effect> {
    let next = model.view_date + Duration::days(days);
    if model.planner.in_window(next) {
        model.view_date = next;
        model.day_scroll = 0;
        vec![Effect::Redraw(Dirty::Full)]
    } else {
        Vec::new() // the arrow is drawn dead at the window edge; taps land here only via races
    }
}

fn flash(model: &mut AppModel, text: &str) {
    model.flash = Some(Flash { text: text.to_string(), expires_at: model.now + Duration::seconds(FLASH_SECS) });
}

fn region(r: Rect) -> Dirty {
    Dirty::Region(r.x, r.y, r.w, r.h)
}
