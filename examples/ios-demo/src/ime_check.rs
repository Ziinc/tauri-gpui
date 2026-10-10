//! IME composition check for the CI smoke test (`IOS_DEMO_IME=1`).
//!
//! A simulator cannot be made to type through a Japanese keyboard, so this
//! drives the plugin's input view the way UIKit's keyboard does: it marks
//! text, replaces the marked text, commits it and presses return, each from
//! the main queue like a real keystroke. Each step logs the marked range and
//! text the view reports back.

use std::{thread, time::Duration};

use dispatch2::DispatchQueue;
use objc2::{MainThreadMarker, Message, msg_send, rc::Retained};
use objc2_foundation::{NSRange, NSString};
use objc2_ui_kit::{UIApplication, UITextPosition, UITextRange, UIView};

pub fn enabled() -> bool {
    std::env::var_os("IOS_DEMO_IME").is_some()
}

/// Starts the steps once the keyboard has had time to come up.
pub fn run() {
    thread::spawn(|| {
        thread::sleep(Duration::from_secs(3));
        on_main(|view| {
            mark(view, "にほ");
            log_marked(view, "mark");
        });
        thread::sleep(Duration::from_secs(1));
        on_main(|view| {
            mark(view, "日本");
            log_marked(view, "convert");
        });
        thread::sleep(Duration::from_secs(3));
        on_main(|view| {
            let _: () = unsafe { msg_send![view, unmarkText] };
            log_marked(view, "commit");
        });
        thread::sleep(Duration::from_secs(1));
        on_main(|view| {
            let _: () = unsafe { msg_send![view, insertText: &*NSString::from_str("\n")] };
        });
    });
}

fn on_main(step: fn(&UIView)) {
    DispatchQueue::main().exec_async(move || {
        let mtm = MainThreadMarker::new().expect("the main queue runs on the main thread");
        match input_view(mtm) {
            Some(view) => step(&view),
            None => log::error!(target: "ios-demo", "ime no input view"),
        }
    });
}

/// The plugin's `TauriGpuiInputView`, the keyboard's first responder.
fn input_view(mtm: MainThreadMarker) -> Option<Retained<UIView>> {
    fn find(view: &UIView) -> Option<Retained<UIView>> {
        if view.class().name().to_str() == Ok("TauriGpuiInputView") {
            return Some(view.retain());
        }
        view.subviews().iter().find_map(|child| find(&child))
    }
    #[allow(deprecated)]
    let windows = UIApplication::sharedApplication(mtm).windows();
    windows.iter().find_map(|window| find(&window))
}

fn mark(view: &UIView, text: &str) {
    let text = NSString::from_str(text);
    let selected = NSRange::new(text.length(), 0);
    let _: () = unsafe { msg_send![view, setMarkedText: &*text, selectedRange: selected] };
}

fn log_marked(view: &UIView, step: &str) {
    let marked: Option<Retained<UITextRange>> = unsafe { msg_send![view, markedTextRange] };
    let marked = marked.map_or_else(
        || "none".to_string(),
        |range| {
            let start: Retained<UITextPosition> = unsafe { msg_send![&*range, start] };
            let end: Retained<UITextPosition> = unsafe { msg_send![&*range, end] };
            format!("{}..{}", offset(view, &start), offset(view, &end))
        },
    );
    let begin: Retained<UITextPosition> = unsafe { msg_send![view, beginningOfDocument] };
    let end: Retained<UITextPosition> = unsafe { msg_send![view, endOfDocument] };
    let all: Option<Retained<UITextRange>> =
        unsafe { msg_send![view, textRangeFromPosition: &*begin, toPosition: &*end] };
    let text: Option<Retained<NSString>> =
        all.and_then(|all| unsafe { msg_send![view, textInRange: &*all] });
    let text = text.map(|text| text.to_string()).unwrap_or_default();
    log::info!(target: "ios-demo", "ime step={step} marked={marked} text={text:?}");
}

fn offset(view: &UIView, position: &UITextPosition) -> isize {
    let begin: Retained<UITextPosition> = unsafe { msg_send![view, beginningOfDocument] };
    unsafe { msg_send![view, offsetFromPosition: &*begin, toPosition: position] }
}
