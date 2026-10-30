use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub(super) struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub visible: bool,
}

pub(super) const SLASH_COMMANDS: [SlashCommand; 14] = [
    SlashCommand {
        name: "/provider",
        description: "choose or reconnect a provider",
        visible: true,
    },
    SlashCommand {
        name: "/model",
        description: "choose a provider model",
        visible: true,
    },
    SlashCommand {
        name: "/reasoning",
        description: "choose model reasoning effort",
        visible: true,
    },
    SlashCommand {
        name: "/new",
        description: "start a fresh session",
        visible: true,
    },
    SlashCommand {
        name: "/exec",
        description: "run a command directly in nono",
        visible: true,
    },
    SlashCommand {
        name: "/clear",
        description: "clear the screen",
        visible: true,
    },
    SlashCommand {
        name: "/details",
        description: "show the last tool's full output",
        visible: true,
    },
    SlashCommand {
        name: "/undo",
        description: "restore files to before the last turn",
        visible: true,
    },
    SlashCommand {
        name: "/theme",
        description: "choose the interface theme",
        visible: true,
    },
    SlashCommand {
        name: "/help",
        description: "show available commands",
        visible: true,
    },
    SlashCommand {
        name: "/detach",
        description: "keep this session running and detach",
        visible: true,
    },
    SlashCommand {
        name: "/q",
        description: "leave nonaut",
        visible: true,
    },
    SlashCommand {
        name: "/quit",
        description: "leave nonaut",
        visible: false,
    },
    SlashCommand {
        name: "/exit",
        description: "leave nonaut",
        visible: false,
    },
];

pub(super) fn parse_exec(session_root: &Path, input: &str) -> Result<(PathBuf, String), String> {
    let (cwd, command) = if let Some(rest) = input.strip_prefix("--cwd ") {
        let (raw_cwd, command) = take_word(rest.trim_start())?;
        let cwd = PathBuf::from(raw_cwd);
        let cwd = if cwd.is_absolute() {
            cwd
        } else {
            session_root.join(cwd)
        };
        (cwd, command.trim())
    } else {
        (session_root.to_path_buf(), input.trim())
    };
    if command.is_empty() {
        return Err("command may not be empty".to_owned());
    }
    Ok((cwd, command.to_owned()))
}

fn take_word(input: &str) -> Result<(String, &str), String> {
    let Some(first) = input.chars().next() else {
        return Err("--cwd needs a directory followed by a command".to_owned());
    };
    if matches!(first, '\'' | '"') {
        let rest = &input[first.len_utf8()..];
        let end = rest
            .find(first)
            .ok_or_else(|| "--cwd has an unterminated quoted directory".to_owned())?;
        let command = &rest[end + first.len_utf8()..];
        if command.trim().is_empty() {
            return Err("--cwd needs a command".to_owned());
        }
        return Ok((rest[..end].to_owned(), command));
    }
    let end = input
        .find(char::is_whitespace)
        .ok_or_else(|| "--cwd needs a directory followed by a command".to_owned())?;
    Ok((input[..end].to_owned(), &input[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_cwd_accepts_quoted_paths() {
        let (cwd, command) =
            parse_exec(Path::new("/work"), "--cwd 'two words' cargo test").unwrap();
        assert_eq!(cwd, PathBuf::from("/work/two words"));
        assert_eq!(command, "cargo test");
    }
}
