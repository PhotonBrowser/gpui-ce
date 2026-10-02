use crate::{
    CatalogState, FaceFamily, FaceRequest, FontCatalog, FontStore, GlyphRasterizer,
    SwashGlyphRasterizer, SystemFonts,
};
use anyhow::{Context as _, Result};
use gpui::{
    Bounds, CaretAffinity, CaretMovement, CaretPosition, Font, FontId, FontMetrics, GlyphId,
    InlineBoxRequest, InlineLayout, InlineLayoutRequest, InlineRangeGeometry, InlineTextMetrics,
    InlineTextStyle, InlineVisualLine, LineLayout, PaintFragment, PaintStyle, Pixels,
    PlatformTextLayout, PlatformTextSystem, PositionedInlineBox, PreparedRasterStyle,
    RasterStyleRequest, RasterizedGlyph, RenderGlyphParams, ShapedGlyph, Size, TextAlign,
    TextBoundary as Boundary, TextDirection as Direction, TextLayoutRequest, TextMovement,
    TextRenderingMode, TextRun, TextSelectionKind, VisualDirection, VisualLine, align_inline_boxes,
    is_paragraph_separator, point, px, size,
};
use parking_lot::{Mutex, RwLock};
use parley::setting::Tag;
use parley::{
    Affinity, Alignment, AlignmentOptions, CHROMIUM_LINE_BREAK_OVERRIDE, Cluster, Cursor,
    FontContext, FontFamily, FontFamilyName, FontFeature, FontFeatures, FontStyle, FontWeight,
    GenericFamily, InlineBox, InlineBoxKind, Layout, LayoutContext, LineHeight,
    PositionedLayoutItem, Selection, StyleProperty,
};
use skrifa::instance::NormalizedCoord;
use std::{borrow::Cow, ops::Range};
use unicode_segmentation::UnicodeSegmentation as _;

mod paragraphs;

use paragraphs::{ParagraphLayout, ParleyDocumentLayout, local_range, paragraph_ranges};

struct ParleyState {
    fonts: FontContext,
    layout: LayoutContext<PaintStyle>,
}

struct ParleyLayoutParams<'a> {
    text: TextLayoutRequest<'a>,
    inline: Option<ParleyInlineLayoutParams<'a>>,
}

struct ParleyInlineLayoutParams<'a> {
    boxes: &'a [InlineBoxRequest],
    text_styles: &'a [InlineTextStyle],
    line_height: Pixels,
    text_metrics: InlineTextMetrics,
    text_align: TextAlign,
}

struct ParleyLayoutResult {
    layout: LineLayout,
    inline_lines: Vec<InlineVisualLine>,
    inline_boxes: Vec<PositionedInlineBox>,
    size: Size<Pixels>,
    is_rtl: bool,
}

fn inline_alignment_offset(text_align: TextAlign, lines: &[InlineVisualLine]) -> Pixels {
    let Some(line) = lines.first() else {
        return Pixels::ZERO;
    };

    match text_align {
        TextAlign::Left => Pixels::ZERO,
        TextAlign::Center => line.origin.x + line.size.width / 2.,
        TextAlign::Right => line.origin.x + line.size.width,
    }
}

#[derive(Clone, Debug)]
struct ParleyLayout {
    layout: Layout<PaintStyle>,
    inline_lines: Vec<InlineVisualLine>,
    text_len: usize,
    caret_stops: Vec<ParleyCaretStop>,
    graphemes: Vec<std::ops::Range<usize>>,
}

#[derive(Clone, Copy, Debug)]
struct ParleyCaretStop {
    cursor: Cursor,
    block: f64,
    inline: f64,
}

trait IntoAffinity<T> {
    fn into_affinity(self) -> T;
}

impl IntoAffinity<Affinity> for CaretAffinity {
    fn into_affinity(self) -> Affinity {
        match self {
            Self::Downstream => Affinity::Downstream,
            Self::Upstream => Affinity::Upstream,
        }
    }
}

impl IntoAffinity<CaretAffinity> for Affinity {
    fn into_affinity(self) -> CaretAffinity {
        match self {
            Self::Downstream => CaretAffinity::Downstream,
            Self::Upstream => CaretAffinity::Upstream,
        }
    }
}

impl ParleyLayout {
    fn new(layout: Layout<PaintStyle>, text: &str, inline_lines: Vec<InlineVisualLine>) -> Self {
        let graphemes = text
            .grapheme_indices(true)
            .map(|(start, grapheme)| start..start + grapheme.len())
            .collect::<Vec<_>>();
        let caret_stops = Self::collect_caret_stops(&layout, text, &graphemes);
        Self {
            layout,
            inline_lines,
            text_len: text.len(),
            caret_stops,
            graphemes,
        }
    }

    fn caret_position(cursor: Cursor) -> CaretPosition {
        let gpui_affinity = cursor.affinity().into_affinity();
        CaretPosition {
            index: cursor.index(),
            affinity: gpui_affinity,
        }
    }

    fn cursor(&self, caret: CaretPosition) -> Cursor {
        let parley_affinity = caret.affinity.into_affinity();
        Cursor::from_byte_index(&self.layout, caret.index, parley_affinity)
    }

    fn cursor_position(layout: &Layout<PaintStyle>, cursor: Cursor) -> (f64, f64) {
        let geometry = cursor.geometry(layout, 0.0);
        (geometry.y0, geometry.x0)
    }

    fn collect_caret_stops(
        layout: &Layout<PaintStyle>,
        text: &str,
        graphemes: &[std::ops::Range<usize>],
    ) -> Vec<ParleyCaretStop> {
        let boundaries = std::iter::once(0)
            .chain(graphemes.iter().map(|range| range.end))
            .chain(std::iter::once(text.len()));

        let mut stops = Vec::with_capacity((graphemes.len() + 2) * 2);
        for index in boundaries {
            for affinity in [Affinity::Downstream, Affinity::Upstream] {
                let cursor = Cursor::from_byte_index(layout, index, affinity);
                let (block, inline) = Self::cursor_position(layout, cursor);
                stops.push(ParleyCaretStop {
                    cursor,
                    block,
                    inline,
                });
            }
        }

        stops.sort_by(|left, right| {
            left.block
                .total_cmp(&right.block)
                .then_with(|| left.inline.total_cmp(&right.inline))
        });

        let mut unique_stops: Vec<ParleyCaretStop> = Vec::with_capacity(stops.len());
        for mut stop in stops {
            if let Some(previous) = unique_stops.last_mut()
                && previous.block == stop.block
                && previous.inline == stop.inline
            {
                let geometry = stop.cursor.geometry(layout, 0.0);
                let hit = Cursor::from_point(
                    layout,
                    stop.inline as f32,
                    ((geometry.y0 + geometry.y1) * 0.5) as f32,
                );

                if Self::cursor_position(layout, hit) == (stop.block, stop.inline) {
                    previous.cursor = hit;
                }

                continue;
            }

            let geometry = stop.cursor.geometry(layout, 0.0);
            let hit = Cursor::from_point(
                layout,
                stop.inline as f32,
                ((geometry.y0 + geometry.y1) * 0.5) as f32,
            );

            if Self::cursor_position(layout, hit) == (stop.block, stop.inline) {
                stop.cursor = hit;
            }

            unique_stops.push(stop);
        }

        unique_stops
    }

    fn caret_stop_index(&self, cursor: Cursor) -> Option<usize> {
        let (block, inline) = Self::cursor_position(&self.layout, cursor);
        self.caret_stops
            .binary_search_by(|stop| {
                stop.block
                    .total_cmp(&block)
                    .then_with(|| stop.inline.total_cmp(&inline))
            })
            .ok()
    }

    fn adjacent_caret_stop(
        &self,
        cursor: Cursor,
        direction: VisualDirection,
    ) -> Option<ParleyCaretStop> {
        let index = self.caret_stop_index(cursor)?;
        match direction {
            VisualDirection::Left => index
                .checked_sub(1)
                .and_then(|index| self.caret_stops.get(index)),
            VisualDirection::Right => self.caret_stops.get(index + 1),
        }
        .copied()
    }

    fn direct_visual_move(&self, cursor: Cursor, direction: VisualDirection) -> Cursor {
        match direction {
            VisualDirection::Left => cursor.previous_visual(&self.layout),
            VisualDirection::Right => cursor.next_visual(&self.layout),
        }
    }

    fn opposite(direction: VisualDirection) -> VisualDirection {
        match direction {
            VisualDirection::Left => VisualDirection::Right,
            VisualDirection::Right => VisualDirection::Left,
        }
    }

    fn native_y_for_line(&self, line_index: usize) -> f32 {
        self.layout
            .get(line_index)
            .map(|line| {
                let metrics = line.metrics();
                (metrics.block_min_coord + metrics.block_max_coord) * 0.5
            })
            .unwrap_or_else(|| self.layout.height())
    }
}

impl PlatformTextLayout for ParleyLayout {
    fn len(&self) -> usize {
        self.text_len
    }

    fn line_count(&self) -> usize {
        self.layout.len()
    }

    fn size(&self) -> Size<Pixels> {
        let width = if self.text_len == 0 && self.layout.inline_boxes().is_empty() {
            Pixels::ZERO
        } else {
            px(self.layout.width())
        };

        size(width, px(self.layout.height()))
    }

    fn byte_index_from_pixel_point(
        &self,
        pixel_point: gpui::Point<Pixels>,
        line_height: Pixels,
    ) -> std::result::Result<usize, usize> {
        let closest = self
            .caret_from_pixel_point(pixel_point, line_height)
            .unwrap_or_else(|caret| caret)
            .index;
        if self.text_len == 0 || pixel_point.y < Pixels::ZERO || line_height <= Pixels::ZERO {
            return Err(closest);
        }
        let line_index = (pixel_point.y / line_height) as usize;
        let Some(line) = self.layout.get(line_index) else {
            return Err(closest);
        };

        let metrics = line.metrics();
        let left = metrics.inline_min_coord + metrics.offset;
        let right = left + metrics.advance;
        if f32::from(pixel_point.x) < left || f32::from(pixel_point.x) >= right {
            return Err(closest);
        }

        Cluster::from_point(
            &self.layout,
            pixel_point.x.into(),
            self.native_y_for_line(line_index),
        )
        .map(|(cluster, _)| cluster.text_range().start)
        .ok_or(closest)
    }

    fn caret_from_pixel_point(
        &self,
        pixel_point: gpui::Point<Pixels>,
        line_height: Pixels,
    ) -> std::result::Result<CaretPosition, CaretPosition> {
        let line_index = if line_height > px(0.0) && pixel_point.y >= Pixels::ZERO {
            (pixel_point.y / line_height) as usize
        } else {
            0
        };

        let caret = Self::caret_position(Cursor::from_point(
            &self.layout,
            pixel_point.x.into(),
            self.native_y_for_line(line_index),
        ));
        if self.text_len == 0 {
            return Err(caret);
        }

        let Some(line) = self.layout.get(line_index) else {
            return Err(caret);
        };

        let metrics = line.metrics();
        let left = metrics.inline_min_coord + metrics.offset;
        let right = left + metrics.advance;
        if pixel_point.y >= Pixels::ZERO
            && line_height > Pixels::ZERO
            && f32::from(pixel_point.x) >= left
            && f32::from(pixel_point.x) < right
        {
            Ok(caret)
        } else {
            Err(caret)
        }
    }

    fn caret_bounds(&self, caret: CaretPosition, line_height: Pixels) -> Option<Bounds<Pixels>> {
        if caret.index > self.len() {
            return None;
        }

        if self.text_len == 0 && self.layout.inline_boxes().is_empty() {
            return Some(Bounds::new(
                point(self.inline_lines[0].origin.x, Pixels::ZERO),
                size(Pixels::ZERO, line_height),
            ));
        }

        let cursor = self.cursor(caret);
        let geometry = cursor.geometry(&self.layout, 0.0);
        let line_index = self
            .layout
            .lines()
            .position(|line| {
                let metrics = line.metrics();
                geometry.y0 as f32 >= metrics.block_min_coord
                    && (geometry.y0 as f32) < metrics.block_max_coord
            })
            .unwrap_or_else(|| self.layout.len().saturating_sub(1));
        Some(Bounds::from_corners(
            point(px(geometry.x0 as f32), line_height * line_index),
            point(px(geometry.x1 as f32), line_height * (line_index + 1)),
        ))
    }

    fn normalized_caret(&self, caret: CaretPosition) -> CaretPosition {
        Self::caret_position(self.cursor(caret))
    }

    fn adjacent_visual_caret(
        &self,
        caret: CaretPosition,
        direction: VisualDirection,
    ) -> Option<CaretPosition> {
        if self.text_len == 0 && self.layout.inline_boxes().is_empty() {
            return None;
        }

        let cursor = self.cursor(caret);
        let adjacent = self.adjacent_caret_stop(cursor, direction)?;
        let moved = self.direct_visual_move(cursor, direction);
        let moved_position = Self::cursor_position(&self.layout, moved);
        let inverse = self.direct_visual_move(moved, Self::opposite(direction));
        let inverse_position = Self::cursor_position(&self.layout, inverse);
        let current_position = Self::cursor_position(&self.layout, cursor);

        let result = if moved_position == (adjacent.block, adjacent.inline)
            && inverse_position == current_position
        {
            moved
        } else {
            adjacent.cursor
        };

        Some(Self::caret_position(result))
    }

    fn selection_bounds(
        &self,
        byte_range: std::ops::Range<usize>,
        line_height: Pixels,
    ) -> Vec<Bounds<Pixels>> {
        let anchor = Cursor::from_byte_index(&self.layout, byte_range.start, Affinity::Downstream);
        let focus = Cursor::from_byte_index(&self.layout, byte_range.end, Affinity::Upstream);
        Selection::new(anchor, focus)
            .geometry(&self.layout)
            .into_iter()
            .map(|(geometry, line_index)| {
                Bounds::from_corners(
                    point(px(geometry.x0 as f32), line_height * line_index),
                    point(px(geometry.x1 as f32), line_height * (line_index + 1)),
                )
            })
            .collect()
    }

    fn inline_geometry(&self, range: std::ops::Range<usize>) -> Option<Vec<InlineRangeGeometry>> {
        if range.is_empty() {
            return None;
        }

        let cursor_at = |index, affinity| Cursor::from_byte_index(&self.layout, index, affinity);
        let selection = Selection::new(
            cursor_at(range.start, Affinity::Downstream),
            cursor_at(range.end, Affinity::Upstream),
        );
        let mut regions = Vec::new();

        selection.geometry_with(&self.layout, |selection_bounds, line_index| {
            let Some(line) = self.layout.get(line_index) else {
                return;
            };

            let metrics = line.metrics();

            // Parley adds a selection-only extension after explicit newlines.
            let clamped_right = (selection_bounds.x1 as f32)
                .min(metrics.inline_min_coord + metrics.offset + metrics.advance);

            if clamped_right > selection_bounds.x0 as f32 {
                // Inline boxes and styled leading can change the row after Parley positions it.
                let inline_line = self.inline_lines[line_index];
                regions.push(InlineRangeGeometry {
                    bounds: Bounds::from_corners(
                        point(px(selection_bounds.x0 as f32), inline_line.origin.y),
                        point(
                            px(clamped_right),
                            inline_line.origin.y + inline_line.size.height,
                        ),
                    ),
                    visual_line_index: line_index,
                });
            }
        });

        Some(regions)
    }

    fn logical_cluster_before(&self, caret: CaretPosition) -> Option<std::ops::Range<usize>> {
        self.graphemes
            .iter()
            .rev()
            .find(|range| range.start < caret.index)
            .cloned()
    }

    fn logical_cluster_after(&self, caret: CaretPosition) -> Option<std::ops::Range<usize>> {
        self.graphemes
            .iter()
            .find(|range| range.end > caret.index)
            .cloned()
    }

    fn caret_movement(
        &self,
        caret: CaretPosition,
        movement: TextMovement,
        vertical_navigation_x: Option<Pixels>,
    ) -> CaretMovement {
        let cursor = self.cursor(caret);
        let moved = match (movement.direction, movement.boundary) {
            (Direction::Left, Boundary::Cluster) => {
                return CaretMovement {
                    result: self
                        .adjacent_visual_caret(caret, VisualDirection::Left)
                        .unwrap_or(caret),
                    vertical_navigation_x: None,
                };
            }
            (Direction::Right, Boundary::Cluster) => {
                return CaretMovement {
                    result: self
                        .adjacent_visual_caret(caret, VisualDirection::Right)
                        .unwrap_or(caret),
                    vertical_navigation_x: None,
                };
            }
            (Direction::Left, Boundary::Word) => cursor.previous_visual_word(&self.layout),
            (Direction::Right, Boundary::Word) => cursor.next_visual_word(&self.layout),
            (Direction::Start, Boundary::VisualLine) => Selection::from(cursor)
                .line_start(&self.layout, false)
                .focus(),
            (Direction::End, Boundary::VisualLine) => Selection::from(cursor)
                .line_end(&self.layout, false)
                .focus(),
            (Direction::Start, Boundary::HardLine) => Selection::from(cursor)
                .hard_line_start(&self.layout, false)
                .focus(),
            (Direction::End, Boundary::HardLine) => Selection::from(cursor)
                .hard_line_end(&self.layout, false)
                .focus(),
            (Direction::Up | Direction::Down, Boundary::VisualLine) => {
                let delta = if movement.direction == Direction::Up {
                    -1
                } else {
                    1
                };

                let geometry = cursor.geometry(&self.layout, 0.0);
                let line_index = self
                    .layout
                    .lines()
                    .position(|line| {
                        let metrics = line.metrics();
                        geometry.y0 as f32 >= metrics.block_min_coord
                            && (geometry.y0 as f32) < metrics.block_max_coord
                    })
                    .unwrap_or_else(|| self.layout.len().saturating_sub(1));
                let target_ix = line_index
                    .checked_add_signed(delta)
                    .filter(|&target_ix| self.layout.get(target_ix).is_some());
                let Some(target_ix) = target_ix else {
                    let selection = Selection::from(cursor);
                    let moved = if delta < 0 {
                        selection.previous_line(&self.layout, false)
                    } else {
                        selection.next_line(&self.layout, false)
                    };
                    return CaretMovement {
                        result: Self::caret_position(moved.focus()),
                        vertical_navigation_x,
                    };
                };

                let x = vertical_navigation_x
                    .map_or_else(|| cursor.geometry(&self.layout, 0.0).x0 as f32, f32::from);
                let moved = Cursor::from_point(&self.layout, x, self.native_y_for_line(target_ix));
                return CaretMovement {
                    result: Self::caret_position(moved),
                    vertical_navigation_x: Some(px(x)),
                };
            }
            _ => cursor,
        };
        CaretMovement {
            result: Self::caret_position(moved),
            vertical_navigation_x: None,
        }
    }

    fn selection_from_pixel_point(
        &self,
        pixel_point: gpui::Point<Pixels>,
        line_height: Pixels,
        kind: TextSelectionKind,
    ) -> std::ops::Range<usize> {
        let line_index = if line_height > Pixels::ZERO && pixel_point.y >= Pixels::ZERO {
            (pixel_point.y / line_height) as usize
        } else {
            0
        };

        let y = self.native_y_for_line(line_index);
        match kind {
            TextSelectionKind::Word => {
                Selection::word_from_point(&self.layout, pixel_point.x.into(), y)
            }
            TextSelectionKind::VisualLine => {
                Selection::line_from_point(&self.layout, pixel_point.x.into(), y)
            }
            TextSelectionKind::HardLine => {
                Selection::hard_line_from_point(&self.layout, pixel_point.x.into(), y)
            }
        }
        .text_range()
    }
}

impl ParleyState {
    fn new(system_fonts: SystemFonts) -> (Self, FontCatalog) {
        let collection = fontique::Collection::new(fontique::CollectionOptions {
            shared: true,
            system_fonts: system_fonts == SystemFonts::Load,
        });

        let source_cache = fontique::SourceCache::new_shared();
        let catalog = FontCatalog::from_shared(collection.clone(), source_cache.clone());
        (
            Self {
                fonts: FontContext {
                    collection,
                    source_cache,
                },
                layout: LayoutContext::new(),
            },
            catalog,
        )
    }
}

/// Shapes with Parley and paints exact font instances with an injected glyph rasterizer.
pub struct ParleyTextSystem {
    catalog: FontCatalog,
    fonts: RwLock<FontStore>,
    rasterizer: Mutex<Box<dyn GlyphRasterizer>>,
    parley: Mutex<ParleyState>,
    system_font_fallback: String,
    additional_fallbacks: Vec<String>,
}

impl ParleyTextSystem {
    /// Creates a text system using GPUI's default system-font family.
    pub fn new(system_fonts: SystemFonts) -> Self {
        Self::new_with_system_font(system_fonts, ".SystemUIFont")
    }

    /// Creates a text system with the concrete family used for GPUI's system-font alias.
    pub fn new_with_system_font(
        system_fonts: SystemFonts,
        system_font_fallback: impl Into<String>,
    ) -> Self {
        Self::new_with_rasterizer(
            system_fonts,
            system_font_fallback,
            SwashGlyphRasterizer::default(),
        )
    }

    /// Creates a text system which delegates only glyph rasterization to `rasterizer`.
    pub fn new_with_rasterizer(
        system_fonts: SystemFonts,
        system_font_fallback: impl Into<String>,
        rasterizer: impl GlyphRasterizer + 'static,
    ) -> Self {
        let (parley, catalog) = ParleyState::new(system_fonts);
        Self {
            catalog,
            fonts: RwLock::new(FontStore::default()),
            rasterizer: Mutex::new(Box::new(rasterizer)),
            parley: Mutex::new(parley),
            system_font_fallback: system_font_fallback.into(),
            additional_fallbacks: Vec::new(),
        }
    }

    /// Creates a deterministic text system without operating-system fonts.
    pub fn without_system_fonts() -> Self {
        Self::new_with_system_font(SystemFonts::Skip, "IBM Plex Sans")
    }

    /// Adds fallback families after those supplied by each text style.
    pub fn with_fallback_families(
        mut self,
        families: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.additional_fallbacks = families.into_iter().map(Into::into).collect();
        self
    }

    fn resolve_canonical_font(&self, descriptor: &Font) -> Result<FontId> {
        let mut families = Vec::with_capacity(
            1 + descriptor
                .fallbacks
                .as_ref()
                .map_or(0, |fallbacks| fallbacks.fallback_list().len()),
        );
        push_face_families(
            &mut families,
            descriptor.family.as_ref(),
            &self.system_font_fallback,
        );

        if let Some(fallbacks) = &descriptor.fallbacks {
            for family in fallbacks.fallback_list() {
                push_face_families(&mut families, family, &self.system_font_fallback);
            }
        }

        for family in &self.additional_fallbacks {
            push_face_families(&mut families, family, &self.system_font_fallback);
        }

        let resolved = self
            .catalog
            .resolve(&FaceRequest {
                families: &families,
                weight: descriptor.weight.0,
                style: descriptor.style,
                character: None,
            })
            .with_context(|| format!("Fontique could not resolve '{}'", descriptor.family))?;
        let font_id = self.fonts.write().intern_synthesized(
            resolved.data,
            resolved.index,
            resolved.synthesis,
        )?;
        Ok(font_id)
    }

    fn parley_layout(&self, params: ParleyLayoutParams<'_>) -> Result<ParleyLayoutResult> {
        let TextLayoutRequest {
            text,
            font_size,
            runs,
            line_clamp,
            ..
        } = params.text;
        let (inline_boxes, text_styles) =
            params.inline.as_ref().map_or((&[][..], &[][..]), |inline| {
                (inline.boxes, inline.text_styles)
            });

        // Parley 0.11 resolves one base direction per layout, including across newlines.
        if !text.chars().any(is_paragraph_separator) {
            return self.parley_paragraph_layout(params);
        }

        let run_ranges = run_ranges(runs);

        let mut paragraphs = Vec::new();
        let mut visual_lines = Vec::new();
        let mut paint_fragments = Vec::new();
        let mut inline_lines = Vec::new();
        let mut positioned_inline_boxes = Vec::new();

        let mut document_size = Size::<Pixels>::default();
        let mut width = Pixels::ZERO;
        let mut ascent = Pixels::ZERO;
        let mut descent = Pixels::ZERO;

        for source in paragraph_ranges(text) {
            let first_run = run_ranges.partition_point(|range| range.end <= source.content.start);
            let mut paragraph_runs = run_ranges
                .iter()
                .enumerate()
                .skip(first_run)
                .take_while(|(_idx, range)| range.start < source.content.end)
                .filter_map(|(idx, range)| {
                    let local = local_range(range, &source.content)?;

                    Some(TextRun {
                        len: local.len(),
                        ..runs[idx].clone()
                    })
                })
                .collect::<Vec<_>>();
            let mut paragraph_styles = text_styles
                .iter()
                .filter_map(|style| {
                    let range = local_range(&style.range, &source.content)?;

                    Some(InlineTextStyle {
                        range,
                        ..style.clone()
                    })
                })
                .collect::<Vec<_>>();

            if source.content.is_empty() {
                if let Some(run) = runs.get(first_run.min(runs.len().saturating_sub(1))) {
                    paragraph_runs.push(TextRun {
                        len: 0,
                        ..run.clone()
                    });
                }

                let style_idx = source.content.start.min(text.len().saturating_sub(1));
                paragraph_styles.extend(
                    text_styles
                        .iter()
                        .filter(|style| style.range.contains(&style_idx))
                        .map(|style| InlineTextStyle {
                            range: 0..0,
                            ..style.clone()
                        }),
                );
            }

            let paragraph_boxes = inline_boxes
                .iter()
                .filter(|inline_box| {
                    inline_box.index >= source.content.start
                        && (inline_box.index < source.separator.end
                            || source.separator.is_empty() && inline_box.index == text.len())
                })
                .map(|inline_box| InlineBoxRequest {
                    index: inline_box.index.min(source.content.end) - source.content.start,
                    ..*inline_box
                })
                .collect::<Vec<_>>();
            let mut result = self.parley_paragraph_layout(ParleyLayoutParams {
                text: TextLayoutRequest {
                    text: &text[source.content.clone()],
                    runs: &paragraph_runs,
                    // Clamping stops soft wrapping after the budget, but preserves hard breaks.
                    line_clamp: line_clamp.map(|count| count.saturating_sub(visual_lines.len())),
                    ..params.text
                },
                inline: params
                    .inline
                    .as_ref()
                    .map(|inline| ParleyInlineLayoutParams {
                        boxes: &paragraph_boxes,
                        text_styles: &paragraph_styles,
                        ..*inline
                    }),
            })?;

            let first_line = visual_lines.len();
            let first_fragment = paint_fragments.len();
            let block_offset = document_size.height;

            for line in &mut result.layout.visual_lines {
                line.text_range.start += source.content.start;
                line.text_range.end += source.content.start;
                line.fragment_range.start += first_fragment;
                line.fragment_range.end += first_fragment;
            }

            if let Some(line) = result.layout.visual_lines.last_mut() {
                // Keep separators in document indices without adding native trailing rows.
                line.text_range.end = source.separator.end;
            }

            for line in &mut result.inline_lines {
                line.origin.y += block_offset;
            }

            for inline_box in &mut result.inline_boxes {
                inline_box.line_index += first_line;
                inline_box.bounds.origin.y += block_offset;
            }

            width = width.max(result.layout.width);
            ascent = ascent.max(result.layout.ascent);
            descent = descent.max(result.layout.descent);
            document_size.width = document_size.width.max(result.size.width);
            document_size.height += result.size.height;

            let last_line = result.inline_lines.last().unwrap();
            let newline_width = (result.layout.ascent + result.layout.descent) * 0.25;
            let newline = if result.is_rtl {
                last_line.origin.x - newline_width..last_line.origin.x
            } else {
                let newline_x = last_line.origin.x + last_line.size.width;

                newline_x..newline_x + newline_width
            };

            paragraphs.push(ParagraphLayout {
                source,
                first_line,
                block_offset,
                native: result.layout.platform_layout,
                newline,
                is_rtl: result.is_rtl,
            });
            visual_lines.extend(result.layout.visual_lines);
            paint_fragments.extend(result.layout.paint_fragments);
            inline_lines.extend(result.inline_lines);
            positioned_inline_boxes.extend(result.inline_boxes);
        }

        let is_rtl = paragraphs[0].is_rtl;
        let platform_layout = ParleyDocumentLayout::new(paragraphs, text, document_size);

        Ok(ParleyLayoutResult {
            layout: LineLayout {
                font_size,
                width,
                ascent,
                descent,
                visual_lines: visual_lines.into_iter().collect(),
                paint_fragments,
                len: text.len(),
                platform_layout: std::sync::Arc::new(platform_layout),
            },
            inline_lines,
            inline_boxes: positioned_inline_boxes,
            size: document_size,
            is_rtl,
        })
    }

    fn parley_paragraph_layout(
        &self,
        params: ParleyLayoutParams<'_>,
    ) -> Result<ParleyLayoutResult> {
        let TextLayoutRequest {
            text,
            font_size,
            runs,
            wrap_width,
            line_clamp,
        } = params.text;

        let wrap = (wrap_width.is_some() || line_clamp.is_some())
            .then_some((wrap_width.unwrap_or(Pixels::MAX), line_clamp));

        let (inline_boxes, text_styles, line_height, inline_text_metrics, text_align) =
            match params.inline.as_ref() {
                Some(inline) => (
                    inline.boxes,
                    inline.text_styles,
                    Some(inline.line_height),
                    Some(inline.text_metrics),
                    Some(inline.text_align),
                ),
                None => (&[][..], &[][..], None, None, None),
            };

        let run_ranges = run_ranges(runs);
        let line_height = if text.is_empty() {
            text_styles
                .last()
                .map(|style| style.line_height)
                .or(line_height)
        } else {
            line_height
        };

        let family_lists = runs
            .iter()
            .map(|run| {
                let descriptor = &run.font;
                let mut families = Vec::new();
                push_parley_families(
                    &mut families,
                    descriptor.family.as_ref(),
                    &self.system_font_fallback,
                );

                if let Some(fallbacks) = &descriptor.fallbacks {
                    for family in fallbacks.fallback_list() {
                        push_parley_families(&mut families, family, &self.system_font_fallback);
                    }
                }

                families.extend(
                    self.additional_fallbacks
                        .iter()
                        .map(|family| FontFamilyName::Named(Cow::Borrowed(family.as_str()))),
                );
                families
            })
            .collect::<Vec<_>>();
        let feature_lists = runs
            .iter()
            .map(|run| {
                let descriptor = &run.font;
                descriptor
                    .features
                    .tag_value_list()
                    .iter()
                    .map(|(tag, value)| {
                        let tag = Tag::parse(tag)
                            .with_context(|| format!("invalid OpenType feature tag '{tag}'"))?;
                        let value = (*value).try_into().with_context(|| {
                            format!("OpenType feature '{tag}' value is larger than u16")
                        })?;

                        Ok(FontFeature::new(tag, value))
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;

        let mut state = self.parley.lock();
        let ParleyState { fonts, layout } = &mut *state;
        let mut builder = layout.ranged_builder(fonts, text, 1.0, false);
        builder.set_line_break_override(Some(CHROMIUM_LINE_BREAK_OVERRIDE));
        builder.push_default(StyleProperty::FontSize(f32::from(font_size)));

        if let Some(line_height) = line_height {
            builder.push_default(StyleProperty::LineHeight(LineHeight::Absolute(f32::from(
                line_height,
            ))));
        }

        let mut push_style = |property, range| {
            if text.is_empty() {
                builder.push_default(property);
            } else {
                builder.push(property, range);
            }
        };

        for (run_index, run) in runs.iter().enumerate() {
            let descriptor = &run.font;
            let range = run_ranges[run_index].clone();
            push_style(
                StyleProperty::FontFamily(FontFamily::from(family_lists[run_index].as_slice())),
                range.clone(),
            );
            push_style(
                StyleProperty::FontWeight(FontWeight::new(descriptor.weight.0)),
                range.clone(),
            );
            push_style(
                StyleProperty::FontStyle(match descriptor.style {
                    gpui::FontStyle::Normal => FontStyle::Normal,
                    gpui::FontStyle::Italic => FontStyle::Italic,
                    gpui::FontStyle::Oblique => FontStyle::Oblique(None),
                }),
                range.clone(),
            );

            if !feature_lists[run_index].is_empty() {
                push_style(
                    StyleProperty::FontFeatures(FontFeatures::from(
                        feature_lists[run_index].as_slice(),
                    )),
                    range.clone(),
                );
            }

            if let Some(letter_spacing) = run.letter_spacing {
                push_style(
                    StyleProperty::LetterSpacing(f32::from(letter_spacing)),
                    range.clone(),
                );
            }

            let paint_style = PaintStyle::from(run);
            push_style(StyleProperty::Brush(paint_style.clone()), range.clone());

            if let Some(underline) = run.underline {
                push_style(StyleProperty::Underline(true), range.clone());
                push_style(
                    StyleProperty::UnderlineSize(Some(underline.thickness.into())),
                    range.clone(),
                );
                push_style(
                    StyleProperty::UnderlineBrush(Some(paint_style.clone())),
                    range.clone(),
                );
            }

            if let Some(strikethrough) = run.strikethrough {
                push_style(StyleProperty::Strikethrough(true), range.clone());
                push_style(
                    StyleProperty::StrikethroughSize(Some(strikethrough.thickness.into())),
                    range.clone(),
                );
                push_style(
                    StyleProperty::StrikethroughBrush(Some(paint_style)),
                    range.clone(),
                );
            }
        }

        for style in text_styles {
            push_style(
                StyleProperty::FontSize(f32::from(style.font_size)),
                style.range.clone(),
            );

            push_style(
                StyleProperty::LineHeight(LineHeight::Absolute(f32::from(style.line_height))),
                style.range.clone(),
            );
        }

        for inline_box in inline_boxes {
            if inline_box.index > text.len() || !text.is_char_boundary(inline_box.index) {
                anyhow::bail!("inline box index does not align with the input text");
            }

            builder.push_inline_box(InlineBox {
                id: inline_box.id,
                kind: InlineBoxKind::InFlow,
                index: inline_box.index,
                width: f32::from(inline_box.size.width),
                height: f32::from(inline_box.size.height),
            });
        }

        let mut layout = builder.build(text);

        if let Some((wrap_width, max_lines)) = wrap {
            if let Some(max_lines) = max_lines {
                let mut breaker = layout.break_lines();
                breaker.state_mut().set_layout_max_advance(f32::MAX);
                breaker
                    .state_mut()
                    .set_line_max_advance(f32::from(wrap_width));

                for _ in 0..max_lines.saturating_sub(1) {
                    if breaker.break_next().is_none() {
                        break;
                    }
                }

                breaker.break_remaining(f32::MAX);
            } else {
                layout.break_all_lines(Some(f32::from(wrap_width)));
            }
        } else {
            layout.break_all_lines(None);
        }

        // Parley uses an unbounded line width for empty layouts. Align their empty
        // row here so centered and right-aligned carets stay inside the container.
        let empty_alignment = (text.is_empty() && inline_boxes.is_empty()).then(|| {
            let width = wrap
                .map(|(width, _max_lines)| width)
                .filter(|width| *width < Pixels::MAX)
                .unwrap_or_default();

            match text_align {
                Some(TextAlign::Right) => width,
                Some(TextAlign::Center) => width / 2.,
                _ => Pixels::ZERO,
            }
        });

        if let Some(text_align) = text_align
            && empty_alignment.is_none()
        {
            let alignment = match text_align {
                TextAlign::Left => Alignment::Left,
                TextAlign::Center => Alignment::Center,
                TextAlign::Right => Alignment::Right,
            };

            layout.align(alignment, AlignmentOptions::default());
        }

        let mut visual_lines = Vec::new();
        let mut paint_fragments = Vec::new();

        let mut positioned_inline_boxes = Vec::new();
        let mut inline_lines = Vec::new();
        let mut inline_line_metrics = Vec::new();
        let mut inline_text_bounds = Vec::new();

        let mut width = px(0.0);
        let mut ascent = px(0.0);
        let mut descent = px(0.0);

        let mut saw_line = false;

        for (line_index, line) in layout.lines().enumerate() {
            saw_line = true;

            let fragment_start = paint_fragments.len();
            let metrics = *line.metrics();
            let line_x =
                empty_alignment.unwrap_or_else(|| px(metrics.inline_min_coord + metrics.offset));
            let line_advance = if empty_alignment.is_some() {
                Pixels::ZERO
            } else {
                px(metrics.advance)
            };

            let mut text_metrics = inline_text_metrics.unwrap_or_default();
            let leading =
                ((line_height.unwrap_or_default() - text_metrics.ascent - text_metrics.descent)
                    / 2.)
                    .max(Pixels::ZERO);

            let mut text_top = -text_metrics.ascent - leading;
            let mut text_bottom = text_metrics.descent + leading;

            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    let PositionedLayoutItem::InlineBox(inline_box) = item else {
                        unreachable!();
                    };

                    positioned_inline_boxes.push(PositionedInlineBox {
                        id: inline_box.id,
                        line_index,
                        bounds: Bounds::new(
                            point(px(inline_box.x), px(inline_box.y)),
                            size(px(inline_box.width), px(inline_box.height)),
                        ),
                    });

                    continue;
                };

                let run = glyph_run.run();
                let normalized_coords = run
                    .normalized_coords()
                    .iter()
                    .copied()
                    .map(NormalizedCoord::from_bits)
                    .collect::<Vec<_>>();
                let font_id = self.fonts.write().intern(
                    run.font().data.clone(),
                    run.font().index,
                    &normalized_coords,
                    run.synthesis(),
                )?;

                let baseline = glyph_run.baseline();
                let run_metrics = glyph_run.run().metrics();

                // Resolve leading from the input ranges. Parley 0.11 can report the
                // following style's line height in a shaped run's cached metrics.
                let range = run.text_range();

                let run_line_height = text_styles
                    .iter()
                    .filter(|style| style.range.start < range.end && style.range.end > range.start)
                    .map(|style| f32::from(style.line_height))
                    .reduce(f32::max)
                    .unwrap_or_else(|| {
                        line_height
                            .map(f32::from)
                            .unwrap_or(run_metrics.line_height)
                    });

                let half_leading =
                    ((run_line_height - run_metrics.ascent - run_metrics.descent) / 2.).max(0.);

                text_metrics.ascent = text_metrics.ascent.max(px(run_metrics.ascent));
                text_metrics.descent = text_metrics.descent.max(px(run_metrics.descent));
                text_top = text_top.min(px(-run_metrics.ascent - half_leading));
                text_bottom = text_bottom.max(px(run_metrics.descent + half_leading));

                if text.is_empty() {
                    continue;
                }

                let parley_style = glyph_run.style();
                let mut paint_style = parley_style.brush.clone();
                let underline_offset = parley_style.underline.as_ref().map(|decoration| {
                    let mut underline = decoration.brush.underline.unwrap_or_default();
                    underline.thickness = px(decoration.size.unwrap_or(run_metrics.underline_size));
                    paint_style.underline = Some(underline);
                    px(decoration.offset.unwrap_or(run_metrics.underline_offset))
                });

                let strikethrough_offset = parley_style.strikethrough.as_ref().map(|decoration| {
                    let mut strikethrough = decoration.brush.strikethrough.unwrap_or_default();
                    strikethrough.thickness =
                        px(decoration.size.unwrap_or(run_metrics.strikethrough_size));
                    paint_style.strikethrough = Some(strikethrough);
                    px(decoration
                        .offset
                        .unwrap_or(run_metrics.strikethrough_offset))
                });

                let glyphs = {
                    let fonts = self.fonts.read();
                    let color_glyphs = fonts
                        .get(font_id)
                        .context("canonical font missing after interning")?
                        .color_glyphs()?;
                    let rasterizer = self.rasterizer.lock();
                    glyph_run
                        .positioned_glyphs()
                        .map(|glyph| {
                            let id = GlyphId(glyph.id);
                            ShapedGlyph {
                                id,
                                position: point(px(glyph.x) - line_x, px(glyph.y - baseline)),
                                is_emoji: color_glyphs
                                    .kind(id)
                                    .is_some_and(|kind| rasterizer.supports_color_glyph(kind)),
                            }
                        })
                        .collect()
                };

                let start = px(glyph_run.offset()) - line_x;
                paint_fragments.push(PaintFragment {
                    font_id,
                    font_size: px(run.font_size()),
                    glyphs,
                    x_range: start..start + px(glyph_run.advance()),
                    style: paint_style,
                    underline_offset,
                    strikethrough_offset,
                });
            }

            let parley_text_range = line.text_range();
            let text_range =
                parley_text_range.start.min(text.len())..parley_text_range.end.min(text.len());

            visual_lines.push(VisualLine {
                text_range,
                fragment_range: fragment_start..paint_fragments.len(),
                advance_width: line_advance,
            });

            inline_lines.push(InlineVisualLine {
                origin: point(line_x, px(metrics.block_min_coord)),
                size: size(
                    line_advance,
                    px(metrics.block_max_coord - metrics.block_min_coord),
                ),
                baseline: px(metrics.baseline - metrics.block_min_coord),
            });

            inline_line_metrics.push(text_metrics);
            inline_text_bounds.push((text_top, text_bottom));

            width = width.max(line_advance);
            ascent = ascent.max(px(metrics.ascent));
            descent = descent.max(px(metrics.descent));
        }

        if !saw_line {
            anyhow::bail!("Parley produced no line");
        }

        let mut size = size(px(layout.width()), px(layout.height()));

        if empty_alignment.is_some() {
            size.width = Pixels::ZERO;
        }

        if let (Some(text_metrics), Some(line_height)) = (inline_text_metrics, line_height) {
            align_inline_boxes(
                &mut inline_lines,
                &mut positioned_inline_boxes,
                &mut size,
                inline_boxes,
                &inline_line_metrics,
                &inline_text_bounds,
                text_metrics,
                line_height,
            );
        }

        let is_rtl = layout.is_rtl();
        let platform_layout = ParleyLayout::new(layout, text, inline_lines.clone());
        let line_layout = LineLayout {
            font_size,
            width,
            ascent,
            descent,
            visual_lines: visual_lines.iter().cloned().collect(),
            paint_fragments,
            len: text.len(),
            platform_layout: std::sync::Arc::new(platform_layout),
        };

        Ok(ParleyLayoutResult {
            layout: line_layout,
            inline_lines,
            inline_boxes: positioned_inline_boxes,
            size,
            is_rtl,
        })
    }
}

fn run_ranges(runs: &[TextRun]) -> Vec<Range<usize>> {
    let mut start = 0;

    runs.iter()
        .map(|run| {
            let range = start..start + run.len;
            start = range.end;

            range
        })
        .collect()
}

fn push_face_families<'a>(
    families: &mut Vec<FaceFamily<'a>>,
    name: &'a str,
    system_font_fallback: &'a str,
) {
    if name == ".SystemUIFont" {
        families.push(FaceFamily::SystemUi);
        families.push(FaceFamily::Named(system_font_fallback));
    } else {
        families.push(FaceFamily::Named(name));
    }
}

fn push_parley_families<'a>(
    families: &mut Vec<FontFamilyName<'a>>,
    name: &'a str,
    system_font_fallback: &str,
) {
    if name == ".SystemUIFont" {
        families.push(FontFamilyName::Generic(GenericFamily::SystemUi));
        families.push(FontFamilyName::Named(Cow::Owned(
            system_font_fallback.to_string(),
        )));
    } else {
        families.push(FontFamilyName::Named(Cow::Owned(name.to_string())));
    }
}

impl PlatformTextSystem for ParleyTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        let blobs = fonts
            .iter()
            .map(|bytes| fontique::Blob::from(bytes.as_ref().to_vec()))
            .collect::<Vec<_>>();
        let mut state = self.catalog.state.write();
        let mut next: CatalogState = state.clone();
        next.register_blobs(&blobs)?;
        *state = next;
        Ok(())
    }

    fn all_font_names(&self) -> Vec<String> {
        let mut names = self.catalog.family_names();
        names.push(".SystemUIFont".to_owned());
        names.sort_unstable();
        names.dedup();
        names
    }

    fn font_generation(&self) -> u64 {
        self.catalog.generation()
    }

    fn font_id(&self, descriptor: &Font) -> Result<FontId> {
        self.resolve_canonical_font(descriptor)
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        self.fonts
            .read()
            .get(font_id)
            .expect("Parley FontId missing from its store")
            .metrics()
            .expect("stored font failed Skrifa metrics")
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        self.fonts
            .read()
            .get(font_id)
            .context("Parley FontId missing from its store")?
            .glyph_bounds(glyph_id)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.fonts
            .read()
            .get(font_id)
            .context("Parley FontId missing from its store")?
            .advance(glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.fonts.read().get(font_id)?.glyph_for_char(ch).ok()?
    }

    fn rasterize_glyph(&self, params: &RenderGlyphParams) -> Result<RasterizedGlyph> {
        let fonts = self.fonts.read();
        let font = fonts
            .get(params.font_id)
            .context("Parley FontId missing from its store")?;
        let data_identity = font.data_identity();
        let face_index = font.index;
        let variations = font.variations.clone();
        self.rasterizer
            .lock()
            .rasterize(font.raster_face(params.font_id), params)
            .with_context(|| {
                format!(
                    "native rasterization failed for FontId {:?}, data identity {data_identity}, face index {face_index}, variations {variations:?}",
                    params.font_id
                )
            })
    }

    fn prepare_raster_style(&self, request: RasterStyleRequest) -> PreparedRasterStyle {
        self.rasterizer.lock().prepare_style(request)
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        self.rasterizer.lock().recommended_mode()
    }

    fn layout_text(&self, request: TextLayoutRequest<'_>) -> LineLayout {
        self.parley_layout(ParleyLayoutParams {
            text: request,
            inline: None,
        })
        .expect("Parley failed to lay out a validated GPUI document")
        .layout
    }

    fn layout_inline(&self, request: InlineLayoutRequest<'_>) -> InlineLayout {
        let result = self
            .parley_layout(ParleyLayoutParams {
                text: TextLayoutRequest {
                    text: request.text,
                    font_size: request.font_size,
                    runs: request.runs,
                    wrap_width: request.wrap_width,
                    line_clamp: request.line_clamp,
                },
                inline: Some(ParleyInlineLayoutParams {
                    boxes: request.boxes,
                    text_styles: request.text_styles,
                    line_height: request.line_height,
                    text_metrics: request.text_metrics,
                    text_align: request.text_align,
                }),
            })
            .expect("Parley failed to lay out a validated GPUI inline document");
        InlineLayout {
            layout: std::sync::Arc::new(result.layout),
            alignment_offset: inline_alignment_offset(request.text_align, &result.inline_lines),
            lines: result.inline_lines,
            boxes: result.inline_boxes,
            size: result.size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font_fixtures::{
        IBM_PLEX, IBM_PLEX_SEMIBOLD, LILEX, NOTO_COLOR_EMOJI, SOURCE_SERIF,
    };
    use crate::{FontSynthesis, FontVariation, RasterFace};
    use gpui::{
        AppContext, CaretSelection, Context, FontFallbacks, FontFeatures as GpuiFontFeatures,
        FontStyle as GpuiFontStyle, FontWeight as GpuiFontWeight, GlyphRenderMode,
        HeadlessAppContext, HighlightStyle, Hsla, IntoElement, Point, RasterizedGlyphFormat,
        Render, ScaledPixels, StrikethroughStyle, Styled, StyledText, TextSystem, UnderlineStyle,
        VerticalAlign, Window, WindowHandle, WindowTextSystem, div, font, hsla, prelude::*,
    };
    use std::{cell::Cell, cell::RefCell, rc::Rc, sync::Arc};

    const NOTO_ARABIC: &[u8] =
        include_bytes!("../../../assets/fonts/noto-sans-arabic/NotoSansArabic-Regular.ttf");
    const NOTO_HEBREW: &[u8] =
        include_bytes!("../../../assets/fonts/noto-sans-hebrew/NotoSansHebrew-Regular.ttf");

    #[test]
    fn caret_affinity_round_trips_through_parley() {
        for affinity in [CaretAffinity::Downstream, CaretAffinity::Upstream] {
            let parley_affinity: Affinity = affinity.into_affinity();
            let gpui_affinity: CaretAffinity = parley_affinity.into_affinity();
            assert_eq!(gpui_affinity, affinity);
        }
    }

    fn test_system() -> Arc<ParleyTextSystem> {
        let system = Arc::new(
            ParleyTextSystem::new_with_system_font(SystemFonts::Skip, IBM_PLEX.family)
                .with_fallback_families([
                    IBM_PLEX.family,
                    LILEX.family,
                    SOURCE_SERIF.family,
                    NOTO_COLOR_EMOJI.family,
                    "Noto Sans Arabic",
                    "Noto Sans Hebrew",
                ]),
        );
        system
            .add_fonts(vec![
                Cow::Borrowed(IBM_PLEX.data),
                Cow::Borrowed(IBM_PLEX_SEMIBOLD.data),
                Cow::Borrowed(LILEX.data),
                Cow::Borrowed(SOURCE_SERIF.data),
                Cow::Borrowed(NOTO_COLOR_EMOJI.data),
                Cow::Borrowed(NOTO_ARABIC),
                Cow::Borrowed(NOTO_HEBREW),
            ])
            .unwrap();
        system
    }

    fn text_run(text: &str, family: &str) -> TextRun {
        TextRun {
            len: text.len(),
            font: font(family),
            ..Default::default()
        }
    }

    fn layout_line(
        system: &ParleyTextSystem,
        text: &str,
        font_size: Pixels,
        runs: &[TextRun],
    ) -> LineLayout {
        system.layout_text(TextLayoutRequest {
            text,
            font_size,
            runs,
            wrap_width: None,
            line_clamp: None,
        })
    }

    fn layout_wrapped(
        system: &ParleyTextSystem,
        text: &str,
        font_size: Pixels,
        runs: &[TextRun],
        wrap_width: Pixels,
        line_clamp: Option<usize>,
    ) -> LineLayout {
        system.layout_text(TextLayoutRequest {
            text,
            font_size,
            runs,
            wrap_width: Some(wrap_width),
            line_clamp,
        })
    }

    fn wrapped(layout: LineLayout, width: Pixels) -> gpui::WrappedLineLayout {
        gpui::WrappedLineLayout {
            layout: Arc::new(layout),
            wrap_width: Some(width),
        }
    }

    fn positioned_box_bounds(
        layout: &InlineLayout,
        inline_box: &PositionedInlineBox,
    ) -> Bounds<Pixels> {
        assert!(
            inline_box.line_index < layout.lines.len(),
            "inline box {} refers to missing line {}",
            inline_box.id,
            inline_box.line_index
        );
        inline_box.bounds
    }

    fn assert_inline_geometry_is_contained(layout: &InlineLayout, width: Pixels) {
        let epsilon = px(0.01);
        for (line_index, line) in layout.lines.iter().enumerate() {
            assert!(
                line.origin.x + line.size.width <= width + epsilon,
                "line {line_index} extends past {width:?}: {line:?}"
            );
            assert!(
                line.origin.y + line.size.height <= layout.size.height + epsilon,
                "line {line_index} extends past the layout height"
            );
        }

        for inline_box in &layout.boxes {
            let bounds = positioned_box_bounds(layout, inline_box);
            let line = layout.lines[inline_box.line_index];
            assert!(
                bounds.right() <= width + epsilon,
                "inline box {} extends past the available width",
                inline_box.id
            );
            assert!(
                bounds.origin.y + epsilon >= line.origin.y
                    && bounds.bottom() <= line.origin.y + line.size.height + epsilon,
                "inline box {} is outside its assigned line",
                inline_box.id
            );
        }
    }

    fn assert_document_contract(text: &str, layout: &LineLayout) {
        assert_eq!(layout.len, text.len());
        assert!(!layout.visual_lines.is_empty());
        assert_eq!(layout.visual_lines[0].text_range.start, 0);
        assert_eq!(
            layout.visual_lines.last().unwrap().text_range.end,
            text.len()
        );
        assert_eq!(layout.visual_lines[0].fragment_range.start, 0);
        assert_eq!(
            layout.visual_lines.last().unwrap().fragment_range.end,
            layout.paint_fragments.len()
        );

        for pair in layout.visual_lines.windows(2) {
            assert_eq!(pair[0].text_range.end, pair[1].text_range.start);
            assert_eq!(pair[0].fragment_range.end, pair[1].fragment_range.start);
        }

        for line in &layout.visual_lines {
            assert!(text.is_char_boundary(line.text_range.start));
            assert!(text.is_char_boundary(line.text_range.end));
            assert!(
                f32::from(line.advance_width).is_finite() && line.advance_width >= Pixels::ZERO
            );
            for fragment in &layout.paint_fragments[line.fragment_range.clone()] {
                assert!(f32::from(fragment.x_range.start).is_finite());
                assert!(f32::from(fragment.x_range.end).is_finite());
                assert!(fragment.x_range.start <= fragment.x_range.end);
                assert!(fragment.x_range.start >= -layout.font_size * 2.0);
                assert!(fragment.x_range.end <= line.advance_width + layout.font_size * 2.0);
                for glyph in &fragment.glyphs {
                    assert!(f32::from(glyph.position.x).is_finite());
                    assert!(f32::from(glyph.position.y).is_finite());
                }
            }
        }

        let line_height = px(24.0);
        let wrapped = wrapped(layout.clone_for_test(), px(120.0));
        let mut caret = wrapped
            .closest_caret_for_pixel_point(point(px(-100.0), line_height * 0.5), line_height)
            .unwrap_err();
        let mut seen = Vec::new();
        let max_steps = text.chars().count() * 4 + wrapped.line_count() * 4 + 8;
        for _ in 0..max_steps {
            assert!(text.is_char_boundary(caret.index));
            assert!(!seen.contains(&caret), "visual caret traversal cycled");
            seen.push(caret);
            let Some(next) = wrapped.adjacent_visual_caret(caret, VisualDirection::Right) else {
                break;
            };

            caret = next;
        }
        assert!(
            wrapped
                .adjacent_visual_caret(caret, VisualDirection::Right)
                .is_none()
        );
        for _ in 0..max_steps {
            let Some(previous) = wrapped.adjacent_visual_caret(caret, VisualDirection::Left) else {
                break;
            };

            caret = previous;
        }
        assert!(
            wrapped
                .adjacent_visual_caret(caret, VisualDirection::Left)
                .is_none()
        );

        for caret in seen {
            let bounds = wrapped
                .visual_position_for_caret(caret, line_height)
                .expect("native caret must have geometry");
            assert!(f32::from(bounds.x).is_finite() && f32::from(bounds.y).is_finite());
        }

        if text.chars().any(|character| !character.is_whitespace()) {
            assert!(
                !wrapped
                    .selection_bounds(0..text.len(), line_height)
                    .is_empty()
            );
        }
    }

    #[test]
    fn inline_ranges_forward_font_metrics_and_keep_native_geometry() {
        let system = test_system();

        let text = "small\nBIG words\nend";
        let runs = [TextRun {
            len: text.len(),
            font: font("IBM Plex Sans"),
            ..Default::default()
        }];

        let text_styles = [InlineTextStyle {
            range: 6..15,
            font_size: px(30.),
            line_height: px(46.),
        }];

        let layout = system.layout_inline(InlineLayoutRequest {
            text,
            runs: &runs,
            text_styles: &text_styles,
            boxes: &[],
            font_size: px(14.),
            line_height: px(20.),
            text_metrics: InlineTextMetrics {
                ascent: px(11.),
                descent: px(3.),
                x_height: px(7.),
            },

            wrap_width: Some(px(300.)),
            line_clamp: None,
            text_align: TextAlign::Left,
        });

        assert_eq!(layout.lines.len(), 3);
        assert!(
            layout.lines[1].size.height >= px(45.99),
            "{:?}",
            layout.lines
        );
        assert!(
            layout
                .layout
                .paint_fragments
                .iter()
                .any(|fragment| fragment.font_size == px(30.))
        );
        assert!(
            layout
                .layout
                .paint_fragments
                .iter()
                .any(|fragment| fragment.font_size == px(14.))
        );

        let native = layout
            .layout
            .platform_layout
            .inline_geometry(16..19)
            .unwrap();
        assert_eq!(native.len(), 1);
        assert_eq!(native[0].visual_line_index, 2);
        assert!(native[0].bounds.origin.y >= px(65.99), "{native:?}");

        let selection = layout
            .layout
            .platform_layout
            .selection_bounds(16..19, px(20.));
        assert_eq!(
            selection[0].origin.y,
            px(40.),
            "ordinary selection retains its existing row policy"
        );

        assert!(
            layout
                .layout
                .platform_layout
                .inline_geometry(5..6)
                .unwrap()
                .is_empty(),
            "newlines add no span hit region"
        );
        assert!(
            layout
                .layout
                .platform_layout
                .inline_geometry(6..6)
                .is_none()
        );
    }

    #[test]
    fn inline_layout_flows_boxes_with_styled_wrapped_text() {
        let system = test_system();
        let text = "alpha beta gamma delta";
        let split = "alpha beta ".len();

        let first_color = hsla(0.0, 0.8, 0.4, 1.0);
        let second_color = hsla(0.6, 0.8, 0.4, 1.0);

        let runs = [
            TextRun {
                len: split,
                color: first_color,
                font: font("IBM Plex Sans"),
                ..Default::default()
            },
            TextRun {
                len: text.len() - split,
                color: second_color,
                font: font("Source Serif 4"),
                ..Default::default()
            },
        ];

        let boxes = [
            InlineBoxRequest {
                id: 7,
                index: "alpha ".len(),
                size: size(px(28.0), px(32.0)),
                vertical_align: VerticalAlign::Baseline,
            },
            InlineBoxRequest {
                id: 9,
                index: split,
                size: size(px(18.0), px(14.0)),
                vertical_align: VerticalAlign::Middle,
            },
            InlineBoxRequest {
                id: 11,
                index: "alpha beta gamma ".len(),
                size: size(px(20.0), px(18.0)),
                vertical_align: VerticalAlign::Top,
            },
            InlineBoxRequest {
                id: 13,
                index: "alpha beta gamma ".len(),
                size: size(px(16.0), px(28.0)),
                vertical_align: VerticalAlign::Bottom,
            },
        ];

        let text_metrics = InlineTextMetrics {
            ascent: px(14.0),
            descent: px(4.0),
            x_height: px(8.0),
        };

        let request = InlineLayoutRequest {
            text_styles: &[],
            text,
            runs: &runs,
            boxes: &boxes,
            font_size: px(18.0),
            line_height: px(24.0),
            text_metrics,
            wrap_width: Some(px(160.0)),
            line_clamp: None,
            text_align: TextAlign::Center,
        };

        let layout = system.layout_inline(request);

        assert!(layout.lines.len() >= 2);
        assert_eq!(layout.lines.len(), layout.layout.visual_lines.len());
        assert_eq!(
            layout
                .boxes
                .iter()
                .map(|inline_box| inline_box.id)
                .collect::<Vec<_>>(),
            vec![7, 9, 11, 13]
        );
        assert!(layout.size.width <= px(160.0));
        assert!(layout.lines.iter().any(|line| line.origin.x > Pixels::ZERO));
        assert!(
            layout
                .layout
                .paint_fragments
                .iter()
                .any(|fragment| fragment.style.color == first_color)
        );
        assert!(
            layout
                .layout
                .paint_fragments
                .iter()
                .any(|fragment| fragment.style.color == second_color)
        );

        let box_and_line = |box_id| {
            let inline_box = layout
                .boxes
                .iter()
                .find(|inline_box| inline_box.id == box_id)
                .unwrap();
            let line = &layout.lines[inline_box.line_index];
            let bounds = positioned_box_bounds(&layout, inline_box);
            (bounds, line)
        };

        let (baseline_box, baseline_line) = box_and_line(7);
        assert!(
            (baseline_box.bottom() - (baseline_line.origin.y + baseline_line.baseline)).abs()
                < px(0.01)
        );
        let (middle_box, middle_line) = box_and_line(9);
        assert!(
            (middle_box.center().y
                - (middle_line.origin.y + middle_line.baseline - text_metrics.x_height / 2.))
                .abs()
                < px(0.01)
        );
        let (top_box, top_line) = box_and_line(11);
        assert!((top_box.origin.y - top_line.origin.y).abs() < px(0.01));
        let (bottom_box, bottom_line) = box_and_line(13);
        assert!(
            (bottom_box.bottom() - (bottom_line.origin.y + bottom_line.size.height)).abs()
                < px(0.01)
        );

        for lines in layout.lines.windows(2) {
            assert!(lines[0].origin.y + lines[0].size.height <= lines[1].origin.y);
        }

        assert_inline_geometry_is_contained(&layout, px(160.0));
    }

    #[test]
    fn inline_layout_supports_documents_containing_only_boxes() {
        let system = test_system();
        let boxes = [
            InlineBoxRequest {
                id: 1,
                index: 0,
                size: size(px(30.0), px(12.0)),
                vertical_align: VerticalAlign::Baseline,
            },
            InlineBoxRequest {
                id: 2,
                index: 0,
                size: size(px(20.0), px(36.0)),
                vertical_align: VerticalAlign::Middle,
            },
        ];

        let layout = system.layout_inline(InlineLayoutRequest {
            text_styles: &[],
            text: "",
            runs: &[],
            boxes: &boxes,
            font_size: px(18.0),
            line_height: px(24.0),
            text_metrics: InlineTextMetrics {
                ascent: px(14.0),
                descent: px(4.0),
                x_height: px(8.0),
            },
            wrap_width: Some(px(24.0)),
            line_clamp: None,
            text_align: TextAlign::Left,
        });

        assert_eq!(layout.boxes.len(), 2);
        assert_eq!(layout.lines.len(), 2);
        assert_eq!(layout.layout.paint_fragments.len(), 0);
        assert_eq!(
            layout
                .boxes
                .iter()
                .map(|inline_box| inline_box.line_index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(layout.size.width, px(30.0));
        assert!(layout.size.height >= px(48.0));
        assert_inline_geometry_is_contained(&layout, layout.size.width);
    }

    trait CloneLineLayoutForTest {
        fn clone_for_test(&self) -> LineLayout;
    }

    impl CloneLineLayoutForTest for LineLayout {
        fn clone_for_test(&self) -> LineLayout {
            LineLayout {
                font_size: self.font_size,
                width: self.width,
                ascent: self.ascent,
                descent: self.descent,
                visual_lines: self.visual_lines.clone(),
                paint_fragments: self.paint_fragments.clone(),
                len: self.len,
                platform_layout: self.platform_layout.clone(),
            }
        }
    }

    #[test]
    fn paragraph_direction_does_not_leak_across_newlines() {
        let system = test_system();
        let text = "שלום עולם\nabc אבג def\nx (مرحبا) y";
        let runs = [text_run(text, "IBM Plex Sans")];
        let document = wrapped(layout_line(&system, text, px(18.0), &runs), px(500.0));

        for (left, right) in [("abc", "def"), ("x", "y")] {
            let left_position = document
                .visual_position_for_byte_index(text.find(left).unwrap(), px(24.0))
                .unwrap();
            let right_position = document
                .visual_position_for_byte_index(text.find(right).unwrap(), px(24.0))
                .unwrap();

            assert_eq!(left_position.y, right_position.y);
            assert!(
                left_position.x < right_position.x,
                "{left} must precede {right}"
            );
        }
    }

    #[test]
    fn paragraph_paint_and_carets_match_independent_layouts() {
        let system = test_system();
        let sample =
            "שלום עולם\nمرحبا بالعالم\nabc אבג def\nx (مرحبا) y\nEnglish ثم عربي ثم English";

        for text in [
            sample.to_string(),
            format!("Latin first\n{sample}\n\n"),
            "אבג\u{2028}abc אבג def\n123 (45)\n\u{2067}אבג\u{2069} abc\nمرحبا".to_string(),
        ] {
            for width in [px(500.), px(90.)] {
                let document = layout_wrapped(
                    &system,
                    &text,
                    px(18.),
                    &[text_run(&text, "IBM Plex Sans")],
                    width,
                    None,
                );
                assert_document_contract(&text, &document);

                let mut first_line = 0;

                for source in paragraph_ranges(&text) {
                    let content = &text[source.content.clone()];
                    let independent = layout_wrapped(
                        &system,
                        content,
                        px(18.),
                        &[text_run(content, "IBM Plex Sans")],
                        width,
                        None,
                    );

                    for (idx, expected) in independent.visual_lines.iter().enumerate() {
                        let actual = &document.visual_lines[first_line + idx];
                        assert_eq!(actual.advance_width, expected.advance_width);
                        assert_eq!(
                            document.paint_fragments[actual.fragment_range.clone()],
                            independent.paint_fragments[expected.fragment_range.clone()],
                            "{content:?} at {width:?}"
                        );
                    }

                    for (idx, _character) in content
                        .char_indices()
                        .chain(std::iter::once((content.len(), '\0')))
                    {
                        for affinity in [CaretAffinity::Downstream, CaretAffinity::Upstream] {
                            let local = CaretPosition {
                                index: idx,
                                affinity,
                            };
                            let global = CaretPosition {
                                index: source.content.start + idx,
                                affinity,
                            };
                            let mut expected = independent
                                .platform_layout
                                .caret_bounds(local, px(24.))
                                .unwrap();
                            expected.origin.y += px(24.) * first_line;

                            assert_eq!(
                                document.platform_layout.caret_bounds(global, px(24.)),
                                Some(expected)
                            );
                        }
                    }

                    first_line += independent.visual_lines.len();
                }

                assert_eq!(first_line, document.visual_lines.len());
                assert!(
                    document
                        .paint_fragments
                        .iter()
                        .flat_map(|fragment| &fragment.glyphs)
                        .all(|glyph| glyph.id != GlyphId(0)),
                    "fixtures must cover the sample"
                );
            }
        }

        let arabic = "مرحبا بالعالم";
        let layout = layout_line(
            &system,
            arabic,
            px(24.),
            &[text_run(arabic, "Noto Sans Arabic")],
        );
        let fragment = &layout.paint_fragments[0];
        let nominal = arabic
            .chars()
            .map(|character| system.glyph_for_char(fragment.font_id, character).unwrap())
            .collect::<Vec<_>>();

        assert!(
            fragment
                .glyphs
                .iter()
                .any(|glyph| !nominal.contains(&glyph.id)),
            "Arabic must use contextual forms"
        );
    }

    #[test]
    fn paragraph_interaction_preserves_breaks_and_visual_traversal() {
        let system = test_system();
        let text = "שלום עולם\r\n\nabc אבג def\u{2029}مرحبا بالعالم\n";
        let layout = layout_wrapped(
            &system,
            text,
            px(18.),
            &[text_run(text, "IBM Plex Sans")],
            px(100.),
            None,
        );
        let native = &layout.platform_layout;
        let line_height = px(24.);
        let geometry = |caret| native.caret_bounds(caret, line_height).unwrap();
        let mut caret = native
            .caret_from_pixel_point(point(px(-100.), px(12.)), line_height)
            .unwrap_err();
        let mut steps = 0;

        while let Some(next) = native.adjacent_visual_caret(caret, VisualDirection::Right) {
            let previous = native
                .adjacent_visual_caret(next, VisualDirection::Left)
                .unwrap();
            assert_eq!(
                geometry(previous),
                geometry(caret),
                "visual movement must be reversible"
            );

            let bounds = geometry(next);
            let hit = native
                .caret_from_pixel_point(
                    point(bounds.origin.x, bounds.origin.y + line_height / 2.),
                    line_height,
                )
                .unwrap_or_else(|caret| caret);
            assert_eq!(geometry(hit), bounds);
            assert!(text.is_char_boundary(next.index));
            caret = next;
            steps += 1;
            assert!(steps < text.len() * 4);
        }

        assert!(steps > text.chars().count() / 2);

        for source in paragraph_ranges(text) {
            if source.separator.is_empty() {
                continue;
            }

            let before = CaretPosition::attached_to_next_cluster(source.separator.start);
            let after = CaretPosition::attached_to_next_cluster(source.separator.end);
            assert_eq!(
                native.logical_cluster_after(before),
                Some(source.separator.clone())
            );
            assert_eq!(
                native.logical_cluster_before(after),
                Some(source.separator.clone())
            );
            assert!(
                !native
                    .selection_bounds(source.separator.clone(), line_height)
                    .is_empty()
            );
            assert!(
                native
                    .inline_geometry(source.separator.clone())
                    .unwrap()
                    .is_empty()
            );

            let position = geometry(before).origin + point(px(0.), line_height / 2.);
            let selected = native.selection_from_pixel_point(
                position,
                line_height,
                TextSelectionKind::HardLine,
            );
            assert_eq!(selected, source.content.start..source.separator.end);

            for (movement, expected) in [
                (
                    Direction::Start.with_boundary(Boundary::HardLine),
                    source.content.start,
                ),
                (
                    Direction::End.with_boundary(Boundary::HardLine),
                    source.content.end,
                ),
            ] {
                assert_eq!(
                    native.caret_movement(before, movement, None).result.index,
                    expected
                );
            }
        }

        let crlf = text.find('\r').unwrap();
        let inside = CaretPosition::attached_to_next_cluster(crlf + 1);
        assert_eq!(native.normalized_caret(inside).index, crlf);

        let start = CaretPosition::default();
        let down = native.caret_movement(
            start,
            Direction::Down.with_boundary(Boundary::VisualLine),
            None,
        );
        assert_eq!(
            geometry(down.result).origin.y,
            geometry(start).origin.y + line_height
        );
        assert_eq!(down.vertical_navigation_x, Some(geometry(start).origin.x));

        let mut vertical_caret = start;
        let mut vertical_navigation_x = None;

        for line_index in 1..native.line_count() {
            let moved = native.caret_movement(
                vertical_caret,
                Direction::Down.with_boundary(Boundary::VisualLine),
                vertical_navigation_x,
            );
            vertical_caret = moved.result;
            vertical_navigation_x = moved.vertical_navigation_x;

            assert_eq!(geometry(vertical_caret).origin.y, line_height * line_index);
            assert_eq!(vertical_navigation_x, Some(geometry(start).origin.x));
        }

        for line_index in (0..native.line_count() - 1).rev() {
            let moved = native.caret_movement(
                vertical_caret,
                Direction::Up.with_boundary(Boundary::VisualLine),
                vertical_navigation_x,
            );
            vertical_caret = moved.result;
            vertical_navigation_x = moved.vertical_navigation_x;

            assert_eq!(geometry(vertical_caret).origin.y, line_height * line_index);
        }

        for direction in [VisualDirection::Left, VisualDirection::Right] {
            let movement = match direction {
                VisualDirection::Left => Direction::Left.with_boundary(Boundary::Word),
                VisualDirection::Right => Direction::Right.with_boundary(Boundary::Word),
            };
            let edge_x = match direction {
                VisualDirection::Left => px(-100.),
                VisualDirection::Right => px(10_000.),
            };
            let empty_row = native
                .caret_from_pixel_point(point(edge_x, line_height * 1.5), line_height)
                .unwrap_or_else(|caret| caret);
            let word = native.caret_movement(empty_row, movement, None).result;
            assert_ne!(geometry(word).origin.y, geometry(empty_row).origin.y);
        }

        let selected = native.selection_bounds(0..text.len(), line_height);
        assert!(
            selected
                .windows(2)
                .all(|pair| pair[0].origin.y <= pair[1].origin.y)
        );

        let mixed = "abc אבג\nאבג abc\n";
        let layout = layout_line(&system, mixed, px(18.), &[text_run(mixed, "IBM Plex Sans")]);

        for (line_index, source) in paragraph_ranges(mixed).into_iter().take(2).enumerate() {
            let selection = layout
                .platform_layout
                .selection_bounds(source.separator, line_height);
            let bounds = selection[0];
            assert_eq!(bounds.origin.y, line_height * line_index);

            if line_index == 0 {
                assert!(bounds.origin.x >= layout.visual_lines[line_index].advance_width);
            } else {
                assert!(bounds.right() <= Pixels::ZERO);
            }
        }
    }

    #[test]
    fn paragraph_inline_layout_preserves_styles_empty_rows_and_boundary_boxes() {
        let system = test_system();
        let text = "אבג\r\nalpha مرحبا omega\r\n\r\n";
        let split = text.find("مرحبا").unwrap();
        let runs = [
            TextRun {
                len: split,
                color: hsla(0.2, 0.8, 0.5, 1.),
                underline: Some(UnderlineStyle {
                    thickness: px(1.),
                    ..Default::default()
                }),
                ..text_run(text, "IBM Plex Sans")
            },
            TextRun {
                len: text.len() - split,
                color: hsla(0.7, 0.8, 0.5, 1.),
                ..text_run(text, "IBM Plex Sans")
            },
        ];
        let styles = [InlineTextStyle {
            range: split..text.len(),
            font_size: px(26.),
            line_height: px(48.),
        }];
        let separator = text.find('\r').unwrap();
        let boxes = [0, separator, separator + 1, separator + 2, text.len()]
            .into_iter()
            .enumerate()
            .map(|(idx, index)| InlineBoxRequest {
                id: idx as u64,
                index,
                size: size(px(10.), px(12.)),
                vertical_align: VerticalAlign::Baseline,
            })
            .collect::<Vec<_>>();

        for text_align in [TextAlign::Left, TextAlign::Center, TextAlign::Right] {
            let inline = system.layout_inline(InlineLayoutRequest {
                text,
                runs: &runs,
                text_styles: &styles,
                boxes: &boxes,
                font_size: px(18.),
                line_height: px(28.),
                text_metrics: InlineTextMetrics {
                    ascent: px(16.),
                    descent: px(4.),
                    x_height: px(9.),
                },
                wrap_width: Some(px(260.)),
                line_clamp: None,
                text_align,
            });
            let mut ids = inline
                .boxes
                .iter()
                .map(|inline_box| inline_box.id)
                .collect::<Vec<_>>();
            ids.sort_unstable();

            assert_eq!(ids, vec![0, 1, 2, 3, 4]);
            assert_inline_geometry_is_contained(&inline, px(260.));
            assert_eq!(inline.layout.len, text.len());

            for inline_box in &inline.boxes {
                let expected_line = match inline_box.id {
                    0..=2 => 0,
                    3 => 1,
                    4 => inline.lines.len() - 1,
                    _ => unreachable!(),
                };

                assert_eq!(inline_box.line_index, expected_line);
            }

            for line in inline.lines.iter().rev().take(2) {
                assert!(
                    line.size.height >= px(47.99),
                    "empty rows must retain styled line height: {line:?}"
                );
            }

            let empty_line = inline.lines[inline.lines.len() - 2];
            let empty_caret = inline
                .layout
                .platform_layout
                .caret_bounds(
                    CaretPosition::attached_to_next_cluster(text.len() - 2),
                    px(28.),
                )
                .unwrap();
            assert_eq!(empty_line.size.width, Pixels::ZERO);
            assert_eq!(empty_caret.origin.x, empty_line.origin.x);

            assert!(
                inline
                    .layout
                    .paint_fragments
                    .iter()
                    .any(|fragment| fragment.style.color == runs[0].color
                        && fragment.style.underline.is_some())
            );
            assert!(
                inline
                    .layout
                    .paint_fragments
                    .iter()
                    .any(|fragment| fragment.style.color == runs[1].color
                        && fragment.font_size == px(26.))
            );

            let regions = inline
                .layout
                .platform_layout
                .inline_geometry(split..text.len())
                .unwrap();
            assert!(regions.iter().all(|geometry| geometry.bounds.origin.y
                >= inline.lines[geometry.visual_line_index].origin.y));
        }
    }

    #[test]
    fn paragraph_composition_preserves_existing_latin_wrapping_and_clamping() {
        let system = test_system();
        let text = "one two three four\nfive six seven eight\nnine ten\n";
        let runs = [text_run(text, "IBM Plex Sans")];

        for max_lines in [None, Some(0), Some(1), Some(3), Some(8)] {
            let document = layout_wrapped(&system, text, px(18.), &runs, px(80.), max_lines);
            let previous = system
                .parley_paragraph_layout(ParleyLayoutParams {
                    text: TextLayoutRequest {
                        text,
                        font_size: px(18.),
                        runs: &runs,
                        wrap_width: Some(px(80.)),
                        line_clamp: max_lines,
                    },
                    inline: None,
                })
                .unwrap();

            assert_eq!(
                document
                    .visual_lines
                    .iter()
                    .map(|line| line.text_range.clone())
                    .collect::<Vec<_>>(),
                previous
                    .layout
                    .visual_lines
                    .iter()
                    .map(|line| line.text_range.clone())
                    .collect::<Vec<_>>()
            );
            assert_document_contract(text, &document);
        }
    }

    #[test]
    fn document_layout_contract_covers_scripts_breaks_wrapping_and_clamping() {
        let system = test_system();
        let cases = [
            ("empty", "", px(120.0), None, 1usize),
            ("hard breaks", "one\ntwo\nthree", px(500.0), None, 3),
            ("trailing break", "one\n", px(500.0), None, 2),
            ("latin wrap", "one two three four five", px(54.0), None, 2),
            (
                "clamped",
                "one two three four five six",
                px(45.0),
                Some(2),
                2,
            ),
            ("mixed bidi", "abc אבג العربية xyz", px(500.0), None, 1),
            ("cjk", "日本語中文テキスト", px(500.0), None, 1),
            ("thai", "ภาษาไทย ก้ กี", px(500.0), None, 1),
            ("ligatures", "office affine ffi", px(500.0), None, 1),
            ("emoji", "👩🏽‍💻 family 👨‍👩‍👧‍👦 🇬🇧", px(500.0), None, 1),
        ];

        for (name, text, width, max_lines, minimum_lines) in cases {
            let runs = (!text.is_empty())
                .then(|| text_run(text, IBM_PLEX.family))
                .into_iter()
                .collect::<Vec<_>>();
            let layout = layout_wrapped(&system, text, px(18.0), &runs, width, max_lines);
            assert_document_contract(text, &layout);

            if max_lines.is_some() {
                assert_eq!(layout.visual_lines.len(), minimum_lines, "{name}");
            } else {
                assert!(layout.visual_lines.len() >= minimum_lines, "{name}");
            }
        }

        let layout = layout_line(&system, "", px(18.0), &[]);
        assert_document_contract("", &layout);
        assert_eq!(layout.visual_lines[0].text_range, 0..0);
        assert_eq!(layout.width, Pixels::ZERO);
        assert!(layout.paint_fragments.is_empty());
    }

    #[test]
    fn wrapping_does_not_move_breaks_forward_as_width_shrinks() {
        let system = test_system();
        let cases = [
            (
                "code punctuation",
                "Lilex regular: fn main() { println!(\"hello\"); }",
                LILEX.family,
            ),
            (
                "prose punctuation",
                "One sentence with punctuation, followed by another.",
                IBM_PLEX.family,
            ),
            (
                "nested delimiters",
                "call(value, other_value) } trailing",
                IBM_PLEX.family,
            ),
            (
                "mixed scripts",
                "English العربية 日本語 punctuation.",
                IBM_PLEX.family,
            ),
        ];

        for (name, text, family) in cases {
            let runs = [text_run(text, family)];
            let mut previous_end = text.len();
            for half_width in (200..=1200).rev() {
                let width = px(half_width as f32 / 2.0);
                let layout = layout_wrapped(&system, text, px(16.0), &runs, width, None);
                let first_line_end = layout.visual_lines[0].text_range.end;
                assert!(
                    first_line_end <= previous_end,
                    "{name}: first break moved forward at {width:?}: {previous_end} -> {first_line_end}"
                );
                previous_end = first_line_end;
            }
        }
    }

    #[test]
    fn styled_document_uses_parley_paint_runs_without_changing_geometry() {
        let system = test_system();
        let text = "office café العربية";
        let first_end = "office ".len();
        let second_end = first_end + "café ".len();
        let mut first_font = font(SOURCE_SERIF.family);
        first_font.features = GpuiFontFeatures::disable_ligatures();
        first_font.fallbacks = Some(FontFallbacks::from_fonts(vec![
            IBM_PLEX.family.into(),
            NOTO_COLOR_EMOJI.family.into(),
        ]));
        let base_runs = vec![
            TextRun {
                len: first_end,
                font: first_font,
                letter_spacing: Some(px(0.4)),
                color: hsla(0.0, 0.8, 0.4, 1.0),
                ..Default::default()
            },
            TextRun {
                len: second_end - first_end,
                font: font(IBM_PLEX.family).bold(),
                color: hsla(0.35, 0.7, 0.35, 1.0),
                ..Default::default()
            },
            TextRun {
                len: text.len() - second_end,
                font: font(IBM_PLEX.family),
                color: hsla(0.6, 0.8, 0.45, 1.0),
                ..Default::default()
            },
        ];
        let mut decorated_runs = base_runs.clone();
        decorated_runs[0].background_color = Some(hsla(0.1, 0.5, 0.5, 0.3));
        decorated_runs[0].underline = Some(UnderlineStyle {
            thickness: px(1.5),
            color: None,
            wavy: true,
        });

        decorated_runs[1].strikethrough = Some(StrikethroughStyle {
            thickness: px(1.0),
            color: None,
        });

        let plain = layout_line(&system, text, px(20.0), &base_runs);
        let decorated = layout_line(&system, text, px(20.0), &decorated_runs);
        let geometry = |layout: &LineLayout| {
            layout
                .paint_fragments
                .iter()
                .flat_map(|fragment| {
                    fragment
                        .glyphs
                        .iter()
                        .map(move |glyph| (fragment.font_id, glyph.id, glyph.position))
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(plain.width, decorated.width);
        assert_eq!(geometry(&plain), geometry(&decorated));
        assert!(decorated.paint_fragments.iter().any(|fragment| {
            fragment.style.background_color.is_some() && fragment.style.underline.is_some()
        }));

        assert!(
            decorated
                .paint_fragments
                .iter()
                .any(|fragment| fragment.style.strikethrough.is_some())
        );
    }

    #[test]
    fn font_registration_invalidates_layouts_and_distinguishes_requested_instances() {
        let backend = Arc::new(
            ParleyTextSystem::new_with_system_font(SystemFonts::Skip, IBM_PLEX.family)
                .with_fallback_families([IBM_PLEX.family]),
        );
        backend
            .add_fonts(vec![Cow::Borrowed(IBM_PLEX.data)])
            .unwrap();
        let text_system = Arc::new(TextSystem::new(backend.clone()));
        let window_text_system = WindowTextSystem::new(text_system);
        let text = "registered later";
        let run = text_run(text, SOURCE_SERIF.family);
        let before = window_text_system
            .shape_text(text, px(18.0), std::slice::from_ref(&run), None, None)
            .unwrap();
        let fallback_id = before.paint_fragments[0].font_id;

        backend
            .add_fonts(vec![Cow::Borrowed(SOURCE_SERIF.data)])
            .unwrap();
        let after = window_text_system
            .shape_text(text, px(18.0), &[run], None, None)
            .unwrap();
        assert_ne!(fallback_id, after.paint_fragments[0].font_id);

        backend
            .add_fonts(vec![Cow::Borrowed(IBM_PLEX_SEMIBOLD.data)])
            .unwrap();
        let regular = backend.font_id(&font(IBM_PLEX.family)).unwrap();
        let bold = backend.font_id(&font(IBM_PLEX.family).bold()).unwrap();
        assert_ne!(regular, bold);

        let mut variable = font(SOURCE_SERIF.family);
        variable.weight = GpuiFontWeight(725.0);
        variable.style = GpuiFontStyle::Oblique;
        let variable_id = backend.font_id(&variable).unwrap();
        let source_serif_regular = backend.font_id(&font(SOURCE_SERIF.family)).unwrap();
        assert_ne!(source_serif_regular, variable_id);
    }

    #[test]
    fn native_interaction_handles_bidi_atomic_clusters_and_semantic_selection() {
        let system = test_system();
        for text in ["👩🏽‍💻", "👨‍👩‍👧‍👦", "🇬🇧", "ก้"] {
            let layout = layout_line(&system, text, px(22.0), &[text_run(text, IBM_PLEX.family)]);
            let wrapped = wrapped(layout, px(500.0));
            assert_eq!(
                wrapped.logical_cluster_after(CaretPosition::default()),
                Some(0..text.len()),
                "{text:?} must be one editable cluster"
            );
        }

        let single_line_text = "abcd";
        let single_line = wrapped(
            layout_line(
                &system,
                single_line_text,
                px(20.0),
                &[text_run(single_line_text, IBM_PLEX.family)],
            ),
            px(500.0),
        );
        let middle = CaretPosition::attached_to_next_cluster(2);
        assert_eq!(
            single_line
                .caret_movement(
                    middle,
                    Direction::Up.with_boundary(Boundary::VisualLine),
                    None
                )
                .result
                .index,
            0
        );
        assert_eq!(
            single_line
                .caret_movement(
                    middle,
                    Direction::Down.with_boundary(Boundary::VisualLine),
                    None
                )
                .result
                .index,
            single_line_text.len()
        );

        let text = "abc אבג العربية xyz";
        let layout = wrapped(
            layout_wrapped(
                &system,
                text,
                px(20.0),
                &[text_run(text, IBM_PLEX.family)],
                px(90.0),
                None,
            ),
            px(90.0),
        );
        let line_height = px(26.0);
        let start = layout
            .closest_caret_for_pixel_point(point(px(-10.0), px(10.0)), line_height)
            .unwrap_err();
        let end = layout
            .closest_caret_for_pixel_point(point(px(10_000.0), px(10.0)), line_height)
            .unwrap_err();
        let selection = CaretSelection {
            anchor: end,
            caret: start,
        };
        let collapsed_left = layout.selection_movement(
            selection,
            Direction::Left.with_boundary(Boundary::Cluster),
            false,
            None,
            line_height,
        );
        assert!(collapsed_left.result.is_empty());
        assert_eq!(collapsed_left.result.caret, start);
        let collapsed_right = layout.selection_movement(
            selection,
            Direction::Right.with_boundary(Boundary::Cluster),
            false,
            None,
            line_height,
        );
        assert_eq!(collapsed_right.result.caret, end);

        let word = layout.selection_movement(
            start.into(),
            Direction::Right.with_boundary(Boundary::Word),
            true,
            None,
            line_height,
        );
        assert_eq!(word.result.anchor, start);
        assert_ne!(word.result.caret, start);
        let down = layout.selection_movement(
            word.result.caret.into(),
            Direction::Down.with_boundary(Boundary::VisualLine),
            false,
            None,
            line_height,
        );
        assert!(down.vertical_navigation_x.is_some());
        let maintained_x = layout
            .selection_movement(
                down.result,
                Direction::Down.with_boundary(Boundary::VisualLine),
                false,
                down.vertical_navigation_x,
                line_height,
            )
            .vertical_navigation_x;
        assert_eq!(maintained_x, down.vertical_navigation_x);
        let selection = layout.selection_from_pixel_point(
            point(px(12.0), px(10.0)),
            line_height,
            TextSelectionKind::Word,
        );
        assert!(!selection.is_empty());
        assert!(text.is_char_boundary(selection.start));
        assert!(text.is_char_boundary(selection.end));
    }

    #[test]
    fn rasterizes_monochrome_and_color_glyphs_with_expected_buffer_formats() {
        let system = test_system();

        let mixed_color_face = layout_line(
            &system,
            "1 1",
            px(24.0),
            &[text_run("1 1", NOTO_COLOR_EMOJI.family)],
        );
        let color_flags = mixed_color_face
            .paint_fragments
            .iter()
            .flat_map(|fragment| fragment.glyphs.iter().map(|glyph| glyph.is_emoji))
            .collect::<Vec<_>>();
        assert_eq!(color_flags, [true, false, true]);

        for (text, mode, expected_format) in [
            (
                "A",
                GlyphRenderMode::Grayscale,
                RasterizedGlyphFormat::AlphaMask,
            ),
            (
                "A",
                GlyphRenderMode::Subpixel,
                RasterizedGlyphFormat::BgraSubpixelMask,
            ),
            (
                "😀",
                GlyphRenderMode::Color,
                RasterizedGlyphFormat::BgraColor,
            ),
        ] {
            let expect_color = mode == GlyphRenderMode::Color;
            let layout = layout_line(&system, text, px(24.0), &[text_run(text, IBM_PLEX.family)]);
            let glyph = layout
                .paint_fragments
                .iter()
                .flat_map(|fragment| {
                    fragment
                        .glyphs
                        .iter()
                        .map(move |glyph| (fragment.font_id, glyph))
                })
                .find(|(_, glyph)| glyph.is_emoji == expect_color)
                .unwrap();
            let raster = system
                .rasterize_glyph(&RenderGlyphParams {
                    font_id: glyph.0,
                    glyph_id: glyph.1.id,
                    font_size: px(24.0),
                    subpixel_variant: Default::default(),
                    scale_factor: 1.0,
                    raster_style: PreparedRasterStyle::independent(mode),
                })
                .unwrap();
            assert_eq!(raster.bounds.size, raster.size);
            assert!(raster.size.width.0 > 0 && raster.size.height.0 > 0);
            let channels = match raster.format {
                gpui::RasterizedGlyphFormat::AlphaMask => 1,
                gpui::RasterizedGlyphFormat::BgraSubpixelMask
                | gpui::RasterizedGlyphFormat::BgraColor => 4,
            };

            assert_eq!(raster.format, expected_format);
            assert_eq!(
                raster.pixels.len(),
                raster.size.width.0 as usize * raster.size.height.0 as usize * channels
            );
            match expected_format {
                RasterizedGlyphFormat::AlphaMask => {
                    assert!(raster.pixels.iter().any(|&alpha| alpha > 0));
                }
                RasterizedGlyphFormat::BgraSubpixelMask => {
                    assert!(
                        raster
                            .pixels
                            .chunks_exact(4)
                            .any(|pixel| { pixel[0] != pixel[1] || pixel[1] != pixel[2] })
                    );
                }
                RasterizedGlyphFormat::BgraColor => {
                    assert!(raster.pixels.chunks_exact(4).any(|pixel| {
                        pixel[3] > 128
                            && (pixel[0] != pixel[1]
                                || pixel[1] != pixel[2]
                                || pixel[0] != pixel[2])
                    }));
                }
            }
        }
    }

    #[test]
    fn native_backend_receives_the_face_instance_selected_during_shaping() {
        let seen = Arc::new(Mutex::new(None));
        let system = ParleyTextSystem::new_with_rasterizer(
            SystemFonts::Skip,
            SOURCE_SERIF.family,
            RecordingRasterizer { seen: seen.clone() },
        );
        system
            .add_fonts(vec![Cow::Borrowed(SOURCE_SERIF.data)])
            .unwrap();

        let text = "A";
        let layout = layout_line(
            &system,
            text,
            px(22.0),
            &[TextRun {
                len: text.len(),
                font: font(SOURCE_SERIF.family).bold().italic(),
                ..Default::default()
            }],
        );
        let fragment = &layout.paint_fragments[0];
        let font_id = fragment.font_id;
        let params = RenderGlyphParams {
            font_id,
            glyph_id: fragment.glyphs[0].id,
            font_size: px(22.0),
            subpixel_variant: point(2, 0),
            scale_factor: 1.5,
            raster_style: PreparedRasterStyle::independent(GlyphRenderMode::Grayscale),
        };

        let raster = system.rasterize_glyph(&params).unwrap();
        assert_eq!(raster.format, RasterizedGlyphFormat::AlphaMask);
        assert_eq!(raster.size, Size::default());
        assert!(raster.pixels.is_empty());

        let seen = seen.lock().clone().expect("rasterizer saw a face");
        assert_eq!(seen.font_id, font_id);
        assert_eq!(seen.face_index, 0);
        assert!(seen.data_matches);
        assert!(!seen.has_color_glyphs);
        assert_eq!(seen.synthesis.embolden, false);
        assert_eq!(seen.synthesis.skew_degrees, Some(14.0));
        let weight = seen
            .variations
            .iter()
            .find(|variation| variation.tag == skrifa::Tag::new(b"wght"))
            .expect("weight design coordinate");
        assert!((weight.value - 700.0).abs() < 0.05, "{weight:?}");
    }

    #[derive(Clone, Debug)]
    struct SeenRasterFace {
        font_id: FontId,
        face_index: u32,
        data_matches: bool,
        variations: Vec<FontVariation>,
        synthesis: FontSynthesis,
        has_color_glyphs: bool,
    }

    struct RecordingRasterizer {
        seen: Arc<Mutex<Option<SeenRasterFace>>>,
    }

    impl GlyphRasterizer for RecordingRasterizer {
        fn prepare_style(&self, request: RasterStyleRequest) -> PreparedRasterStyle {
            PreparedRasterStyle::independent(request.requested_mode)
        }

        fn rasterize(
            &mut self,
            face: RasterFace<'_>,
            _params: &RenderGlyphParams,
        ) -> Result<RasterizedGlyph> {
            *self.seen.lock() = Some(SeenRasterFace {
                font_id: face.font_id,
                face_index: face.face_index,
                data_matches: face.data == SOURCE_SERIF.data,
                variations: face.variations.to_vec(),
                synthesis: face.synthesis,
                has_color_glyphs: face.has_color_glyphs,
            });

            Ok(RasterizedGlyph::empty(RasterizedGlyphFormat::AlphaMask))
        }
    }

    mod inline_reflow;
}
