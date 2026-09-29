use std::collections::HashSet;

fn live_children(pid: u32) -> Vec<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|child| child.parse::<u32>().ok())
        .filter(|child| {
            std::fs::read_to_string(format!("/proc/{child}/status"))
                .is_ok_and(|status| !status.lines().any(|line| line.starts_with("State:\tZ")))
        })
        .collect()
}

pub fn busy_surface_ids(surface_ids: &HashSet<String>) -> HashSet<String> {
    if surface_ids.is_empty() {
        return HashSet::new();
    }

    let mut roots = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return HashSet::new();
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        let path = entry.path();
        let Ok(environ) = std::fs::read(path.join("environ")) else {
            continue;
        };
        let Some(surface_id) = environ
            .split(|byte| *byte == 0)
            .filter_map(|variable| variable.strip_prefix(b"GHOSTDECK_SURFACE_ID="))
            .filter_map(|value| std::str::from_utf8(value).ok())
            .find(|value| surface_ids.contains(*value))
        else {
            continue;
        };
        let Ok(status) = std::fs::read_to_string(path.join("status")) else {
            continue;
        };
        if status.lines().any(|line| line.starts_with("State:\tZ")) {
            continue;
        }
        let Some(parent) = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:\t"))
            .and_then(|value| value.trim().parse::<u32>().ok())
        else {
            continue;
        };
        if parent != std::process::id() {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(path.join("comm")) else {
            continue;
        };
        roots.push((surface_id.to_string(), pid, comm.trim().to_string()));
    }

    roots
        .into_iter()
        .filter_map(|(surface_id, pid, comm)| {
            let children = live_children(pid);
            let shell = crate::process_cwd::SHELL_NAMES.contains(&comm.as_str());
            let wrapper = comm == "sh"
                && std::fs::read(format!("/proc/{pid}/cmdline"))
                    .is_ok_and(|cmdline| cmdline.split(|byte| *byte == 0).nth(1) == Some(b"-c"));
            if wrapper && children.len() == 1 {
                let child = children[0];
                let child_shell = std::fs::read_to_string(format!("/proc/{child}/comm"))
                    .is_ok_and(|comm| crate::process_cwd::SHELL_NAMES.contains(&comm.trim()));
                if child_shell {
                    return (!live_children(child).is_empty()).then_some(surface_id);
                }
            }
            (!shell || !children.is_empty()).then_some(surface_id)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    #[test]
    fn detects_child_process_and_clears_after_it_exits() {
        let surface_id = format!("process-usage-test-{}", uuid::Uuid::new_v4());
        let targets = HashSet::from([surface_id.clone()]);
        let mut shell = Command::new("/bin/sh")
            .args(["-c", "/bin/bash"])
            .env("GHOSTDECK_SURFACE_ID", &surface_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("start shell");
        for _ in 0..100 {
            if !live_children(shell.id()).is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !live_children(shell.id()).is_empty(),
            "wrapper never started bash"
        );
        assert!(!busy_surface_ids(&targets).contains(&surface_id));

        writeln!(shell.stdin.as_mut().expect("shell stdin"), "sleep 2").expect("run child");
        let mut busy = false;
        for _ in 0..100 {
            busy = busy_surface_ids(&targets).contains(&surface_id);
            if busy {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(busy, "shell child was never detected");

        let mut idle_again = false;
        for _ in 0..250 {
            idle_again = !busy_surface_ids(&targets).contains(&surface_id);
            if idle_again {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        shell.kill().expect("stop shell");
        shell.wait().expect("reap shell");
        assert!(
            idle_again,
            "shell did not become idle after its child exited"
        );
    }
}
