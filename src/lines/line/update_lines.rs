use std::borrow::Cow;
use floem::text::{Attrs, FamilyOwned, LineHeightValue};
use lapce_xi_rope::Interval;
use crate::lines::{DocLines};
use crate::lines::buffer::rope_text::RopeText;
use crate::lines::delta_compute::{Offset, origin_lines_delta, OriginLinesDelta};
use crate::lines::line::{OriginFoldedLine, OriginLine};
use anyhow::Result;

impl DocLines {

    pub fn init_all_origin_line_new(
        &self,
        lines_delta: &mut Option<OriginLinesDelta>
    ) -> Result<Vec<OriginLine>> {
        let (recompute_first_line, copy_line_start_offset, copy_line_start, recompute_line_start, recompute_offset_end, copy_line_end, copy_line_end_offset, _, recompute_last_line) = origin_lines_delta(lines_delta);

        let mut origin_lines = Vec::with_capacity(self.buffer().num_lines());
        let mut line_index = 0;
        if recompute_first_line {
            let line = self.init_origin_line(0)?;
            origin_lines.push(line);
            line_index += 1;
        }
        if !copy_line_start.is_empty() {
            origin_lines.extend(self.copy_origin_line(copy_line_start, copy_line_start_offset, line_index));
        }
        let last_line = self.buffer().last_line();
        for x in recompute_line_start..=last_line {
            let line = self.init_origin_line(x)?;
            let end = line.start_offset + line.len;
            origin_lines.push(line);
            if end >= recompute_offset_end {
                break;
            }
        }
        if !copy_line_end.is_empty() {
            let line_offset = Offset::new(copy_line_end.start, origin_lines.len());
            if let Some(delta) = lines_delta {
                delta.copy_line_end.line_offset = line_offset;
            }
            origin_lines.extend(self.copy_origin_line(copy_line_end, copy_line_end_offset, origin_lines.len()));
        }
        if recompute_last_line {
            origin_lines.push(self.init_origin_line(last_line)?);
        }
        Ok(origin_lines)
    }

    // pub fn init_all_origin_folded_line_new(
    //     &mut self,
    //     lines_delta: &Option<OriginLinesDelta>, all_origin_lines: &[OriginLine]
    // ) -> Result<Vec<OriginLine>> {
    //
    //     let font_size = self.config.font_size;
    //     let family =
    //         Cow::Owned(FamilyOwned::parse_list(&self.config.font_family).collect());
    //     let attrs = Attrs::new()
    //         .color(self.editor_style.ed_text_color())
    //         .family(&family)
    //         .font_size(font_size as f32)
    //         .line_height(LineHeightValue::Px(self.line_height as f32));
    //
    //     let (recompute_first_line, copy_line_start_offset, copy_line_start, recompute_line_start, recompute_offset_end, _, _copy_line_end_offset, copy_line_end_line_offset, recompute_last_line) = origin_lines_delta(lines_delta);
    //
    //     let mut origin_folded_lines = Vec::with_capacity(self.buffer().num_lines());
    //     let mut origin_line_index = 0;
    //     let mut origin_folded_line_index = 0;
    //     if !recompute_first_line && !copy_line_start.is_empty() {
    //         origin_folded_lines.extend(self.copy_origin_folded_line(copy_line_start, copy_line_start_offset, Offset::None, &mut origin_line_index));
    //     }
    //     let mut x = origin_folded_lines.last().map(|x| x.origin_line_end).unwrap_or_default().max(recompute_line_start);
    //     let last_line = self.buffer().last_line();
    //     while x <= last_line  {
    //         let line = self.init_folded_line(x, all_origin_lines, font_size, attrs, origin_folded_lines.len())?;
    //         x = line.origin_line_end + 1;
    //         let end = line.origin_interval.end;
    //         origin_folded_lines.push(line);
    //         if end >= recompute_offset_end {
    //             // break;
    //         }
    //     }
    //     // if !copy_line_end.is_empty() {
    //     //     origin_folded_lines.extend(self.copy_origin_line(copy_line_end, copy_line_end_offset, &mut origin_line_index));
    //     // }
    //     // if recompute_last_line {
    //     //     origin_folded_lines.push(self.init_folded_line(last_line)?);
    //     // }
    //     Ok(origin_folded_lines)
    // }

    fn init_folded_line(&mut self, current_origin_line: usize, all_origin_lines: &[OriginLine], font_size: usize, attrs: Attrs, origin_folded_line_index: usize) -> Result<OriginFoldedLine> {
        let (text_layout, semantic_styles, diagnostic_styles) = self
            .new_text_layout_2(
                current_origin_line,
                all_origin_lines,
                font_size,
                attrs
            )?;
        // duration += time.elapsed().unwrap();
        let origin_line_start = text_layout.phantom_text.line;
        let origin_line_end = text_layout.phantom_text.last_line;

        let width = text_layout.text.size().width;
        if width > self.max_width {
            self.max_width = width;
        }
        let origin_interval = Interval {
            start: self.buffer().offset_of_line(origin_line_start)?,
            end:   self.buffer().offset_of_line(origin_line_end + 1)?
        };

        Ok(OriginFoldedLine {
            line_index: origin_folded_line_index,
            origin_line_start,
            origin_line_end,
            origin_interval,
            text_layout,
            semantic_styles,
            diagnostic_styles
        })
    }
    fn copy_origin_line<'a>(&'a self, copy_line: Interval, offset: Offset, line_index: usize) -> impl IntoIterator<Item = OriginLine> + 'a {
        let line_offset = Offset::new(copy_line.start, line_index);
        (&self.origin_lines[copy_line.start..copy_line.end]).into_iter().map(move |x| {
            x.adjust(offset, line_offset)
        })
    }

    fn copy_origin_folded_line<'a>(&'a self, copy_line: Interval, offset: Offset, line_offset: Offset, line_index: &'a mut usize) -> impl IntoIterator<Item = OriginFoldedLine> + 'a {
        self.origin_folded_lines
            .iter()
            .filter_map(move |folded| {
                if copy_line.start <= folded.origin_line_start
                    && folded.origin_line_end < copy_line.end
                {
                    let mut x = folded.clone();
                    x.line_index = *line_index;
                    *line_index += 1;
                    x.adjust(offset, line_offset);
                    Some(x)
                } else {
                    None
                }
            })
    }
}