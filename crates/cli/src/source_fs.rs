//! Reads a materialized source directory into an in-memory `SourceSet` for the resolver.
//! Read-only. Symlinks are recorded, never followed, so the resolver decides containment.
//! A `.git` directory is repository metadata, not source content, and is skipped.

use dfm_model::resolve::SourceSet;
use dfm_model::{Code, Refusal};
use std::fs;
use std::io::Read;
use std::path::Path;

const MAX_FILE_BYTES: u64 = 1 << 20;
const MAX_ENTRIES: usize = 20_000;
const MAX_TOTAL_BYTES: u64 = 64 << 20;

pub fn load_source_set(root: &Path) -> Result<SourceSet, Refusal> {
    struct State {
        set: SourceSet,
        entries: usize,
        bytes: u64,
    }
    fn refuse(code: Code, what: String) -> Refusal {
        Refusal::new(code, what)
    }
    fn go(dir: &Path, rel: &str, st: &mut State) -> Result<(), Refusal> {
        let io = |e: std::io::Error, p: &str| {
            refuse(
                Code::IoFailed,
                format!("cannot read source {p}: {}", e.kind()),
            )
        };
        let mut names: Vec<_> = fs::read_dir(dir)
            .map_err(|e| io(e, rel))?
            .collect::<Result<_, _>>()
            .map_err(|e| io(e, rel))?;
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let name = e
                .file_name()
                .into_string()
                .map_err(|_| refuse(Code::SourceUntrusted, format!("non-UTF-8 name in {rel}")))?;
            if name.chars().any(char::is_control) {
                return Err(refuse(
                    Code::SourceUntrusted,
                    format!("control character in a name under {rel}"),
                ));
            }
            if rel.is_empty() && name == ".git" {
                continue;
            }
            let path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            st.entries += 1;
            if st.entries > MAX_ENTRIES {
                return Err(refuse(
                    Code::LimitExceeded,
                    "source tree has too many entries".into(),
                ));
            }
            let ty = e.file_type().map_err(|e| io(e, &path))?;
            if ty.is_symlink() {
                let target = fs::read_link(e.path()).map_err(|e| io(e, &path))?;
                let target = target.into_os_string().into_string().map_err(|_| {
                    refuse(
                        Code::SourceUntrusted,
                        format!("non-UTF-8 link target at {path}"),
                    )
                })?;
                st.set.insert_link(path, target);
            } else if ty.is_dir() {
                go(&e.path(), &path, st)?;
            } else if ty.is_file() {
                let mut buf = Vec::new();
                fs::File::open(e.path())
                    .and_then(|f| f.take(MAX_FILE_BYTES + 1).read_to_end(&mut buf))
                    .map_err(|e| io(e, &path))?;
                st.bytes += buf.len() as u64;
                if buf.len() as u64 > MAX_FILE_BYTES || st.bytes > MAX_TOTAL_BYTES {
                    return Err(refuse(
                        Code::LimitExceeded,
                        format!("source {path} exceeds the read limit"),
                    ));
                }
                st.set.insert_file(path, buf);
            } else {
                return Err(refuse(
                    Code::SourceUntrusted,
                    format!("special file at {path}"),
                ));
            }
        }
        Ok(())
    }
    let mut st = State {
        set: SourceSet::new(),
        entries: 0,
        bytes: 0,
    };
    go(root, "", &mut st)?;
    Ok(st.set)
}
