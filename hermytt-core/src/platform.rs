use portable_pty::CommandBuilder;

/// Default shell for the current platform.
pub fn default_shell() -> &'static str {
    if cfg!(windows) {
        "powershell.exe"
    } else {
        "/bin/bash"
    }
}

/// Apply platform-specific environment to a PTY command.
pub fn configure_command(cmd: &mut CommandBuilder) {
    if cfg!(windows) {
        // Windows ConPTY doesn't use TERM.
        cmd.env("PROMPT", "$P$G");
    } else {
        cmd.env("TERM", "xterm-256color");
    }
}
