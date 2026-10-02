use helix_core::doc_formatter::FormattedGrapheme;
use helix_core::Position;
use helix_view::theme::Style;

use crate::ui::document::{LinePos, TextRenderer};
use crate::ui::text_decorations::Decoration;

/// Renders blame information as virtual text after the end of the cursor line.
pub struct InlineBlame {
    text: String,
    style: Style,
    line: usize,
    /// char index of the line ending of `line`
    line_end: usize,
    /// set once the line ending has been rendered, so that the annotation is only drawn after
    /// the last visual line of a soft-wrapped line
    reached_line_end: bool,
}

impl InlineBlame {
    pub fn new(text: String, style: Style, line: usize, line_end: usize) -> Self {
        InlineBlame {
            text,
            style,
            line,
            line_end,
            reached_line_end: false,
        }
    }
}

impl Decoration for InlineBlame {
    fn reset_pos(&mut self, pos: usize) -> usize {
        if pos <= self.line_end {
            self.line_end
        } else {
            usize::MAX
        }
    }

    fn decorate_grapheme(
        &mut self,
        _renderer: &mut TextRenderer,
        _grapheme: &FormattedGrapheme,
    ) -> usize {
        self.reached_line_end = true;
        usize::MAX
    }

    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        virt_off: Position,
    ) -> Position {
        if !self.reached_line_end || pos.doc_line != self.line {
            return Position::new(0, 0);
        }
        self.reached_line_end = false;

        // leave one cell of space after the line ending (like end of line diagnostics)
        let col = virt_off.col + 1;
        let Some(col) = col.checked_sub(renderer.offset.col) else {
            return Position::new(0, 0);
        };
        let width = renderer.viewport.width as usize;
        if col >= width {
            return Position::new(0, 0);
        }
        let style = self.style;
        let (end_x, _) = renderer.set_string_truncated(
            renderer.viewport.x + col as u16,
            pos.visual_line,
            &self.text,
            width - col,
            |_| style,
            true,
            false,
        );
        let drawn = (end_x - renderer.viewport.x) as usize - col;
        Position::new(0, drawn + 1)
    }
}
