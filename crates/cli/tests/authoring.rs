//! Runs docs/authoring.md. Each `console` block holds commands (`$ ` lines, `> ` continuation
//! lines) and the output that the doc shows after each command. The test runs every command in
//! order in one bash process from the repository root, with `DFM` set to the isolated binary
//! and `TMPDIR` and `HOME` set to a scratch directory, and compares stdout and stderr together
//! with the text in the doc. The scratch path that `mktemp -d` returns is printed as `$W`.

mod common;

use common::{BIN, TempDir, repo_root, text};
use std::fs;
use std::process::Command;

struct Step {
    command: String,
    expected: String,
}

fn steps(doc: &str) -> Vec<Step> {
    let mut out: Vec<Step> = Vec::new();
    // The indent of the opening fence, stripped from every line of the block.
    let mut indent: Option<String> = None;
    for raw in doc.lines() {
        let Some(pad) = &indent else {
            let t = raw.trim_start();
            if t == "```console" {
                indent = Some(raw[..raw.len() - t.len()].to_owned());
            }
            continue;
        };
        let line = raw.strip_prefix(pad.as_str()).unwrap_or(raw);
        if line == "```" {
            indent = None;
        } else if let Some(cmd) = line.strip_prefix("$ ") {
            out.push(Step {
                command: cmd.to_owned(),
                expected: String::new(),
            });
        } else {
            let step = out
                .last_mut()
                .expect("console output before the first command");
            match line.strip_prefix("> ").or((line == ">").then_some("")) {
                Some(more) if step.expected.is_empty() => {
                    step.command.push('\n');
                    step.command.push_str(more);
                }
                _ => {
                    step.expected.push_str(line);
                    step.expected.push('\n');
                }
            }
        }
    }
    out
}

#[test]
fn the_authoring_walkthrough_runs_as_written() {
    let doc = fs::read_to_string(repo_root().join("docs/authoring.md")).unwrap();
    let steps = steps(&doc);
    let scratch = TempDir::new("authoring");
    let out = scratch.path().join("out");
    let tmp = scratch.path().join("tmp");
    fs::create_dir_all(&out).unwrap();
    fs::create_dir_all(&tmp).unwrap();
    let mut script = String::new();
    for (i, s) in steps.iter().enumerate() {
        // Braces, not a subshell, so variables and functions carry to the next command.
        script.push_str(&format!(
            "{{\n{}\n}} >\"$OUT/{i}\" 2>&1\nprintf '%s' \"${{W-}}\" >\"$OUT/W\"\n",
            s.command
        ));
    }
    let o = Command::new("/bin/bash")
        .arg("-c")
        .arg(&script)
        .current_dir(repo_root())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", scratch.path())
        .env("TMPDIR", &tmp)
        .env("DFM", BIN)
        .env("OUT", &out)
        .output()
        .unwrap();
    assert!(o.status.success(), "bash failed: {}", text(&o.stderr));

    let w = fs::read_to_string(out.join("W")).unwrap();
    assert!(w.starts_with(tmp.to_str().unwrap()), "W={w:?}");
    let mut failures = String::new();
    for (i, s) in steps.iter().enumerate() {
        let actual = fs::read_to_string(out.join(i.to_string()))
            .unwrap()
            .replace(&w, "$W");
        if actual != s.expected {
            failures.push_str(&format!(
                "--- $ {}\n--- doc:\n{}--- actual:\n{actual}\n",
                s.command, s.expected
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "docs/authoring.md drifted:\n{failures}"
    );
    // The three mistakes the walkthrough must show, with their real error text.
    for code in [
        "error: E_SOURCE_ESCAPE",
        "error: E_OVERRIDE_FORBIDDEN",
        "error: E_MANIFEST_INVALID duplicate resource ID",
    ] {
        assert!(
            steps.iter().any(|s| s.expected.contains(code)),
            "the walkthrough shows no {code}"
        );
    }
}
