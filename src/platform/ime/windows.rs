//! Windows: TAO leaves composition to the system (`DefWindowProc` draws the
//! IME's own composition window) and reports only the committed text. The
//! composition and candidate windows are placed at GPUI's caret through
//! IMM32 on the window's `HWND`, as TAO's own `set_ime_position` would.

use windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    UI::Input::Ime::{
        CANDIDATEFORM, CFS_EXCLUDE, CFS_POINT, COMPOSITIONFORM, ImmGetContext, ImmReleaseContext,
        ImmSetCandidateWindow, ImmSetCompositionWindow,
    },
};

/// Places the IME windows at `caret` (`[left, top, right, bottom]`, physical
/// client coordinates): the composition starts at the caret, and candidates
/// open next to it without covering it.
pub(crate) fn set_position(hwnd: HWND, caret: [i32; 4]) {
    let [left, top, right, bottom] = caret;
    // SAFETY: `hwnd` is the live window GPUI draws into, and the input
    // context taken from it is released before returning.
    unsafe {
        let himc = ImmGetContext(hwnd);
        if himc.is_invalid() {
            // No IME is active for this thread.
            return;
        }
        let composition = COMPOSITIONFORM {
            dwStyle: CFS_POINT,
            ptCurrentPos: POINT { x: left, y: top },
            rcArea: RECT::default(),
        };
        let _ = ImmSetCompositionWindow(himc, &composition);
        let candidates = CANDIDATEFORM {
            dwIndex: 0,
            dwStyle: CFS_EXCLUDE,
            ptCurrentPos: POINT { x: left, y: bottom },
            rcArea: RECT {
                left,
                top,
                right,
                bottom,
            },
        };
        let _ = ImmSetCandidateWindow(himc, &candidates);
        let _ = ImmReleaseContext(hwnd, himc);
    }
}
