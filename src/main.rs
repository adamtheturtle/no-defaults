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
    message
        .contains("failed printing to stdout")
        .then(|| message.contains("Broken pipe"))
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
