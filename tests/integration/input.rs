//! Real X11 keyboard and mouse input (via xdotool) into a GPUI window:
//! X11 → GTK → TAO → plugin → GPUI.

use std::{thread, time::Duration};

use crate::{
    ensure,
    support::{Ctx, Fail, TestResult, xdotool},
};

crate::tests!["input" =>
    typed_text_arrives_once,
    named_keys_and_shortcuts_map_to_gpui_keys,
    random_clicks_and_scrolls_arrive,
];

/// Opens a focused probe window and returns its label and X11 id.
fn focused_probe(cx: &mut Ctx) -> Result<(String, String), Fail> {
    cx.require_wm()?;
    if xdotool(&["version"]).is_err() {
        return Err(Fail::Skip("needs xdotool"));
    }
    let label = cx.open_probe("input", (500., 400.));
    let xid = cx
        .xid(&label)
        .ok_or_else(|| Fail::Fail(format!("xdotool found no window `{label}`")))?;
    xdotool(&["windowactivate", "--sync", &xid]).ok();
    xdotool(&["windowfocus", "--sync", &xid]).ok();
    thread::sleep(Duration::from_millis(300));
    Ok((label, xid))
}

fn typed_text_arrives_once(cx: &mut Ctx) -> TestResult {
    let (label, _) = focused_probe(cx)?;
    xdotool(&["type", "--delay", "40", "Hello, World 42!"]).map_err(Fail::Fail)?;
    let typed = cx.wait_seen(&label, |s| s.text == "Hello, World 42!");
    let seen = cx.seen(&label).unwrap_or_default();
    ensure!(typed, "text = {:?}", seen.text);
    Ok(())
}

fn named_keys_and_shortcuts_map_to_gpui_keys(cx: &mut Ctx) -> TestResult {
    let (label, _) = focused_probe(cx)?;
    xdotool(&["key", "ctrl+a", "Return", "Escape", "Left"]).map_err(Fail::Fail)?;
    let expected = ["a", "enter", "escape", "left"].map(String::from);
    let ok = cx.wait_seen(&label, |s| s.keys.ends_with(&expected));
    let seen = cx.seen(&label).unwrap_or_default();
    ensure!(ok, "keys = {:?}", seen.keys);
    ensure!(
        seen.text.is_empty(),
        "shortcuts inserted text {:?}",
        seen.text
    );
    Ok(())
}

fn random_clicks_and_scrolls_arrive(cx: &mut Ctx) -> TestResult {
    let (label, xid) = focused_probe(cx)?;
    let (mut clicks, mut scrolls) = (0, 0);
    for _ in 0..60 {
        let (x, y) = (cx.rng.range(5..495), cx.rng.range(5..395));
        xdotool(&[
            "mousemove",
            "--window",
            &xid,
            &x.to_string(),
            &y.to_string(),
        ])
        .ok();
        match cx.rng.range(0..3) {
            0 => {
                xdotool(&["click", "1"]).ok();
                clicks += 1;
            }
            1 => {
                let button = if cx.rng.range(0..2) == 0 { "4" } else { "5" };
                xdotool(&["click", button]).ok();
                scrolls += 1;
            }
            _ => {}
        }
    }
    let ok = cx.wait_seen(&label, |s| {
        s.mouse_downs == clicks && s.mouse_ups == clicks && s.scrolls >= scrolls
    });
    let s = cx.seen(&label).unwrap_or_default();
    ensure!(
        ok,
        "sent {clicks} clicks/{scrolls} scrolls; GPUI saw {}/{} down/up, {} scrolls",
        s.mouse_downs,
        s.mouse_ups,
        s.scrolls
    );
    Ok(())
}
