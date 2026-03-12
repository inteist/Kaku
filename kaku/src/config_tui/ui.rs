use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use super::{App, Mode};
use crate::tui_core::theme::{accent, bg, muted, panel, primary, text_fg};

const MIN_KEY_COLUMN_WIDTH: usize = 24;
const KEY_VALUE_GAP: usize = 4;
const WINDOW_BACKGROUND_OPACITY_KEY: &str = "window_background_opacity";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MainLayoutMode {
    HeaderOnly,
    HeaderAndFooter,
    Expanded,
    Compact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FooterCopy {
    primary_key: &'static str,
    primary_long: &'static str,
    primary_short: &'static str,
    secondary_key: Option<&'static str>,
    secondary_long: Option<&'static str>,
    secondary_short: Option<&'static str>,
    tertiary_key: Option<&'static str>,
    tertiary_long: Option<&'static str>,
    tertiary_short: Option<&'static str>,
}

const ESC_KEY_STR: &'static str = "Esc";
const ENTER_KEY_STR: &'static str = "Enter";
const Q_KEY_STR: &'static str = "q";
const E_KEY_STR: &'static str = "e";

fn footer_copy(mode: Mode) -> FooterCopy {
    let apply_edit_long = "Apply Current Change";
    let apply_short = " apply";
    let cancel_short = " cancel";

    match mode {
        Mode::Normal => FooterCopy {
            primary_key: ESC_KEY_STR,
            primary_long: " Apply Changes and Quit",
            primary_short: apply_short,
            secondary_key: Some(E_KEY_STR),
            secondary_long: Some(" Open Config (opens default editor)"),
            secondary_short: Some(" config"),
            tertiary_key: Some(Q_KEY_STR),
            tertiary_long: Some(" Discard Changes and Quit"),
            tertiary_short: Some(" quit"),
        },
        Mode::Selecting => FooterCopy {
            primary_key: ENTER_KEY_STR,
            primary_long: " Apply Current Change",
            primary_short: apply_short,
            secondary_key: Some(ESC_KEY_STR),
            secondary_long: Some(" Apply & Exit"),
            secondary_short: Some(apply_short),
            tertiary_key: None,
            tertiary_long: None,
            tertiary_short: None,
        },
        Mode::Editing => FooterCopy {
            primary_key: ENTER_KEY_STR,
            primary_long: apply_edit_long,
            primary_short: apply_short,
            secondary_key: Some(ESC_KEY_STR),
            secondary_long: Some(" Cancel Editing"),
            secondary_short: Some(cancel_short),
            tertiary_key: None,
            tertiary_long: None,
            tertiary_short: None,
        },
    }
}

pub(super) fn ui(frame: &mut ratatui::Frame, app: &mut App) {
    let full = frame.area();
    if full.width < 2 || full.height < 2 {
        return;
    }

    // Keep one column on the right to avoid edge-wrap artifacts, while using
    // full height so the footer can stick to the bottom.
    let area = Rect::new(full.x, full.y, full.width - 1, full.height);

    frame.render_widget(Clear, area);

    let content_rows = rendered_field_row_count(app);
    match resolve_main_layout(area.height, content_rows) {
        MainLayoutMode::HeaderOnly => {
            let chunks = Layout::vertical([Constraint::Length(2)]).split(area);
            render_header(frame, chunks[0]);
        }
        MainLayoutMode::HeaderAndFooter => {
            let chunks =
                Layout::vertical([Constraint::Length(2), Constraint::Length(1)]).split(area);
            render_header(frame, chunks[0]);
            render_footer(frame, chunks[1], app.mode);
        }
        MainLayoutMode::Expanded => {
            let chunks = Layout::vertical([
                Constraint::Length(2),            // header
                Constraint::Length(content_rows), // content
                Constraint::Fill(1),              // flexible gap
                Constraint::Length(1),            // spacer above footer
                Constraint::Length(1),            // footer (stick to bottom)
            ])
            .split(area);

            render_header(frame, chunks[0]);
            render_fields(frame, chunks[1], app);
            render_footer(frame, chunks[4], app.mode);
        }
        MainLayoutMode::Compact => {
            let chunks = Layout::vertical([
                Constraint::Length(2), // header
                Constraint::Fill(1),   // content
                Constraint::Length(1), // spacer above footer
                Constraint::Length(1), // footer (stick to bottom)
            ])
            .split(area);

            render_header(frame, chunks[0]);
            render_fields(frame, chunks[1], app);
            render_footer(frame, chunks[3], app.mode);
        }
    }

    if app.mode == Mode::Selecting {
        render_selector(frame, area, app);
    } else if app.mode == Mode::Editing {
        render_editor(frame, area, app);
    }
}

fn resolve_main_layout(area_height: u16, content_rows: u16) -> MainLayoutMode {
    let remaining_height = area_height.saturating_sub(2);
    if remaining_height == 0 {
        MainLayoutMode::HeaderOnly
    } else if remaining_height == 1 {
        MainLayoutMode::HeaderAndFooter
    } else if content_rows + 2 <= remaining_height {
        MainLayoutMode::Expanded
    } else {
        MainLayoutMode::Compact
    }
}

fn rendered_field_row_count(app: &App) -> u16 {
    let mut rows = app.fields.len() as u16;
    let mut sections = 0u16;
    let mut last_section: Option<&str> = None;

    for field in &app.fields {
        if last_section != Some(field.section) {
            sections += 1;
            if last_section.is_some() {
                rows += 1;
            }
            last_section = Some(field.section);
        }
    }

    rows + sections
}

fn render_header(frame: &mut ratatui::Frame, area: Rect) {
    let version = format!("v{}", config::wezterm_version());
    let line = Line::from(vec![
        Span::styled(
            "  Kaku ",
            Style::default().fg(primary()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(version, Style::default().fg(primary())),
        Span::styled(" · ", Style::default().fg(muted())),
        Span::styled("Settings", Style::default().fg(text_fg())),
    ]);
    frame.render_widget(Paragraph::new(vec![line, Line::from("")]), area);
}

fn key_column_width(app: &App) -> usize {
    let widest_key = app
        .fields
        .iter()
        .map(|field| field.key.chars().count())
        .max()
        .unwrap_or(0);
    MIN_KEY_COLUMN_WIDTH.max(widest_key + KEY_VALUE_GAP)
}

fn render_fields(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let area = area.inner(Margin::new(0, 0));
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_flat: Option<usize> = None;
    let mut flat = 0usize;
    let key_width = key_column_width(app);
    let mut current_section: Option<&str> = None;

    for (idx, field) in app.fields.iter().enumerate() {
        if current_section != Some(field.section) {
            if current_section.is_some() {
                items.push(ListItem::new(Line::from("")));
                flat += 1;
            }

            items.push(ListItem::new(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    field.section,
                    Style::default().fg(muted()).add_modifier(Modifier::BOLD),
                ),
            ])));
            flat += 1;
            current_section = Some(field.section);
        }

        let is_selected = idx == app.selected;
        let is_disabled = app.is_field_disabled(field.lua_key);
        if is_selected && !is_disabled {
            selected_flat = Some(flat);
        }

        let display_value = app.display_value(field);
        let has_options = field.has_options();
        let has_horizontal_adjust = (App::numeric_step_for(field.lua_key).is_some()
            || field.lua_key == "active_pane_indicator")
            && !is_disabled;

        let key_style = if is_disabled {
            Style::default().fg(muted())
        } else if is_selected {
            Style::default().fg(primary()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(text_fg())
        };

        let value_style = if is_disabled {
            Style::default().fg(muted())
        } else if is_selected {
            Style::default().fg(primary()).add_modifier(Modifier::BOLD)
        } else if field.value.is_empty() {
            Style::default().fg(muted())
        } else {
            Style::default().fg(accent())
        };

        let marker = if is_selected && !is_disabled {
            "› "
        } else {
            "  "
        };
        let suffix = if has_options && field.options.len() > 2 {
            " ▾"
        } else {
            ""
        };
        let value_label = if field.lua_key == WINDOW_BACKGROUND_OPACITY_KEY {
            format!("{}%", display_value)
        } else {
            display_value.to_string()
        };

        let rendered_value = if has_horizontal_adjust {
            format!("◀ {} ▶", value_label)
        } else {
            format!("{}{}", value_label, suffix)
        };

        let line = Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                marker,
                Style::default()
                    .fg(if is_selected { primary() } else { muted() })
                    .add_modifier(if is_selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(
                format!("{:<width$}", field.key, width = key_width),
                key_style,
            ),
            Span::styled(rendered_value, value_style),
        ]);

        items.push(ListItem::new(line));
        flat += 1;
    }

    let mut state = ListState::default();
    state.select(selected_flat);

    let list = List::new(items).highlight_style(Style::default());
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_footer(frame: &mut ratatui::Frame, area: Rect, mode: Mode) {
    let copy = footer_copy(mode);
    let line = if area.width >= 80 {
        let mut spans = vec![
            Span::styled("  ", Style::default()),
            Span::styled(copy.primary_key, Style::default().fg(primary())),
            Span::styled(copy.primary_long, Style::default().fg(muted())),
        ];
        if copy.secondary_key.is_some() || copy.secondary_long.is_some() {
            spans.push(Span::styled("  ·  ", Style::default().fg(muted())));
            spans.push(Span::styled(
                copy.secondary_key.unwrap_or(""),
                Style::default().fg(primary()),
            ));
            spans.push(Span::styled(
                copy.secondary_long.unwrap_or(""),
                Style::default().fg(muted()),
            ));
        }
        if copy.tertiary_key.is_some() || copy.tertiary_long.is_some() {
            spans.push(Span::styled("  ·  ", Style::default().fg(muted())));
            spans.push(Span::styled(
                copy.tertiary_key.unwrap_or(""),
                Style::default().fg(primary()),
            ));
            spans.push(Span::styled(
                copy.tertiary_long.unwrap_or(""),
                Style::default().fg(muted()),
            ));
        }
        Line::from(spans)
    } else if area.width >= 40 {
        let mut spans = vec![
            Span::styled("  ", Style::default()),
            Span::styled(copy.primary_key, Style::default().fg(primary())),
            Span::styled(copy.primary_short, Style::default().fg(muted())),
        ];
        if copy.secondary_key.is_some() || copy.secondary_short.is_some() {
            spans.push(Span::styled("  ·  ", Style::default().fg(muted())));
            spans.push(Span::styled(
                copy.secondary_key.unwrap_or(""),
                Style::default().fg(primary()),
            ));
            spans.push(Span::styled(
                copy.secondary_short.unwrap_or(""),
                Style::default().fg(muted()),
            ));
        }
        if copy.tertiary_key.is_some() || copy.tertiary_short.is_some() {
            spans.push(Span::styled("  ·  ", Style::default().fg(muted())));
            spans.push(Span::styled(
                copy.tertiary_key.unwrap_or(""),
                Style::default().fg(primary()),
            ));
            spans.push(Span::styled(
                copy.tertiary_short.unwrap_or(""),
                Style::default().fg(muted()),
            ));
        }
        Line::from(spans)
    } else {
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(copy.primary_key, Style::default().fg(primary())),
            Span::styled(copy.primary_short, Style::default().fg(muted())),
        ])
    };

    frame.render_widget(Paragraph::new(line), area);
}

fn render_selector(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some((field, select_index)) = app.selecting_view() else {
        return;
    };

    let option_count = field.options.len() as u16;
    let max_popup_width = area.width.saturating_sub(4);
    let min_popup_width = 40u16.min(max_popup_width);
    let longest_option_width = field
        .options
        .iter()
        .map(|opt| opt.chars().count() as u16)
        .max()
        .unwrap_or(0);
    let popup_width = std::cmp::max(
        min_popup_width,
        longest_option_width.saturating_add(10).min(max_popup_width),
    );
    let popup_height = (option_count + 2).min(area.height.saturating_sub(4));
    let popup = Rect::new(
        (area.width.saturating_sub(popup_width)) / 2,
        (area.height.saturating_sub(popup_height)) / 2,
        popup_width,
        popup_height,
    );

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" Select: ", Style::default().fg(primary())),
            Span::styled(field.key, Style::default().fg(text_fg())),
            Span::styled("  ", Style::default()),
            Span::styled("Enter", Style::default().fg(primary())),
            Span::styled(" / ", Style::default().fg(muted())),
            Span::styled("Esc", Style::default().fg(primary())),
            Span::styled(": Apply ", Style::default().fg(muted())),
        ]))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(primary()))
        .style(Style::default().bg(panel()));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let items: Vec<ListItem> = field
        .options
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            let is_sel = i == select_index;
            let marker = if is_sel { "› " } else { "  " };
            let style = if is_sel {
                Style::default().fg(primary()).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(text_fg())
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    marker,
                    Style::default()
                        .fg(if is_sel { primary() } else { muted() })
                        .add_modifier(if is_sel {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(*opt, style),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(select_index));

    let list = List::new(items).highlight_style(Style::default());
    frame.render_stateful_widget(list, inner, &mut state);
}

fn render_editor(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some((field, edit_buf, edit_cursor)) = app.editing_view() else {
        return;
    };

    let popup_width = ((area.width as f32 * 0.7) as u16).min(area.width.saturating_sub(4));
    let popup_height = 5u16.min(area.height.saturating_sub(4));
    let popup = Rect::new(
        (area.width.saturating_sub(popup_width)) / 2,
        (area.height.saturating_sub(popup_height)) / 2,
        popup_width,
        popup_height,
    );

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" Edit: ", Style::default().fg(primary())),
            Span::styled(field.key, Style::default().fg(text_fg())),
            Span::styled("  ", Style::default()),
            Span::styled("Enter", Style::default().fg(primary())),
            Span::styled(": Save  ", Style::default().fg(muted())),
            Span::styled("Esc", Style::default().fg(primary())),
            Span::styled(": Cancel ", Style::default().fg(muted())),
        ]))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(primary()))
        .style(Style::default().bg(panel()));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let content_area = inner.inner(Margin::new(1, 0));

    let line = if edit_buf.is_empty() {
        Line::from(Span::styled(" ", Style::default().bg(primary())))
    } else {
        let char_count = edit_buf.chars().count();
        let byte_pos = edit_buf
            .char_indices()
            .nth(edit_cursor)
            .map(|(i, _)| i)
            .unwrap_or(edit_buf.len());
        let before = &edit_buf[..byte_pos];
        let after = &edit_buf[byte_pos..];

        if edit_cursor >= char_count {
            Line::from(vec![
                Span::styled(before, Style::default().fg(text_fg())),
                Span::styled(" ", Style::default().bg(primary())),
            ])
        } else {
            let mut chars = after.chars();
            let current_char = chars.next().unwrap_or(' ');
            let remaining = chars.as_str();

            Line::from(vec![
                Span::styled(before, Style::default().fg(text_fg())),
                Span::styled(
                    current_char.to_string(),
                    Style::default().bg(primary()).fg(bg()),
                ),
                Span::styled(remaining, Style::default().fg(text_fg())),
            ])
        }
    };

    let input = Paragraph::new(vec![line]).wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(input, content_area);
}

#[cfg(test)]
mod spacing_tests {
    use super::{key_column_width, KEY_VALUE_GAP, MIN_KEY_COLUMN_WIDTH};
    use crate::config_tui::App;
    use std::path::PathBuf;

    #[test]
    fn key_column_width_respects_minimum() {
        let app = App::new(PathBuf::from("/tmp/kaku-config-tui-test.lua"));
        assert!(key_column_width(&app) >= MIN_KEY_COLUMN_WIDTH);
    }

    #[test]
    fn key_column_width_tracks_longest_key_plus_gap() {
        let app = App::new(PathBuf::from("/tmp/kaku-config-tui-test.lua"));
        let widest_key = app
            .fields
            .iter()
            .map(|field| field.key.chars().count())
            .max()
            .unwrap_or(0);
        let expected = widest_key + KEY_VALUE_GAP;
        assert_eq!(key_column_width(&app), expected.max(MIN_KEY_COLUMN_WIDTH));
    }
}

#[cfg(test)]
mod tests {
    use super::{footer_copy, resolve_main_layout, FooterCopy, MainLayoutMode};
    use crate::config_tui::Mode;

    #[test]
    fn keeps_spacer_in_compact_layout() {
        assert_eq!(resolve_main_layout(8, 8), MainLayoutMode::Compact);
    }

    #[test]
    fn requires_room_for_footer_and_spacer_before_expanding() {
        assert_eq!(resolve_main_layout(8, 4), MainLayoutMode::Expanded);
        assert_eq!(resolve_main_layout(8, 5), MainLayoutMode::Compact);
    }

    #[test]
    fn handles_tiny_terminal_heights() {
        assert_eq!(resolve_main_layout(2, 1), MainLayoutMode::HeaderOnly);
        assert_eq!(resolve_main_layout(3, 1), MainLayoutMode::HeaderAndFooter);
    }
}
