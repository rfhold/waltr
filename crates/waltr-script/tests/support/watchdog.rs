use std::{
    io::Read,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct Captured {
    pub success: bool,
    pub killed: bool,
    pub overflow: bool,
    pub output: Vec<u8>,
}

pub fn run(mut command: Command, timeout: Duration, cap: usize) -> Captured {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn child");
    let total = Arc::new(AtomicUsize::new(0));
    let drain = |mut pipe: Box<dyn Read + Send>| {
        let total = total.clone();
        thread::spawn(move || {
            let mut retained = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = pipe.read(&mut buffer).expect("read pipe");
                if count == 0 {
                    break;
                }
                let before = total.fetch_add(count, Ordering::Relaxed);
                let keep = count.min(cap.saturating_sub(before));
                retained.extend_from_slice(&buffer[..keep]);
            }
            retained
        })
    };
    let stdout = drain(Box::new(child.stdout.take().expect("stdout")));
    let stderr = drain(Box::new(child.stderr.take().expect("stderr")));
    let start = Instant::now();
    let (status, killed) = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break (status, false);
        }
        if start.elapsed() >= timeout {
            child.kill().expect("kill");
            break (child.wait().expect("reap"), true);
        }
        thread::sleep(Duration::from_millis(2));
    };
    let mut output = stdout.join().expect("stdout reader");
    output.extend(stderr.join().expect("stderr reader"));
    Captured {
        success: status.success(),
        killed,
        overflow: total.load(Ordering::Relaxed) > cap,
        output,
    }
}
