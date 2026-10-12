//! The crate module doc names every command the dispatcher handles.

#[test]
fn the_module_doc_names_every_dispatched_command() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs")).unwrap();
    let doc: String = src
        .lines()
        .take_while(|l| l.starts_with("//!"))
        .collect::<Vec<_>>()
        .join(" ");
    for cmd in [
        "version",
        "help",
        "bind",
        "state",
        "explain",
        "plan",
        "doctor",
        "status",
        "apply",
        "self-check",
    ] {
        // `bind` is documented as "profile binding".
        let word = if cmd == "bind" { "binding" } else { cmd };
        assert!(doc.contains(word), "the module doc omits {cmd}: {doc}");
    }
}
