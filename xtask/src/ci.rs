//! Running locally everything CI runs.

use std::io::{self, IsTerminal};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use serde_json::Value;

use crate::build::{self, Arch, Kernel};
use crate::cmd::{cargo, exec, on_host, output, show};
use crate::{banned, test};

/// Runs the checks in CI's order and stops at the first that fails.
pub fn ci() -> Result<()> {
    // Checked first, so a missing tool fails before minutes of other checks.
    ensure!(
        cargo()
            .args(["deny", "--version"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success()),
        "cargo deny isn't installed; install it with `cargo install cargo-deny --locked`"
    );

    banned::check()?;
    exec(cargo().args(["fmt", "--all", "--check"]))?;
    exec(cargo().args([
        "fmt",
        "--manifest-path",
        "xtask/Cargo.toml",
        "--all",
        "--check",
    ]))?;

    // The aarch64 arch crate's assembly doesn't build for x86_64; it gets
    // its own passes below.
    clippy(cargo().args(["clippy", "--workspace", "--exclude", "thisos-arch-aarch64"]))?;
    // Not --workspace --all-targets: that builds thisos-console's tests,
    // which need std, for x86_64-unknown-none.
    clippy(cargo().args(["clippy", "-p", "thisos-kernel", "--tests"]))?;
    let aarch64 = Arch::Aarch64.target();
    clippy(cargo().args([
        "clippy",
        "-p",
        "thisos-kernel",
        "-p",
        "thisos-arch-aarch64",
        "--target",
        aarch64,
    ]))?;
    clippy(cargo().args([
        "clippy",
        "-p",
        "thisos-kernel",
        "--tests",
        "--target",
        aarch64,
    ]))?;
    let libs = test::libs()?;
    for manifest in &libs {
        clippy(on_host(cargo().arg("clippy").arg("--manifest-path").arg(manifest)).arg("--tests"))?;
    }
    clippy(
        on_host(cargo().args(["clippy", "--manifest-path", "xtask/Cargo.toml"]))
            .arg("--all-targets"),
    )?;
    exec(on_host(cargo().args([
        "test",
        "--manifest-path",
        "xtask/Cargo.toml",
    ])))?;

    test::test(Arch::X86_64)?;
    // cargo miri takes host-tuple as a target name and fails to build a
    // sysroot for it, so it gets the host's actual tuple.
    let host = output(Command::new("rustc").args(["--print", "host-tuple"]))?;
    for manifest in &libs {
        let mut miri = cargo();
        // The arch crate reaches its page tables, in tests a buffer, by
        // turning physical addresses into pointers, as the kernel does
        // through the physical-memory mapping. That needs exposed
        // provenance, which Miri allows only in permissive mode. There an
        // access must still land inside an allocation whose address was
        // exposed, though not necessarily the buffer. Strict mode would need
        // the tables to carry a pointer in place of the mapping's offset.
        if manifest.ends_with(test::ARCH_X86_64) {
            miri.env("MIRIFLAGS", "-Zmiri-permissive-provenance");
        }
        exec(
            miri.args(["miri", "test", "--target", host.trim(), "--manifest-path"])
                .arg(manifest),
        )?;
    }

    for manifest in ["Cargo.toml", "xtask/Cargo.toml"] {
        exec(cargo().args([
            "deny",
            "--manifest-path",
            manifest,
            "--config",
            "deny.toml",
            "check",
        ]))?;
    }

    build::build(Arch::X86_64, false, Kernel::Main)?;
    // Clippy stops before codegen, so only this assembles and links the
    // aarch64 kernel.
    build::build(Arch::Aarch64, false, Kernel::Main)?;
    Ok(())
}

/// Runs `cargo_clippy`, a cargo clippy command, with `-D warnings`, and fails
/// if it fails or reports any warning. `-D warnings` turns every lint into an
/// error, but not warnings that aren't lints, such as a `clippy.toml` path
/// that names nothing.
fn clippy(cargo_clippy: &mut Command) -> Result<()> {
    let format = if io::stderr().is_terminal() {
        "--message-format=json-diagnostic-rendered-ansi"
    } else {
        "--message-format=json"
    };
    let out = cargo_clippy
        .args([format, "--", "-D", "warnings"])
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("running {}", show(cargo_clippy)))?;
    let messages = String::from_utf8(out.stdout).context("cargo's JSON messages")?;
    let (rendered, warnings) = diagnostics(&messages)?;
    eprint!("{rendered}");
    ensure!(
        out.status.success(),
        "{} failed: {}",
        show(cargo_clippy),
        out.status
    );
    ensure!(
        warnings == 0,
        "{} passed but reported warnings that aren't lints, {warnings} in all",
        show(cargo_clippy)
    );
    Ok(())
}

/// The rendered diagnostics in `messages`, cargo's JSON messages, and how
/// many of them are warnings.
fn diagnostics(messages: &str) -> Result<(String, usize)> {
    let mut rendered = String::new();
    let mut warnings = 0;
    for line in messages.lines() {
        let message: Value =
            serde_json::from_str(line).with_context(|| format!("cargo message {line:?}"))?;
        if message["reason"] != "compiler-message" {
            continue;
        }
        let diagnostic = &message["message"];
        if diagnostic["level"] == "warning" {
            warnings += 1;
        }
        if let Some(text) = diagnostic["rendered"].as_str() {
            rendered.push_str(text);
        }
    }
    Ok((rendered, warnings))
}

#[cfg(test)]
mod tests {
    use super::diagnostics;

    /// Clippy's messages on nightly-2026-11-04 for `thisos-abi` with
    /// `-D warnings`, when `clippy.toml` named `core::mem::nonexistent_fn`.
    /// Paths shortened to `/src/thisos`.
    const BAD_PATH: &str = r#"{"reason":"compiler-message","package_id":"path+file:///src/thisos/abi#thisos-abi@0.0.0","manifest_path":"/src/thisos/abi/Cargo.toml","target":{"kind":["lib"],"crate_types":["lib"],"name":"thisos_abi","src_path":"/src/thisos/abi/src/lib.rs","edition":"2024","doc":true,"doctest":false,"test":false},"message":{"rendered":"warning: `core::mem::nonexistent_fn` does not refer to a reachable function\n  --> /src/thisos/clippy.toml:22:14\n   |\n22 |     { path = \"core::mem::nonexistent_fn\", reason = \"probe\" },\n   |              ^^^^^^^^^^^^^^^^^^^^^^^^^^^\n   |\n   = help: add `allow-invalid = true` to the entry to suppress this warning\n\n","$message_type":"diagnostic","children":[{"children":[],"code":null,"level":"help","message":"add `allow-invalid = true` to the entry to suppress this warning","rendered":null,"spans":[]}],"level":"warning","message":"`core::mem::nonexistent_fn` does not refer to a reachable function","spans":[{"byte_end":1889,"byte_start":1862,"column_end":41,"column_start":14,"expansion":null,"file_name":"/src/thisos/clippy.toml","is_primary":true,"label":null,"line_end":22,"line_start":22,"suggested_replacement":null,"suggestion_applicability":null,"text":[{"highlight_end":41,"highlight_start":14,"text":"    { path = \"core::mem::nonexistent_fn\", reason = \"probe\" },"}]}],"code":null}}
{"reason":"compiler-artifact","package_id":"path+file:///src/thisos/abi#thisos-abi@0.0.0","manifest_path":"/src/thisos/abi/Cargo.toml","target":{"kind":["lib"],"crate_types":["lib"],"name":"thisos_abi","src_path":"/src/thisos/abi/src/lib.rs","edition":"2024","doc":true,"doctest":false,"test":false},"profile":{"opt_level":"1","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":false},"features":[],"filenames":["/src/thisos/target/x86_64-unknown-none/debug/build/thisos-abi/e37af892a56a34a2/out/libthisos_abi-e37af892a56a34a2.rmeta"],"executable":null,"fresh":false}
{"reason":"build-finished","success":true}"#;

    #[test]
    fn counts_a_warning_that_is_not_a_lint() {
        let (rendered, warnings) = diagnostics(BAD_PATH).unwrap();
        assert_eq!(warnings, 1);
        assert!(rendered.starts_with(
            "warning: `core::mem::nonexistent_fn` does not refer to a reachable function\n"
        ));

        let clean: String = BAD_PATH
            .lines()
            .skip(1)
            .map(|line| format!("{line}\n"))
            .collect();
        assert_eq!(diagnostics(&clean).unwrap(), (String::new(), 0));
    }
}
