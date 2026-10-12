//! Reconciles the two paths TAO delivers typed text on.
//!
//! A key press carries its text in `KeyboardInput`, and some backends also
//! commit the same text through `ReceivedImeText`. The order differs: GTK
//! sends the key first, macOS (`insertText:` inside `keyDown:`) sends the
//! commit first. Each typed character must be inserted exactly once, and
//! through the key path when there is one, so GPUI sees the key first and can
//! consume it as a binding.

#[derive(Default)]
pub(crate) struct TypedText {
    /// Text of the last key press, so a commit that follows it is dropped.
    last_key: Option<String>,
    /// A commit waiting to see whether a key press with the same text follows.
    pending_commit: Option<String>,
}

impl TypedText {
    /// Records an IME commit. Returns text that must be inserted right away
    /// (an earlier commit that no key press claimed).
    pub fn commit(&mut self, text: &str) -> Option<String> {
        if self.last_key.take().as_deref() == Some(text) {
            return None;
        }
        self.pending_commit.replace(text.to_owned())
    }

    /// Call before dispatching a key press with `key_char`. Returns a pending
    /// commit that must be inserted before the key (it was not this key's).
    pub fn key_down(&mut self, key_char: Option<&str>) -> Option<String> {
        match self.pending_commit.take() {
            Some(pending) if Some(pending.as_str()) == key_char => None,
            other => other,
        }
    }

    /// Call after a key press was dispatched (and its text inserted or
    /// consumed by GPUI).
    pub fn key_dispatched(&mut self, key_char: Option<String>) {
        self.last_key = key_char;
    }

    /// Takes a commit no key press claimed: a genuine IME commit.
    pub fn flush(&mut self) -> Option<String> {
        self.pending_commit.take()
    }

    pub fn has_pending(&self) -> bool {
        self.pending_commit.is_some()
    }

    pub fn reset(&mut self) {
        self.last_key = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `events` and returns every insertion, in order. `Key` inserts
    /// its text unless GPUI consumes it (`handled`).
    enum Ev {
        Key(&'static str, bool),
        Commit(&'static str),
        Drain,
    }

    fn run(events: &[Ev]) -> Vec<String> {
        let mut t = TypedText::default();
        let mut out = Vec::new();
        for event in events {
            match *event {
                Ev::Key(text, handled) => {
                    out.extend(t.key_down(Some(text)));
                    if !handled {
                        out.push(text.to_owned());
                    }
                    t.key_dispatched(Some(text.to_owned()));
                }
                Ev::Commit(text) => out.extend(t.commit(text)),
                Ev::Drain => out.extend(t.flush()),
            }
        }
        out
    }

    #[test]
    fn gtk_order_key_then_duplicate_commit() {
        use Ev::*;
        let out = run(&[Key("a", false), Commit("a"), Drain, Key("a", false), Drain]);
        assert_eq!(out, ["a", "a"]);
    }

    #[test]
    fn macos_order_commit_then_key() {
        use Ev::*;
        let out = run(&[
            Commit("a"),
            Key("a", false),
            Drain,
            Commit("a"),
            Key("a", false),
            Drain,
            Commit("b"),
            Key("b", false),
            Drain,
        ]);
        assert_eq!(out, ["a", "a", "b"]);
    }

    #[test]
    fn macos_key_consumed_by_gpui_inserts_nothing() {
        use Ev::*;
        assert!(run(&[Commit(" "), Key(" ", true), Drain]).is_empty());
    }

    #[test]
    fn genuine_ime_commit_is_inserted_on_drain() {
        use Ev::*;
        assert_eq!(run(&[Commit("你好"), Drain]), ["你好"]);
    }

    #[test]
    fn unclaimed_commit_is_inserted_before_the_next_key() {
        use Ev::*;
        assert_eq!(run(&[Commit("你好"), Key("x", false)]), ["你好", "x"]);
        assert_eq!(run(&[Commit("é"), Commit("ü"), Drain]), ["é", "ü"]);
    }
}
