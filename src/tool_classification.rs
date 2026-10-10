use std::ffi::OsString;

pub(crate) fn classify(tool: &str, args: &[OsString]) -> Option<&'static str> {
    match tool {
        "go" => match go_verb(args)? {
            "build" | "install" | "run" => Some("build"),
            "test" => Some("test"),
            "vet" => Some("lint"),
            _ => None,
        },
        "cargo" => match cargo_verb(args)? {
            "build" | "check" | "install" => Some("build"),
            "test" | "bench" => Some("test"),
            "clippy" => Some("lint"),
            verb if verb.starts_with('-')
                && !matches!(verb, "--help" | "-h" | "--version" | "-V") =>
            {
                Some("build")
            }
            _ => None,
        },
        "golangci-lint" if args.first()?.to_str()? == "run" => Some("lint"),
        "mise" if args.first()?.to_str()? == "run" => match args.get(1)?.to_str()? {
            "build" => Some("build"),
            "test" => Some("test"),
            "lint" | "check" => Some("lint"),
            _ => None,
        },
        "make" => make_category(args),
        _ => None,
    }
}

fn go_verb(args: &[OsString]) -> Option<&str> {
    let first = args.first()?.to_str()?;
    let index = if first == "-C" {
        2
    } else {
        usize::from(first.starts_with("-C="))
    };
    args.get(index)?.to_str()
}

fn cargo_verb(args: &[OsString]) -> Option<&str> {
    let mut index = 0;
    if args.first()?.to_str()?.starts_with('+') {
        index += 1;
    }
    loop {
        let arg = args.get(index)?.to_str()?;
        if matches!(
            arg,
            "--offline" | "--locked" | "--frozen" | "--quiet" | "-q" | "--verbose" | "-v"
        ) || (arg.starts_with('-') && arg.len() > 1 && arg[1..].bytes().all(|byte| byte == b'v'))
            || arg.starts_with("--color=")
            || arg.starts_with("--config=")
            || (arg.starts_with("-Z") && arg.len() > 2)
        {
            index += 1;
        } else if matches!(arg, "--color" | "--config" | "-Z") {
            index += 2;
        } else {
            return Some(arg);
        }
    }
}

fn make_category(args: &[OsString]) -> Option<&'static str> {
    if args.len() == 1
        && args.iter().any(|arg| {
            arg.to_str()
                .is_some_and(|arg| matches!(arg, "--help" | "--version" | "-h" | "-v"))
        })
    {
        return None;
    }
    if args.iter().any(|arg| arg == "test") {
        Some("test")
    } else if args.iter().any(|arg| arg == "lint" || arg == "check") {
        Some("lint")
    } else {
        Some("build")
    }
}
