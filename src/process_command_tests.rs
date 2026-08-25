use super::{command_program_name_matches, command_tokens};

#[test]
fn tokens_preserve_whitespace_inside_double_quotes() {
    assert_eq!(
        command_tokens(
            r#""C:\Program Files\sing-box-tui.exe" run --config "C:\Proxy Config\config.json""#,
        ),
        vec![
            r#"C:\Program Files\sing-box-tui.exe"#,
            "run",
            "--config",
            r#"C:\Proxy Config\config.json"#,
        ]
    );
}

#[test]
fn program_names_match_paths_case_and_optional_exe_suffix() {
    assert!(command_program_name_matches(
        r#"C:\Program Files\sing-box-tui.exe"#,
        "sing-box-tui"
    ));
    assert!(command_program_name_matches(
        "/usr/local/bin/sing-box-tui",
        "SING-BOX-TUI.exe"
    ));
    assert!(!command_program_name_matches(
        "/usr/local/bin/sing-box",
        "sing-box-tui"
    ));
}
