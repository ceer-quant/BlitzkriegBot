#![cfg(test)]

use super::evolution::{f2, pct};
use super::overlays::wrapped_rows;
use super::*;

/// issue 719: the signed figures now come from the ui_kit shared
/// formatters (the same functions behind the web face's HTML), so the
/// TUI's visible bytes are pinned here too — a format change on either
/// side must fail a test before the two faces can drift apart.
#[test]
fn shared_signed_formatters_render_the_pinned_bytes() {
    assert_eq!(blitzkrieg_ui_kit::web::money(12.341), "+12.34");
    assert_eq!(blitzkrieg_ui_kit::web::money(-12.341), "-12.34");
    assert_eq!(blitzkrieg_ui_kit::web::money(0.0), "+0.00");
    assert_eq!(blitzkrieg_ui_kit::web::pct(3.44), "+3.4%");
    assert_eq!(blitzkrieg_ui_kit::web::pct(-3.44), "-3.4%");
    assert_eq!(blitzkrieg_ui_kit::web::pct(0.0), "+0.0%");
    // The evolution tab's own fraction-scaled pct keeps its local
    // semantics (0.4 → "40.0%", NOT the shared "+0.4%").
    assert_eq!(pct(0.4), "40.0%");
    assert_eq!(f2(3.14519), "3.15");
}

fn line(text: &str) -> Line<'static> {
    Line::from(text.to_string())
}

/// The `n` overlay sized itself from a fixed 20 rows, so a report that grew
/// past it lost its tail silently (#260). The height now comes from the
/// content, and the content WRAPS — counting `lines.len()` would under-count
/// a long detail line and crop it exactly as the fixed height did.
#[test]
fn the_overlay_height_counts_wrapped_rows_not_just_lines() {
    assert_eq!(wrapped_rows(&[], 40), 0, "no content, no rows");

    // An empty line is still a row: the overlay's spacers must be counted.
    assert_eq!(wrapped_rows(&[line("")], 40), 1);
    assert_eq!(wrapped_rows(&[line(""), line("")], 40), 2);

    // A line that fits is one row, whatever it is made of.
    assert_eq!(wrapped_rows(&[line("0123456789")], 10), 1);
    // One column too narrow wraps onto a second row.
    assert_eq!(wrapped_rows(&[line("0123456789X")], 10), 2);
    assert_eq!(wrapped_rows(&[line(&"a".repeat(21))], 10), 3);

    // Width is DISPLAY width, not a character count and not a byte count: a
    // CJK glyph is two columns and three bytes, so six glyphs fill a
    // 12-column box exactly and an eighth wraps. Counting chars or bytes
    // would mis-size the box for every Chinese detail line — the same silent
    // crop, by another route.
    let six_glyphs = "市场".repeat(3); // 6 glyphs, 12 columns, 18 bytes
    assert_eq!(wrapped_rows(&[line(&six_glyphs)], 12), 1);
    assert_eq!(wrapped_rows(&[line(&six_glyphs)], 13), 1);
    assert_eq!(wrapped_rows(&[line(&"市场".repeat(4))], 12), 2); // 8 glyphs = 16 columns

    // A zero-width box would divide by zero; it degrades to "one row per
    // character" rather than panicking inside the render loop.
    assert_eq!(wrapped_rows(&[line("abc")], 0), 3);
}

/// A four-path report is the shape the panel ships with; it must fit in the
/// height the overlay asks for, with room for the headline and the footer.
#[test]
fn a_four_path_report_fits_the_computed_height() {
    let mut lines = vec![line("4/4 paths OK — all network paths OK"), line("")];
    for name in ["venue-rest", "venue-ws", "discovery", "spot-ws"] {
        lines.push(line(&format!(
            "  ok   {}  {}.example  12 ms  ok",
            name, name
        )));
    }
    lines.push(line(""));
    lines.push(line("reading: venue-rest answered in 12 ms"));
    lines.push(line(""));
    lines.push(line(
        "r probe again · n or Esc closes · the same report is in the log pane",
    ));

    // Comfortably narrower than any real terminal: nothing here wraps.
    let rows = wrapped_rows(&lines, 100);
    assert_eq!(rows, lines.len(), "a report that fits must not be padded");
    assert!(
        rows + 2 <= 20,
        "the shipped four-path report outgrew the old box"
    );
}
