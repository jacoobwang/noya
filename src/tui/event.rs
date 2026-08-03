use crate::tui::app::{AgentState, App, TuiAction};
use crossterm::event::{
    Event as CrosstermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use futures_util::StreamExt;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum Event {
    Tick,
    Key(KeyEvent),
    Resize(u16, u16),
    Quit,
}

pub struct EventHandler {
    rx: mpsc::Receiver<Event>,
}

impl EventHandler {
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::channel(128);
        tokio::spawn(async move {
            let mut events = EventStream::new();
            let mut ticks = tokio::time::interval(tick_rate);
            loop {
                tokio::select! {
                    _ = ticks.tick() => {
                        if tx.send(Event::Tick).await.is_err() {
                            break;
                        }
                    }
                    event = events.next() => match event {
                        Some(Ok(CrosstermEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                            if tx.send(Event::Key(key)).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(CrosstermEvent::Resize(width, height))) => {
                            if tx.send(Event::Resize(width, height)).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(error)) => {
                            tracing::warn!(?error, "failed to read terminal event");
                        }
                        None => {
                            let _ = tx.send(Event::Quit).await;
                            break;
                        }
                    }
                }
            }
        });
        Self { rx }
    }

    pub async fn next(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}

pub fn handle_key_event(key: KeyEvent, app: &mut App) -> TuiAction {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => {
                if app.agent_state == AgentState::Idle {
                    app.should_quit = true;
                    TuiAction::Quit
                } else {
                    TuiAction::Cancel
                }
            }
            KeyCode::Char('d') => {
                app.should_quit = true;
                TuiAction::Quit
            }
            _ => TuiAction::None,
        };
    }

    if app.mode == crate::tui::app::AppMode::ConfiguringModel {
        return match key.code {
            KeyCode::Enter => app.submit_model_setup_input(),
            KeyCode::Esc => {
                app.cancel_model_setup();
                TuiAction::None
            }
            _ => handle_editing_key(key, app),
        };
    }

    if app.mode == crate::tui::app::AppMode::SelectingModel {
        return match key.code {
            KeyCode::Up => {
                app.select_previous_model();
                TuiAction::None
            }
            KeyCode::Down => {
                app.select_next_model();
                TuiAction::None
            }
            KeyCode::Enter => app.accept_selected_model(),
            KeyCode::Esc => {
                app.close_model_menu();
                TuiAction::None
            }
            _ => TuiAction::None,
        };
    }

    match key.code {
        KeyCode::Enter => app
            .accept_selected_command()
            .unwrap_or_else(|| app.submit_input()),
        KeyCode::Up if app.select_previous_command() => TuiAction::None,
        KeyCode::Down if app.select_next_command() => TuiAction::None,
        KeyCode::Esc if app.dismiss_command_menu() => TuiAction::None,
        KeyCode::Backspace => {
            if app.cursor_position > 0 {
                let start = prev_char_boundary(&app.input, app.cursor_position);
                app.input.replace_range(start..app.cursor_position, "");
                app.cursor_position = start;
                app.input_changed();
            }
            TuiAction::None
        }
        KeyCode::Delete => {
            if app.cursor_position < app.input.len() {
                let end = next_char_boundary(&app.input, app.cursor_position);
                app.input.replace_range(app.cursor_position..end, "");
                app.input_changed();
            }
            TuiAction::None
        }
        KeyCode::Left => {
            app.cursor_position = prev_char_boundary(&app.input, app.cursor_position);
            TuiAction::None
        }
        KeyCode::Right => {
            app.cursor_position = next_char_boundary(&app.input, app.cursor_position);
            TuiAction::None
        }
        KeyCode::Home => {
            app.cursor_position = 0;
            TuiAction::None
        }
        KeyCode::End => {
            app.cursor_position = app.input.len();
            TuiAction::None
        }
        KeyCode::Tab => {
            app.complete_selected_command();
            TuiAction::None
        }
        KeyCode::Char(character) => {
            app.input.insert(app.cursor_position, character);
            app.cursor_position += character.len_utf8();
            app.input_changed();
            TuiAction::None
        }
        _ => TuiAction::None,
    }
}

fn handle_editing_key(key: KeyEvent, app: &mut App) -> TuiAction {
    match key.code {
        KeyCode::Backspace => {
            if app.cursor_position > 0 {
                let start = prev_char_boundary(&app.input, app.cursor_position);
                app.input.replace_range(start..app.cursor_position, "");
                app.cursor_position = start;
            }
            TuiAction::None
        }
        KeyCode::Delete => {
            if app.cursor_position < app.input.len() {
                let end = next_char_boundary(&app.input, app.cursor_position);
                app.input.replace_range(app.cursor_position..end, "");
            }
            TuiAction::None
        }
        KeyCode::Left => {
            app.cursor_position = prev_char_boundary(&app.input, app.cursor_position);
            TuiAction::None
        }
        KeyCode::Right => {
            app.cursor_position = next_char_boundary(&app.input, app.cursor_position);
            TuiAction::None
        }
        KeyCode::Home => {
            app.cursor_position = 0;
            TuiAction::None
        }
        KeyCode::End => {
            app.cursor_position = app.input.len();
            TuiAction::None
        }
        KeyCode::Char(character) => {
            app.input.insert(app.cursor_position, character);
            app.cursor_position += character.len_utf8();
            TuiAction::None
        }
        _ => TuiAction::None,
    }
}

fn prev_char_boundary(value: &str, position: usize) -> usize {
    if position == 0 {
        return 0;
    }
    let mut position = position.min(value.len()) - 1;
    while position > 0 && !value.is_char_boundary(position) {
        position -= 1;
    }
    position
}

fn next_char_boundary(value: &str, position: usize) -> usize {
    if position >= value.len() {
        return value.len();
    }
    let mut position = position + 1;
    while position < value.len() && !value.is_char_boundary(position) {
        position += 1;
    }
    position
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{AgentState, App, AppInfo, TuiAction};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;

    fn app() -> App {
        App::new(AppInfo {
            workspace: PathBuf::from("/repo"),
            model: "test".to_string(),
            model_id: "test-model".to_string(),
        })
    }

    #[test]
    fn editing_is_utf8_safe() {
        let mut app = app();
        handle_key_event(
            KeyEvent::new(KeyCode::Char('你'), KeyModifiers::NONE),
            &mut app,
        );
        handle_key_event(
            KeyEvent::new(KeyCode::Char('好'), KeyModifiers::NONE),
            &mut app,
        );
        assert_eq!(app.input, "你好");
        assert_eq!(app.cursor_position, "你好".len());

        handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &mut app);
        handle_key_event(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &mut app,
        );
        assert_eq!(app.input, "好");
        assert_eq!(app.cursor_position, 0);
    }

    #[test]
    fn ctrl_c_cancels_active_turn_and_quits_when_idle() {
        let mut app = app();
        app.agent_state = AgentState::Generating;
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(handle_key_event(key, &mut app), TuiAction::Cancel);

        app.agent_state = AgentState::Idle;
        assert_eq!(handle_key_event(key, &mut app), TuiAction::Quit);
    }

    #[test]
    fn slash_menu_navigates_with_arrows_and_runs_the_selected_command() {
        let mut app = app();
        handle_key_event(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut app,
        );
        assert_eq!(app.command_suggestions()[0].name, "new");

        handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut app);
        assert_eq!(app.command_selection, 1);
        assert_eq!(
            handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut app),
            TuiAction::ListModels
        );
        assert!(app.input.is_empty());
    }

    #[test]
    fn selecting_a_command_with_arguments_completes_the_input() {
        let mut app = app();
        for character in "/resu".chars() {
            handle_key_event(
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                &mut app,
            );
        }

        assert_eq!(
            handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut app),
            TuiAction::None
        );
        assert_eq!(app.input, "/resume ");
        assert_eq!(app.cursor_position, app.input.len());
        assert!(app.command_suggestions().is_empty());
    }

    #[test]
    fn up_wraps_to_the_last_suggestion_and_escape_closes_the_menu() {
        let mut app = app();
        handle_key_event(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut app,
        );
        let count = app.command_suggestions().len();

        handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &mut app);
        assert_eq!(app.command_selection, count - 1);
        handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut app);
        assert!(app.command_suggestions().is_empty());
        assert_eq!(app.input, "/");
    }

    #[test]
    fn tab_completes_without_executing_the_command() {
        let mut app = app();
        for character in "/stat".chars() {
            handle_key_event(
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                &mut app,
            );
        }

        assert_eq!(
            handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &mut app),
            TuiAction::None
        );
        assert_eq!(app.input, "/status");
        assert!(app.messages.is_empty());
        assert!(app.command_suggestions().is_empty());
    }

    #[test]
    fn model_menu_uses_arrows_enter_and_escape() {
        let mut app = app();
        app.open_model_menu(vec![
            crate::tui::app::ModelChoice {
                model: "deepseek".to_string(),
                model_id: "deepseek-v4-flash".to_string(),
                current: true,
            },
            crate::tui::app::ModelChoice {
                model: "qwen".to_string(),
                model_id: "qwen3-coder-plus".to_string(),
                current: false,
            },
        ]);

        handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut app);
        assert_eq!(app.model_selection, 1);
        assert_eq!(
            handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut app),
            TuiAction::SwitchModel("qwen".to_string())
        );

        app.open_model_menu(vec![crate::tui::app::ModelChoice {
            model: "deepseek".to_string(),
            model_id: "deepseek-v4-flash".to_string(),
            current: true,
        }]);
        handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut app);
        assert_eq!(app.mode, crate::tui::app::AppMode::Normal);
        assert!(app.model_choices.is_empty());
    }
}
