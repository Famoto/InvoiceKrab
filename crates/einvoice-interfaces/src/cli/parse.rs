//! Argument parsing: raw argv → [`Command`].
//!
//! Pure and IO-free, so the whole flag grammar is unit-tested without touching
//! the process environment.

use super::{Args, CliError, Command};

/// Parses raw argv (excluding the program name) into a [`Command`].
///
/// # Errors
///
/// Returns [`CliError::Usage`] for unknown flags, missing values, or the wrong
/// number of positional arguments.
///
/// # Examples
///
/// ```
/// use einvoice_interfaces::cli::{parse_args, Command};
/// let cmd = parse_args(&["in.xml".into(), "ubl-invoice".into()]).unwrap();
/// assert!(matches!(cmd, Command::Transform(_)));
/// ```
pub fn parse_args(args: &[String]) -> Result<Command, CliError> {
    let mut positionals: Vec<String> = Vec::new();
    let mut source_format: Option<String> = None;
    let mut output: Option<String> = None;
    let mut analyze = false;
    let mut keys = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--list" => return Ok(Command::ListFormats),
            "--analyze" => {
                analyze = true;
                i += 1;
            }
            "--keys" => {
                keys = true;
                i += 1;
            }
            "--from" | "--out" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| CliError::Usage(format!("`{arg}` requires a value")))?;
                if arg == "--from" {
                    source_format = Some(value.clone());
                } else {
                    output = Some(value.clone());
                }
                i += 2;
            }
            flag if flag.starts_with("--") => {
                return Err(CliError::Usage(format!("unknown flag `{flag}`")));
            }
            _ => {
                positionals.push(arg.clone());
                i += 1;
            }
        }
    }

    // `--analyze` and `--keys` are mode switches: the optional format comes from
    // `--from` or a lone positional, so e.g. `--keys`, `--keys ubl-invoice`, and
    // `--keys --from ubl-invoice` all work. They are mutually exclusive.
    if analyze || keys {
        let flag = if analyze { "--analyze" } else { "--keys" };
        if analyze && keys {
            return Err(CliError::Usage(
                "--analyze and --keys cannot be combined".into(),
            ));
        }
        if positionals.len() > 1 {
            return Err(CliError::Usage(format!("{flag} takes at most one format")));
        }
        let format = source_format.or_else(|| positionals.first().cloned());
        return Ok(if analyze {
            Command::Analyze(format)
        } else {
            Command::Keys(format)
        });
    }

    match positionals.as_slice() {
        [] => Ok(Command::Help),
        [input, target_format] => Ok(Command::Transform(Args {
            input: input.clone(),
            target_format: target_format.clone(),
            source_format,
            output,
        })),
        [_] => Err(CliError::Usage(
            "missing <TARGET-FORMAT> (run with --help)".into(),
        )),
        _ => Err(CliError::Usage(
            "too many arguments (run with --help)".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[test]
    fn test_parse_args_two_positionals_is_transform() {
        let cmd = parse_args(&[s("in.xml"), s("ubl-invoice")]).expect("valid");
        assert_eq!(
            cmd,
            Command::Transform(Args {
                input: s("in.xml"),
                target_format: s("ubl-invoice"),
                source_format: None,
                output: None,
            })
        );
    }

    #[test]
    fn test_parse_args_from_and_out_flags_are_captured() {
        let cmd = parse_args(&[
            s("in.xml"),
            s("xrechnung-invoice"),
            s("--from"),
            s("ubl-invoice"),
            s("--out"),
            s("out.xml"),
        ])
        .expect("valid");
        assert_eq!(
            cmd,
            Command::Transform(Args {
                input: s("in.xml"),
                target_format: s("xrechnung-invoice"),
                source_format: Some(s("ubl-invoice")),
                output: Some(s("out.xml")),
            })
        );
    }

    #[test]
    fn test_parse_args_flags_before_positionals() {
        let cmd = parse_args(&[
            s("--from"),
            s("ubl-invoice"),
            s("in.xml"),
            s("xrechnung-invoice"),
        ])
        .expect("valid");
        let Command::Transform(a) = cmd else {
            panic!("expected transform");
        };
        assert_eq!(a.source_format, Some(s("ubl-invoice")));
        assert_eq!(a.input, s("in.xml"));
    }

    #[test]
    fn test_parse_args_analyze_alone_has_no_source() {
        assert_eq!(
            parse_args(&[s("--analyze")]).expect("ok"),
            Command::Analyze(None)
        );
    }

    #[test]
    fn test_parse_args_analyze_with_positional_source() {
        assert_eq!(
            parse_args(&[s("--analyze"), s("ubl-invoice")]).expect("ok"),
            Command::Analyze(Some(s("ubl-invoice")))
        );
    }

    #[test]
    fn test_parse_args_analyze_with_from_source() {
        assert_eq!(
            parse_args(&[s("--analyze"), s("--from"), s("ubl-invoice")]).expect("ok"),
            Command::Analyze(Some(s("ubl-invoice")))
        );
    }

    #[test]
    fn test_parse_args_analyze_too_many_sources_is_usage_error() {
        let err = parse_args(&[s("--analyze"), s("a"), s("b")]).expect_err("should fail");
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn test_parse_args_keys_alone_has_no_format() {
        assert_eq!(parse_args(&[s("--keys")]).expect("ok"), Command::Keys(None));
    }

    #[test]
    fn test_parse_args_keys_with_positional_format() {
        assert_eq!(
            parse_args(&[s("--keys"), s("ubl-invoice")]).expect("ok"),
            Command::Keys(Some(s("ubl-invoice")))
        );
    }

    #[test]
    fn test_parse_args_keys_with_from_format() {
        assert_eq!(
            parse_args(&[s("--keys"), s("--from"), s("ubl-invoice")]).expect("ok"),
            Command::Keys(Some(s("ubl-invoice")))
        );
    }

    #[test]
    fn test_parse_args_keys_too_many_formats_is_usage_error() {
        let err = parse_args(&[s("--keys"), s("a"), s("b")]).expect_err("should fail");
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn test_parse_args_keys_and_analyze_combined_is_usage_error() {
        let err = parse_args(&[s("--keys"), s("--analyze")]).expect_err("should fail");
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn test_parse_args_no_args_is_help() {
        assert_eq!(parse_args(&[]).expect("ok"), Command::Help);
    }

    #[test]
    fn test_parse_args_help_flag() {
        assert_eq!(parse_args(&[s("--help")]).expect("ok"), Command::Help);
        assert_eq!(parse_args(&[s("-h")]).expect("ok"), Command::Help);
    }

    #[test]
    fn test_parse_args_list_flag() {
        assert_eq!(
            parse_args(&[s("--list")]).expect("ok"),
            Command::ListFormats
        );
    }

    #[test]
    fn test_parse_args_single_positional_is_usage_error() {
        let err = parse_args(&[s("in.xml")]).expect_err("should fail");
        assert!(matches!(err, CliError::Usage(_)));
        assert_eq!(err.exit_code(), 64);
    }

    #[test]
    fn test_parse_args_three_positionals_is_usage_error() {
        let err = parse_args(&[s("a"), s("b"), s("c")]).expect_err("should fail");
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn test_parse_args_unknown_flag_is_usage_error() {
        let err =
            parse_args(&[s("in.xml"), s("ubl-invoice"), s("--nope")]).expect_err("should fail");
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn test_parse_args_dangling_value_flag_is_usage_error() {
        let err =
            parse_args(&[s("in.xml"), s("ubl-invoice"), s("--from")]).expect_err("should fail");
        assert!(matches!(err, CliError::Usage(_)));
    }
}
