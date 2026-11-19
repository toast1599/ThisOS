//! The ramdisk: a ustar archive of the user programs, which `thisos-ustar`
//! reads in the kernel and in init.

use std::io::Write;

use anyhow::{Context, Result};
use tar::{Builder, EntryType, Header};

/// The packages of the programs the ramdisk carries. Each is filed under
/// its name without the `thisos-` prefix.
pub const PROGRAMS: [&str; 6] = [
    "thisos-init",
    "thisos-hello",
    "thisos-fault",
    "thisos-ping",
    "thisos-pong",
    "thisos-checks",
];

/// The name `package`'s program has in the ramdisk.
pub fn name(package: &str) -> &str {
    package.strip_prefix("thisos-").unwrap_or(package)
}

/// Writes `files`, each a name and its bytes, to `out` as a ustar archive
/// with fixed owners, modes and times, so that the same files make the
/// same archive.
pub fn write(files: &[(&str, &[u8])], out: impl Write) -> Result<()> {
    let mut builder = Builder::new(out);
    for &(name, bytes) in files {
        let mut header = Header::new_ustar();
        header.set_entry_type(EntryType::Regular);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(u64::try_from(bytes.len()).context("a file's size")?);
        builder
            .append_data(&mut header, name, bytes)
            .with_context(|| format!("adding {name} to the ramdisk"))?;
    }
    builder
        .into_inner()
        .context("ending the ramdisk")?
        .flush()
        .context("writing the ramdisk")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kernels_reader_finds_what_was_written() {
        let long = vec![0xa5; 1000];
        let files: [(&str, &[u8]); 3] =
            [("init", b"\x7fELF init"), ("empty", b""), ("long", &long)];
        let mut archive = Vec::new();
        write(&files, &mut archive).unwrap();
        for (name, bytes) in files {
            assert_eq!(
                thisos_ustar::find(&archive, name),
                Ok(Some(bytes)),
                "{name}"
            );
        }
        assert_eq!(thisos_ustar::find(&archive, "missing"), Ok(None));
    }
}
