use zed_extension_api::{self as zed, Command, LanguageServerId, Result, Worktree};

struct ZedFtpExtension;

impl zed::Extension for ZedFtpExtension {
    fn new() -> Self {
        Self
    }

    // Zed calls this once per worktree the first time a C file is opened.
    // We just return the command to launch `zed-ftp-lsp`; the LSP process
    // then handles textDocument/didOpen for every subsequent open in that
    // worktree.
    //
    // Prerequisite: the `zed-ftp-lsp` binary must be on the user's PATH
    // (typically installed via `cargo install --path .` from the main
    // zed_ftp repo). If it's not found, Zed shows a diagnostic and
    // auto-pull silently no-ops — the editor still works normally.
    fn language_server_command(
        &mut self,
        _lsp_id: &LanguageServerId,
        _worktree: &Worktree,
    ) -> Result<Command> {
        Ok(Command {
            command: "zed-ftp-lsp".to_string(),
            args: vec![],
            env: vec![],
        })
    }
}

zed::register_extension!(ZedFtpExtension);
