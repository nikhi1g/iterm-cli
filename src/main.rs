fn main() {
    // a pane's drainer: delivers messages held behind a permission prompt once it's gone (`iterm2::input`)
    if let Some(uuid) = std::env::var_os(itermkit::iterm2::input::DRAIN_ENV) {
        std::process::exit(itermkit::iterm2::input::drain_main(&uuid.to_string_lossy()));
    }
    // an argument that isn't text is one error line, never the panic `std::env::args` makes of it
    let argv = match itermkit::cli::text_args(std::env::args_os().skip(1)) {
        Ok(a) => a,
        Err(line) => {
            println!("{line}");
            std::process::exit(1);
        }
    };
    // Claude Code's PreToolUse hook (~/.claude/settings.json): blocks Bash calls to it2, never fails the call
    if argv == ["hook", "pretooluse"] {
        std::process::exit(itermkit::hook::pretooluse());
    }
    let env = itermkit::conn::Env::from_process();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let code = itermkit::cli::run(&argv, env, &mut std::io::stdin(), &mut out);
    std::process::exit(code);
}
