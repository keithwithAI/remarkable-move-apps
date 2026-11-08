//! Run the real Diarized parser over a saved Google Calendar `events.list` response.
//!
//!   cargo run -p sources --example parse_live -- live_events.json
//!
//! Verification tool: proves production data parses, without putting a network call in a test.
//!
//! **Read the `marker → checkable` line first.** Blocks carrying the marker but yielding no
//! Todoist id are the failure this tool exists to catch: that is what a change to Diarized's
//! description format looks like, and it degrades the day view to plain uncheckable rows with no
//! error anywhere. `marker` and `checkable` should be equal.
fn main() {
    let path = std::env::args().nth(1).expect("usage: parse_live <events.json>");
    let body: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
    let evs = sources::gcal::parse_events(&body, "calendar@example.com");
    let marked: Vec<_> =
        evs.iter().filter(|e| sources::gcal::is_scheduled_block(&e.description)).collect();
    let checkable: Vec<_> = marked.iter().filter(|e| e.todoist_task.is_some()).collect();
    println!("timed events parsed : {}", evs.len());
    println!("marker → checkable  : {} → {}", marked.len(), checkable.len());
    if marked.len() != checkable.len() {
        println!("  !! {} block(s) carry the marker but no Todoist id — the", marked.len() - checkable.len());
        println!("     \"Open in Todoist\" href moved. These render as plain rows.");
    }
    let mut colours: Vec<_> = marked.iter().map(|e| e.color_id).collect();
    colours.sort();
    colours.dedup();
    println!("colorIds seen       : {colours:?}");
    let mut p: Vec<_> = marked.iter().map(|e| e.priority(None)).collect();
    p.sort();
    p.dedup();
    println!("priorities derived  : {p:?}");
    if let Some(b) = checkable.first() {
        println!("\nsample todoist id   : {:?}", b.todoist_task);
    }
    println!("\nplain events (correctly NOT task blocks): {}", evs.len() - marked.len());
}
