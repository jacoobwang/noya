use super::app::AppMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandScope {
    Always,
    Confirming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: &'static str,
    pub argument: Option<&'static str>,
    pub description: &'static str,
    pub argument_required: bool,
    scope: CommandScope,
}

impl SlashCommand {
    const fn always(
        name: &'static str,
        argument: Option<&'static str>,
        description: &'static str,
    ) -> Self {
        Self {
            name,
            argument,
            description,
            argument_required: argument.is_some(),
            scope: CommandScope::Always,
        }
    }

    const fn optional(
        name: &'static str,
        argument: &'static str,
        description: &'static str,
    ) -> Self {
        Self {
            name,
            argument: Some(argument),
            description,
            argument_required: false,
            scope: CommandScope::Always,
        }
    }

    const fn confirming(
        name: &'static str,
        argument: Option<&'static str>,
        description: &'static str,
    ) -> Self {
        Self {
            name,
            argument,
            description,
            argument_required: argument.is_some(),
            scope: CommandScope::Confirming,
        }
    }

    pub fn input(&self) -> String {
        format!("/{}", self.name)
    }
}

const COMMANDS: &[SlashCommand] = &[
    SlashCommand::always("new", None, "Start a new session"),
    SlashCommand::optional("model", "[name]", "Choose a logged-in model"),
    SlashCommand::always("sessions", None, "List sessions for this workspace"),
    SlashCommand::always("resume", Some("<id>"), "Resume a session by ID prefix"),
    SlashCommand::always("rename", Some("<title>"), "Rename the current session"),
    SlashCommand::always("retry", None, "Retry the latest interrupted request"),
    SlashCommand::always("compact", None, "Compact the current session context"),
    SlashCommand::always("status", None, "Show session, model, and context status"),
    SlashCommand::always("clear", None, "Clear messages from the TUI"),
    SlashCommand::always("reset", None, "Reset the current context"),
    SlashCommand::always("help", None, "Show available commands"),
    SlashCommand::always("cancel", None, "Cancel the active turn"),
    SlashCommand::always("quit", None, "Exit Noya"),
    SlashCommand::confirming("approve", None, "Approve the pending tool call"),
    SlashCommand::confirming("reject", None, "Reject the pending tool call"),
    SlashCommand::confirming(
        "modify",
        Some("<json>"),
        "Modify and approve the pending tool call",
    ),
];

pub fn suggestions(input: &str, mode: AppMode) -> Vec<&'static SlashCommand> {
    let Some(prefix) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if prefix.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter(|command| {
            command.name.starts_with(prefix)
                && (command.scope == CommandScope::Always || mode == AppMode::Confirming)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_commands_by_prefix_and_mode() {
        let normal = suggestions("/re", AppMode::Normal)
            .into_iter()
            .map(|command| command.name)
            .collect::<Vec<_>>();
        assert_eq!(normal, vec!["resume", "rename", "retry", "reset"]);
        assert_eq!(suggestions("/mo", AppMode::Normal)[0].name, "model");
        assert!(suggestions("/app", AppMode::Normal).is_empty());
        assert_eq!(suggestions("/app", AppMode::Confirming)[0].name, "approve");
    }

    #[test]
    fn closes_suggestions_after_an_argument_separator() {
        assert!(suggestions("/resume ", AppMode::Normal).is_empty());
        assert!(suggestions("plain text", AppMode::Normal).is_empty());
    }
}
