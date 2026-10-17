// ------------ Game Command Line ------------
// Builds the command line the helper starts the game with, quoting each argument the way CreateProcess expects, and
// explains the one start error people actually hit (a folder on a mapped or subst drive).

pub fn quote_argument(argument: &str) -> String {
    if !argument.is_empty() && !argument.contains([' ', '\t', '"']) {
        return argument.to_string();
    }
    quote_always(argument)
}

pub fn quote_always(argument: &str) -> String {
    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for character in argument.chars() {
        match character {
            '\\' => {
                backslashes += 1;
                quoted.push('\\');
            }
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes + 1));
                quoted.push('"');
                backslashes = 0;
            }
            other => {
                backslashes = 0;
                quoted.push(other);
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes));
    quoted.push('"');
    quoted
}

pub fn build_command_line(executable: &str, arguments: &[String]) -> String {
    let mut line = quote_argument(executable);
    for argument in arguments {
        line.push(' ');
        line.push_str(&quote_argument(argument));
    }
    line
}

pub fn describe_start_error(code: u32) -> Option<&'static str> {
    matches!(code, 2 | 3 | 267).then_some(
        "Windows could not find that folder from an administrator program. If it is on a mapped \
         or subst drive letter, move it to a local drive.",
    )
}

#[cfg(test)]
mod tests {
    use super::quote_argument;

    #[test]
    fn quote_argument_doubles_backslashes_before_the_closing_quote() {
        assert_eq!(quote_argument(r"D:\my logs\"), r#""D:\my logs\\""#);
        assert_eq!(quote_argument(r"D:\logs\"), r"D:\logs\");
        assert_eq!(quote_argument(""), "\"\"");
    }
}
