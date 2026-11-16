//! The `duckdb-extension/v1` package adapter (docs/packages.md, "DuckDB extensions"): one
//! extension from DuckDB's own extension repository, which is the tool's own owner and needs no
//! grant. `duckdb` is a mise tool, so every call runs `mise exec -- duckdb`, as the `go` updater
//! runs `go`, with mise's auto-install off: the probe never installs DuckDB. The probe reads
//! `duckdb_extensions()` in an in-memory database; the install runs `INSTALL <name>`.

use super::package::{PROBE_TIMEOUT, change, ended, request};
use dfm_engine::{End, Runner};
use dfm_model::display::visible;
use dfm_model::plan::PackageState;
use dfm_model::resolve::Value;
use dfm_model::{Code, Refusal};

pub(crate) const ADAPTER: &str = "duckdb-extension/v1";

/// DuckDB changes its behaviour when it sees an agent's variables (`AI_AGENT`, `CLAUDECODE`); the
/// runner clears them, and `DUCKDB_AGENT_MODE=0` turns the mode off as the dotfiles script did.
/// mise must not install a missing DuckDB.
fn env() -> Vec<(String, String)> {
    [
        ("DUCKDB_AGENT_MODE", "0"),
        ("MISE_EXEC_AUTO_INSTALL", "false"),
        ("MISE_NOT_FOUND_AUTO_INSTALL", "false"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// The declared extension name: ASCII letters, digits and `_`, at most 64 characters. The name
/// goes into a SQL statement, so anything else is `E_MANIFEST_INVALID`.
pub(crate) fn name(value: &Value) -> Result<&str, Refusal> {
    let n = value.get("name").and_then(Value::as_str).ok_or_else(|| {
        Refusal::new(
            Code::ManifestInvalid,
            "a duckdb-extension/v1 declaration needs name",
        )
    })?;
    if n.is_empty() || n.len() > 64 || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(Refusal::new(
            Code::ManifestInvalid,
            format!(
                "duckdb extension name {} must be ASCII letters, digits or '_', at most 64 characters",
                visible(n)
            ),
        ));
    }
    Ok(n)
}

/// The arguments of `mise exec -- duckdb` with an empty init file and one SQL statement.
fn args(sql: &str) -> Vec<String> {
    [
        "exec",
        "--",
        "duckdb",
        "-init",
        "/dev/null",
        "-noheader",
        "-csv",
        "-c",
        sql,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Whether the extension is installed: one `v:<version>` line is `Installed`, no line is
/// `Missing`. A missing `mise` or `duckdb`, or any other answer, is `Unknown` with the tool
/// named, so only this resource is blocked.
pub(crate) fn probe(runner: &Runner, value: &Value) -> Result<PackageState, Refusal> {
    let n = name(value)?;
    let a = args(&format!(
        "SELECT 'v:' || coalesce(extension_version, '') FROM duckdb_extensions() WHERE extension_name = '{n}' AND installed;"
    ));
    let out = match runner.run(&request("mise", &a, &env(), PROBE_TIMEOUT)) {
        Ok(o) => o,
        Err(e) => return Ok(PackageState::Unknown(e.detail)),
    };
    if let Some(why) = ended("mise exec -- duckdb", &out) {
        return Ok(PackageState::Unknown(why));
    }
    if let End::Exited(c) = out.end
        && c != 0
    {
        // mise's own message when the program is not installed; it holds no provider secret.
        if String::from_utf8_lossy(&out.stderr).contains("couldn't exec process") {
            return Ok(PackageState::Unknown(
                "duckdb is not installed: mise exec could not start it".into(),
            ));
        }
        return Ok(PackageState::Unknown(format!(
            "mise exec -- duckdb exited {c}; its {} bytes of standard error are withheld",
            out.stderr.len()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    Ok(match lines.as_slice() {
        [] => PackageState::Missing(Vec::new()),
        [l] if l.starts_with("v:") => PackageState::Installed(match l[2..].trim() {
            "" => "installed".into(),
            v => v.into(),
        }),
        _ => PackageState::Unknown(format!(
            "duckdb_extensions() for {n} is not the one line the adapter reads"
        )),
    })
}

/// Runs `INSTALL <name>` from DuckDB's own repository. The caller probes again afterwards.
pub(crate) fn install(runner: &Runner, value: &Value) -> Result<(), Refusal> {
    let n = name(value)?;
    let sql = format!("INSTALL {n};");
    change(
        runner,
        "mise",
        &args(&sql),
        &env(),
        &format!("duckdb {sql}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(n: &str) -> Value {
        Value::Table(toml::from_str(&format!("name = {n:?}")).unwrap())
    }

    #[test]
    fn a_plain_name_is_accepted_and_anything_else_is_a_manifest_error() {
        for n in ["spatial", "avro", "h3_ext", "Json2"] {
            assert_eq!(name(&value(n)).unwrap(), n);
        }
        let long = "x".repeat(65);
        for n in ["", "a b", "x';--", "a-b", "a.b", "ünï", long.as_str()] {
            let e = name(&value(n)).unwrap_err();
            assert_eq!(e.code, Code::ManifestInvalid, "{n}: {e}");
        }
        let e = name(&Value::Table(Default::default())).unwrap_err();
        assert_eq!(e.code, Code::ManifestInvalid);
    }
}
