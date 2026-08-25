/// Splits a process-list command line while preserving whitespace inside
/// double-quoted arguments.
pub(crate) fn command_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for character in command.chars() {
        match character {
            '"' => in_quotes = !in_quotes,
            value if value.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            value => current.push(value),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Matches executable basenames across Unix/Windows paths and an optional
/// Windows `.exe` suffix.
pub(crate) fn command_program_name_matches(program: &str, expected: &str) -> bool {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let expected = expected.rsplit(['/', '\\']).next().unwrap_or(expected);
    name.eq_ignore_ascii_case(expected)
        || name
            .strip_suffix(".exe")
            .is_some_and(|base| base.eq_ignore_ascii_case(expected))
        || expected
            .strip_suffix(".exe")
            .is_some_and(|base| name.eq_ignore_ascii_case(base))
}

#[cfg(test)]
#[path = "process_command_tests.rs"]
mod tests;
