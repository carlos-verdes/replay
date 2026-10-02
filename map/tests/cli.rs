//! The binary end to end: write, re-run, check, and catch a stale map.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DOMAIN: &str = r#"
define_aggregate! {
    Light {
        namespace: "light",
        state: { on: bool },
        commands: { Switch },
        events: { Switched }
    }
}

impl Aggregate for Light {
    async fn handle(&self, command: Self::Command, _: &()) -> Result<Vec<Self::Event>, Error> {
        match command {
            LightCommand::Switch => Ok(vec![LightEvent::Switched]),
        }
    }
}
"#;

/// A fresh directory per test, so tests running in parallel never share files.
fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("replay-map-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/light.rs"), DOMAIN).unwrap();
    dir
}

fn replay_map(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cargo-replay-map"))
        .arg("replay-map")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

#[test]
fn the_map_is_written_once_and_rerunning_changes_nothing() {
    let dir = workspace("rerun");

    assert!(replay_map(&dir, &[]).status.success());
    let first = fs::read_to_string(dir.join("docs/domain-map.md")).unwrap();
    assert!(replay_map(&dir, &[]).status.success());

    assert_eq!(
        fs::read_to_string(dir.join("docs/domain-map.md")).unwrap(),
        first
    );
    assert!(first.contains("cmd-Light-Switch --> evt-Light-Switched"));
}

#[test]
fn check_passes_on_a_current_map_and_fails_once_the_source_moves_on() {
    let dir = workspace("check");
    assert!(replay_map(&dir, &["--src", "src", "--out", "map.md"])
        .status
        .success());
    assert!(
        replay_map(&dir, &["--src", "src", "--out", "map.md", "--check"])
            .status
            .success()
    );

    let grown = DOMAIN.replace("events: { Switched }", "events: { Switched, Dimmed }");
    fs::write(dir.join("src/light.rs"), grown).unwrap();

    assert!(
        !replay_map(&dir, &["--src", "src", "--out", "map.md", "--check"])
            .status
            .success()
    );
}

#[test]
fn an_unreadable_arm_fails_the_run_with_its_file_and_line() {
    let dir = workspace("unreadable");
    let helper = DOMAIN.replace("Ok(vec![LightEvent::Switched])", "self.switch()");
    fs::write(dir.join("src/light.rs"), &helper).unwrap();

    let output = replay_map(&dir, &[]);

    assert!(!output.status.success());
    let line = helper
        .lines()
        .position(|l| l.contains("self.switch()"))
        .unwrap()
        + 1;
    assert!(String::from_utf8_lossy(&output.stderr).contains(&format!("light.rs:{line}:")));
}
