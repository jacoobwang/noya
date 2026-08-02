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

    match key.code {
        KeyCode::Enter => app.submit_input(),
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
        KeyCode::Tab => {
            autocomplete(app);
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

fn autocomplete(app: &mut App) {
    const COMMANDS: &[&str] = &[
        "/help", "/clear", "/reset", "/status", "/cancel", "/approve", "/confirm", "/reject",
        "/modify", "/quit", "/exit",
    ];
    if !app.input.starts_with('/') || app.input.contains(' ') {
        return;
    }
    let matches = COMMANDS
        .iter()
        .copied()
        .filter(|command| command.starts_with(&app.input))
        .collect::<Vec<_>>();
    if matches.len() == 1 {
        app.input = matches[0].to_string();
        app.cursor_position = app.input.len();
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
}
