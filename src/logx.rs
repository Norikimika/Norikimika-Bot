use std::io::Write;

fn stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Append to <state_dir>/<name>.log and echo to stderr.
pub fn log_to(name: &str, msg: &str) {
    // the module name is part of the line: without it every module looks the same in the terminal
    let line = format!("[{}] {}: {}\n", stamp(), name, msg);
    eprintln!("{}", line.trim_end());
    let path = crate::config::state_file(&format!("{name}.log"));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! logln {
    ($modname:expr, $($arg:tt)*) => {
        $crate::logx::log_to($modname, &format!($($arg)*))
    };
}
