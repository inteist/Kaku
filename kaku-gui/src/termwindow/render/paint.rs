use crate::customglyph::{BlockAlpha, BlockCoord, Poly, PolyCommand, PolyStyle};
use crate::termwindow::box_model::*;
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER,
    TOP_RIGHT_ROUNDED_CORNER,
};
use crate::termwindow::{DimensionContext, RenderFrame, TermWindowNotif};
use crate::utilsprites::RenderMetrics;
use ::window::bitmaps::atlas::OutOfTextureSpace;
use ::window::WindowOps;
use anyhow::Context;
use config::{ActivePaneIndicator, Dimension};
use smol::Timer;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use wezterm_font::ClearShapeCache;
use window::color::LinearRgba;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowImage {
    Yes,
    Scale(usize),
    No,
}

const ACTIVE_GUTTER_LEFT_PADDING: f32 = 8.0;

fn active_pane_indicator_size_px(size: usize) -> f32 {
    size.max(1) as f32
}

fn active_pane_indicator_bounds(
    content_left: f32,
    top_pixel_y: f32,
    cell_width: f32,
    cell_height: f32,
    split_pane_gap: usize,
    pos_left: usize,
    pos_top: usize,
    pos_width: usize,
    pos_height: usize,
) -> ::window::RectF {
    let split_col_gutter = (1 + 2 * split_pane_gap).max(1) as f32;
    let left_split_inset = cell_width * split_col_gutter / 2.0;

    euclid::rect(
        content_left + (pos_left as f32 * cell_width) - left_split_inset,
        top_pixel_y + (pos_top as f32 * cell_height),
        (pos_width as f32 * cell_width) + (cell_width * split_col_gutter),
        pos_height as f32 * cell_height,
    )
}

fn active_pane_gutter_radius(width: f32) -> f32 {
    (width / 2.0).max(0.0)
}

fn active_pane_left_indicator_segment(
    bounds: ::window::RectF,
    width: f32,
    left_padding: f32,
    used_rows: usize,
    total_rows: usize,
) -> Option<::window::RectF> {
    if bounds.width() <= 0.0 || bounds.height() <= 0.0 || total_rows == 0 {
        return None;
    }

    let used_rows = used_rows.clamp(1, total_rows);
    let padding = left_padding.max(0.0);
    let available_width = (bounds.width() - padding).max(0.0);
    if available_width <= 0.0 {
        return None;
    }

    let w = width.max(1.0).min(available_width);
    let x = bounds.min_x() + padding.min((bounds.width() - w).max(0.0));
    let h = (bounds.height() * used_rows as f32 / total_rows as f32).clamp(1.0, bounds.height());

    Some(euclid::rect(x, bounds.min_y(), w, h))
}

fn active_pane_left_pill_segment(
    bounds: ::window::RectF,
    width: f32,
    left_padding: f32,
) -> Option<::window::RectF> {
    if bounds.width() <= 0.0 || bounds.height() <= 0.0 {
        return None;
    }

    let padding = left_padding.max(0.0);
    let available_width = (bounds.width() - padding).max(0.0);
    if available_width <= 0.0 {
        return None;
    }

    let w = width.max(1.0).min(available_width);
    let x = bounds.min_x() + padding.min((bounds.width() - w).max(0.0));
    let h = (bounds.height() * 0.15).clamp(1.0, bounds.height());
    let y = bounds.min_y() + ((bounds.height() - h) / 2.0);

    Some(euclid::rect(x, y, w, h))
}

fn toast_colors_for_palette(
    palette: &wezterm_term::color::ColorPalette,
    alpha: f32,
) -> (LinearRgba, LinearRgba) {
    if crate::termwindow::is_light_color(&palette.background) {
        let bg_linear = palette.colors.0[3].to_linear();
        (
            LinearRgba(bg_linear.0, bg_linear.1, bg_linear.2, 0.9 * alpha),
            LinearRgba(0.1, 0.1, 0.1, alpha),
        )
    } else {
        let bg_linear = palette.colors.0[13].to_linear();
        (
            LinearRgba(bg_linear.0, bg_linear.1, bg_linear.2, 0.9 * alpha),
            LinearRgba(1.0, 1.0, 1.0, alpha),
        )
    }
}

impl crate::TermWindow {
    pub fn paint_impl(&mut self, frame: &mut RenderFrame) -> anyhow::Result<()> {
        self.num_frames += 1;
        // If nothing on screen needs animating, then we can avoid
        // invalidating as frequently
        *self.has_animation.borrow_mut() = None;
        // Start with the assumption that we should allow images to render
        self.allow_images = AllowImage::Yes;

        let start = Instant::now();

        {
            let diff = start.duration_since(self.last_fps_check_time);
            if diff > Duration::from_secs(1) {
                let seconds = diff.as_secs_f32();
                self.fps = self.num_frames as f32 / seconds;
                self.num_frames = 0;
                self.last_fps_check_time = start;
            }
        }

        'pass: for pass in 0.. {
            match self.paint_pass() {
                Ok(_) => match self.render_state.as_mut().unwrap().allocated_more_quads() {
                    Ok(allocated) => {
                        if !allocated {
                            break 'pass;
                        }
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();
                    }
                    Err(err) => {
                        log::error!("{:#}", err);
                        break 'pass;
                    }
                },
                Err(err) => {
                    if let Some(&OutOfTextureSpace {
                        size: Some(size),
                        current_size,
                    }) = err.root_cause().downcast_ref::<OutOfTextureSpace>()
                    {
                        let result = if pass == 0 {
                            // Let's try clearing out the atlas and trying again
                            // self.clear_texture_atlas()
                            log::trace!("recreate_texture_atlas");
                            self.recreate_texture_atlas(Some(current_size))
                        } else {
                            log::trace!("grow texture atlas to {}", size);
                            self.recreate_texture_atlas(Some(size))
                        };
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();

                        if let Err(err) = result {
                            self.allow_images = match self.allow_images {
                                AllowImage::Yes => AllowImage::Scale(2),
                                AllowImage::Scale(2) => AllowImage::Scale(4),
                                AllowImage::Scale(4) => AllowImage::Scale(8),
                                AllowImage::Scale(8) => AllowImage::No,
                                AllowImage::No | _ => {
                                    log::error!(
                                        "Failed to {} texture: {}",
                                        if pass == 0 { "clear" } else { "resize" },
                                        err
                                    );
                                    break 'pass;
                                }
                            };

                            log::info!(
                                "Not enough texture space ({:#}); \
                                     will retry render with {:?}",
                                err,
                                self.allow_images,
                            );
                        }
                    } else if err.root_cause().downcast_ref::<ClearShapeCache>().is_some() {
                        self.invalidate_fancy_tab_bar();
                        self.invalidate_modal();
                        self.shape_generation += 1;
                        self.shape_cache.borrow_mut().clear();
                        self.line_to_ele_shape_cache.borrow_mut().clear();
                    } else {
                        log::error!("paint_pass failed: {:#}", err);
                        break 'pass;
                    }
                }
            }
        }

        log::debug!("paint_impl before call_draw elapsed={:?}", start.elapsed());

        self.call_draw(frame)?;
        self.last_frame_duration = start.elapsed();
        log::debug!(
            "paint_impl elapsed={:?}, fps={}",
            self.last_frame_duration,
            self.fps
        );
        metrics::histogram!("gui.paint.impl").record(self.last_frame_duration);
        metrics::histogram!("gui.paint.impl.rate").record(1.);

        // If self.has_animation is some, then the last render detected
        // image attachments with multiple frames, so we also need to
        // invalidate the viewport when the next frame is due
        if self.focused.is_some() {
            if let Some(next_due) = *self.has_animation.borrow() {
                let prior = self.scheduled_animation.borrow_mut().take();
                match prior {
                    Some(prior) if prior <= next_due => {
                        // Already due before that time
                    }
                    _ => {
                        self.scheduled_animation.borrow_mut().replace(next_due);
                        let window = self.window.clone().take().unwrap();
                        promise::spawn::spawn(async move {
                            Timer::at(next_due).await;
                            let win = window.clone();
                            window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                                tw.scheduled_animation.borrow_mut().take();
                                // Modal content is cached, so blinking carets and other
                                // time-based modal elements must be explicitly reconfigured.
                                tw.invalidate_modal();
                                win.invalidate();
                            })));
                        })
                        .detach();
                    }
                }
            }
        }

        Ok(())
    }

    pub fn paint_modal(&mut self) -> anyhow::Result<()> {
        if let Some(modal) = self.get_modal() {
            for computed in modal.computed_element(self)?.iter() {
                let mut ui_items = computed.ui_items();

                let gl_state = self.render_state.as_ref().unwrap();
                self.render_element(&computed, gl_state, None)?;

                self.ui_items.append(&mut ui_items);
            }
        }

        Ok(())
    }

    pub fn paint_pass(&mut self) -> anyhow::Result<()> {
        {
            let gl_state = self.render_state.as_ref().unwrap();
            for layer in gl_state.layers.borrow().iter() {
                layer.clear_quad_allocation();
            }
        }

        // Clear out UI item positions; we'll rebuild these as we render
        self.ui_items.clear();

        let panes = self.get_panes_to_render();
        let focused = self.focused.is_some();
        let window_is_transparent =
            !self.window_background.is_empty() || self.config.window_background_opacity != 1.0;

        let start = Instant::now();
        let gl_state = self.render_state.as_ref().unwrap();
        let layer = gl_state
            .layer_for_zindex(0)
            .context("layer_for_zindex(0)")?;
        let mut layers = layer.quad_allocator();
        log::trace!("quad map elapsed {:?}", start.elapsed());
        metrics::histogram!("quad.map").record(start.elapsed());

        let mut paint_terminal_background = false;

        // Render the full window background
        match (self.window_background.is_empty(), self.allow_images) {
            (false, AllowImage::Yes | AllowImage::Scale(_)) => {
                let bg_color = self.palette().background.to_linear();

                let top = panes
                    .iter()
                    .find(|p| p.is_active)
                    .map(|p| match self.get_viewport(p.pane.pane_id()) {
                        Some(top) => top,
                        None => p.pane.get_dimensions().physical_top,
                    })
                    .unwrap_or(0);

                let loaded_any = self
                    .render_backgrounds(bg_color, top)
                    .context("render_backgrounds")?;

                if !loaded_any {
                    // Either there was a problem loading the background(s)
                    // or they haven't finished loading yet.
                    // Use the regular terminal background until that changes.
                    paint_terminal_background = true;
                }
            }
            _ if window_is_transparent => {
                // Avoid doubling up the background color for the main pane area.
                // We still need to paint strips that are intentionally excluded
                // from pane background quads.
                let strip_background = if panes.len() == 1 {
                    panes[0].pane.palette().background
                } else {
                    self.palette().background
                }
                .to_linear()
                .mul_alpha(self.config.window_background_opacity);
                let border = self.get_os_border();
                let tab_bar_height = if self.show_tab_bar {
                    self.tab_bar_pixel_height()
                        .context("tab_bar_pixel_height")?
                } else {
                    0.0
                };
                let (_, padding_bottom) = self.effective_vertical_padding();
                let padding_bottom = padding_bottom as f32;
                let top_fill_height = border.top.get() as f32
                    + if self.config.tab_bar_at_bottom {
                        0.0
                    } else {
                        tab_bar_height
                    };
                let bottom_fill_height = padding_bottom
                    + border.bottom.get() as f32
                    + if self.config.tab_bar_at_bottom {
                        tab_bar_height
                    } else {
                        0.0
                    };
                let right_fill_width =
                    self.effective_right_padding(&self.config) as f32 + border.right.get() as f32;
                let left_fill_width =
                    self.config
                        .window_padding
                        .left
                        .evaluate_as_pixels(DimensionContext {
                            dpi: self.dimensions.dpi as f32,
                            pixel_max: self.terminal_size.pixel_width as f32,
                            pixel_cell: self.render_metrics.cell_size.width as f32,
                        })
                        + border.left.get() as f32;
                let window_width = self.dimensions.pixel_width as f32;
                let window_height = self.dimensions.pixel_height as f32;

                if top_fill_height > 0.0 {
                    self.filled_rectangle(
                        &mut layers,
                        0,
                        euclid::rect(0.0, 0.0, window_width, top_fill_height.min(window_height)),
                        strip_background,
                    )
                    .context("filled_rectangle for transparent top strip")?;
                }

                if bottom_fill_height > 0.0 {
                    let clamped_height = bottom_fill_height.min(window_height);
                    self.filled_rectangle(
                        &mut layers,
                        0,
                        euclid::rect(
                            0.0,
                            (window_height - clamped_height).max(0.0),
                            window_width,
                            clamped_height,
                        ),
                        strip_background,
                    )
                    .context("filled_rectangle for transparent bottom strip")?;
                }

                if right_fill_width > 0.0 {
                    let clamped_width = right_fill_width.min(window_width);
                    let right_fill_y = top_fill_height.min(window_height);
                    let right_fill_height =
                        (window_height - right_fill_y - bottom_fill_height.min(window_height))
                            .max(0.0);
                    self.filled_rectangle(
                        &mut layers,
                        0,
                        euclid::rect(
                            window_width - clamped_width,
                            right_fill_y,
                            clamped_width,
                            right_fill_height,
                        ),
                        strip_background,
                    )
                    .context("filled_rectangle for transparent right strip")?;
                }

                if left_fill_width > 0.0 {
                    let clamped_width = left_fill_width.min(window_width);
                    let left_fill_y = top_fill_height.min(window_height);
                    let left_fill_height =
                        (window_height - left_fill_y - bottom_fill_height.min(window_height))
                            .max(0.0);
                    self.filled_rectangle(
                        &mut layers,
                        0,
                        euclid::rect(0.0, left_fill_y, clamped_width, left_fill_height),
                        strip_background,
                    )
                    .context("filled_rectangle for transparent left strip")?;
                }
            }
            _ => {
                paint_terminal_background = true;
            }
        }

        if paint_terminal_background {
            // Regular window background color
            let background = if panes.len() == 1 {
                // If we're the only pane, use the pane's palette
                // to draw the padding background
                panes[0].pane.palette().background
            } else {
                self.palette().background
            }
            .to_linear()
            .mul_alpha(self.config.window_background_opacity);

            self.filled_rectangle(
                &mut layers,
                0,
                euclid::rect(
                    0.,
                    0.,
                    self.dimensions.pixel_width as f32,
                    self.dimensions.pixel_height as f32,
                ),
                background,
            )
            .context("filled_rectangle for window background")?;
        }

        let hide_transition_content = self
            .window
            .as_ref()
            .map(|window| window.is_zoom_animation_active())
            .unwrap_or(false);
        if hide_transition_content {
            // During fullscreen transition, keep only a stable background to avoid
            // one-frame text scale pops.
            let hide_background = self.palette().background.to_linear();
            self.filled_rectangle(
                &mut layers,
                0,
                euclid::rect(
                    0.,
                    0.,
                    self.dimensions.pixel_width as f32,
                    self.dimensions.pixel_height as f32,
                ),
                hide_background,
            )
            .context("filled_rectangle for fullscreen transition hide")?;
            drop(layers);
            return Ok(());
        }

        let num_panes = panes.len();
        let mut active_pane_top_right: Option<(f32, f32, bool)> = None;
        let mut active_pane_indicator: Option<(::window::RectF, usize, usize)> = None;

        for pos in panes {
            if pos.is_active && num_panes > 1 {
                let cell_width = self.render_metrics.cell_size.width as f32;
                let cell_height = self.render_metrics.cell_size.height as f32;
                let (_, padding_top) = self.padding_left_top();
                let border = self.get_os_border();
                let tab_bar_height = if self.show_tab_bar {
                    self.tab_bar_pixel_height().unwrap_or(0.)
                } else {
                    0.
                };
                let top_bar_height = if self.config.tab_bar_at_bottom {
                    0.0
                } else {
                    tab_bar_height
                };
                let top_pixel_y = top_bar_height + padding_top + border.top.get() as f32;

                let x = self.content_left_inset() + ((pos.left + pos.width) as f32 * cell_width);
                let y = top_pixel_y + (pos.top as f32 * cell_height);
                let is_top_pane = pos.top == 0;
                active_pane_top_right = Some((x, y, is_top_pane));
                let bounds = active_pane_indicator_bounds(
                    self.content_left_inset(),
                    top_pixel_y,
                    cell_width,
                    cell_height,
                    self.config.split_pane_gap as usize,
                    pos.left,
                    pos.top,
                    pos.width,
                    pos.height,
                );

                let dims = pos.pane.get_dimensions();
                let cursor = pos.pane.get_cursor_position();
                let used_rows = (cursor.y - dims.physical_top + 1).max(1) as usize;
                let total_rows = pos.height.max(1);

                active_pane_indicator = Some((bounds, used_rows.min(total_rows), total_rows));
            }
            if pos.is_active {
                if self.get_modal().is_none() {
                    self.update_text_cursor(&pos);
                }
                if focused {
                    pos.pane.advise_focus();
                    mux::Mux::get().record_focus_for_current_identity(pos.pane.pane_id());
                }
                // Bell state cleared in clear_active_tab_bell_state() to ensure badge sync
            }
            self.paint_pane(&pos, &mut layers).context("paint_pane")?;
        }

        static CIRCLE_POLY: &[Poly] = &[Poly {
            path: &[PolyCommand::Circle {
                center: (BlockCoord::Frac(1, 2), BlockCoord::Frac(1, 2)),
                radius: BlockCoord::Frac(1, 2),
            }],
            intensity: BlockAlpha::Full,
            style: PolyStyle::Fill,
        }];

        const RIGHT_INSET: f32 = 3.0;
        const TOP_PANE_MARGIN_WITH_TAB_BAR: f32 = 24.0;
        const TOP_PANE_MARGIN_NO_TAB_BAR: f32 = 14.0;
        const LOWER_PANE_MARGIN: f32 = 20.0;
        let indicator_size = active_pane_indicator_size_px(self.config.active_pane_indicator_size);

        // Draw dot indicator for the active pane when split
        if self.config.active_pane_indicator == ActivePaneIndicator::Bell {
            if let Some((dot_x, dot_y, is_top_pane)) = active_pane_top_right {
                let top_pane_margin = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
                    TOP_PANE_MARGIN_WITH_TAB_BAR
                } else {
                    TOP_PANE_MARGIN_NO_TAB_BAR
                };
                let margin_top = if is_top_pane {
                    top_pane_margin
                } else {
                    LOWER_PANE_MARGIN
                };

                const DOT_ALPHA: f32 = 0.5;
                let dot_color = self.palette().cursor_bg.to_linear().mul_alpha(DOT_ALPHA);

                self.poly_quad(
                    &mut layers,
                    2,
                    euclid::point2(dot_x - indicator_size - RIGHT_INSET, dot_y + margin_top),
                    CIRCLE_POLY,
                    1,
                    euclid::size2(indicator_size, indicator_size),
                    dot_color,
                )
                .context("active pane indicator")?;
            }
        }

        if let Some(pane) = self.get_active_pane_or_overlay() {
            let splits = self.get_splits();
            for split in &splits {
                self.paint_split(&mut layers, split, &splits, &pane)
                    .context("paint_split")?;
            }

            if self.config.active_pane_indicator == ActivePaneIndicator::Gutter
                || self.config.active_pane_indicator == ActivePaneIndicator::Pill
            {
                if let Some((bounds, used_rows, total_rows)) = active_pane_indicator {
                    const ACTIVE_PANE_INDICATOR_ALPHA: f32 = 0.9;
                    let indicator_color = pane
                        .palette()
                        .cursor_bg
                        .to_linear()
                        .mul_alpha(ACTIVE_PANE_INDICATOR_ALPHA);
                    let indicator_left_padding = ACTIVE_GUTTER_LEFT_PADDING;

                    let segment = if self.config.active_pane_indicator == ActivePaneIndicator::Pill
                    {
                        active_pane_left_pill_segment(
                            bounds,
                            indicator_size,
                            indicator_left_padding,
                        )
                    } else {
                        active_pane_left_indicator_segment(
                            bounds,
                            indicator_size,
                            indicator_left_padding,
                            used_rows,
                            total_rows,
                        )
                    };

                    if let Some(indicator) = segment {
                        let cap_diameter = indicator.width().min(indicator.height());
                        let cap_radius = active_pane_gutter_radius(cap_diameter);

                        if indicator.height() <= cap_diameter {
                            self.poly_quad(
                                &mut layers,
                                2,
                                euclid::point2(
                                    indicator.min_x(),
                                    indicator.min_y() + (indicator.height() - cap_diameter) / 2.0,
                                ),
                                CIRCLE_POLY,
                                1,
                                euclid::size2(cap_diameter, cap_diameter),
                                indicator_color,
                            )
                            .context("active pane left indicator cap")?;
                        } else {
                            self.poly_quad(
                                &mut layers,
                                2,
                                euclid::point2(indicator.min_x(), indicator.min_y()),
                                CIRCLE_POLY,
                                1,
                                euclid::size2(cap_diameter, cap_diameter),
                                indicator_color,
                            )
                            .context("active pane left indicator top cap")?;

                            self.filled_rectangle(
                                &mut layers,
                                2,
                                euclid::rect(
                                    indicator.min_x(),
                                    indicator.min_y() + cap_radius,
                                    indicator.width(),
                                    (indicator.height() - 2.0 * cap_radius).max(0.0),
                                ),
                                indicator_color,
                            )
                            .context("active pane left indicator body")?;

                            self.poly_quad(
                                &mut layers,
                                2,
                                euclid::point2(indicator.min_x(), indicator.max_y() - cap_diameter),
                                CIRCLE_POLY,
                                1,
                                euclid::size2(cap_diameter, cap_diameter),
                                indicator_color,
                            )
                            .context("active pane left indicator bottom cap")?;
                        }
                    }
                }
            }
        }

        // Clear bell state for active tab and update Dock badge (always, regardless of tab bar visibility)
        self.clear_active_tab_bell_state();

        // Draw visual notification dot on inactive tabs with unread bell
        if self.show_tab_bar {
            self.paint_tab_bar(&mut layers).context("paint_tab_bar")?;
            self.paint_tab_bell_indicators(&mut layers)?;
        }

        self.paint_window_borders(&mut layers)
            .context("paint_window_borders")?;
        drop(layers);
        self.paint_modal().context("paint_modal")?;
        self.paint_toast().context("paint_toast")?;

        Ok(())
    }

    /// Clear bell state for all panes in the active tab and update global Dock badge.
    /// Called every paint cycle to ensure badge stays in sync regardless of tab bar visibility.
    /// Only clears when window has focus, so Dock badge persists while window is unfocused.
    fn clear_active_tab_bell_state(&mut self) {
        if self.focused.is_none() {
            return;
        }
        // Fast path: skip mux lock if no panes have unread bells
        if !self.pane_state.borrow().values().any(|s| s.has_unread_bell) {
            return;
        }
        let mux = mux::Mux::get();
        let mux_window = match mux.get_window(self.mux_window_id) {
            Some(w) => w,
            None => return,
        };
        let active_tab_idx = mux_window.get_active_idx();

        if let Some(active_tab) = mux_window.get_by_idx(active_tab_idx) {
            let active_tab_panes = active_tab.iter_panes_ignoring_zoom();
            let mut cleared_count: isize = 0;
            for pos in &active_tab_panes {
                let mut state = self.pane_state(pos.pane.pane_id());
                if state.has_unread_bell {
                    state.has_unread_bell = false;
                    cleared_count += 1;
                }
            }
            if cleared_count > 0 {
                crate::frontend::front_end().adjust_unread_bell_count(-cleared_count);
            }
        }
    }

    /// Draw a dot on inactive tabs that have panes with unread bell events.
    fn paint_tab_bell_indicators(
        &mut self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        use crate::tabbar::TabBarItem;

        // Fast path: skip mux lock entirely if no panes have unread bells
        if self.config.active_pane_indicator != ActivePaneIndicator::Bell
            || !self.pane_state.borrow().values().any(|s| s.has_unread_bell)
        {
            return Ok(());
        }

        // Figure out which tab indices have unread bell panes
        let mux = mux::Mux::get();
        let mux_window = match mux.get_window(self.mux_window_id) {
            Some(w) => w,
            None => return Ok(()),
        };
        let active_tab_idx = mux_window.get_active_idx();

        let mut tabs_with_bell: HashSet<usize> = HashSet::new();
        for (idx, tab) in mux_window.iter().enumerate() {
            if idx == active_tab_idx {
                continue; // skip active tab
            }
            let panes = tab.iter_panes_ignoring_zoom();
            for pos in &panes {
                if self.pane_state(pos.pane.pane_id()).has_unread_bell {
                    tabs_with_bell.insert(idx);
                    break;
                }
            }
        }

        if tabs_with_bell.is_empty() {
            return Ok(());
        }

        let notif_color = self.palette().cursor_bg.to_linear().mul_alpha(0.9);

        static CIRCLE_POLY: &[Poly] = &[Poly {
            path: &[PolyCommand::Circle {
                center: (BlockCoord::Frac(1, 2), BlockCoord::Frac(1, 2)),
                radius: BlockCoord::Frac(1, 2),
            }],
            intensity: BlockAlpha::Full,
            style: PolyStyle::Fill,
        }];

        const DOT_RIGHT_MARGIN: f32 = 3.0;
        let dot_size = active_pane_indicator_size_px(self.config.active_pane_indicator_size);

        for ui_item in &self.ui_items {
            if let crate::termwindow::UIItemType::TabBar(TabBarItem::Tab {
                tab_idx,
                active: false,
            }) = &ui_item.item_type
            {
                if tabs_with_bell.contains(tab_idx) {
                    // Draw dot at the right side of the tab, vertically centered
                    let dot_x = (ui_item.x + ui_item.width) as f32 - dot_size - DOT_RIGHT_MARGIN;
                    let dot_y =
                        ui_item.y as f32 + ((ui_item.height as f32 - dot_size) / 2.0).round();

                    self.poly_quad(
                        layers,
                        2,
                        euclid::point2(dot_x, dot_y),
                        CIRCLE_POLY,
                        1,
                        euclid::size2(dot_size, dot_size),
                        notif_color,
                    )
                    .context("tab bell indicator")?;
                }
            }
        }

        Ok(())
    }

    /// Render the toast notification
    pub fn paint_toast(&mut self) -> anyhow::Result<()> {
        let (toast_at, message, lifetime) = match &self.toast {
            Some((t, msg, lifetime)) if t.elapsed() < *lifetime => (*t, msg.clone(), *lifetime),
            _ => return Ok(()),
        };

        let font = self.fonts.title_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());

        // Fade out during the last 500ms of the configured lifetime.
        let elapsed_ms = toast_at.elapsed().as_millis() as f32;
        let lifetime_ms = lifetime.as_millis() as f32;
        let fade_start_ms = (lifetime_ms - 500.0).max(0.0);
        let alpha = if elapsed_ms > fade_start_ms {
            (1.0 - (elapsed_ms - fade_start_ms) / 500.0).max(0.0)
        } else {
            1.0
        };

        // Match the toast to the currently visible terminal palette so it
        // stays in sync with theme changes and client palette overrides.
        let palette = if let Some(pane) = self.get_active_pane_or_overlay() {
            pane.palette()
        } else {
            self.palette().clone()
        };
        let (bg_color, text_color) = toast_colors_for_palette(&palette, alpha);
        let toast_radius = Dimension::Pixels(8.0);

        let text = Element::new(&font, ElementContent::Text(message.clone()))
            .colors(ElementColors {
                border: BorderColor::default(),
                bg: LinearRgba::TRANSPARENT.into(),
                text: text_color.into(),
            })
            .display(DisplayType::Block);

        let element = Element::new(&font, ElementContent::Children(vec![text]))
            .colors(ElementColors {
                // Rounded corner polys use border colors even if the border
                // width is zero; match bg to avoid corner gaps.
                border: BorderColor::new(bg_color.into()),
                bg: bg_color.into(),
                text: text_color.into(),
            })
            .padding(BoxDimension {
                left: Dimension::Cells(0.75),
                right: Dimension::Cells(0.75),
                top: Dimension::Cells(0.25),
                bottom: Dimension::Cells(0.25),
            })
            .border(BoxDimension::new(Dimension::Pixels(0.0)))
            .border_corners(Some(Corners {
                top_left: SizedPoly {
                    width: toast_radius,
                    height: toast_radius,
                    poly: TOP_LEFT_ROUNDED_CORNER,
                },
                top_right: SizedPoly {
                    width: toast_radius,
                    height: toast_radius,
                    poly: TOP_RIGHT_ROUNDED_CORNER,
                },
                bottom_left: SizedPoly {
                    width: toast_radius,
                    height: toast_radius,
                    poly: BOTTOM_LEFT_ROUNDED_CORNER,
                },
                bottom_right: SizedPoly {
                    width: toast_radius,
                    height: toast_radius,
                    poly: BOTTOM_RIGHT_ROUNDED_CORNER,
                },
            }));

        let dimensions = self.dimensions;
        let border = self.get_os_border();
        // Calculate width based on message length (each char ~cell_width + padding)
        let approx_width = (message.len() as f32 + 1.5) * metrics.cell_size.width as f32;
        let toast_height = metrics.cell_size.height as f32 * 1.5;
        // Use consistent margin based on cell size
        let h_margin = metrics.cell_size.width as f32 * 2.0;
        let v_margin = metrics.cell_size.height as f32 * 2.0;

        // Position at bottom-right with fixed margin from window edge
        let right_x =
            dimensions.pixel_width as f32 - approx_width - h_margin - border.right.get() as f32;
        let bottom_y =
            dimensions.pixel_height as f32 - toast_height - v_margin - border.bottom.get() as f32;

        let computed = self.compute_element(
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
                bounds: euclid::rect(right_x, bottom_y, approx_width, toast_height),
                metrics: &metrics,
                gl_state: self.render_state.as_ref().unwrap(),
                zindex: 120,
            },
            &element,
        )?;

        let gl_state = self.render_state.as_ref().unwrap();
        self.render_element(&computed, gl_state, None)?;

        // Keep redrawing during fade-out
        if elapsed_ms > fade_start_ms {
            let next = Instant::now() + Duration::from_millis(16);
            let mut anim = self.has_animation.borrow_mut();
            match *anim {
                Some(existing) if existing <= next => {}
                _ => {
                    *anim = Some(next);
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        active_pane_gutter_radius, active_pane_indicator_bounds, active_pane_indicator_size_px,
        active_pane_left_indicator_segment, active_pane_left_pill_segment,
        toast_colors_for_palette,
    };
    use wezterm_term::color::{ColorPalette, SrgbaTuple};
    use window::color::LinearRgba;

    #[test]
    fn light_palette_uses_gold_background_and_dark_text() {
        let mut palette = ColorPalette::default();
        palette.background = SrgbaTuple(0.95, 0.95, 0.95, 1.0);
        palette.colors.0[3] = SrgbaTuple(0.8, 0.7, 0.2, 1.0);

        let (bg, text) = toast_colors_for_palette(&palette, 1.0);
        let expected_bg = palette.colors.0[3].to_linear();

        assert_eq!(
            bg,
            LinearRgba(expected_bg.0, expected_bg.1, expected_bg.2, 0.9)
        );
        assert_eq!(text, LinearRgba(0.1, 0.1, 0.1, 1.0));
    }

    #[test]
    fn dark_palette_uses_accent_background_and_light_text() {
        let mut palette = ColorPalette::default();
        palette.background = SrgbaTuple(0.08, 0.08, 0.08, 1.0);
        palette.colors.0[13] = SrgbaTuple(0.5, 0.3, 0.8, 1.0);

        let (bg, text) = toast_colors_for_palette(&palette, 1.0);
        let expected_bg = palette.colors.0[13].to_linear();

        assert_eq!(
            bg,
            LinearRgba(expected_bg.0, expected_bg.1, expected_bg.2, 0.9)
        );
        assert_eq!(text, LinearRgba(1.0, 1.0, 1.0, 1.0));
    }

    #[test]
    fn left_indicator_tracks_used_rows_height() {
        let bounds = euclid::rect(10.0, 20.0, 100.0, 50.0);
        let indicator =
            active_pane_left_indicator_segment(bounds, 8.0, 4.0, 10, 20).expect("indicator");

        assert_eq!(indicator, euclid::rect(14.0, 20.0, 8.0, 25.0));
    }

    #[test]
    fn left_indicator_extends_full_height_when_all_rows_used() {
        let bounds = euclid::rect(10.0, 20.0, 100.0, 50.0);
        let indicator =
            active_pane_left_indicator_segment(bounds, 8.0, 4.0, 20, 20).expect("indicator");

        assert_eq!(indicator, euclid::rect(14.0, 20.0, 8.0, 50.0));
    }

    #[test]
    fn left_indicator_clamps_rows_and_handles_invalid_geometry() {
        let bounds = euclid::rect(0.0, 0.0, 3.0, 40.0);
        let indicator =
            active_pane_left_indicator_segment(bounds, 8.0, 1.0, 99, 10).expect("indicator");

        assert_eq!(indicator, euclid::rect(1.0, 0.0, 2.0, 40.0));

        assert!(active_pane_left_indicator_segment(bounds, 2.0, 3.5, 1, 10).is_none());
        assert!(active_pane_left_indicator_segment(bounds, 2.0, 1.0, 1, 0).is_none());
    }

    #[test]
    fn gutter_radius_is_half_width() {
        assert_eq!(active_pane_gutter_radius(8.0), 4.0);
        assert_eq!(active_pane_gutter_radius(5.0), 2.5);
    }

    #[test]
    fn indicator_bounds_include_split_gutter_for_non_leftmost_panes() {
        let leftmost = active_pane_indicator_bounds(10.0, 20.0, 10.0, 20.0, 0, 0, 0, 10, 5);
        let inner = active_pane_indicator_bounds(10.0, 20.0, 10.0, 20.0, 0, 12, 0, 10, 5);

        assert_eq!(leftmost, euclid::rect(5.0, 20.0, 110.0, 100.0));
        assert_eq!(inner, euclid::rect(125.0, 20.0, 110.0, 100.0));
    }

    #[test]
    fn leftmost_and_inner_panes_share_same_gutter_overlap_profile() {
        let content_left = 10.0;
        let cell_width = 10.0;
        let left_padding = 4.0;
        let width = 8.0;

        let leftmost_bounds =
            active_pane_indicator_bounds(content_left, 20.0, cell_width, 20.0, 0, 0, 0, 10, 5);
        let inner_bounds =
            active_pane_indicator_bounds(content_left, 20.0, cell_width, 20.0, 0, 12, 0, 10, 5);

        let leftmost =
            active_pane_left_indicator_segment(leftmost_bounds, width, left_padding, 5, 5)
                .expect("leftmost");
        let inner = active_pane_left_indicator_segment(inner_bounds, width, left_padding, 5, 5)
            .expect("inner");

        let leftmost_content_start = content_left;
        let inner_content_start = content_left + (12.0 * cell_width);

        let leftmost_overlap = leftmost.max_x() - leftmost_content_start;
        let inner_overlap = inner.max_x() - inner_content_start;

        assert_eq!(leftmost_overlap, inner_overlap);
    }

    #[test]
    fn indicator_size_px_has_minimum_of_one() {
        assert_eq!(active_pane_indicator_size_px(0), 1.0);
        assert_eq!(active_pane_indicator_size_px(1), 1.0);
        assert_eq!(active_pane_indicator_size_px(6), 6.0);
    }

    #[test]
    fn pill_segment_is_centered_and_uses_15_percent_height() {
        let bounds = euclid::rect(10.0, 20.0, 100.0, 200.0);
        let pill = active_pane_left_pill_segment(bounds, 8.0, 4.0).expect("pill");

        assert!((pill.width() - 8.0).abs() < 0.001);
        assert!((pill.height() - 30.0).abs() < 0.001);
        assert!((pill.min_x() - 14.0).abs() < 0.001);
        assert!((pill.min_y() - 105.0).abs() < 0.001);
    }
}
