use std::any::Any;
use std::panic;
use std::process::ExitCode;

fn panic_message(payload: &(dyn Any + Send)) -> Option<&str> {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
}

fn stdout_error(payload: &(dyn Any + Send)) -> Option<bool> {
    let message = panic_message(payload)?;
    message.contains("failed printing to stdout").then(|| {
        message.contains("Broken pipe")
            || message.contains("os error 109")
            || message.contains("os error 232")
    })
}

fn main() -> ExitCode {
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if stdout_error(info.payload()).is_none() {
            original_hook(info);
        }
    }));
    match panic::catch_unwind(no_defaults::main) {
        Ok(status) => status,
        Err(payload) => match stdout_error(payload.as_ref()) {
            Some(true) => ExitCode::SUCCESS,
            Some(false) => ExitCode::from(2),
            None => panic::resume_unwind(payload),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::stdout_error;

    #[test]
    fn platform_closed_pipe_errors_are_normal_termination() {
        for message in [
            "failed printing to stdout: Broken pipe (os error 32)",
            "failed printing to stdout: The pipe has been ended. (os error 109)",
            "failed printing to stdout: The pipe is being closed. (os error 232)",
        ] {
            assert_eq!(stdout_error(&message.to_owned()), Some(true), "{message}");
        }
    }

    #[test]
    fn other_stdout_errors_remain_operational_failures() {
        let message = "failed printing to stdout: No space left on device (os error 28)";
        assert_eq!(stdout_error(&message.to_owned()), Some(false));
    }
}
