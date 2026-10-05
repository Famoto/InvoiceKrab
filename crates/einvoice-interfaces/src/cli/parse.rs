//! Argument parsing: raw argv → [`Command`].
//!
//! Pure and IO-free, so the whole flag grammar is unit-tested without touching
//! the process environment.

use super::{AnalyzeArgs, Args, CliError, Command};

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
    let mut target_format: Option<String> = None;
    let mut output: Option<String> = None;
    let mut analyze = false;
    let mut deny_lossy = false;
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
            "--deny-lossy" => {
                deny_lossy = true;
                i += 1;
            }
            "--from" | "--to" | "--out" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| CliError::Usage(format!("`{arg}` requires a value")))?;
                match arg.as_str() {
                    "--from" => source_format = Some(value.clone()),
                    "--to" => target_format = Some(value.clone()),
                    _ => output = Some(value.clone()),
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

    // `--analyze` and `--keys` are mode switches: the optional format(s) come
    // from `--from` / `--to` or positionals, so e.g. `--keys`, `--keys
    // ubl-invoice`, `--keys --from ubl-invoice`, `--analyze ubl-invoice
    // xrechnung-invoice` and `--analyze --from ubl-invoice --to xrechnung-invoice`
    // all work. They are mutually exclusive.
    if analyze && keys {
        return Err(CliError::Usage(
            "--analyze and --keys cannot be combined".into(),
        ));
    }
    if analyze {
        // Positionals fill only the slots `--from` / `--to` left open, so a
        // surplus one is an error rather than silently dropped.
        let open_slots =
            usize::from(source_format.is_none()) + usize::from(target_format.is_none());
        if positionals.len() > open_slots {
            return Err(CliError::Usage(
                "--analyze takes at most a source and a target format (positionally or via --from/--to)"
                    .into(),
            ));
        }
        let mut positionals = positionals.into_iter();
        let source = source_format.or_else(|| positionals.next());
        let target = target_format.or_else(|| positionals.next());
        if target.is_some() && source.is_none() {
            return Err(CliError::Usage(
                "--analyze needs a source format (--from) to go with the target".into(),
            ));
        }
        return Ok(Command::Analyze(AnalyzeArgs {
            source,
            target,
            deny_lossy,
        }));
    }
    if deny_lossy {
        return Err(CliError::Usage(
            "--deny-lossy only applies to --analyze".into(),
        ));
    }
    if target_format.is_some() {
        return Err(CliError::Usage("--to only applies to --analyze".into()));
    }
    if keys {
        if positionals.len() > 1 {
            return Err(CliError::Usage("--keys takes at most one format".into()));
        }
        let format = source_format.or_else(|| positionals.first().cloned());
        return Ok(Command::Keys(format));
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

    fn analyze(source: Option<&str>, target: Option<&str>, deny_lossy: bool) -> Command {
        Command::Analyze(AnalyzeArgs {
            source: source.map(s),
            target: target.map(s),
            deny_lossy,
        })
    }

    #[test]
    fn test_parse_args_analyze_alone_has_no_source() {
        assert_eq!(
            parse_args(&[s("--analyze")]).expect("ok"),
            analyze(None, None, false)
        );
    }

    #[test]
    fn test_parse_args_analyze_with_positional_source() {
        assert_eq!(
            parse_args(&[s("--analyze"), s("ubl-invoice")]).expect("ok"),
            analyze(Some("ubl-invoice"), None, false)
        );
    }

    #[test]
    fn test_parse_args_analyze_with_from_source() {
        assert_eq!(
            parse_args(&[s("--analyze"), s("--from"), s("ubl-invoice")]).expect("ok"),
            analyze(Some("ubl-invoice"), None, false)
        );
    }

    #[test]
    fn test_parse_args_analyze_pair_positional_or_flags() {
        assert_eq!(
            parse_args(&[s("--analyze"), s("ubl-invoice"), s("xrechnung-invoice")]).expect("ok"),
            analyze(Some("ubl-invoice"), Some("xrechnung-invoice"), false)
        );
        assert_eq!(
            parse_args(&[
                s("--analyze"),
                s("--from"),
                s("ubl-invoice"),
                s("--to"),
                s("xrechnung-invoice"),
                s("--deny-lossy"),
            ])
            .expect("ok"),
            analyze(Some("ubl-invoice"), Some("xrechnung-invoice"), true)
        );
        assert_eq!(
            parse_args(&[s("--deny-lossy"), s("--analyze")]).expect("ok"),
            analyze(None, None, true)
        );
    }

    #[test]
    fn test_parse_args_analyze_too_many_formats_is_usage_error() {
        let err = parse_args(&[s("--analyze"), s("a"), s("b"), s("c")]).expect_err("should fail");
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn test_parse_args_analyze_positionals_beyond_the_open_slots_are_usage_errors() {
        // `--from` fills the source slot: two positionals leave one surplus.
        let err = parse_args(&[s("--analyze"), s("a"), s("b"), s("--from"), s("c")])
            .expect_err("should fail");
        assert!(matches!(err, CliError::Usage(_)));
        // Both flags given: any positional is surplus.
        let err = parse_args(&[
            s("--analyze"),
            s("--from"),
            s("a"),
            s("--to"),
            s("b"),
            s("c"),
        ])
        .expect_err("should fail");
        assert!(matches!(err, CliError::Usage(_)));
        // One flag plus one positional fills the other slot.
        assert_eq!(
            parse_args(&[s("--analyze"), s("b"), s("--from"), s("a")]).expect("ok"),
            analyze(Some("a"), Some("b"), false)
        );
    }

    #[test]
    fn test_parse_args_analyze_target_without_source_is_usage_error() {
        let err = parse_args(&[s("--analyze"), s("--to"), s("b")]).expect_err("should fail");
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn test_parse_args_deny_lossy_or_to_outside_analyze_is_usage_error() {
        let err = parse_args(&[s("in.xml"), s("ubl-invoice"), s("--deny-lossy")])
            .expect_err("should fail");
        assert!(matches!(err, CliError::Usage(_)));
        let err = parse_args(&[s("--keys"), s("--to"), s("ubl-invoice")]).expect_err("should fail");
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
