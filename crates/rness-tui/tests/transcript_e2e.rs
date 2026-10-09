//! WP-4 long-transcript e2e (TestBackend-level): the user runs sessions with
//! hundreds of compactions, so these are the normal case.
//!
//! - every part of the transcript is reachable by scrolling (top included);
//! - frames are identical regardless of the display-cache budget
//!   (`cache_bytes` 0 / 1024 / default) — eviction must be invisible;
//! - a resize round-trip (and a resize storm) converges to the same frame as
//!   a fresh open at that size;
//! - scrolling actually moves the view, PageUp/PageDown are inverse;
//! - expanding/collapsing a card does not corrupt neighbouring rows.
//!
//! Sizes: RNESS_BENCH_TURNS (default 400 turns ≈ 2.4k entries, 57 compactions).
//! Run: cargo test -p rness-tui --test transcript_e2e -- --nocapture
mod common;

use common::*;
use rness_tui::app::{Action, App};
use serde_json::{json, Value};

fn config_with_cache(bytes: Option<u64>) -> Value {
    let mut c = flavor_config();
    if let Some(b) = bytes {
        c["cache_bytes"] = json!(b);
    }
    c
}

/// Render enough frames for progressive layout / refills to settle.
fn settle(app: &mut App, w: u16, h: u16) -> Vec<String> {
    let mut last = rows(&render(app, w, h));
    for _ in 0..400 {
        let cur = rows(&render(app, w, h));
        if cur == last {
            // Two equal frames in a row is not proof (stale estimates may be
            // off-screen); render a few more to be sure.
            let a = rows(&render(app, w, h));
            let b = rows(&render(app, w, h));
            if a == cur && b == cur {
                return cur;
            }
        }
        last = cur;
    }
    last
}

fn scroll_to(app: &mut App, from_bottom: usize, w: u16, h: u16) -> Vec<String> {
    app.model.scroll_from_bottom = 0;
    render(app, w, h);
    app.apply(Action::ScrollUp(from_bottom));
    settle(app, w, h)
}

fn turns() -> usize {
    env_usize("RNESS_BENCH_TURNS", 400)
}

#[test]
fn eviction_budget_is_invisible() {
    let b = long_session("s-evict", turns(), 2, 7);
    let (w, h) = (120u16, 40u16);
    let positions = [0usize, 37, 500, 2000, 9000];
    let mut frames: Vec<(String, Vec<Vec<String>>)> = Vec::new();
    for budget in [None, Some(1024u64), Some(0)] {
        let (mut app, _be) = new_app("s-evict", b.history(), config_with_cache(budget));
        let mut per = Vec::new();
        for &p in &positions {
            per.push(scroll_to(&mut app, p, w, h));
        }
        // And back down in reverse order: cached rows were evicted meanwhile.
        for &p in positions.iter().rev() {
            per.push(scroll_to(&mut app, p, w, h));
        }
        frames.push((format!("{budget:?}"), per));
    }
    let base = &frames[0].1;
    let mut diffs = Vec::new();
    for (name, per) in &frames[1..] {
        for (i, (a, b)) in base.iter().zip(per).enumerate() {
            if a != b {
                let row = a.iter().zip(b).position(|(x, y)| x != y).unwrap_or(0);
                diffs.push(format!(
                    "budget {name} pos#{i}: first diff row {row}\n  default: {:?}\n  {name}: {:?}",
                    a.get(row),
                    b.get(row)
                ));
            }
        }
    }
    for f in &base[..] {
        assert!(f.iter().any(|r| !r.trim().is_empty()), "blank frame");
    }
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}

#[test]
fn top_of_long_transcript_is_reachable() {
    // 3k turns ≈ 18k entries; with tool cards this is well above 65 535 rows.
    let n = env_usize("RNESS_BENCH_TOP_TURNS", 3000);
    let mut b = LogBuilder::new("s-top");
    b.user("FIRST-MESSAGE-SENTINEL");
    // Same builder: splicing a second builder's envelopes would duplicate
    // event ids (entry ids key the chat's scroll anchor).
    extend_long_session(&mut b, n, 2, 7);
    let (mut app, _be) = new_app("s-top", b.history(), flavor_config());
    let (w, h) = (120u16, 40u16);
    settle(&mut app, w, h);
    // PageUp until the view stops changing (what a user does).
    let mut last = Vec::new();
    let mut presses = 0;
    for _ in 0..20_000 {
        app.apply(Action::ScrollUp(10));
        presses += 1;
        let cur = rows(&render(&mut app, w, h));
        if cur == last {
            break;
        }
        last = cur;
    }
    let mut top = settle(&mut app, w, h);
    // Also the extreme: offset saturated.
    app.model.scroll_from_bottom = usize::MAX;
    let top2 = settle(&mut app, w, h);
    eprintln!(
        "presses={presses} scroll_from_bottom={} top-row={:?}",
        app.model.scroll_from_bottom,
        top.first()
    );
    top.extend(top2);
    assert!(
        top.iter().any(|r| r.contains("FIRST-MESSAGE-SENTINEL")),
        "first message unreachable after {presses} PageUps"
    );
}

#[test]
fn page_up_down_are_inverse_and_move_the_view() {
    let b = long_session("s-page", turns(), 2, 7);
    let (mut app, _be) = new_app("s-page", b.history(), flavor_config());
    let (w, h) = (100u16, 30u16);
    let bottom = settle(&mut app, w, h);
    let mut stack = vec![bottom.clone()];
    for _ in 0..50 {
        app.apply(Action::ScrollUp(10));
        let f = settle(&mut app, w, h);
        assert_ne!(Some(&f), stack.last(), "PageUp did not change the view");
        stack.push(f);
    }
    let mut mismatches = 0;
    for i in (0..50).rev() {
        app.apply(Action::ScrollDown(10));
        let f = settle(&mut app, w, h);
        if f != stack[i] {
            mismatches += 1;
            if mismatches == 1 {
                eprintln!(
                    "PageDown #{i} differs from PageUp frame:\n up: {:?}\n dn: {:?}",
                    stack[i].get(0..3),
                    f.get(0..3)
                );
            }
        }
    }
    assert_eq!(
        mismatches, 0,
        "{mismatches}/50 PageDown frames differ from the PageUp frames"
    );
    assert_eq!(app.model.scroll_from_bottom, 0);
}

#[test]
fn resize_round_trip_matches_fresh_open() {
    let b = long_session("s-resize", turns(), 2, 7);
    let sizes = [
        (160u16, 50u16),
        (80, 24),
        (40, 10),
        (200, 60),
        (20, 5),
        (1, 1),
        (120, 40),
    ];
    for scroll in [0usize, 300] {
        let (mut app, _be) = new_app("s-resize", b.history(), flavor_config());
        app.model.scroll_from_bottom = scroll;
        for &(w, h) in &sizes {
            render(&mut app, w, h);
        }
        // Storm: 200 random-ish sizes without settling.
        let mut x = 7u32;
        for _ in 0..200 {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            let w = 1 + (x >> 8) as u16 % 400;
            let h = 1 + (x >> 20) as u16 % 100;
            render(&mut app, w, h);
        }
        let (w, h) = (120u16, 40u16);
        let after = settle(&mut app, w, h);
        let (mut fresh, _be2) = new_app("s-resize", b.history(), flavor_config());
        fresh.model.scroll_from_bottom = scroll;
        let expect = settle(&mut fresh, w, h);
        if scroll == 0 {
            assert_eq!(
                after, expect,
                "pinned-to-bottom frame after resize storm differs from fresh open"
            );
        } else if after != expect {
            // Scrolled views keep an anchor across resizes by design; only
            // report whether the anchor drifted.
            eprintln!(
                "NOTE scrolled view after resize storm differs from fresh open at same offset (anchor kept):\n after: {:?}\n fresh: {:?}",
                after.first(),
                expect.first()
            );
        }
        assert!(
            after.iter().any(|r| !r.trim().is_empty()),
            "blank after resize storm"
        );
    }
}

#[test]
fn toggle_tool_card_keeps_layout_consistent() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let b = long_session("s-toggle", 60, 2, 7);
    let (mut app, _be) = new_app("s-toggle", b.history(), flavor_config());
    let (w, h) = (100u16, 40u16);
    let before = settle(&mut app, w, h);
    let alt_up = Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    let ctrl_o = Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    app.on_term_event(alt_up.clone());
    let focused = settle(&mut app, w, h);
    app.on_term_event(ctrl_o.clone());
    let expanded = settle(&mut app, w, h);
    app.on_term_event(ctrl_o);
    let collapsed = settle(&mut app, w, h);
    assert_ne!(focused, expanded, "ctrl+o did not expand the focused card");
    assert_eq!(
        focused, collapsed,
        "collapse did not restore the focused frame"
    );
    let _ = before;
}

#[test]
fn hundreds_of_compactions_render_and_scroll() {
    // 1 500 turns with a compaction every 5 turns = 299 compactions.
    let b = long_session(
        "s-compact",
        env_usize("RNESS_BENCH_COMPACT_TURNS", 1500),
        1,
        5,
    );
    let n_compactions = b
        .envelopes
        .iter()
        .filter(|e| matches!(e.event, rness_protocol::events::SessionEvent::Compaction(_)))
        .count();
    let t = std::time::Instant::now();
    let (mut app, _be) = new_app("s-compact", b.history(), flavor_config());
    let load = t.elapsed();
    let (w, h) = (120u16, 40u16);
    let t = std::time::Instant::now();
    let f = render(&mut app, w, h);
    let first = t.elapsed();
    eprintln!(
        "compactions={n_compactions} entries={} load={load:?} first_frame={first:?}",
        app.model.entries.len()
    );
    assert!(
        text(&f).contains("done 1499") || text(&f).contains("1499"),
        "last turn not visible:\n{}",
        text(&f)
    );
    // Old compaction cards must still render when scrolled to.
    app.apply(Action::ScrollUp(3000));
    let mid = settle(&mut app, w, h);
    assert!(mid.iter().any(|r| !r.trim().is_empty()));
    assert!(n_compactions >= 250);
}
