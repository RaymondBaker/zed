use std::ops::Range;

use gpui::{
    Bounds, Context, HighlightStyle, KeyBindingContextPredicate, Pixels, Window, point, px,
};
use language::Bias;
use multi_buffer::{Anchor, ToOffset as _};
use settings::Settings as _;
use text::TransactionId;
use theme::ActiveTheme as _;
use util::ResultExt as _;

use crate::{
    DisplayPoint, Editor, EditorSettings, EditorSnapshot, HighlightKey,
    display_map::{DisplaySnapshot, ToDisplayPoint as _},
};

const PIXELS_PER_STEP: Pixels = px(10.);
const PIXELS_PER_STEP_MAGNITUDE: Pixels = px(100.);
const MAX_STEP_MAGNITUDE: u32 = 18;
const MAX_DIGITS: usize = 30;
const INDICATOR_GAP: Pixels = px(8.);
// Must exceed `MAX_DIGITS`, so that a number cut off by the context window is always rejected
// for having too many digits instead of being parsed as a shorter number.
const MAX_CONTEXT_CHARS: usize = 64;

pub(crate) struct NumberDragState {
    start: Anchor,
    text: String,
    scaled_value: i128,
    decimal_places: u32,
    click_position: gpui::Point<Pixels>,
    vertical_origin: Pixels,
    step_magnitude: u32,
    first_transaction: Option<TransactionId>,
    dragged: bool,
}


#[derive(Debug, PartialEq)]
struct NumberInLine {
    range: Range<usize>,
    /// The number multiplied by `10^decimal_places`, so that decimals are stepped exactly.
    scaled_value: i128,
    decimal_places: u32,
}

impl Editor {
    pub(crate) fn begin_number_drag(
        &mut self,
        position: DisplayPoint,
        click_position: gpui::Point<Pixels>,
        snapshot: &EditorSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let buffer_snapshot = snapshot.buffer_snapshot();
        let offset = position.to_offset(&snapshot.display_snapshot, Bias::Left);
        let reversed_prefix = buffer_snapshot
            .reversed_chars_at(offset)
            .take(MAX_CONTEXT_CHARS)
            .take_while(|character| *character != '\n')
            .collect::<String>();
        let prefix = reversed_prefix.chars().rev().collect::<String>();
        let suffix = buffer_snapshot
            .chars_at(offset)
            .take(MAX_CONTEXT_CHARS)
            .take_while(|character| *character != '\n')
            .collect::<String>();
        let line = format!("{prefix}{suffix}");

        let Some(number) = number_at_offset(&line, prefix.len()) else {
            return false;
        };
        let Some(text) = line.get(number.range.clone()) else {
            return false;
        };

        let start_offset = offset - prefix.len() + number.range.start;
        let state = NumberDragState {
            start: buffer_snapshot.anchor_before(start_offset),
            text: text.to_string(),
            scaled_value: number.scaled_value,
            decimal_places: number.decimal_places,
            click_position,
            vertical_origin: click_position.y,
            step_magnitude: 0,
            first_transaction: None,
            dragged: false,
        };
        // Starting a drag skips the regular selection handling, which is what focuses the editor.
        if !self.focus_handle.is_focused(window) {
            self.last_focused_descendant = None;
            window.focus(&self.focus_handle, cx);
        }
        self.refresh_number_drag_highlight(&state, cx);
        self.number_drag_state = Some(state);
        true
    }

    /// Ends the current number drag, returning whether it changed the number.
    pub(crate) fn end_number_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(state) = self.number_drag_state.take() else {
            return false;
        };
        self.clear_highlights(HighlightKey::NumberDrag, cx);
        if state.dragged {
            self.run_number_drag_end_actions(&state, window, cx);
        }
        state.dragged
    }

    fn run_number_drag_end_actions(
        &mut self,
        state: &NumberDragState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entries = EditorSettings::get_global(cx)
            .number_drag_end_actions
            .clone();
        if entries.is_empty() {
            return;
        }

        let mut editor_context = self.key_context(window, cx);
        if let Some(language) = self.buffer.read(cx).language_at(state.start, cx) {
            editor_context.set("language", language.name().to_string());
        }
        let mut context_stack = window.context_stack();
        context_stack.push(editor_context);

        for entry in entries {
            if let Some(context) = &entry.context {
                let Some(predicate) = KeyBindingContextPredicate::parse(context).log_err() else {
                    continue;
                };
                if !predicate.eval(&context_stack) {
                    continue;
                }
            }
            for action_name in entry.actions.iter().flatten() {
                if let Some(action) = cx.build_action(action_name, None).log_err() {
                    window.dispatch_action(action, cx);
                }
            }
        }
    }

    /// Returns the display range of the number being dragged, if it sits on a single row.
    pub(crate) fn number_drag_display_range(
        &self,
        snapshot: &DisplaySnapshot,
    ) -> Option<Range<DisplayPoint>> {
        let state = self.number_drag_state.as_ref()?;
        let start = state.start.to_offset(snapshot.buffer_snapshot());
        let end = start + state.text.len();
        let range = start.to_display_point(snapshot)..end.to_display_point(snapshot);
        (range.start.row() == range.end.row()).then_some(range)
    }

    pub(crate) fn is_dragging_number(&self) -> bool {
        self.number_drag_state
            .as_ref()
            .is_some_and(|state| state.dragged)
    }

    fn refresh_number_drag_highlight(&mut self, state: &NumberDragState, cx: &mut Context<Self>) {
        let Some(digit_range) = step_digit_range(&state.text, state.step_magnitude) else {
            self.clear_highlights(HighlightKey::NumberDrag, cx);
            return;
        };
        let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let start = state.start.to_offset(&buffer_snapshot);
        let range = buffer_snapshot.anchor_after(start + digit_range.start)
            ..buffer_snapshot.anchor_before(start + digit_range.end);
        let style = HighlightStyle {
            background_color: Some(cx.theme().players().local().selection),
            ..Default::default()
        };
        self.highlight_text(HighlightKey::NumberDrag, vec![range], style, cx);
    }

    pub(crate) fn update_number_drag(
        &mut self,
        mouse_position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut state) = self.number_drag_state.take() else {
            return;
        };

        let horizontal_distance = (mouse_position.x - state.click_position.x).abs();
        let step_magnitude = ((horizontal_distance / PIXELS_PER_STEP_MAGNITUDE) as u32)
            .min(MAX_STEP_MAGNITUDE)
            .min(largest_step_magnitude(&state.text));
        let mut highlight_outdated = step_magnitude != state.step_magnitude;
        state.step_magnitude = step_magnitude;

        let steps = ((state.vertical_origin - mouse_position.y) / PIXELS_PER_STEP).trunc();
        if steps != 0. {
            // Keep the remainder, so that slow drags still accumulate into steps.
            state.vertical_origin = state.vertical_origin - PIXELS_PER_STEP * steps;
            state.dragged = true;

            let scaled_value = state
                .scaled_value
                .saturating_add((steps as i128).saturating_mul(10i128.pow(step_magnitude)));
            let new_text = format_number(scaled_value, state.decimal_places);

            let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
            let start = state.start.to_offset(&buffer_snapshot);
            let end = start + state.text.len();
            // The number may have been changed by something else since the last step
            // (e.g. a collaborator), in which case overwriting the range would corrupt the text.
            if new_text != state.text && buffer_snapshot.contains_str_at(start, &state.text) {
                let transaction = self.transact(window, cx, |editor, _, cx| {
                    editor.edit([(start..end, new_text.as_str())], cx);
                });
                if let Some(transaction) = transaction {
                    if let Some(first_transaction) = state.first_transaction {
                        self.buffer.update(cx, |buffer, cx| {
                            buffer.group_until_transaction(first_transaction, cx)
                        });
                    } else {
                        state.first_transaction = Some(transaction);
                    }
                    state.text = new_text;
                    state.scaled_value = scaled_value;
                    // The number may have lost digits, e.g. when stepping `100` down by 100.
                    state.step_magnitude = state
                        .step_magnitude
                        .min(largest_step_magnitude(&state.text));
                    highlight_outdated = true;
                }
            }
        }

        if highlight_outdated {
            self.refresh_number_drag_highlight(&state, cx);
        }
        // The indicator line follows the mouse, so it has to be repainted on every move.
        cx.notify();
        self.number_drag_state = Some(state);
    }
}

fn number_at_offset(line: &str, offset: usize) -> Option<NumberInLine> {
    let bytes = line.as_bytes();
    let byte_at = |index: usize| bytes.get(index).copied();
    let byte_before =
        |index: usize| -> Option<u8> { index.checked_sub(1).and_then(byte_at) };
    let is_digit_at = |index: usize| byte_at(index).is_some_and(|byte| byte.is_ascii_digit());
    let digits_start = |mut index: usize| {
        while byte_before(index).is_some_and(|byte| byte.is_ascii_digit()) {
            index -= 1;
        }
        index
    };
    let digits_end = |mut index: usize| {
        while is_digit_at(index) {
            index += 1;
        }
        index
    };

    // A decimal point between two digits belongs to the number just like its digits do.
    let offset = if byte_at(offset) == Some(b'.')
        && byte_before(offset).is_some_and(|byte| byte.is_ascii_digit())
        && is_digit_at(offset + 1)
    {
        offset - 1
    } else {
        offset
    };
    if !is_digit_at(offset) {
        return None;
    }

    let mut start = digits_start(offset);
    let mut end = digits_end(offset);
    let mut decimal_places = 0;
    if byte_at(end) == Some(b'.') && is_digit_at(end + 1) {
        let fraction_end = digits_end(end + 1);
        decimal_places = fraction_end - (end + 1);
        end = fraction_end;
    } else if byte_before(start) == Some(b'.')
        && start >= 2
        && byte_before(start - 1).is_some_and(|byte| byte.is_ascii_digit())
    {
        decimal_places = end - start;
        start = digits_start(start - 1);
    }

    // Reject dotted sequences such as version numbers or IP addresses.
    if byte_at(end) == Some(b'.') && is_digit_at(end + 1) {
        return None;
    }
    if byte_before(start) == Some(b'.')
        && start >= 2
        && byte_before(start - 1).is_some_and(|byte| byte.is_ascii_digit())
    {
        return None;
    }

    // Reject digits that are part of an identifier, a hex/binary/octal literal, an exponent
    // or a literal with digit separators, since stepping them as a decimal number would be wrong.
    if byte_before(start)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || !byte.is_ascii())
    {
        return None;
    }
    if byte_at(end) == Some(b'_') {
        return None;
    }
    if line.get(start..end) == Some("0")
        && byte_at(end).is_some_and(|byte| matches!(byte.to_ascii_lowercase(), b'x' | b'b' | b'o'))
    {
        return None;
    }

    let digit_count = line
        .get(start..end)?
        .bytes()
        .filter(|byte| byte.is_ascii_digit())
        .count();
    if digit_count > MAX_DIGITS {
        return None;
    }
    let mut scaled_value = line
        .get(start..end)?
        .bytes()
        .filter(|byte| byte.is_ascii_digit())
        .fold(0i128, |value, digit| value * 10 + (digit - b'0') as i128);

    // A minus counts as the number's sign unless it follows an operand, where it is a subtraction.
    if byte_before(start) == Some(b'-')
        && !byte_before(start - 1).is_some_and(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b')' | b']') || !byte.is_ascii()
        })
    {
        start -= 1;
        scaled_value = -scaled_value;
    }

    Some(NumberInLine {
        range: start..end,
        scaled_value,
        decimal_places: decimal_places as u32,
    })
}

/// Returns the line from the mouse towards the dragged number, which ends at a fixed distance
/// from the number so that it never covers it. Returns `None` when the mouse is too close.
pub(crate) fn indicator_line(
    mouse_position: gpui::Point<Pixels>,
    number_bounds: Bounds<Pixels>,
) -> Option<(gpui::Point<Pixels>, gpui::Point<Pixels>)> {
    let center = number_bounds.center();
    let half_width = f32::from(number_bounds.size.width) / 2.;
    let half_height = f32::from(number_bounds.size.height) / 2.;
    let radius = half_width.hypot(half_height) + f32::from(INDICATOR_GAP);

    let horizontal_offset = f32::from(mouse_position.x - center.x);
    let vertical_offset = f32::from(mouse_position.y - center.y);
    let distance = horizontal_offset.hypot(vertical_offset);
    if distance <= radius {
        return None;
    }

    let scale = radius / distance;
    let end = point(
        center.x + px(horizontal_offset * scale),
        center.y + px(vertical_offset * scale),
    );
    Some((mouse_position, end))
}

/// Returns the magnitude of the step that changes the most significant digit of `text`.
fn largest_step_magnitude(text: &str) -> u32 {
    let digit_count = text.bytes().filter(|byte| byte.is_ascii_digit()).count();
    digit_count.saturating_sub(1) as u32
}

/// Returns the range within `text` of the digit that a step of `10^step_magnitude` changes.
fn step_digit_range(text: &str, step_magnitude: u32) -> Option<Range<usize>> {
    let (index, _) = text
        .bytes()
        .enumerate()
        .rev()
        .filter(|(_, byte)| byte.is_ascii_digit())
        .nth(step_magnitude as usize)?;
    Some(index..index + 1)
}

fn format_number(scaled_value: i128, decimal_places: u32) -> String {
    let sign = if scaled_value < 0 { "-" } else { "" };
    let magnitude = scaled_value.unsigned_abs();
    if decimal_places == 0 {
        return format!("{sign}{magnitude}");
    }
    let divisor = 10u128.pow(decimal_places);
    format!(
        "{sign}{}.{:0width$}",
        magnitude / divisor,
        magnitude % divisor,
        width = decimal_places as usize
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn number(line: &str, offset: usize) -> Option<(&str, i128, u32)> {
        let number = number_at_offset(line, offset)?;
        Some((
            line.get(number.range)?,
            number.scaled_value,
            number.decimal_places,
        ))
    }

    #[test]
    fn test_number_at_offset() {
        assert_eq!(number("let a = 42;", 8), Some(("42", 42, 0)));
        assert_eq!(number("let a = 42;", 9), Some(("42", 42, 0)));
        assert_eq!(number("let a = 42;", 10), None);
        assert_eq!(number("let a = 42;", 4), None);

        assert_eq!(number("x = -7", 5), Some(("-7", -7, 0)));
        assert_eq!(number("x-7", 2), Some(("7", 7, 0)));
        assert_eq!(number("f(x)-7", 5), Some(("7", 7, 0)));
        assert_eq!(number("-7", 1), Some(("-7", -7, 0)));

        assert_eq!(number("1.25", 0), Some(("1.25", 125, 2)));
        assert_eq!(number("1.25", 3), Some(("1.25", 125, 2)));
        assert_eq!(number("1.25", 1), Some(("1.25", 125, 2)));
        assert_eq!(number("x = -10.5;", 7), Some(("-10.5", -105, 1)));
        assert_eq!(number("0..10", 1), None);
        assert_eq!(number("tuple.0", 5), None);
        assert_eq!(number("1.2.3", 1), None);
        assert_eq!(number("opacity: -0.50;", 13), Some(("-0.50", -50, 2)));
        assert_eq!(number("0..10", 0), Some(("0", 0, 0)));
        assert_eq!(number("0..10", 3), Some(("10", 10, 0)));
        assert_eq!(number("tuple.0", 6), Some(("0", 0, 0)));

        assert_eq!(number("width: 10px", 7), Some(("10", 10, 0)));
        assert_eq!(number("1.5em", 2), Some(("1.5", 15, 1)));

        assert_eq!(number("1.2.3", 0), None);
        assert_eq!(number("1.2.3", 2), None);
        assert_eq!(number("1.2.3", 4), None);
        assert_eq!(number("foo2", 3), None);
        assert_eq!(number("0x1F", 0), None);
        assert_eq!(number("0x1F", 2), None);
        assert_eq!(number("1_000", 0), None);
        assert_eq!(number("1_000", 3), None);
        assert_eq!(number(&"9".repeat(MAX_DIGITS + 1), 0), None);
    }

    #[test]
    fn test_indicator_line() {
        let number_bounds = Bounds::new(point(px(100.), px(100.)), gpui::size(px(60.), px(80.)));
        // The center is (130, 140) and the radius is 50 plus the gap.
        let radius = px(50.) + INDICATOR_GAP;

        assert_eq!(indicator_line(point(px(130.), px(140.)), number_bounds), None);
        assert_eq!(indicator_line(point(px(130.), px(100.)), number_bounds), None);

        let assert_line_end = |mouse_position, expected_end: gpui::Point<Pixels>| {
            let (start, end) = indicator_line(mouse_position, number_bounds).unwrap();
            assert_eq!(start, mouse_position);
            assert!((end.x - expected_end.x).abs() < px(0.01), "{end:?}");
            assert!((end.y - expected_end.y).abs() < px(0.01), "{end:?}");
        };
        assert_line_end(
            point(px(130.), px(40.)),
            point(px(130.), px(140.) - radius),
        );
        assert_line_end(
            point(px(330.), px(140.)),
            point(px(130.) + radius, px(140.)),
        );
    }

    #[test]
    fn test_largest_step_magnitude() {
        assert_eq!(largest_step_magnitude("7"), 0);
        assert_eq!(largest_step_magnitude("42"), 1);
        assert_eq!(largest_step_magnitude("-0.50"), 2);
        for text in ["7", "42", "-0.50"] {
            assert!(step_digit_range(text, largest_step_magnitude(text)).is_some());
        }
    }

    #[test]
    fn test_step_digit_range() {
        assert_eq!(step_digit_range("42", 0), Some(1..2));
        assert_eq!(step_digit_range("42", 1), Some(0..1));
        assert_eq!(step_digit_range("42", 2), None);
        assert_eq!(step_digit_range("-0.50", 0), Some(4..5));
        assert_eq!(step_digit_range("-0.50", 1), Some(3..4));
        assert_eq!(step_digit_range("-0.50", 2), Some(1..2));
        assert_eq!(step_digit_range("-0.50", 3), None);
    }

    #[test]
    fn test_format_number() {
        assert_eq!(format_number(42, 0), "42");
        assert_eq!(format_number(-42, 0), "-42");
        assert_eq!(format_number(125, 2), "1.25");
        assert_eq!(format_number(-5, 2), "-0.05");
        assert_eq!(format_number(0, 2), "0.00");
        assert_eq!(format_number(100, 1), "10.0");
    }
}
