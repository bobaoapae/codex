use super::super::agents_overview::AGENTS_OVERVIEW_VIEW_ID;
use super::*;
use crate::bottom_pane::BottomPaneView;
use crate::bottom_pane::ViewCompletion;
use crate::key_hint::KeyBindingListExt;
use crate::key_hint::ShortcutHint;
use crate::key_hint::is_plain_text_key_event;
use crate::keymap::KeymapContext;
use crate::keymap::KeymapContextSet;
use crate::keymap::ListAction;
use crate::render::renderable::Renderable;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Margin;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Clear;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

impl BottomPaneView for AgentsFleetView {
    fn view_id(&self) -> Option<&'static str> {
        Some(AGENTS_OVERVIEW_VIEW_ID)
    }

    fn selected_index(&self) -> Option<usize> {
        Some(self.selected)
    }

    fn keymap_contexts(&self) -> KeymapContextSet {
        KeymapContextSet::new(KeymapContext::List).with(KeymapContext::Agents)
    }

    fn completion(&self) -> Option<ViewCompletion> {
        self.completion
    }

    fn is_complete(&self) -> bool {
        self.completion.is_some()
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
    }

    fn handle_paste(&mut self, pasted: String) -> bool {
        self.edit_input(|input| {
            input.push_str(&crate::history_cell::sanitize_user_text(pasted.into()))
        })
    }

    fn handle_key_event(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Backspace
            && key.modifiers.is_empty()
            && self.keymap.action_for(key).is_none()
        {
            self.edit_input(|input| {
                input.pop();
            });
            return;
        }
        if is_plain_text_key_event(key)
            && let KeyCode::Char(character) = key.code
        {
            self.edit_input(|input| input.push(character));
            return;
        }

        if self.agents_keymap.search.is_pressed(key) {
            let mut state = self.state();
            if !state.renaming {
                state.searching = !state.searching;
                if !state.searching {
                    state.search.clear();
                }
            }
            return;
        }
        if self.agents_keymap.new_task.is_pressed(key) {
            let mut state = self.state();
            state.input.clear();
            state.search.clear();
            state.searching = false;
            state.renaming = false;
            return;
        }
        if self.agents_keymap.rename.is_pressed(key) {
            self.begin_rename();
            return;
        }
        if self.agents_keymap.stop.is_pressed(key) {
            self.stop_selected();
            return;
        }
        if self.agents_keymap.toggle_grouping.is_pressed(key) {
            let mut state = self.state();
            state.status_grouping = !state.status_grouping;
            return;
        }

        if let Some(action) = self.keymap.action_for(key) {
            match action {
                ListAction::MoveUp => self.move_selection(false),
                ListAction::MoveDown => self.move_selection(true),
                ListAction::PageUp => {
                    for _ in 0..5 {
                        self.move_selection(false);
                    }
                }
                ListAction::PageDown => {
                    for _ in 0..5 {
                        self.move_selection(true);
                    }
                }
                ListAction::JumpTop => {
                    self.selected = self.visible_indices().first().copied().unwrap_or_default();
                    self.remember_selection();
                }
                ListAction::JumpBottom => {
                    self.selected = self.visible_indices().last().copied().unwrap_or_default();
                    self.remember_selection();
                }
                ListAction::MoveRight => self.open_actions(),
                ListAction::MoveLeft => {}
                ListAction::Accept => self.activate(),
                ListAction::Cancel => {
                    let mut state = self.state();
                    if state.searching {
                        state.search.clear();
                        state.searching = false;
                        drop(state);
                        self.selected = 0;
                        self.remember_selection();
                    } else if !state.input.is_empty() || state.renaming {
                        state.input.clear();
                        state.renaming = false;
                    } else {
                        drop(state);
                        if self.exit_on_cancel {
                            self.app_event_tx
                                .send(AppEvent::Exit(crate::app::ExitMode::Immediate));
                        }
                        self.completion = Some(ViewCompletion::Cancelled);
                    }
                }
            }
        } else if key.code == KeyCode::Backspace {
            self.edit_input(|input| {
                input.pop();
            });
        }
    }
}

impl Renderable for AgentsFleetView {
    fn desired_height(&self, _width: u16) -> u16 {
        24
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let state = self.state().clone();
        let (label, input) = if state.searching {
            ("  Search › ", &state.search)
        } else if state.renaming {
            ("  Rename › ", &state.input)
        } else {
            ("  New task › ", &state.input)
        };
        let x = area
            .x
            .saturating_add((label.width() + input.width()) as u16)
            .min(area.right().saturating_sub(3));
        Some((x, area.bottom().saturating_sub(2)))
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 12 || area.height < 8 {
            return;
        }
        Clear.render(area, buf);
        let [
            header,
            summary,
            divider,
            body,
            notice_area,
            prompt,
            footer_area,
        ] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        let inset =
            |rect: Rect| rect.inner(Margin::new(/*horizontal*/ 2, /*vertical*/ 0));
        Line::from("Agent fleet".bold()).render(inset(header), buf);
        self.render_summary(inset(summary), buf);
        Line::from("─".repeat(usize::from(area.width.saturating_sub(4))).dim())
            .render(inset(divider), buf);
        self.render_rows(inset(body), buf);
        if let Some(notice) = self.notice.as_deref() {
            Line::from(crate::text_formatting::truncate_text(
                notice,
                usize::from(notice_area.width),
            ))
            .render(inset(notice_area), buf);
        }
        let state = self.state().clone();
        let (label, input) = if state.searching {
            ("Search › ", &state.search)
        } else if state.renaming {
            ("Rename › ", &state.input)
        } else {
            ("New task › ", &state.input)
        };
        let placeholder = if input.is_empty() && !state.searching && !state.renaming {
            "Describe a task and press enter to dispatch it"
        } else {
            ""
        };
        Line::from(vec![
            label.cyan().bold(),
            input.as_str().into(),
            placeholder.dim(),
        ])
        .render(inset(prompt), buf);

        let list_hint = |action| self.keymap.primary_hint(action);
        let navigation = [ListAction::MoveUp, ListAction::MoveDown]
            .into_iter()
            .filter_map(list_hint)
            .map(|hint| hint.display_label().replace(" + ", "+"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut footer_spans = Vec::new();
        if !navigation.is_empty() {
            footer_spans.extend([navigation.bold(), " navigate  ".dim()]);
        }
        let mut add_hint = |hint: Option<ShortcutHint>, label: &'static str| {
            if let Some(hint) = hint {
                footer_spans.extend([
                    hint.display_label().replace(" + ", "+").bold(),
                    format!(" {label}  ").dim(),
                ]);
            }
        };
        add_hint(list_hint(ListAction::Accept), "open");
        add_hint(list_hint(ListAction::MoveRight), "manage");
        add_hint(
            self.agents_keymap
                .primary_hint("search", &self.agents_keymap.search),
            "search",
        );
        add_hint(
            self.agents_keymap
                .primary_hint("toggle_grouping", &self.agents_keymap.toggle_grouping),
            "group",
        );
        add_hint(
            self.agents_keymap
                .primary_hint("new_task", &self.agents_keymap.new_task),
            "new",
        );
        add_hint(
            self.agents_keymap
                .primary_hint("rename", &self.agents_keymap.rename),
            "rename",
        );
        add_hint(
            self.agents_keymap
                .primary_hint("stop", &self.agents_keymap.stop),
            "stop",
        );
        add_hint(list_hint(ListAction::Cancel), "back");
        Line::from(footer_spans).render(inset(footer_area), buf);
    }
}
