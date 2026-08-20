//! Rectangle arithmetic shared by the screens.
//!
//! Both helpers **clamp to the container**. A rectangle larger than the area it
//! sits in panics ratatui on render, which on the dashboard means taking a live
//! stream down over a cosmetic detail — so shrinking is always preferred to
//! overflowing, and every caller gets the same guarantee.

use ratatui::layout::Rect;

/// A rectangle of at most `width`×`height`, centred in `area`.
pub fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// A rectangle of at most `width`×`height` in the bottom-right of `area`,
/// inset by `pad` columns from the right edge.
///
/// Used for the picker's ready button, which sits *on* the list border rather
/// than inside it — the padding is what keeps the corner glyph visible so the
/// frame still reads as a frame.
pub fn bottom_right(area: Rect, width: u16, height: u16, pad: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    // Saturating: on a terminal narrow enough that the padding does not fit,
    // losing the inset is right and panicking is not.
    let x = area.x + area.width.saturating_sub(w + pad).max(0);
    Rect {
        x: x.max(area.x),
        y: area.y + area.height - h,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(width: u16, height: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    fn contains(outer: Rect, inner: Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.x + inner.width <= outer.x + outer.width
            && inner.y + inner.height <= outer.y + outer.height
    }

    #[test]
    fn centred_never_exceeds_its_container() {
        let small = rect(20, 6);
        let r = centred(small, 70, 14);
        assert!(contains(small, r), "{r:?} escaped {small:?}");
    }

    #[test]
    fn bottom_right_sits_in_the_corner() {
        let area = rect(100, 20);
        let r = bottom_right(area, 12, 3, 2);
        assert_eq!(r.x, 100 - 12 - 2);
        assert_eq!(r.y, 20 - 3);
        assert_eq!((r.width, r.height), (12, 3));
    }

    #[test]
    fn bottom_right_stays_inside_a_tiny_container() {
        // The failure this prevents is a render panic, so sweep the sizes that
        // would produce one rather than testing a single comfortable case.
        for w in 1..=14u16 {
            for h in 1..=4u16 {
                let area = rect(w, h);
                let r = bottom_right(area, 12, 3, 2);
                assert!(contains(area, r), "{r:?} escaped {area:?}");
            }
        }
    }

    #[test]
    fn bottom_right_honours_the_offset_of_a_nested_area() {
        // The picker draws into a sub-area of the frame, not the frame itself.
        let area = Rect {
            x: 10,
            y: 5,
            width: 40,
            height: 12,
        };
        let r = bottom_right(area, 12, 3, 2);
        assert!(contains(area, r), "{r:?} escaped {area:?}");
        assert_eq!(r.y, 5 + 12 - 3);
    }
}
