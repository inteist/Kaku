use crate::customglyph::{BlockAlpha, BlockCoord, Poly, PolyCommand, PolyStyle};
use crate::termwindow::box_model::*;
use crate::termwindow::modal::Modal;
use crate::termwindow::render::corners::{TOP_LEFT_ROUNDED_CORNER, TOP_RIGHT_ROUNDED_CORNER};
use crate::termwindow::{TermWindow, UIItem};
use crate::utilsprites::RenderMetrics;
use anyhow::Context;
use config::{Dimension, DimensionContext, TabBarColors};
use mux::tab::TabId;
use mux::Mux;
use std::cell::{Ref, RefCell};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use termwiz::cell::CellAttributes;
use termwiz::surface::SEQ_ZERO;
use wezterm_term::color::{ColorAttribute, ColorPalette};
use wezterm_term::{KeyCode, KeyModifiers, Line, MouseEvent};
use window::color::LinearRgba;
use window::{DeadKeyStatus, Point, Rect, Size, WindowOps};

const INLINE_CURSOR: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Frac(1, 2), BlockCoord::Zero),
        PolyCommand::LineTo(BlockCoord::Frac(1, 2), BlockCoord::Frac(12, 16)),
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Outline,
}];

pub struct TabRenameModal {
    element: RefCell<Option<Vec<ComputedElement>>>,
    tab_id: TabId,
    anchor: UIItem,
    value: RefCell<String>,
    cursor: RefCell<usize>,
    selection: RefCell<Option<(usize, usize)>>,
}

impl TabRenameModal {
    fn displayed_title(term_window: &TermWindow, tab_id: TabId) -> Option<String> {
        let mux = Mux::get();
        let window = mux.get_window(term_window.mux_window_id)?;

        term_window
            .tab_bar
            .items()
            .iter()
            .find_map(|item| match item.item {
                crate::tabbar::TabBarItem::Tab { tab_idx, .. } => window
                    .get_by_idx(tab_idx)
                    .filter(|tab| tab.tab_id() == tab_id)
                    .map(|_| item.title.as_str().trim().to_string())
                    .filter(|title| !title.is_empty()),
                _ => None,
            })
    }

    pub fn new(
        term_window: &mut TermWindow,
        tab_id: TabId,
        anchor: UIItem,
    ) -> anyhow::Result<Self> {
        let mux = Mux::get();
        let tab = mux
            .get_tab(tab_id)
            .context("tab vanished before rename could start")?;

        // Use the raw tab title first to avoid capturing format-tab-title decorations
        // (icons, indices, prefixes). Fall back to displayed title only if raw title is empty.
        let mut value = tab.get_title();
        if value.is_empty() {
            value = tab
                .get_active_pane()
                .map(|pane| pane.get_title())
                .unwrap_or_default();
        }
        if value.is_empty() {
            value = Self::displayed_title(term_window, tab_id).unwrap_or_default();
        }

        let cursor = value.chars().count();

        let modal = Self {
            element: RefCell::new(None),
            tab_id,
            anchor,
            value: RefCell::new(value),
            cursor: RefCell::new(cursor),
            selection: RefCell::new(None),
        };
        modal.reconfigure(term_window);
        Ok(modal)
    }

    fn value_len(&self) -> usize {
        self.value.borrow().chars().count()
    }

    fn normalized_selection(selection: Option<(usize, usize)>) -> Option<(usize, usize)> {
        selection.and_then(|(start, end)| {
            if start == end {
                None
            } else if start < end {
                Some((start, end))
            } else {
                Some((end, start))
            }
        })
    }

    fn selection_bounds(&self) -> Option<(usize, usize)> {
        Self::normalized_selection(*self.selection.borrow())
    }

    fn clear_selection(&self) {
        self.selection.borrow_mut().take();
    }

    fn select_all(&self) -> bool {
        let len = self.value_len();
        if len == 0 {
            return false;
        }
        *self.selection.borrow_mut() = Some((0, len));
        *self.cursor.borrow_mut() = len;
        true
    }

    fn byte_idx_for_char(s: &str, idx: usize) -> usize {
        if idx == 0 {
            return 0;
        }
        s.char_indices()
            .nth(idx)
            .map(|(idx, _)| idx)
            .unwrap_or(s.len())
    }

    fn delete_selection(&self) -> bool {
        let Some((start, end)) = self.selection_bounds() else {
            return false;
        };

        let mut value = self.value.borrow_mut();
        let start_byte = Self::byte_idx_for_char(&value, start);
        let end_byte = Self::byte_idx_for_char(&value, end);
        value.replace_range(start_byte..end_byte, "");
        *self.cursor.borrow_mut() = start;
        self.clear_selection();
        true
    }

    fn insert_char(&self, c: char) {
        let _ = self.delete_selection();
        let mut value = self.value.borrow_mut();
        let mut cursor = self.cursor.borrow_mut();
        let byte_idx = Self::byte_idx_for_char(&value, *cursor);
        value.insert(byte_idx, c);
        *cursor += 1;
        self.clear_selection();
    }

    fn backspace(&self) -> bool {
        if self.delete_selection() {
            return true;
        }

        let mut value = self.value.borrow_mut();
        let mut cursor = self.cursor.borrow_mut();
        if *cursor == 0 {
            return false;
        }

        let end = Self::byte_idx_for_char(&value, *cursor);
        let start = Self::byte_idx_for_char(&value, cursor.saturating_sub(1));
        value.replace_range(start..end, "");
        *cursor -= 1;
        true
    }

    fn delete(&self) -> bool {
        if self.delete_selection() {
            return true;
        }

        let mut value = self.value.borrow_mut();
        let cursor = *self.cursor.borrow();
        if cursor >= value.chars().count() {
            return false;
        }

        let start = Self::byte_idx_for_char(&value, cursor);
        let end = Self::byte_idx_for_char(&value, cursor + 1);
        value.replace_range(start..end, "");
        true
    }

    fn move_left(&self) -> bool {
        if let Some((start, _)) = self.selection_bounds() {
            *self.cursor.borrow_mut() = start;
            self.clear_selection();
            return true;
        }

        let mut cursor = self.cursor.borrow_mut();
        if *cursor == 0 {
            return false;
        }
        *cursor -= 1;
        true
    }

    fn move_right(&self) -> bool {
        if let Some((_, end)) = self.selection_bounds() {
            *self.cursor.borrow_mut() = end;
            self.clear_selection();
            return true;
        }

        let len = self.value_len();
        let mut cursor = self.cursor.borrow_mut();
        if *cursor >= len {
            return false;
        }
        *cursor += 1;
        true
    }

    fn move_to_start(&self) -> bool {
        self.clear_selection();
        let mut cursor = self.cursor.borrow_mut();
        if *cursor == 0 {
            return false;
        }
        *cursor = 0;
        true
    }

    fn move_to_end(&self) -> bool {
        self.clear_selection();
        let len = self.value_len();
        let mut cursor = self.cursor.borrow_mut();
        if *cursor == len {
            return false;
        }
        *cursor = len;
        true
    }

    fn clear(&self) -> bool {
        let mut value = self.value.borrow_mut();
        if value.is_empty() {
            return false;
        }
        value.clear();
        *self.cursor.borrow_mut() = 0;
        self.clear_selection();
        true
    }

    fn commit(&self, term_window: &mut TermWindow) {
        if let Some(tab) = Mux::get().get_tab(self.tab_id) {
            tab.set_title(self.value.borrow().as_str());
        }
        term_window.cancel_modal();
    }

    fn point_in_bounds(&self, x: i64, y: i64) -> bool {
        self.element
            .borrow()
            .as_ref()
            .and_then(|elements| elements.first())
            .map(|element| {
                let bounds = element.bounds;
                x as f32 >= bounds.min_x()
                    && x as f32 <= bounds.max_x()
                    && y as f32 >= bounds.min_y()
                    && y as f32 <= bounds.max_y()
            })
            .unwrap_or(false)
    }

    fn is_active_in_window(&self, term_window: &TermWindow) -> bool {
        let mux = Mux::get();
        mux.get_window(term_window.mux_window_id)
            .and_then(|window| window.get_active().map(|tab| tab.tab_id() == self.tab_id))
            .unwrap_or(true)
    }

    fn resolved_tab_title_colors(
        &self,
        term_window: &TermWindow,
        palette: &ColorPalette,
    ) -> (Option<LinearRgba>, Option<LinearRgba>) {
        let mux = Mux::get();
        let Some(window) = mux.get_window(term_window.mux_window_id) else {
            return (None, None);
        };

        term_window
            .tab_bar
            .items()
            .iter()
            .find_map(|item| match item.item {
                crate::tabbar::TabBarItem::Tab { tab_idx, .. } => window
                    .get_by_idx(tab_idx)
                    .filter(|tab| tab.tab_id() == self.tab_id)
                    .and_then(|_| item.title.get_cell(0))
                    .map(|cell| {
                        let bg = match cell.attrs().background() {
                            ColorAttribute::Default => None,
                            col => Some(palette.resolve_bg(col).to_linear()),
                        };

                        let fg = match cell.attrs().foreground() {
                            ColorAttribute::Default => None,
                            col => Some(palette.resolve_fg(col).to_linear()),
                        };

                        (bg, fg)
                    }),
                _ => None,
            })
            .unwrap_or((None, None))
    }

    fn title_text_attrs(&self, term_window: &TermWindow) -> CellAttributes {
        let mux = Mux::get();
        if let Some(window) = mux.get_window(term_window.mux_window_id) {
            if let Some(attrs) =
                term_window
                    .tab_bar
                    .items()
                    .iter()
                    .find_map(|item| match item.item {
                        crate::tabbar::TabBarItem::Tab { tab_idx, .. } => window
                            .get_by_idx(tab_idx)
                            .filter(|tab| tab.tab_id() == self.tab_id)
                            .and_then(|_| item.title.get_cell(0))
                            .map(|cell| cell.attrs().clone()),
                        _ => None,
                    })
            {
                return attrs;
            }
        }

        let tab_bar_colors = term_window
            .config
            .colors
            .as_ref()
            .and_then(|c| c.tab_bar.as_ref())
            .cloned()
            .unwrap_or_else(TabBarColors::default);

        if self.is_active_in_window(term_window) {
            tab_bar_colors.active_tab().as_cell_attributes()
        } else {
            tab_bar_colors.inactive_tab().as_cell_attributes()
        }
    }

    fn colors(
        &self,
        term_window: &mut TermWindow,
        palette: &ColorPalette,
    ) -> (ElementColors, Option<Corners>) {
        let tab_bar_colors = term_window
            .config
            .colors
            .as_ref()
            .and_then(|c| c.tab_bar.as_ref())
            .cloned()
            .unwrap_or_else(TabBarColors::default);
        let active = self.is_active_in_window(term_window);
        let (title_bg, title_fg) = self.resolved_tab_title_colors(term_window, palette);
        let corners = Some(Corners {
            top_left: SizedPoly {
                width: Dimension::Cells(0.5),
                height: Dimension::Cells(0.5),
                poly: TOP_LEFT_ROUNDED_CORNER,
            },
            top_right: SizedPoly {
                width: Dimension::Cells(0.5),
                height: Dimension::Cells(0.5),
                poly: TOP_RIGHT_ROUNDED_CORNER,
            },
            bottom_left: SizedPoly {
                width: Dimension::Cells(0.0),
                height: Dimension::Cells(0.33),
                poly: &[],
            },
            bottom_right: SizedPoly {
                width: Dimension::Cells(0.0),
                height: Dimension::Cells(0.33),
                poly: &[],
            },
        });

        let visuals = if active {
            let active_tab = tab_bar_colors.active_tab();
            let bg = title_bg.unwrap_or_else(|| active_tab.bg_color.to_linear());
            let fg = title_fg.unwrap_or_else(|| active_tab.fg_color.to_linear());
            ElementColors {
                border: BorderColor::new(bg),
                bg: bg.into(),
                text: fg.into(),
            }
        } else {
            let inactive_tab = tab_bar_colors.inactive_tab();
            let bg = title_bg.unwrap_or_else(|| inactive_tab.bg_color.to_linear());
            let fg = title_fg.unwrap_or_else(|| inactive_tab.fg_color.to_linear());
            let edge = tab_bar_colors.inactive_tab_edge().to_linear();
            ElementColors {
                border: BorderColor {
                    left: bg,
                    right: edge,
                    top: bg,
                    bottom: bg,
                },
                bg: bg.into(),
                text: fg.into(),
            }
        };

        (visuals, corners)
    }

    fn cursor_color_for_background(
        palette: &ColorPalette,
        bg: LinearRgba,
        text: LinearRgba,
    ) -> LinearRgba {
        let white = LinearRgba::with_components(1.0, 1.0, 1.0, 1.0);
        let black = LinearRgba::with_components(0.0, 0.0, 0.0, 1.0);
        let candidates = [
            palette
                .cursor_border
                .to_linear()
                .when_fully_transparent(palette.cursor_bg.to_linear())
                .when_fully_transparent(palette.cursor_fg.to_linear())
                .when_fully_transparent(text),
            palette.cursor_bg.to_linear().when_fully_transparent(text),
            palette.cursor_fg.to_linear().when_fully_transparent(text),
            text,
            white,
            black,
        ];

        candidates
            .iter()
            .copied()
            .max_by(|a, b| a.contrast_ratio(&bg).total_cmp(&b.contrast_ratio(&bg)))
            .unwrap_or(text)
    }

    fn text_segment(
        font: &std::rc::Rc<wezterm_font::LoadedFont>,
        palette: &ColorPalette,
        text: String,
        attrs: &CellAttributes,
    ) -> Element {
        let line = Line::from_text(&text, attrs, SEQ_ZERO, None);
        Element::with_line(font, &line, palette)
    }

    fn cursor_segment(
        font: &std::rc::Rc<wezterm_font::LoadedFont>,
        metrics: &RenderMetrics,
        fg: LinearRgba,
        visible: bool,
    ) -> Element {
        Element::new(
            font,
            ElementContent::Poly {
                line_width: metrics.underline_height.max(2),
                poly: SizedPoly {
                    poly: if visible { INLINE_CURSOR } else { &[] },
                    width: Dimension::Pixels(2.0),
                    height: Dimension::Pixels((metrics.cell_size.height as f32 - 6.0).max(1.0)),
                },
            },
        )
        .vertical_align(VerticalAlign::Middle)
        // Render the caret on top of the insertion point instead of reserving
        // layout width, so it doesn't look like an extra trailing space.
        .margin(BoxDimension {
            left: Dimension::Pixels(1.0),
            right: Dimension::Pixels(-3.0),
            top: Dimension::Pixels(-6.0),
            bottom: Dimension::Pixels(1.0),
        })
        .colors(ElementColors {
            border: BorderColor::default(),
            bg: LinearRgba::TRANSPARENT.into(),
            text: fg.into(),
        })
    }

    fn update_text_cursor_position(
        term_window: &mut TermWindow,
        computed: &ComputedElement,
        cursor_element_idx: usize,
    ) {
        let cursor_rect = match &computed.content {
            ComputedElementContent::Children(kids) => {
                let Some(kid) = kids.get(cursor_element_idx) else {
                    return;
                };

                match &kid.content {
                    ComputedElementContent::Poly { poly, .. } => Rect::new(
                        Point::new(
                            kid.content_rect.min_x().floor() as isize,
                            kid.content_rect.min_y().floor() as isize,
                        ),
                        Size::new(
                            poly.width.ceil().max(1.0) as isize,
                            poly.height.ceil().max(1.0) as isize,
                        ),
                    ),
                    ComputedElementContent::Text(cells) => {
                        let _ = cells;

                        Rect::new(
                            Point::new(
                                (kid.content_rect.max_x() - 1.0).floor() as isize,
                                kid.content_rect.min_y().floor() as isize,
                            ),
                            Size::new(2, kid.content_rect.height().ceil().max(1.0) as isize),
                        )
                    }
                    ComputedElementContent::Children(_) => Rect::new(
                        Point::new(
                            kid.content_rect.max_x().floor() as isize,
                            kid.content_rect.min_y().floor() as isize,
                        ),
                        Size::new(2, kid.content_rect.height().ceil().max(1.0) as isize),
                    ),
                }
            }
            ComputedElementContent::Text(_) | ComputedElementContent::Poly { .. } => Rect::new(
                Point::new(
                    computed.content_rect.max_x().floor() as isize,
                    computed.content_rect.min_y().floor() as isize,
                ),
                Size::new(2, computed.content_rect.height().ceil().max(1.0) as isize),
            ),
        };

        if let Some(window) = &term_window.window {
            window.set_text_cursor_position(cursor_rect);
        }
    }

    fn compute(&self, term_window: &mut TermWindow) -> anyhow::Result<Vec<ComputedElement>> {
        let font = term_window
            .fonts
            .title_font()
            .context("resolve tab rename font")?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let palette = term_window.palette().clone();
        let (element_colors, border_corners) = self.colors(term_window, &palette);
        let text_attrs = self.title_text_attrs(term_window);
        let text = match &element_colors.text {
            InheritableColor::Color(color) => *color,
            InheritableColor::Inherited | InheritableColor::Animated { .. } => {
                LinearRgba(0.93, 0.94, 0.97, 1.0)
            }
        };
        let bg = match &element_colors.bg {
            InheritableColor::Color(color) => *color,
            InheritableColor::Inherited | InheritableColor::Animated { .. } => {
                palette.background.to_linear()
            }
        };
        let cursor_color = Self::cursor_color_for_background(&palette, bg, text);
        let value = self.value.borrow().clone();
        let value_len = value.chars().count();
        let cursor = (*self.cursor.borrow()).min(value_len);
        let selection = self.selection_bounds();
        let composing = match term_window.composition_status() {
            DeadKeyStatus::Composing(text) if !text.is_empty() => Some(text.clone()),
            DeadKeyStatus::None | DeadKeyStatus::Composing(_) => None,
        };

        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0));
        let blink_period_ms = 1000u128;
        let on_phase_ms = 550u128;
        let phase = epoch.as_millis() % blink_period_ms;
        let cursor_visible = phase < on_phase_ms;
        let ms_to_next_toggle = if cursor_visible {
            on_phase_ms.saturating_sub(phase)
        } else {
            blink_period_ms.saturating_sub(phase)
        };
        term_window.update_next_frame_time(Some(
            std::time::Instant::now()
                + Duration::from_millis(ms_to_next_toggle.max(1).min(u128::from(u64::MAX)) as u64),
        ));

        let width = self.anchor.width.max(1) as f32;
        let height = self.anchor.height.max(1) as f32;
        let x = self.anchor.x as f32;
        let y = self.anchor.y as f32;
        let padding = BoxDimension {
            left: Dimension::Pixels((0.5 * metrics.cell_size.width as f32) + 4.0),
            right: Dimension::Pixels((0.5 * metrics.cell_size.width as f32) + 4.0),
            top: Dimension::Cells(0.2),
            bottom: Dimension::Cells(0.25),
        };

        let mut row = vec![];
        let cursor_element_idx;

        if let Some(composing) = composing {
            let (replace_start, replace_end) = selection.unwrap_or((cursor, cursor));
            let left = value.chars().take(replace_start).collect::<String>();
            let right = value.chars().skip(replace_end).collect::<String>();

            if !left.is_empty() {
                row.push(Self::text_segment(&font, &palette, left, &text_attrs));
            }

            row.push(Self::text_segment(&font, &palette, composing, &text_attrs));
            cursor_element_idx = row.len();
            row.push(Self::cursor_segment(
                &font,
                &metrics,
                cursor_color,
                cursor_visible,
            ));

            if !right.is_empty() {
                row.push(Self::text_segment(&font, &palette, right, &text_attrs));
            }
        } else if let Some((start, end)) = selection {
            let left = value.chars().take(start).collect::<String>();
            let selected = value
                .chars()
                .skip(start)
                .take(end.saturating_sub(start))
                .collect::<String>();
            let right = value.chars().skip(end).collect::<String>();

            if !left.is_empty() {
                row.push(Self::text_segment(&font, &palette, left, &text_attrs));
            }

            row.push(Self::text_segment(&font, &palette, selected, &text_attrs));
            cursor_element_idx = row.len();
            row.push(Self::cursor_segment(
                &font,
                &metrics,
                cursor_color,
                cursor_visible,
            ));

            if !right.is_empty() {
                row.push(Self::text_segment(&font, &palette, right, &text_attrs));
            }
        } else if value.is_empty() {
            cursor_element_idx = row.len();
            row.push(Self::cursor_segment(
                &font,
                &metrics,
                cursor_color,
                cursor_visible,
            ));
        } else {
            let left = value.chars().take(cursor).collect::<String>();
            let right = value.chars().skip(cursor).collect::<String>();

            if !left.is_empty() {
                row.push(Self::text_segment(&font, &palette, left, &text_attrs));
            }

            cursor_element_idx = row.len();
            row.push(Self::cursor_segment(
                &font,
                &metrics,
                cursor_color,
                cursor_visible,
            ));
            if !right.is_empty() {
                row.push(Self::text_segment(&font, &palette, right, &text_attrs));
            }
        }

        let element = Element::new(&font, ElementContent::Children(row))
            .colors(element_colors)
            .padding(padding)
            .border(BoxDimension::new(Dimension::Pixels(1.0)))
            .border_corners(border_corners)
            .min_width(Some(Dimension::Pixels(width)))
            .min_height(Some(Dimension::Pixels(height)))
            .display(DisplayType::Block);

        let dimensions = term_window.dimensions;
        let computed = term_window.compute_element(
            &LayoutContext {
                height: DimensionContext {
                    dpi: dimensions.dpi as f32,
                    pixel_max: dimensions.pixel_height as f32,
                    pixel_cell: metrics.cell_size.height as f32,
                },
                width: DimensionContext {
                    dpi: dimensions.dpi as f32,
                    pixel_max: dimensions.pixel_width as f32,
                    pixel_cell: metrics.cell_size.width as f32,
                },
                bounds: euclid::rect(x, y, width, height),
                metrics: &metrics,
                gl_state: term_window.render_state.as_ref().unwrap(),
                zindex: 120,
            },
            &element,
        )?;

        Self::update_text_cursor_position(term_window, &computed, cursor_element_idx);

        Ok(vec![computed])
    }
}

impl Modal for TabRenameModal {
    fn mouse_event(&self, event: MouseEvent, term_window: &mut TermWindow) -> anyhow::Result<()> {
        if event.kind == wezterm_term::MouseEventKind::Press
            && !self.point_in_bounds(event.x as i64, event.y)
        {
            self.commit(term_window);
        }
        Ok(())
    }

    fn key_down(
        &self,
        key: KeyCode,
        mods: KeyModifiers,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<bool> {
        let handled = match (key, mods) {
            (KeyCode::Escape, KeyModifiers::NONE) | (KeyCode::Char('g'), KeyModifiers::CTRL) => {
                term_window.cancel_modal();
                true
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                self.commit(term_window);
                true
            }
            (KeyCode::Backspace, KeyModifiers::NONE) => self.backspace(),
            (KeyCode::Delete, KeyModifiers::NONE) => self.delete(),
            (KeyCode::Backspace, KeyModifiers::SUPER) | (KeyCode::Delete, KeyModifiers::SUPER) => {
                self.clear()
            }
            (KeyCode::LeftArrow, KeyModifiers::NONE)
            | (KeyCode::ApplicationLeftArrow, KeyModifiers::NONE) => self.move_left(),
            (KeyCode::RightArrow, KeyModifiers::NONE)
            | (KeyCode::ApplicationRightArrow, KeyModifiers::NONE) => self.move_right(),
            (KeyCode::Home, KeyModifiers::NONE) | (KeyCode::Char('a'), KeyModifiers::CTRL) => {
                self.move_to_start()
            }
            (KeyCode::End, KeyModifiers::NONE) | (KeyCode::Char('e'), KeyModifiers::CTRL) => {
                self.move_to_end()
            }
            (KeyCode::Char('a'), KeyModifiers::SUPER) => self.select_all(),
            (KeyCode::Char('u'), KeyModifiers::CTRL) => self.clear(),
            (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                self.insert_char(c);
                true
            }
            _ => false,
        };

        if handled {
            term_window.invalidate_modal();
        } else if matches!(
            term_window.composition_status(),
            DeadKeyStatus::Composing(_)
        ) {
            term_window.invalidate_modal();
        }

        Ok(handled)
    }

    fn focus_changed(&self, focused: bool, term_window: &mut TermWindow) {
        if !focused {
            self.commit(term_window);
        }
    }

    fn computed_element(
        &self,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<Ref<'_, [ComputedElement]>> {
        if self.element.borrow().is_none() {
            let element = self.compute(term_window)?;
            self.element.borrow_mut().replace(element);
        }

        Ok(Ref::map(self.element.borrow(), |value| {
            value.as_ref().unwrap().as_slice()
        }))
    }

    fn reconfigure(&self, _term_window: &mut TermWindow) {
        self.element.borrow_mut().take();
    }
}
