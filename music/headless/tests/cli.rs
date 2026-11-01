//! The headless binary runs the server's command line.

#[test]
fn help_lists_the_server_commands() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_asteria-music"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    for command in ["init", "api-key", "serve"] {
        assert!(text.contains(command), "{command} missing from:\n{text}");
    }
}
