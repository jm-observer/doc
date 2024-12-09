use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, atomic};
use std::sync::atomic::AtomicUsize;

use floem::context::StyleCx;
use floem::kurbo::{Point, Rect};
use floem::peniko::{Brush, Color};
use floem::reactive::{
    batch, ReadSignal, RwSignal, Scope, SignalGet, SignalUpdate, SignalWith,
};
use floem::text::{
    Attrs, AttrsList, FamilyOwned, FONT_SYSTEM, LineHeightValue, TextLayout, Wrap,
};
use floem::views::editor::EditorStyle;
use layout::{TextLayoutLine};
use floem::views::editor::listener::Listener;
use phantom_text::{
    PhantomText, PhantomTextKind, PhantomTextLine, PhantomTextMultiLine,
};
use floem::views::editor::text::{PreeditData, SystemClipboard, WrapMethod};
use floem::views::editor::visual_line::{hit_position_aff, LayoutEvent, VLine, VLineInfo};
use floem_editor_core::command::EditCommand;
use floem_editor_core::indent::IndentStyle;
use floem_editor_core::line_ending::LineEnding;
use floem_editor_core::mode::MotionMode;
use floem_editor_core::register::Register;
use itertools::Itertools;
use lapce_xi_rope::{Interval, Rope, RopeDelta, Transformer};
use lapce_xi_rope::spans::{Spans, SpansBuilder};
use lsp_types::{DiagnosticSeverity, InlayHint, InlayHintLabel, Location, Position};
use smallvec::SmallVec;
use log::{info, warn};
use line::{OriginFoldedLine, OriginLine, VisualLine};
use signal::Signals;
use style::NewLineStyle;

use crate::{DiagnosticData, EditorViewKind};
use crate::lines::action::UpdateFolding;
use crate::config::EditorConfig;
use crate::lines::buffer::{Buffer, InvalLines};
use crate::lines::buffer::rope_text::RopeText;
use crate::lines::cursor::{Cursor, CursorAffinity, CursorMode};
use crate::lines::edit::{Action, EditConf, EditType};
use crate::lines::encoding::{offset_utf16_to_utf8, offset_utf8_to_utf16};
use crate::lines::fold::{FoldingDisplayItem, FoldingRanges};
use crate::lines::phantom_text::Text;
use crate::lines::screen_lines::{ScreenLines};
use crate::lines::selection::Selection;
use crate::lines::word::{CharClassification, get_char_property};
use crate::syntax::{BracketParser, Syntax};
use crate::syntax::edit::SyntaxEdit;

pub mod action;
pub mod diff;
pub mod fold;
pub mod screen_lines;
pub mod encoding;
pub mod line;
pub mod util;
mod signal;
mod style;
pub mod phantom_text;
pub mod layout;
pub mod edit;
pub mod buffer;
pub mod indent;
pub mod cursor;
pub mod word;
pub mod selection;

// /// Minimum width that we'll allow the view to be wrapped at.
// const MIN_WRAPPED_WIDTH: f32 = 100.0;

#[derive(Clone)]
pub struct LinesOfOriginOffset {
    pub origin_offset: usize,
    pub origin_line: OriginLine,
    pub origin_folded_line: OriginFoldedLine,
    // 在折叠行的偏移值
    pub origin_folded_line_offest: usize,
    pub visual_line: VisualLine,
    // 在视觉行的偏移值
    pub visual_line_offest: usize,
}

#[derive(Clone, Copy)]
pub struct DocLinesManager {
    lines: RwSignal<DocLines>,
}

impl DocLinesManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cx: Scope,
        diagnostics: DiagnosticData,
        syntax: Syntax,
        parser: BracketParser,
        viewport: Rect,
        editor_style: EditorStyle,
        config: EditorConfig,
        buffer: Buffer,
        kind: RwSignal<EditorViewKind>,
    ) -> Self {
        Self {
            lines: cx.create_rw_signal(DocLines::new(
                cx,
                diagnostics,
                syntax,
                parser,
                viewport,
                editor_style,
                config,
                buffer,
                kind,
            )),
        }
    }

    pub fn with_untracked<O>(&self, f: impl FnOnce(&DocLines) -> O) -> O {
        self.lines.with_untracked(f)
    }

    pub fn get(&self) -> DocLines {
        self.lines.get()
    }

    pub fn update(&self, f: impl FnOnce(&mut DocLines)) {
        // not remove `batch`!
        batch(|| {
            self.lines.update(f);
        });
    }

    pub fn try_update<O>(
        &self,
        f: impl FnOnce(&mut DocLines) -> O,
    ) -> Option<O> {
        // not remove `batch`!
        batch(|| self.lines.try_update(f))
    }

    pub fn lines_of_origin_offset(
        &self,
        origin_offset: usize,
    ) -> LinesOfOriginOffset {
        self.with_untracked(|x| x.lines_of_origin_offset(origin_offset))
    }
}

#[derive(Clone)]
pub struct DocLines {
    origin_lines: Vec<OriginLine>,
    origin_folded_lines: Vec<OriginFoldedLine>,
    pub visual_lines: Vec<VisualLine>,
    // pub font_sizes: Rc<EditorFontSizes>,
    // font_size_cache_id: FontSizeCacheId,
    // wrap: ResolvedWrap,
    pub layout_event: Listener<LayoutEvent>,
    max_width: f64,

    // editor: Editor
    pub inlay_hints: Option<Spans<InlayHint>>,
    pub completion_lens: Option<String>,
    pub completion_pos: (usize, usize),
    pub folding_ranges: FoldingRanges,
    // pub buffer: Buffer,
    pub diagnostics: DiagnosticData,

    /// Current inline completion text, if any.
    /// This will be displayed even on views that are not focused.
    /// (line, col)
    pub inline_completion: Option<(String, usize, usize)>,
    pub preedit: PreeditData,
    // tree-sitter
    pub syntax: Syntax,
    // lsp
    pub semantic_styles: Option<(Option<String>, Spans<String>)>,
    pub parser: BracketParser,
    pub line_styles: HashMap<usize, Vec<NewLineStyle>>,
    pub editor_style: EditorStyle,
    viewport: Rect,
    pub config: EditorConfig,
    pub buffer: Buffer,
    pub kind: RwSignal<EditorViewKind>,
    pub(crate) signals: Signals,
    style_from_lsp: bool,
    folding_items: Vec<FoldingDisplayItem>,
    pub line_height: usize,
    pub screen_lines: ScreenLines,
    // start from 1
    pub last_line: usize,
}

impl DocLines {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cx: Scope,
        diagnostics: DiagnosticData,
        syntax: Syntax,
        parser: BracketParser,
        viewport: Rect,
        editor_style: EditorStyle,
        config: EditorConfig,
        buffer: Buffer,
        kind: RwSignal<EditorViewKind>,
    ) -> Self {
        let screen_lines = ScreenLines::new(cx, viewport, 0.0);
        let last_line = buffer.last_line() + 1;
        let signals =
            Signals::new(cx, &editor_style, viewport, buffer.rev(), buffer.clone(), screen_lines.clone(), last_line);
        let mut lines = Self {
            signals,
            screen_lines,
            last_line,
            // font_size_cache_id: id,
            layout_event: Listener::new_empty(cx), // font_size_cache_id: id,
            viewport,
            config,
            editor_style,
            origin_lines: vec![],
            origin_folded_lines: vec![],
            visual_lines: vec![],
            max_width: 0.0,

            inlay_hints: None,
            completion_pos: (0, 0),
            folding_ranges: Default::default(),
            // buffer: Buffer::new(""),
            diagnostics,
            completion_lens: None,
            inline_completion: None,
            preedit: PreeditData::new(cx),
            syntax,
            semantic_styles: None,
            parser,
            line_styles: Default::default(),
            buffer,
            kind,
            style_from_lsp: false,
            folding_items: Default::default(),
            line_height: 0,
        };
        lines.update_lines();
        lines
    }

    // pub fn update_cache_id(&mut self) {
    //     let current_id = self.font_sizes.cache_id();
    //     if current_id != self.font_size_cache_id {
    //         self.font_size_cache_id = current_id;
    //         self.update()
    //     }
    // }

    // pub fn update_font_sizes(&mut self, font_sizes: Rc<EditorFontSizes>) {
    //     self.font_sizes = font_sizes;
    //     self.update()
    // }

    fn clear(&mut self) {
        self.origin_lines.clear();
        self.origin_folded_lines.clear();
        self.visual_lines.clear();
        self.max_width = 0.0;
        self.line_height = 0;
        self.last_line = 0;
    }

    fn update_parser(&mut self) {
        if self.syntax.styles.is_some() {
            self.parser.update_code(&self.buffer, Some(&self.syntax));
        } else {
            self.parser.update_code(&self.buffer, None);
        }
    }

    fn update_lines(&mut self) {
        self.clear();
        let last_line = self.buffer.last_line();
        // self.update_parser(buffer);
        let mut current_line = 0;
        let mut origin_folded_line_index = 0;
        let mut visual_line_index = 0;
        self.line_height = self.config.line_height;

        let font_size = self.config.font_size;
        let family = Cow::Owned(
            FamilyOwned::parse_list(&self.config.font_family).collect(),
        );
        let attrs = Attrs::new()
            .color(self.editor_style.ed_text_color())
            .family(&family)
            .font_size(font_size as f32)
            .line_height(LineHeightValue::Px(self.line_height as f32));
        let viewport = self.viewport;
        // let mut duration = Duration::from_secs(0);
        while current_line <= last_line {
            let start_offset = self.buffer.offset_of_line(current_line);
            let end_offset = self.buffer.offset_of_line(current_line + 1);
            // let time = std::time::SystemTime::now();
            let text_layout = self.new_text_layout(
                current_line,
                start_offset,
                end_offset,
                font_size,
                attrs,
                viewport,
            );
            // duration += time.elapsed().unwrap();
            let origin_line_start = text_layout.phantom_text.line;
            let origin_line_end = text_layout.phantom_text.last_line;

            let width = text_layout.text.size().width;
            if width > self.max_width {
                self.max_width = width;
            }

            for origin_line in origin_line_start..=origin_line_end {
                self.origin_lines.push(OriginLine {
                    line_index: origin_line,
                    start_offset,
                });
            }

            let origin_interval = Interval {
                start: self.buffer.offset_of_line(origin_line_start),
                end: self.buffer.offset_of_line(origin_line_end + 1),
            };

            let mut visual_offset_start = 0;
            let mut visual_offset_end;

            // [visual_offset_start..visual_offset_end)
            for (origin_folded_line_sub_index, layout) in
                text_layout.text.line_layout().iter().enumerate()
            {
                if layout.glyphs.is_empty() {
                    self.visual_lines.push(VisualLine {
                        line_index: visual_line_index,
                        origin_interval: Interval::new(
                            origin_interval.end,
                            origin_interval.end,
                        ),
                        visual_interval: Interval::new(
                            visual_offset_start,
                            visual_offset_start,
                        ),
                        origin_line: origin_line_start,
                        origin_folded_line: origin_folded_line_index,
                        origin_folded_line_sub_index: 0,
                        text_layout: text_layout.clone(),
                    });
                    continue;
                }
                visual_offset_end = visual_offset_start + layout.glyphs.len() - 1;
                let offset_info = text_layout
                    .phantom_text
                    .cursor_position_of_final_col(visual_offset_start);
                let origin_interval_start =
                    self.buffer.offset_of_line(offset_info.0) + offset_info.1;
                let offset_info = text_layout
                    .phantom_text
                    .cursor_position_of_final_col(visual_offset_end);

                let origin_interval_end =
                    self.buffer.offset_of_line(offset_info.0) + offset_info.1;
                let origin_interval = Interval {
                    start: origin_interval_start,
                    end: origin_interval_end + 1,
                };

                self.visual_lines.push(VisualLine {
                    line_index: visual_line_index,
                    origin_interval,
                    origin_line: origin_line_start,
                    origin_folded_line: origin_folded_line_index,
                    origin_folded_line_sub_index,
                    text_layout: text_layout.clone(),
                    visual_interval: Interval::new(
                        visual_offset_start,
                        visual_offset_end + 1,
                    ),
                });

                visual_offset_start = visual_offset_end;
                visual_line_index += 1;
            }

            self.origin_folded_lines.push(OriginFoldedLine {
                line_index: origin_folded_line_index,
                origin_line_start,
                origin_line_end,
                origin_interval,
                text_layout,
            });

            current_line = origin_line_end + 1;
            origin_folded_line_index += 1;
        }
        // info!("new_text_layout {:?}", duration);
    }

    // pub fn wrap(&self, viewport: Rect, es: &EditorStyle) -> ResolvedWrap {
    //     match es.wrap_method() {
    //         WrapMethod::None => ResolvedWrap::None,
    //         WrapMethod::EditorWidth => {
    //             ResolvedWrap::Width((viewport.width() as f32).max(MIN_WRAPPED_WIDTH))
    //         }
    //         WrapMethod::WrapColumn { .. } => todo!(),
    //         WrapMethod::WrapWidth { width } => ResolvedWrap::Width(width),
    //     }
    // }

    /// Set the wrapping style
    ///
    /// Does nothing if the wrapping style is the same as the current one.
    /// Will trigger a clear of the text layouts if the wrapping style is different.
    // pub fn set_wrap(&mut self, wrap: ResolvedWrap, _editor: &Editor) {
    //     if wrap == self.wrap {
    //         return;
    //     }
    //     self.wrap = wrap;
    //     // self.update(editor);
    // }

    pub fn max_width(&self) -> f64 {
        self.max_width
    }

    /// ~~视觉~~行的text_layout信息
    pub fn text_layout_of_visual_line(&self, line: usize) -> Arc<TextLayoutLine> {
        self.origin_folded_lines[self.visual_lines[line].origin_folded_line]
            .text_layout
            .clone()
    }

    // 原始行的第一个视觉行。原始行可能会有多个视觉行
    pub fn start_visual_line_of_origin_line(
        &self,
        origin_line: usize,
    ) -> &VisualLine {
        let folded_line = self.folded_line_of_origin_line(origin_line);
        self.start_visual_line_of_folded_line(folded_line.line_index)
    }

    pub fn start_visual_line_of_folded_line(
        &self,
        origin_folded_line: usize,
    ) -> &VisualLine {
        for visual_line in &self.visual_lines {
            if visual_line.origin_folded_line == origin_folded_line {
                return visual_line;
            }
        }
        panic!()
    }

    pub fn folded_line_of_origin_line(
        &self,
        origin_line: usize,
    ) -> &OriginFoldedLine {
        for folded_line in &self.origin_folded_lines {
            if folded_line.origin_line_start <= origin_line
                && origin_line <= folded_line.origin_line_end
            {
                return folded_line;
            }
        }
        panic!()
    }

    pub fn visual_line_of_folded_line_and_sub_index(
        &self,
        origin_folded_line: usize,
        sub_index: usize,
    ) -> &VisualLine {
        for visual_line in &self.visual_lines {
            if visual_line.origin_folded_line == origin_folded_line
                && visual_line.origin_folded_line_sub_index == sub_index
            {
                return visual_line;
            }
        }
        panic!()
    }

    pub fn last_visual_line(&self) -> &VisualLine {
        &self.visual_lines[self.visual_lines.len() - 1]
    }

    /// 原始字符所在的视觉行，以及行的偏移位置和是否是最后一个字符
    pub fn visual_line_of_origin_line_offset(
        &self,
        origin_line: usize,
        offset: usize,
        _affinity: CursorAffinity,
    ) -> (VLineInfo, usize, bool) {
        // 位于的原始行，以及在原始行的起始offset
        // let (origin_line, offset_of_line) = self.font_sizes.doc.with_untracked(|x| {
        //     let text = x.text();
        //     let origin_line = text.line_of_offset(offset);
        //     let origin_line_start_offset = text.offset_of_line(origin_line);
        //     (origin_line, origin_line_start_offset)
        // });
        // let mut offset = offset - offset_of_line;
        let before_cursor = _affinity.before_cursor();
        let folded_line = self.folded_line_of_origin_line(origin_line);
        let mut final_offset = folded_line
            .text_layout
            .phantom_text
            .final_col_of_col(origin_line, offset, before_cursor);
        let folded_line_layout = folded_line.text_layout.text.line_layout();
        let mut sub_line_index = folded_line_layout.len() - 1;
        let mut last_char = false;
        for (index, sub_line) in folded_line_layout.iter().enumerate() {
            if before_cursor && final_offset < sub_line.glyphs.len() {
                sub_line_index = index;
                last_char = final_offset == sub_line.glyphs.len() - 1;
                break;
            } else if before_cursor {
                final_offset -= sub_line.glyphs.len();
            } else if final_offset <= sub_line.glyphs.len() {
                sub_line_index = index;
                last_char = final_offset + 1 >= sub_line.glyphs.len();
                break;
            } else {
                final_offset -= sub_line.glyphs.len();
            }
        }
        let visual_line = self.visual_line_of_folded_line_and_sub_index(
            folded_line.line_index,
            sub_line_index,
        );

        (visual_line.vline_info(), final_offset, last_char)
    }

    pub fn buffer_offset_of_click(
        &self,
        _mode: &CursorMode,
        point: Point,
    ) -> (usize, bool) {
        let info = self.screen_lines.visual_line_of_y(point.y);
        // info.visual_line.origin_line
        let text_layout = &info.visual_line.text_layout;
        let y = text_layout.get_layout_y(info.visual_line.origin_folded_line_sub_index).unwrap_or(0.0);
        let hit_point = text_layout.text.hit_point(Point::new(point.x, y as f64));
        // let index = if hit_point.index {
        //     hit_point.index
        // } else {
        //     hit_point.index.max(1) - 1
        // };
        let (origin_line, origin_col, _offset_of_line) = text_layout
            .phantom_text
            .cursor_position_of_final_col(hit_point.index);
        let offset_of_buffer = self.buffer.offset_of_line_col(origin_line, origin_col);
        (offset_of_buffer, hit_point.is_inside)
    }
    pub fn result_of_left_click(
        &mut self,
        point: Point,
    ) -> ClickResult {
        let info = self.screen_lines.visual_line_of_y(point.y);
        let text_layout = &info.visual_line.text_layout;
        let y = text_layout.get_layout_y(info.visual_line.origin_folded_line_sub_index).unwrap_or(0.0);
        let hit_point = text_layout.text.hit_point(Point::new(point.x, y as f64));
        if let Text::Phantom { text: phantom } = text_layout
            .phantom_text
            .text_of_final_col(hit_point.index) {
            let phantom_offset = hit_point.index - phantom.final_col;
            if let PhantomTextKind::InlayHint = phantom.kind {
                let line = phantom.line as u32;
                let index = phantom.col as u32;
                if let Some(hints) = &self.inlay_hints {
                    if let Some(location) = hints.iter().find_map(|(_, hint)| {
                        if hint.position.line == line
                            && hint.position.character == index
                        {
                            if let InlayHintLabel::LabelParts(parts) = &hint.label {
                                let mut start = 0;
                                for part in parts {
                                    let end = start + part.value.len();
                                    if start <= phantom_offset
                                        && phantom_offset < end
                                    {
                                        return part.location.clone();
                                    }
                                    start = end;
                                }
                            }
                        }
                        None
                    }) {
                        return ClickResult::MatchHint(location);
                    }
                }
            } else if let PhantomTextKind::LineFoldedRang { start_position, .. } = phantom.kind {
                self.update_folding_ranges(start_position.into());
                return ClickResult::MatchFolded;
            }
            ClickResult::MatchWithoutLocation
        } else {
            ClickResult::NoHint
        }
    }

    /// 原始位移字符所在的行信息（折叠行、原始行、视觉行）
    pub fn lines_of_origin_offset(
        &self,
        origin_offset: usize,
    ) -> LinesOfOriginOffset {
        // 位于的原始行，以及在原始行的起始offset
        let origin_line = self
            .buffer
            .line_of_offset(origin_offset);
        let origin_line = self.origin_lines[origin_line];
        let offset = origin_offset - origin_line.start_offset;
        let folded_line = self.folded_line_of_origin_line(origin_line.line_index);
        let origin_folded_line_offest = folded_line
            .text_layout
            .phantom_text
            .final_col_of_col(origin_line.line_index, offset, false);
        let folded_line_layout = folded_line.text_layout.text.line_layout();
        let mut sub_line_index = folded_line_layout.len() - 1;
        let mut visual_line_offest = origin_folded_line_offest;
        for (index, sub_line) in folded_line_layout.iter().enumerate() {
            if visual_line_offest < sub_line.glyphs.len() {
                sub_line_index = index;
                break;
            } else {
                visual_line_offest -= sub_line.glyphs.len();
            }
        }
        let visual_line = self.visual_line_of_folded_line_and_sub_index(
            folded_line.line_index,
            sub_line_index,
        );
        LinesOfOriginOffset {
            origin_offset: 0,
            origin_line,
            origin_folded_line: folded_line.clone(),
            origin_folded_line_offest: 0,
            visual_line: visual_line.clone(),
            visual_line_offest: 0,
        }
    }

    /// 视觉行的偏移位置，对应的上一行的偏移位置（原始文本）和是否为最后一个字符
    pub fn previous_visual_line(
        &self,
        visual_line_index: usize,
        mut line_offset: usize,
        _affinity: CursorAffinity,
    ) -> (VisualLine, usize, bool) {
        let prev_visual_line = &self.visual_lines[visual_line_index.max(1) - 1];
        let mut last_char = 0;
        for (index, layout) in self.origin_folded_lines
            [prev_visual_line.origin_folded_line]
            .text_layout
            .text
            .line_layout()
            .iter()
            .enumerate()
        {
            if index < prev_visual_line.origin_folded_line_sub_index {
                line_offset += layout.glyphs.len();
            } else if index >= prev_visual_line.origin_folded_line_sub_index {
                last_char = layout.glyphs.len() - 1;
                break;
            }
        }
        let (_origin_line, offset_line, _offset_buffer) = self.origin_folded_lines
            [prev_visual_line.origin_folded_line]
            .text_layout
            .phantom_text
            .cursor_position_of_final_col(line_offset);
        (
            prev_visual_line.clone(),
            offset_line,
            offset_line == last_char,
        )
    }

    /// 视觉行的偏移位置，对应的上一行的偏移位置（原始文本）和是否为最后一个字符
    pub fn next_visual_line(
        &self,
        visual_line_index: usize,
        mut line_offset: usize,
        _affinity: CursorAffinity,
    ) -> (VisualLine, usize, bool) {
        let next_visual_line = &self.visual_lines
            [visual_line_index.min(self.visual_lines.len() - 2) + 1];
        let mut last_char = 0;
        for (index, layout) in self.origin_folded_lines
            [next_visual_line.origin_folded_line]
            .text_layout
            .text
            .line_layout()
            .iter()
            .enumerate()
        {
            if index < next_visual_line.origin_folded_line_sub_index {
                line_offset += layout.glyphs.len();
            } else if index >= next_visual_line.origin_folded_line_sub_index {
                last_char = layout.glyphs.len().max(1) - 1;
                break;
            }
        }
        let (_origin_line, offset_line, _offset_buffer) = self.origin_folded_lines
            [next_visual_line.origin_folded_line]
            .text_layout
            .phantom_text
            .cursor_position_of_final_col(line_offset);
        (
            next_visual_line.clone(),
            offset_line,
            offset_line == last_char,
        )
    }

    /// 原始位移字符所在的视觉行，以及视觉行的偏移位置，合并行的偏移位置和是否是最后一个字符，point
    pub fn visual_line_of_offset(
        &self,
        offset: usize,
        _affinity: CursorAffinity,
    ) -> (VisualLine, usize, usize, bool) {
        // 位于的原始行，以及在原始行的起始offset
        let (origin_line, offset_of_origin_line) = {
            let origin_line = self.buffer.line_of_offset(offset);
            let origin_line_start_offset = self.buffer.offset_of_line(origin_line);
            (origin_line, origin_line_start_offset)
        };
        let offset = offset - offset_of_origin_line;
        let folded_line = self.folded_line_of_origin_line(origin_line);

        let (sub_line_index, offset_of_visual, offset_of_folded) = folded_line.visual_line_of_line_and_offset(origin_line, offset);
        let visual_line = self.visual_line_of_folded_line_and_sub_index(
            folded_line.line_index,
            sub_line_index,
        );
        let last_char = offset_of_folded >= folded_line.len_without_rn(self.buffer.line_ending());

        (visual_line.clone(), offset_of_visual, offset_of_folded, last_char)
    }

    pub fn visual_lines(&self, start: usize, end: usize) -> Vec<VisualLine> {
        let start = start.min(self.visual_lines.len() - 1);
        let end = end.min(self.visual_lines.len() - 1);

        let mut vline_infos = Vec::with_capacity(end - start + 1);
        for index in start..=end {
            vline_infos.push(self.visual_lines[index].clone());
        }
        vline_infos
    }

    pub fn vline_infos(&self, start: usize, end: usize) -> Vec<VLineInfo<VLine>> {
        let start = start.min(self.visual_lines.len() - 1);
        let end = end.min(self.visual_lines.len() - 1);

        let mut vline_infos = Vec::with_capacity(end - start + 1);
        for index in start..=end {
            vline_infos.push(self.visual_lines[index].vline_info());
        }
        vline_infos
    }

    pub fn first_vline_info(&self) -> VLineInfo<VLine> {
        self.visual_lines[0].vline_info()
    }

    fn phantom_text(
        &self,
        line: usize,
    ) -> PhantomTextLine {
        let (start_offset, end_offset) =
            (self.buffer.offset_of_line(line), self.buffer.offset_of_line(line + 1));

        let origin_text_len = end_offset - start_offset;
        // lsp返回的字符包括换行符，现在长度不考虑，后续会有问题
        // let line_ending = self.buffer.line_ending().get_chars().len();
        // if origin_text_len >= line_ending {
        //     origin_text_len -= line_ending;
        // }
        // if line == 8 {
        //     tracing::info!("start_offset={start_offset} end_offset={end_offset} line_ending={line_ending} origin_text_len={origin_text_len}");
        // }

        let folded_ranges =
            self.folding_ranges.get_folded_range_by_line(line as u32);

        // If hints are enabled, and the hints field is filled, then get the hints for this line
        // and convert them into PhantomText instances
        let hints = self.config
            .enable_inlay_hints
            .then_some(())
            .and(self.inlay_hints.as_ref())
            .map(|hints| hints.iter_chunks(start_offset..end_offset))
            .into_iter()
            .flatten()
            .filter(|(interval, hint)| {
                interval.start >= start_offset
                    && interval.start < end_offset
                    && !folded_ranges.contain_position(hint.position)
            })
            .map(|(interval, inlay_hint)| {
                let (col, affinity) = {
                    let mut cursor =
                        lapce_xi_rope::Cursor::new(self.buffer.text(), interval.start);

                    let next_char = cursor.peek_next_codepoint();
                    let prev_char = cursor.prev_codepoint();

                    let mut affinity = None;
                    if let Some(prev_char) = prev_char {
                        let c = get_char_property(prev_char);
                        if c == CharClassification::Other {
                            affinity = Some(CursorAffinity::Backward)
                        } else if matches!(
                            c,
                            CharClassification::Lf
                                | CharClassification::Cr
                                | CharClassification::Space
                        ) {
                            affinity = Some(CursorAffinity::Forward)
                        }
                    };
                    if affinity.is_none() {
                        if let Some(next_char) = next_char {
                            let c = get_char_property(next_char);
                            if c == CharClassification::Other {
                                affinity = Some(CursorAffinity::Forward)
                            } else if matches!(
                                c,
                                CharClassification::Lf
                                    | CharClassification::Cr
                                    | CharClassification::Space
                            ) {
                                affinity = Some(CursorAffinity::Backward)
                            }
                        }
                    }

                    let (_, col) = self.buffer.offset_to_line_col(interval.start);
                    (col, affinity)
                };
                let mut text = match &inlay_hint.label {
                    InlayHintLabel::String(label) => label.to_string(),
                    InlayHintLabel::LabelParts(parts) => {
                        parts.iter().map(|p| &p.value).join("")
                    }
                };
                match (text.starts_with(':'), text.ends_with(':')) {
                    (true, true) => {
                        text.push(' ');
                    }
                    (true, false) => {
                        text.push(' ');
                    }
                    (false, true) => {
                        text = format!(" {} ", text);
                    }
                    (false, false) => {
                        text = format!(" {}", text);
                    }
                }
                PhantomText {
                    kind: PhantomTextKind::InlayHint,
                    col,
                    text,
                    affinity,
                    fg: Some(self.config.inlay_hint_fg),
                    // font_family: Some(self.config.inlay_hint_font_family()),
                    font_size: Some(self.config.inlay_hint_font_size()),
                    bg: Some(self.config.inlay_hint_bg),
                    under_line: None,
                    final_col: col,
                    line,
                    merge_col: col,
                }
            });
        // You're quite unlikely to have more than six hints on a single line
        // this later has the diagnostics added onto it, but that's still likely to be below six
        // overall.
        let mut text: SmallVec<[PhantomText; 6]> = hints.collect();

        // If error lens is enabled, and the diagnostics field is filled, then get the diagnostics
        // that end on this line which have a severity worse than HINT and convert them into
        // PhantomText instances

        let mut diag_text: SmallVec<[PhantomText; 6]> = self.config
            .enable_error_lens
            .then_some(())
            .map(|_| self.diagnostics.diagnostics_span.get_untracked())
            .map(|diags| {
                diags
                    .iter_chunks(start_offset..end_offset)
                    .filter_map(|(iv, diag)| {
                        let end = iv.end();
                        let end_line = self.buffer.line_of_offset(end);
                        if end_line == line
                            && diag.severity < Some(DiagnosticSeverity::HINT)
                            && !folded_ranges.contain_position(diag.range.start)
                            && !folded_ranges.contain_position(diag.range.end)
                        {
                            let fg = {
                                let severity = diag
                                    .severity
                                    .unwrap_or(DiagnosticSeverity::WARNING);
                                self.config.color_of_error_lens(severity)
                            };

                            let text = if self.config.only_render_error_styling {
                                "".to_string()
                            } else if self.config.error_lens_multiline {
                                format!("    {}", diag.message)
                            } else {
                                format!("    {}", diag.message.lines().join(" "))
                            };
                            Some(PhantomText {
                                kind: PhantomTextKind::Diagnostic,
                                col: end_offset - start_offset,
                                affinity: Some(CursorAffinity::Backward),
                                text,
                                fg: Some(fg),
                                font_size: Some(
                                    self.config.error_lens_font_size(),
                                ),
                                bg: None,
                                under_line: None,
                                final_col: end_offset - start_offset,
                                line,
                                merge_col: end_offset - start_offset,
                            })
                        } else {
                            None
                        }
                    })
                    .collect::<SmallVec<[PhantomText; 6]>>()
            })
            .unwrap_or_default();

        text.append(&mut diag_text);

        let (completion_line, completion_col) = self.completion_pos;
        let completion_text = self.config
            .enable_completion_lens
            .then_some(())
            .and(self.completion_lens.as_ref())
            // TODO: We're probably missing on various useful completion things to include here!
            .filter(|_| {
                line == completion_line
                    && !folded_ranges.contain_position(Position {
                    line: completion_line as u32,
                    character: completion_col as u32,
                })
            })
            .map(|completion| PhantomText {
                kind: PhantomTextKind::Completion,
                col: completion_col,
                text: completion.clone(),
                fg: Some(self.config.completion_lens_foreground),
                font_size: Some(self.config.completion_lens_font_size()),
                affinity: Some(CursorAffinity::Backward),
                // font_family: Some(self.config.editor.completion_lens_font_family()),
                bg: None,
                under_line: None,
                final_col: completion_col,
                line,
                merge_col: completion_col,
                // TODO: italics?
            });
        if let Some(completion_text) = completion_text {
            text.push(completion_text);
        }

        // TODO: don't display completion lens and inline completion at the same time
        // and/or merge them so that they can be shifted between like multiple inline completions
        // can
        // let (inline_completion_line, inline_completion_col) =
        //     self.inline_completion_pos;
        let inline_completion_text = self.config
            .enable_inline_completion
            .then_some(())
            .and(self.inline_completion.as_ref())
            .filter(|(_, inline_completion_line, inline_completion_col)| {
                line == *inline_completion_line
                    && !folded_ranges.contain_position(Position {
                    line: *inline_completion_line as u32,
                    character: *inline_completion_col as u32,
                })
            })
            .map(|(completion, _, inline_completion_col)| {
                PhantomText {
                    kind: PhantomTextKind::Completion,
                    col: *inline_completion_col,
                    text: completion.clone(),
                    affinity: Some(CursorAffinity::Backward),
                    fg: Some(self.config.completion_lens_foreground),
                    font_size: Some(self.config.completion_lens_font_size()),
                    // font_family: Some(self.config.completion_lens_font_family()),
                    bg: None,
                    under_line: None,
                    final_col: *inline_completion_col,
                    line,
                    merge_col: *inline_completion_col,
                    // TODO: italics?
                }
            });
        if let Some(inline_completion_text) = inline_completion_text {
            text.push(inline_completion_text);
        }

        // todo filter by folded?
        if let Some(preedit) = util::preedit_phantom(
            &self.preedit,
            &self.buffer,
            Some(self.config.editor_foreground),
            line,
        ) {
            text.push(preedit)
        }

        let fg = self.config.inlay_hint_fg;
        let font_size = self.config.inlay_hint_font_size();
        let bg = self.config.inlay_hint_bg;
        text.extend(
            folded_ranges.into_phantom_text(&self.buffer, line, font_size, fg, bg),
        );

        PhantomTextLine::new(line, origin_text_len, start_offset, text)
    }

    #[allow(clippy::too_many_arguments)]
    fn new_text_layout(
        &mut self,
        line: usize,
        start_offset: usize,
        end_offset: usize,
        font_size: usize,
        attrs: Attrs,
        viewport: Rect,
    ) -> Arc<TextLayoutLine> {
        // TODO: we could share text layouts between different editor views given some knowledge of
        let mut line_content = String::new();
        // Get the line content with newline characters replaced with spaces
        // and the content without the newline characters
        // TODO: cache or add some way that text layout is created to auto insert the spaces instead
        // though we immediately combine with phantom text so that's a thing.

        let mut font_system = FONT_SYSTEM.lock();
        {
            let line_content_original = self.buffer.line_content(line);
            util::push_strip_suffix(&line_content_original, &mut line_content);
        }
        let mut diagnostic_styles = Vec::new();
        let mut max_severity: Option<DiagnosticSeverity> = None;
        diagnostic_styles.extend(self.get_line_diagnostic_styles(
            start_offset,
            end_offset,
            &mut max_severity,
            0,
        ));

        let phantom_text = self.phantom_text(line);
        let mut collapsed_line_col = phantom_text.folded_line();

        let mut phantom_text = PhantomTextMultiLine::new(phantom_text);
        let mut attrs_list = AttrsList::new(attrs);
        if let Some(styles) = self.line_styles(line) {
            for (start, end, color) in styles.into_iter() {
                let (Some(start), Some(end)) =
                    (phantom_text.col_at(start), phantom_text.col_at(end))
                else {
                    continue;
                };
                attrs_list.add_span(start..end, attrs.color(color));
            }
        }

        while let Some(collapsed_line) = collapsed_line_col.take() {
            {
                util::push_strip_suffix(
                    self.buffer.line_content(collapsed_line).as_ref(),
                    &mut line_content,
                );
            }

            let offset_col = phantom_text.origin_text_len;
            let next_phantom_text =
                self.phantom_text(collapsed_line);
            let start_offset = self.buffer.offset_of_line(collapsed_line);
            let end_offset = self.buffer.offset_of_line(collapsed_line + 1);
            collapsed_line_col = next_phantom_text.folded_line();
            diagnostic_styles.extend(self.get_line_diagnostic_styles(
                start_offset,
                end_offset,
                &mut max_severity,
                offset_col,
            ));

            phantom_text.merge(next_phantom_text);
            if let Some(styles) = self.line_styles(collapsed_line) {
                for (start, end, color) in styles.into_iter() {
                    let start = start + offset_col;
                    let end = end + offset_col;
                    let (Some(start), Some(end)) =
                        (phantom_text.col_at(start), phantom_text.col_at(end))
                    else {
                        continue;
                    };
                    attrs_list.add_span(start..end, attrs.color(color));
                }
            }
        }
        let phantom_color = self.editor_style.phantom_color();
        phantom_text.add_phantom_style(
            &mut attrs_list,
            attrs,
            font_size,
            phantom_color,
        );

        // if line == 1 {
        //     tracing::info!("\nstart");
        //     for (range, attr) in attrs_list.spans() {
        //         tracing::info!("{range:?} {attr:?}");
        //     }
        //     tracing::info!("");
        // }

        // tracing::info!("{line} {line_content}");
        // TODO: we could move tab width setting to be done by the document
        let final_line_content = phantom_text.final_line_content(&line_content);
        let mut text_layout = TextLayout::new_with_font_system(
            line,
            &final_line_content,
            attrs_list,
            &mut font_system,
        );
        drop(font_system);
        // text_layout.set_tab_width(style.tab_width(edid, line));

        // dbg!(self.editor_style.with(|s| s.wrap_method()));
        match self.editor_style.wrap_method() {
            WrapMethod::None => {}
            WrapMethod::EditorWidth => {
                let width = viewport.width();
                text_layout.set_wrap(Wrap::WordOrGlyph);
                text_layout.set_size(width as f32, f32::MAX);
            }
            WrapMethod::WrapWidth { width } => {
                text_layout.set_wrap(Wrap::WordOrGlyph);
                text_layout.set_size(width, f32::MAX);
            }
            // TODO:
            WrapMethod::WrapColumn { .. } => {}
        }

        // ?
        // let indent_line = self.indent_line(line, &line_content_original);
        // ? comment for performance
        // let offset = self.buffer.first_non_blank_character_on_line(line);
        // let (_, col) = self.buffer.offset_to_line_col(offset);
        // let indent = text_layout.hit_position(col).point.x;
        let indent = 0.0;

        let mut layout_line = TextLayoutLine {
            text: text_layout,
            extra_style: Vec::new(),
            whitespaces: None,
            indent,
            phantom_text,
        };
        // 下划线？背景色？
        util::apply_layout_styles(&mut layout_line);
        self.apply_diagnostic_styles(
            &mut layout_line,
            diagnostic_styles,
            max_severity,
        );

        Arc::new(layout_line)
    }

    // pub fn update_folding_item(&mut self, item: FoldingDisplayItem) {
    //     match item.ty {
    //         FoldingDisplayType::UnfoldStart | FoldingDisplayType::Folded => {
    //             self.folding_ranges.0.iter_mut().find_map(|range| {
    //                 if range.start == item.position {
    //                     range.status.click();
    //                     Some(())
    //                 } else {
    //                     None
    //                 }
    //             });
    //         }
    //         FoldingDisplayType::UnfoldEnd => {
    //             self.folding_ranges.0.iter_mut().find_map(|range| {
    //                 if range.end == item.position {
    //                     range.status.click();
    //                     Some(())
    //                 } else {
    //                     None
    //                 }
    //             });
    //         }
    //     }
    //     self.update_lines();
    // }

    fn update(&mut self) {
        self.update_with_trigger_buffer()
    }

    fn update_with_trigger_buffer(&mut self) {
        self.update_lines();
        batch(|| {
            // don't change the sort of statements
            self.trigger_screen_lines();
            self.trigger_folding_items();
            // self.trigger_buffer_rev(self.buffer.rev());
            // if trigger_buffer {
            //     self.trigger_buffer(self.buffer.clone());
            // }
            self.trigger_last_line(self.buffer.last_line() + 1);
        });

    }

    // pub fn update_folding_ranges(&mut self, new: Vec<FoldingRange>) {
    //     self.folding_ranges.update_ranges(new);
    //     self.update_lines();
    // }

    fn update_completion_lens(&mut self, delta: &RopeDelta) {
        let Some(completion) = &mut self.completion_lens else {
            return;
        };
        let (line, col) = self.completion_pos;
        let offset = self.buffer.offset_of_line_col(line, col);
        if delta.as_simple_insert().is_some() {
            let (iv, new_len) = delta.summary();
            if iv.start() == iv.end()
                && iv.start() == offset
                && new_len <= completion.len()
            {
                // Remove the # of newly inserted characters
                // These aren't necessarily the same as the characters literally in the
                // text, but the completion will be updated when the completion widget
                // receives the update event, and it will fix this if needed.
                // TODO: this could be smarter and use the insert's content
                self.completion_lens = Some(completion[new_len..].to_string());
            }
        }

        // Shift the position by the rope delta
        let mut transformer = Transformer::new(delta);

        let new_offset = transformer.transform(offset, true);
        let new_pos = self.buffer.offset_to_line_col(new_offset);
        self.completion_pos = new_pos;
    }

    /// init by lsp
    fn init_diagnostics_with_buffer(&self) {
        let len = self.buffer.len();
        let diagnostics = self.diagnostics.diagnostics.get_untracked();
        let mut span = SpansBuilder::new(len);
        for diag in diagnostics.into_iter() {
            let start = self.buffer.offset_of_position(&diag.range.start);
            let end = self.buffer.offset_of_position(&diag.range.end);
            // warn!("start={start} end={end} {:?}", diag);
            span.add_span(Interval::new(start, end), diag);
        }
        let span = span.build();
        self.diagnostics.diagnostics_span.set(span);
    }

    fn update_diagnostics(&mut self, delta: &RopeDelta) {
        if self
            .diagnostics
            .diagnostics
            .with_untracked(|d| d.is_empty())
        {
            return;
        }

        self.diagnostics.diagnostics_span.update(|diagnostics| {
            diagnostics.apply_shape(delta);
        });
    }


    fn line_styles(
        &mut self,
        line: usize,
    ) -> Option<Vec<(usize, usize, Color)>> {
        let mut styles: Vec<(usize, usize, Color)> =
            self.line_style(line)?;
        if let Some(bracket_styles) = self.parser.bracket_pos.get(&line) {
            let mut bracket_styles = bracket_styles
                .iter()
                .filter_map(|bracket_style| {
                    if let Some(fg_color) = bracket_style.fg_color.as_ref() {
                        if let Some(fg_color) = self.config.syntax_style_color(fg_color) {
                            return Some((
                                bracket_style.start,
                                bracket_style.end,
                                fg_color,
                            ));
                        }
                    }
                    None
                })
                .collect();
            styles.append(&mut bracket_styles);
        }
        Some(styles)
    }

    // 文本样式，前景色
    fn line_style(
        &mut self,
        line: usize,
    ) -> Option<Vec<(usize, usize, Color)>> {
        // let styles = self.styles();
        let styles = self.line_styles.get(&line)?;
        Some(
            styles
                .iter()
                .filter_map(|x| {
                    if let Some(fg) = &x.fg_color {
                        if let Some(color) = self.config.syntax_style_color(fg) {
                            return Some((
                                x.origin_line_offset_start,
                                x.origin_line_offset_end,
                                color,
                            ));
                        }
                    }
                    None
                })
                .collect(),
        )
        // .entry(line)
        // .or_insert_with(|| {
        //     let line_styles = styles
        //         .map(|styles| {
        //             let text = self.buffer.text();
        //             line_styles(text, line, &styles)
        //         })
        //         .unwrap_or_default();
        //     line_styles
        // })
        // .clone()
    }

    // fn indent_line(
    //     &self,
    //     line: usize,
    //     line_content: &str,
    // ) -> usize {
    //     if line_content.trim().is_empty() {
    //         let offset = self.buffer.offset_of_line(line);
    //         if let Some(offset) = self.syntax.parent_offset(offset) {
    //             return self.buffer.line_of_offset(offset);
    //         }
    //     }
    //     line
    // }

    fn _compute_screen_lines(&mut self) -> ScreenLines {
        // TODO: this should probably be a get since we need to depend on line-height
        // let doc_lines = doc.doc_lines.get_untracked();
        let view_kind = self.kind.get_untracked();
        let base = self.viewport;

        let line_height = self.line_height;
        let (y0, y1) = (base.y0, base.y1);
        // Get the start and end (visual) lines that are visible in the viewport
        let min_val = (y0 / line_height as f64).floor() as usize;
        let max_val = (y1 / line_height as f64).floor() as usize;
        let vline_infos = self.visual_lines(min_val, max_val);
        util::compute_screen_lines(
            view_kind,
            self.viewport,
            vline_infos,
            min_val,
            line_height,
            y0,
        )
    }

    pub fn viewport(&self) -> Rect {
        self.viewport
    }
    pub fn screen_lines(&self) -> ScreenLines {
        self.screen_lines.clone()
    }

    pub fn log(&self) {
        warn!(
            "DocLines viewport={:?} buffer.len()=[{}]",
            self.viewport,
            self.buffer.text().len()
        );
        warn!(
            "{:?}",self.config
        );
        for origin_folded_line in &self.origin_folded_lines {
            warn!("{:?}", origin_folded_line);
        }
        for visual_line in &self.visual_lines {
            warn!("{:?}", visual_line);
        }
        warn!("screen_lines");
        for visual_line in &self.screen_lines.visual_lines {
            warn!("{:?}", visual_line);
        }
        warn!("folding_items");
        for item in &self.folding_items {
            warn!("{:?}", item);
        }
        warn!("folding_ranges");
        for range in &self.folding_ranges.0 {
            warn!("{:?}", range);
        }
    }

    fn apply_diagnostic_styles(
        &self,
        layout_line: &mut TextLayoutLine,
        line_styles: Vec<(usize, usize, Color)>,
        _max_severity: Option<DiagnosticSeverity>,
    ) {
        let layout = &mut layout_line.text;
        let phantom_text = &layout_line.phantom_text;

        // 暂不考虑
        for (start, end, color) in line_styles {
            // col_at(end)可以为空，因为end是不包含的
            let (Some(start), Some(end)) = (phantom_text.col_at(start), phantom_text.col_at(end - 1)) else {
                warn!("line={} start={start}, end={end}, color={color:?} col_at empty", phantom_text.line);
                continue;
            };
            let styles =
                util::extra_styles_for_range(layout, start, end + 1, None, None, Some(color));
            layout_line.extra_style.extend(styles);
        }

        // 不要背景色，因此暂时comment
        // Add the styling for the diagnostic severity, if applicable
        // if let Some(max_severity) = max_severity {
        //     let size = layout_line.text.size();
        //     let x1 = if !config.error_lens_end_of_line {
        //         let error_end_x = size.width;
        //         Some(error_end_x.max(size.width))
        //     } else {
        //         None
        //     };
        //
        //     // TODO(minor): Should we show the background only on wrapped lines that have the
        //     // diagnostic actually on that line?
        //     // That would make it more obvious where it is from and matches other editors.
        //     layout_line.extra_style.push(LineExtraStyle {
        //         x: 0.0,
        //         y: 0.0,
        //         width: x1,
        //         height: size.height,
        //         bg_color: Some(self.config.color_of_error_lens(max_severity)),
        //         under_line: None,
        //         wave_line: None,
        //     });
        // }
    }

    /// return (line,start, end, color)
    fn get_line_diagnostic_styles(
        &self,
        start_offset: usize,
        end_offset: usize,
        max_severity: &mut Option<DiagnosticSeverity>,
        line_offset: usize,
    ) -> Vec<(usize, usize, Color)> {
        self.config
            .enable_error_lens
            .then_some(())
            .map(|_|self.diagnostics.diagnostics_span.with_untracked(|diags| {
            diags
                .iter_chunks(start_offset..end_offset)
                .filter_map(|(iv, diag)| {
                    let start = iv.start();
                    let end = iv.end();
                    let severity = diag.severity?;
                    // warn!("start_offset={start_offset} end_offset={end_offset} interval={iv:?}");
                    if start <= end_offset
                        && start_offset <= end
                        && severity < DiagnosticSeverity::HINT
                    {
                        match (severity, *max_severity) {
                            (severity, Some(max)) => {
                                if severity < max {
                                    *max_severity = Some(severity);
                                }
                            }
                            (severity, None) => {
                                *max_severity = Some(severity);
                            }
                        }
                        let color = self.config.color_of_diagnostic(severity)?;
                        Some((
                            start + line_offset - start_offset,
                            end + line_offset - start_offset,
                            color,
                        ))
                    } else {
                        None
                    }
                })
                .collect()
        })).unwrap_or_default()
    }
    fn update_inlay_hints(&mut self, delta: &RopeDelta) {
        if let Some(hints) = self.inlay_hints.as_mut() {
            hints.apply_shape(delta);
        }
    }
}


type ComputeLines = DocLines;

impl ComputeLines {
    /// return (visual line of offset, offset of visual line, offset of folded line, is last char, position of cursor, line_height)
    ///
    /// last_char should be check in future
    pub fn cursor_position_of_buffer_offset(
        &self,
        offset: usize,
        affinity: CursorAffinity,
    ) -> (VisualLine, usize, usize, bool, Option<Point>, f64) {
        let (vl, offset_of_visual, offset_folded, last_char) = self.visual_line_of_offset(offset, affinity);
        let mut point = hit_position_aff(
            &vl.text_layout.text,
            offset_folded,
            true,
        )
            .point;
        let line_height = self.screen_lines.line_height;
        let screen_line = self.screen_lines.visual_line_info_of_visual_line(vl.origin_folded_line, vl.origin_folded_line_sub_index).cloned();

        let point = if let Some(screen_line) = &screen_line {
            point.y = self.viewport.y0 + screen_line.vline_y;
            Some(point)
        } else {
            None
        };


        (vl, offset_of_visual, offset_folded, last_char, point, line_height)
    }
}

type UpdateLines = DocLines;

impl UpdateLines {
    pub fn init_buffer(&mut self, content: Rope) -> bool {
        self.buffer.init_content(content);
        self.buffer.detect_indent(|| {
            IndentStyle::from_str(self.syntax.language.indent_unit())
        });
        info!("line_ending {:?}", self.buffer.line_ending());
        // let time = std::time::SystemTime::now();
        self.on_update_buffer();
        self.update();
        // info!("update elapsed {:?}", time.elapsed().unwrap());
        true
    }

    pub fn set_line_ending(&mut self, line_ending: LineEnding) {
        self.buffer.set_line_ending(line_ending);
        self.on_update_buffer();
        self.update();
    }

    pub fn edit_buffer(
        &mut self,
        iter: &[(impl AsRef<Selection>, &str)],
        edit_type: EditType,
    ) -> (Rope, RopeDelta, InvalLines) {
        let rs = self.buffer.edit(iter, edit_type);
        self.apply_delta(&rs.1);
        self.on_update_buffer();
        self.update_with_trigger_buffer();
        rs
    }
    pub fn reload_buffer(&mut self, content: Rope, set_pristine: bool) -> (Rope, RopeDelta, InvalLines) {
        let rs = self.buffer.reload(content, set_pristine);
        self.apply_delta(&rs.1);
        self.on_update_buffer();
        self.update_with_trigger_buffer();
        rs
    }

    pub fn execute_motion_mode(
        &mut self,
        cursor: &mut Cursor,
        motion_mode: MotionMode,
        range: Range<usize>,
        is_vertical: bool,
        register: &mut Register,
    ) -> Vec<(Rope, RopeDelta, InvalLines)> {
        let rs = Action::execute_motion_mode(
            cursor,
            &mut self.buffer,
            motion_mode,
            range,
            is_vertical,
            register,
        );
        for delta in &rs {
            self.apply_delta(&delta.1);
        }
        self.on_update_buffer();
        self.update_with_trigger_buffer();
        rs
    }

    pub fn do_edit_buffer(
        &mut self,
        cursor: &mut Cursor,
        cmd: &EditCommand,
        modal: bool,
        register: &mut Register,
        smart_tab: bool,
    ) -> Vec<(Rope, RopeDelta, InvalLines)> {
        let syntax = &self.syntax;
        let mut clipboard = SystemClipboard::new();
        let old_cursor = cursor.mode().clone();
        let deltas =
            Action::do_edit(
                cursor,
                &mut self.buffer,
                cmd,
                &mut clipboard,
                register,
                EditConf {
                    comment_token: syntax.language.comment_token(),
                    modal,
                    smart_tab,
                    keep_indent: true,
                    auto_indent: true,
                },
            );
        if !deltas.is_empty() {
            self.buffer.set_cursor_before(old_cursor);
            self.buffer.set_cursor_after(cursor.mode().clone());
            for delta in &deltas {
                self.apply_delta(&delta.1);
            }
        }
        self.on_update_buffer();
        self.update_with_trigger_buffer();
        deltas
    }

    pub fn do_insert_buffer(
        &mut self,
        cursor: &mut Cursor,
        s: &str,
    ) -> Vec<(Rope, RopeDelta, InvalLines)> {
        let config = &self.config;
        let old_cursor = cursor.mode().clone();
        let syntax = &self.syntax;
        let deltas = Action::insert(
            cursor,
            &mut self.buffer,
            s,
            &|buffer, c, offset| {
                util::syntax_prev_unmatched(buffer, syntax, c, offset)
            },
            config.auto_closing_matching_pairs,
            config.auto_surround,
        );
        self.buffer.set_cursor_before(old_cursor);
        self.buffer.set_cursor_after(cursor.mode().clone());
        for delta in &deltas {
            self.apply_delta(&delta.1);
        }
        self.on_update_buffer();
        self.update_with_trigger_buffer();
        deltas
    }


    pub fn update_semantic_styles(&mut self, semantic_styles: (Option<String>, Spans<String>), rev: u64) -> bool {
        if self.buffer.rev() != rev {
            return false;
        }
        self.semantic_styles = Some(semantic_styles);
        self.update();
        true
    }
    pub fn clear_completion_lens(&mut self) {
        self.completion_lens = None;
        self.update();
    }
    pub fn init_diagnostics(&mut self) {
        self.init_diagnostics_with_buffer();
        self.update();
    }
    pub fn update_viewport_by_scroll(&mut self, viewport: Rect) {
        if self.viewport != viewport {
            self.viewport = viewport;
            self.trigger_screen_lines();
            self.trigger_folding_items();
        }
    }

    pub fn update_config(&mut self, config: EditorConfig) {
        if self.config != config {
            self.config = config;
            self.update();
        }
    }

    fn on_update_buffer(&mut self) {
        if self.syntax.styles.is_some() {
            self.parser.update_code(&self.buffer, Some(&self.syntax));
        } else {
            self.parser.update_code(&self.buffer, None);
        }
        self.init_diagnostics_with_buffer();
        // self.update();
    }
    pub fn update_folding_ranges(&mut self, action: UpdateFolding) {
        match action {
            UpdateFolding::UpdateByItem(item) => {
                self.folding_ranges.update_folding_item(item);
            }
            UpdateFolding::New(ranges) => {
                self.folding_ranges.update_ranges(ranges);
            }
            UpdateFolding::UpdateByPhantom(position) => {
                self.folding_ranges.update_by_phantom(position);
            }
        }
        self.update();
    }

    pub fn update_inline_completion(&mut self, delta: &RopeDelta) {
        let Some((completion, ..)) = self.inline_completion.take() else {
            return;
        };
        let (line, col) = self.completion_pos;
        let offset = self.buffer.offset_of_line_col(line, col);

        // Shift the position by the rope delta
        let mut transformer = Transformer::new(delta);

        let new_offset = transformer.transform(offset, true);
        let new_pos = self.buffer.offset_to_line_col(new_offset);

        if delta.as_simple_insert().is_some() {
            let (iv, new_len) = delta.summary();
            if iv.start() == iv.end()
                && iv.start() == offset
                && new_len <= completion.len()
            {
                // Remove the # of newly inserted characters
                // These aren't necessarily the same as the characters literally in the
                // text, but the completion will be updated when the completion widget
                // receives the update event, and it will fix this if needed.
                self.inline_completion =
                    Some((completion[new_len..].to_string(), new_pos.0, new_pos.1));
            }
        } else {
            self.inline_completion = Some((completion, new_pos.0, new_pos.1));
        }
        self.update();
    }

    pub fn apply_delta(&mut self, delta: &RopeDelta) {
        if self.style_from_lsp {
            if let Some(styles) = &mut self.semantic_styles {
                styles.1.apply_shape(delta);
            }
        } else if let Some(styles) = self.syntax.styles.as_mut() {
            styles.apply_shape(delta);
        }
        self.syntax.lens.apply_delta(delta);
        self.update_diagnostics(delta);
        self.update_inlay_hints(delta);
        self.update_completion_lens(delta);
        //
        // self.update();
    }
    pub fn trigger_syntax_change(
        &mut self,
        _edits: Option<SmallVec<[SyntaxEdit; 3]>>,
    ) {
        self.syntax.cancel_flag.store(1, atomic::Ordering::Relaxed);
        self.syntax.cancel_flag = Arc::new(AtomicUsize::new(0));
        self.update();
    }
    pub fn set_inline_completion(
        &mut self,
        inline_completion: String,
        line: usize,
        col: usize,
    ) {
        self.inline_completion = Some((inline_completion, line, col));
        self.update();
    }

    pub fn clear_inline_completion(&mut self) {
        self.inline_completion = None;
        self.update();
    }

    pub fn set_syntax_with_rev(&mut self, syntax: Syntax, rev: u64) -> bool {
        if self.buffer.rev() != rev {
            return false;
        }
        self.set_syntax(syntax)
    }
    pub fn set_syntax(&mut self, syntax: Syntax) -> bool {
        self.syntax = syntax;
        if self.style_from_lsp {
            return false;
        }
        // if self.semantic_styles.is_none() {
        //     self.line_styles.clear();
        // }
        self.line_styles.clear();
        if let Some(x) = self.syntax.styles.as_ref() {
            x.iter().for_each(|(Interval { start, end }, style)| {
                let origin_line = self.buffer.line_of_offset(start);
                let origin_line_offset = self.buffer.offset_of_line(origin_line);
                let entry = self.line_styles.entry(origin_line).or_default();
                entry.push(NewLineStyle {
                    origin_line,
                    origin_line_offset_start: start - origin_line_offset,
                    origin_line_offset_end: end - origin_line_offset,
                    fg_color: Some(style.clone()),
                });
            })
        };
        self.update_parser();

        self.update();
        true
    }

    pub fn set_inlay_hints(&mut self, inlay_hint: Spans<InlayHint>) {
        self.inlay_hints = Some(inlay_hint);
        self.update();
    }

    pub fn set_completion_lens(
        &mut self,
        completion_lens: String,
        line: usize,
        col: usize,
    ) {
        self.completion_lens = Some(completion_lens);
        self.completion_pos = (line, col);
        self.update();
    }

    pub fn update_semantic_styles_from_lsp(
        &mut self,
        styles: (Option<String>, Spans<String>),
    ) {
        // self.semantic_styles = Some(styles);
        self.style_from_lsp = true;
        styles
            .1
            .iter()
            .for_each(|(Interval { start, end }, fg_color)| {
                let origin_line = self.buffer.line_of_offset(start);
                let origin_line_offset = self.buffer.offset_of_line(origin_line);
                let entry = self.line_styles.entry(origin_line).or_default();
                entry.push(NewLineStyle {
                    origin_line,
                    origin_line_offset_start: start - origin_line_offset,
                    origin_line_offset_end: end - origin_line_offset,
                    fg_color: Some(fg_color.clone()),
                });
            });
        self.update();
    }
}

type LinesEditorStyle = DocLines;

impl LinesEditorStyle {
    pub fn modal(&self) -> bool {
        self.editor_style.modal()
    }
    pub fn current_line_color(&self) -> Option<Color> {
        EditorStyle::current_line(&self.editor_style)
    }
    pub fn scroll_beyond_last_line(&self) -> bool {
        EditorStyle::scroll_beyond_last_line(&self.editor_style)
    }

    pub fn ed_caret(&self) -> Brush {
        self.editor_style.ed_caret()
    }

    pub fn selection_color(&self) -> Color {
        self.editor_style.selection()
    }

    pub fn indent_style(&self) -> IndentStyle {
        self.editor_style.indent_style()
    }

    pub fn indent_guide(&self) -> Color {
        self.editor_style.indent_guide()
    }

    pub fn visible_whitespace(&self) -> Color {
        self.editor_style.visible_whitespace()
    }

    pub fn update_editor_style(&mut self, cx: &mut StyleCx<'_>) -> bool {
        let old_show_indent_guide = self.show_indent_guide();
        // todo
        let updated = self.editor_style.read(cx);

        let new_show_indent_guide = self.show_indent_guide();
        if old_show_indent_guide != new_show_indent_guide {
            self.trigger_show_indent_guide(new_show_indent_guide)
        }
        if updated {
            self.update_lines();
        }
        updated
    }

    pub fn show_indent_guide(&self) -> (bool, Color) {
        (
            self.editor_style.show_indent_guide(),
            self.editor_style.indent_guide(),
        )
    }
}

type LinesSignals = DocLines;

#[allow(dead_code)]
/// 以界面为单位，进行触发。
impl LinesSignals {
    pub fn trigger_screen_lines(&mut self) {
        let screen_lines = self._compute_screen_lines();
        self.screen_lines = screen_lines.clone();
        self.signals.screen_lines_signal.set(screen_lines);
    }
    pub fn screen_lines_signal(&self) -> ReadSignal<ScreenLines> {
        self.signals.screen_lines_signal.read_only()
    }

    pub fn trigger_folding_items(&mut self) {
        let folding_items = self.folding_ranges.to_display_items(&self.screen_lines);
        self.folding_items = folding_items.clone();
        self.signals.folding_items_signal.set(folding_items);
    }

    // pub fn trigger_buffer_rev(&mut self, buffer_rev: u64) {
    //     self.signals.buffer_rev.set(buffer_rev);
    // }
    // pub fn trigger_buffer(&mut self, buffer: Buffer) {
    //     self.signals.buffer.set(buffer);
    // }

    pub fn folding_items_signal(&self) -> ReadSignal<Vec<FoldingDisplayItem>> {
        self.signals.folding_items_signal.read_only()
    }

    fn trigger_viewport(&mut self, viewport: Rect) {
        if self.viewport != viewport {
            self.viewport = viewport;
            self.signals.viewport.set(viewport);
            self.update_lines();
            // todo udpate screen_lines
        }
    }
    pub fn signal_viewport(&self) -> ReadSignal<Rect> {
        self.signals.viewport.read_only()
    }
    fn trigger_show_indent_guide(&self, show_indent_guide: (bool, Color)) {
        self.signals.show_indent_guide.set(show_indent_guide);
    }
    pub fn signal_show_indent_guide(&self) -> ReadSignal<(bool, Color)> {
        self.signals.show_indent_guide.read_only()
    }

    pub fn signal_buffer_rev(&self) -> ReadSignal<u64> {
        self.signals.buffer_rev.read_only()
    }

    pub fn signal_buffer(&self) -> ReadSignal<Buffer> {
        self.signals.buffer.read_only()
    }


    fn trigger_last_line(&mut self, last_line: usize) {
        if self.last_line != last_line {
            self.last_line = last_line;
            self.signals.last_line.set(last_line);
        }
    }
    pub fn signal_last_line(&self) -> ReadSignal<usize> {
        self.signals.last_line.read_only()
    }
}


pub trait RopeTextPosition: RopeText {
    /// Converts a UTF8 offset to a UTF16 LSP position
    /// Returns None if it is not a valid UTF16 offset
    fn offset_to_position(&self, offset: usize) -> Position {
        let (line, col) = self.offset_to_line_col(offset);
        let line_offset = self.offset_of_line(line);

        let utf16_col =
            offset_utf8_to_utf16(self.char_indices_iter(line_offset..), col);

        Position {
            line: line as u32,
            character: utf16_col as u32,
        }
    }

    fn offset_of_position(&self, pos: &Position) -> usize {
        let (line, column) = self.position_to_line_col(pos);

        self.offset_of_line_col(line, column)
    }

    fn position_to_line_col(&self, pos: &Position) -> (usize, usize) {
        let line = pos.line as usize;
        let line_offset = self.offset_of_line(line);

        let column = offset_utf16_to_utf8(
            self.char_indices_iter(line_offset..),
            pos.character as usize,
        );

        (line, column)
    }
}

impl<T: RopeText> RopeTextPosition for T {}

#[derive(Debug)]
pub enum ClickResult {
    NoHint,
    MatchWithoutLocation,
    MatchFolded,
    MatchHint(Location),
}
