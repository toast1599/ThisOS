//! A reader for the ACPI tables that list a machine's CPUs: the RSDP, the
//! XSDT it points to, and the MADT. Each table arrives as a byte slice that
//! the caller read from physical memory, sized by [`table_size`] for the
//! XSDT and the MADT, and is checked whole when it is made, so walking it
//! afterwards can't fail.

#![no_std]
#![forbid(unsafe_code)]

/// The size of the RSDP from ACPI 2.0 on, the first with the XSDT.
pub const RSDP_SIZE: usize = 36;

/// The size of every other table's header, which [`table_size`] reads the
/// table's size from.
pub const HEADER_SIZE: usize = 36;

const RSDP_SIGNATURE: &[u8; 8] = b"RSD PTR ";
/// The ACPI 1.0 RSDP's size, which its own checksum covers.
const RSDP_V1_SIZE: usize = 20;
/// The MADT's header, then the local APIC's address and the flags.
const MADT_ENTRIES: usize = HEADER_SIZE + 8;

const LOCAL_APIC: u8 = 0;
const LOCAL_X2APIC: u8 = 9;
const ENABLED: u32 = 1 << 0;
const ONLINE_CAPABLE: u32 = 1 << 1;

/// What is wrong with a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The slice is shorter than the table's header or than its length.
    Truncated,
    /// The table's signature isn't the one expected.
    BadSignature,
    /// The table's bytes don't sum to zero.
    BadChecksum,
    /// The RSDP's revision, older than 2, which has no XSDT.
    Unsupported(u8),
    /// The table's entries are malformed at this offset into the table.
    Malformed(usize),
    /// Two MADT entries that may be started give this APIC ID.
    DuplicateId(u32),
}

fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    let word = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
}

fn le64(bytes: &[u8], at: usize) -> Option<u64> {
    let low = le32(bytes, at)?;
    let high = le32(bytes, at.checked_add(4)?)?;
    Some(u64::from(high) << 32 | u64::from(low))
}

fn length(bytes: &[u8], at: usize) -> Result<usize, Error> {
    let field = le32(bytes, at).ok_or(Error::Truncated)?;
    usize::try_from(field).map_err(|_| Error::Truncated)
}

fn sums_to_zero(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)) == 0
}

/// The XSDT's physical address, from the RSDP at the start of `rsdp`,
/// after checking its signature, both its checksums and its revision.
pub fn xsdt_address(rsdp: &[u8]) -> Result<u64, Error> {
    let v1 = rsdp.get(..RSDP_V1_SIZE).ok_or(Error::Truncated)?;
    if !v1.starts_with(RSDP_SIGNATURE) {
        return Err(Error::BadSignature);
    }
    if !sums_to_zero(v1) {
        return Err(Error::BadChecksum);
    }
    let revision = v1[15];
    if revision < 2 {
        return Err(Error::Unsupported(revision));
    }
    let size = length(rsdp, 20)?;
    if size < RSDP_SIZE {
        return Err(Error::Truncated);
    }
    let whole = rsdp.get(..size).ok_or(Error::Truncated)?;
    if !sums_to_zero(whole) {
        return Err(Error::BadChecksum);
    }
    le64(whole, 24).ok_or(Error::Truncated)
}

/// The table's size, from the first [`HEADER_SIZE`] bytes of it, for a
/// table that arrives as a physical address.
pub fn table_size(header: &[u8]) -> Result<usize, Error> {
    if header.len() < HEADER_SIZE {
        return Err(Error::Truncated);
    }
    let size = length(header, 4)?;
    if size < HEADER_SIZE {
        return Err(Error::Truncated);
    }
    Ok(size)
}

/// The table's signature, from its header.
pub fn signature(header: &[u8]) -> Option<[u8; 4]> {
    header.get(..4)?.try_into().ok()
}

/// The table at the start of `bytes`, after checking its signature, its
/// length and its checksum.
fn table(bytes: &[u8], expected: [u8; 4]) -> Result<&[u8], Error> {
    let size = table_size(bytes)?;
    let table = bytes.get(..size).ok_or(Error::Truncated)?;
    if signature(table) != Some(expected) {
        return Err(Error::BadSignature);
    }
    if !sums_to_zero(table) {
        return Err(Error::BadChecksum);
    }
    Ok(table)
}

/// A checked XSDT.
#[derive(Clone, Copy, Debug)]
pub struct Xsdt<'a> {
    entries: &'a [u8],
}

impl<'a> Xsdt<'a> {
    /// Checks the XSDT at the start of `bytes`.
    pub fn new(bytes: &'a [u8]) -> Result<Self, Error> {
        let entries = &table(bytes, *b"XSDT")?[HEADER_SIZE..];
        if entries.len() % 8 != 0 {
            return Err(Error::Malformed(HEADER_SIZE + entries.len() / 8 * 8));
        }
        Ok(Self { entries })
    }

    /// The physical address of every table the XSDT lists.
    pub fn tables(&self) -> impl Iterator<Item = u64> + 'a {
        let (entries, _) = self.entries.as_chunks::<8>();
        entries.iter().map(|&entry| u64::from_le_bytes(entry))
    }
}

/// Whether the OS may start a CPU, from its MADT entry's flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Ready to start.
    Enabled,
    /// Off, but may be started; firmware means it for hot-adding.
    OnlineCapable,
    /// Not to be started.
    Disabled,
}

/// A CPU as an MADT entry lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cpu {
    /// Its APIC ID: 8 bits from a Local APIC entry, 32 from a Local x2APIC
    /// one.
    pub apic_id: u32,
    /// The ACPI processor UID that ties it to the namespace.
    pub uid: u32,
    /// Whether it may be started.
    pub state: State,
}

/// A checked MADT.
#[derive(Clone, Copy, Debug)]
pub struct Madt<'a> {
    entries: &'a [u8],
}

impl<'a> Madt<'a> {
    /// Checks the MADT at the start of `bytes`: its header, that every
    /// entry lies inside it and that each CPU entry is long enough, and that
    /// no two CPUs that may be started share an APIC ID.
    pub fn new(bytes: &'a [u8]) -> Result<Self, Error> {
        let table = table(bytes, *b"APIC")?;
        let entries = table.get(MADT_ENTRIES..).ok_or(Error::Truncated)?;
        let mut at = 0;
        while at < entries.len() {
            let offset = MADT_ENTRIES + at;
            let Some(&[kind, len]) = entries.get(at..at + 2) else {
                return Err(Error::Malformed(offset));
            };
            let len = usize::from(len);
            let least = match kind {
                LOCAL_APIC => 8,
                LOCAL_X2APIC => 16,
                _ => 2,
            };
            if len < least || at + len > entries.len() {
                return Err(Error::Malformed(offset));
            }
            at += len;
        }
        let madt = Self { entries };
        let startable = |cpu: &Cpu| cpu.state != State::Disabled;
        for (i, cpu) in madt.cpus().filter(startable).enumerate() {
            if madt
                .cpus()
                .filter(startable)
                .skip(i + 1)
                .any(|c| c.apic_id == cpu.apic_id)
            {
                return Err(Error::DuplicateId(cpu.apic_id));
            }
        }
        Ok(madt)
    }

    /// Every CPU the Local APIC and Local x2APIC entries list, in the
    /// table's order.
    pub fn cpus(&self) -> impl Iterator<Item = Cpu> + 'a {
        let mut rest = self.entries;
        core::iter::from_fn(move || {
            loop {
                let (&[kind, len], _) = rest.split_first_chunk()?;
                let (entry, after) = rest.split_at_checked(usize::from(len))?;
                rest = after;
                let (apic_id, uid, flags) = match kind {
                    LOCAL_APIC => (u32::from(entry[3]), u32::from(entry[2]), le32(entry, 4)?),
                    LOCAL_X2APIC => (le32(entry, 4)?, le32(entry, 12)?, le32(entry, 8)?),
                    _ => continue,
                };
                let state = if flags & ENABLED != 0 {
                    State::Enabled
                } else if flags & ONLINE_CAPABLE != 0 {
                    State::OnlineCapable
                } else {
                    State::Disabled
                };
                return Some(Cpu {
                    apic_id,
                    uid,
                    state,
                });
            }
        })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use super::*;

    /// Sets the checksum byte at `at` so that `bytes` sums to zero.
    fn seal(bytes: &mut [u8], at: usize) {
        bytes[at] = 0;
        let sum = bytes.iter().fold(0u8, |s, &b| s.wrapping_add(b));
        bytes[at] = sum.wrapping_neg();
    }

    /// A table with `signature`, a header and `body`, its length and
    /// checksum filled in.
    fn table(signature: [u8; 4], body: &[u8]) -> Vec<u8> {
        let mut t = Vec::new();
        t.extend(signature);
        let len = u32::try_from(HEADER_SIZE + body.len()).unwrap();
        t.extend(len.to_le_bytes());
        t.resize(HEADER_SIZE, 0);
        t.extend(body);
        seal(&mut t, 9);
        t
    }

    fn rsdp(revision: u8, xsdt: u64) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend(RSDP_SIGNATURE);
        r.resize(15, 0);
        r.push(revision);
        r.extend(0u32.to_le_bytes());
        r.extend(u32::try_from(RSDP_SIZE).unwrap().to_le_bytes());
        r.extend(xsdt.to_le_bytes());
        r.resize(RSDP_SIZE, 0);
        seal(&mut r[..RSDP_V1_SIZE], 8);
        seal(&mut r, 32);
        r
    }

    fn local_apic(uid: u8, id: u8, flags: u32) -> Vec<u8> {
        let mut e = std::vec![LOCAL_APIC, 8, uid, id];
        e.extend(flags.to_le_bytes());
        e
    }

    fn local_x2apic(id: u32, flags: u32, uid: u32) -> Vec<u8> {
        let mut e = std::vec![LOCAL_X2APIC, 16, 0, 0];
        e.extend(id.to_le_bytes());
        e.extend(flags.to_le_bytes());
        e.extend(uid.to_le_bytes());
        e
    }

    fn madt(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut body = std::vec![0; MADT_ENTRIES - HEADER_SIZE];
        for e in entries {
            body.extend(e);
        }
        table(*b"APIC", &body)
    }

    #[test]
    fn rsdp_gives_the_xsdt() {
        assert_eq!(xsdt_address(&rsdp(2, 0x7fe1_4000)), Ok(0x7fe1_4000));
    }

    #[test]
    fn rsdp_checks_signature_checksums_and_revision() {
        let mut r = rsdp(2, 0x1000);
        r[0] = b'X';
        assert_eq!(xsdt_address(&r), Err(Error::BadSignature));

        let mut r = rsdp(2, 0x1000);
        r[RSDP_V1_SIZE - 1] ^= 1;
        assert_eq!(xsdt_address(&r), Err(Error::BadChecksum));

        // The extended checksum alone covers the XSDT's address.
        let mut r = rsdp(2, 0x1000);
        r[24] ^= 1;
        assert_eq!(xsdt_address(&r), Err(Error::BadChecksum));

        assert_eq!(xsdt_address(&rsdp(0, 0x1000)), Err(Error::Unsupported(0)));
        assert_eq!(xsdt_address(&rsdp(2, 0x1000)[..30]), Err(Error::Truncated));
    }

    #[test]
    fn xsdt_lists_its_tables() {
        let mut body = Vec::new();
        for addr in [0x1000u64, 0x1_2345_6000] {
            body.extend(addr.to_le_bytes());
        }
        let t = table(*b"XSDT", &body);
        let xsdt = Xsdt::new(&t).unwrap();
        assert_eq!(xsdt.tables().collect::<Vec<_>>(), [0x1000, 0x1_2345_6000]);
    }

    #[test]
    fn xsdt_rejects_a_partial_entry() {
        let t = table(*b"XSDT", &[0; 12]);
        assert_eq!(Xsdt::new(&t).err(), Some(Error::Malformed(HEADER_SIZE + 8)));
    }

    #[test]
    fn tables_check_signature_length_and_checksum() {
        let t = table(*b"FACP", &[]);
        assert_eq!(Xsdt::new(&t).err(), Some(Error::BadSignature));
        let mut t = table(*b"XSDT", &[0; 8]);
        t[HEADER_SIZE] = 1;
        assert_eq!(Xsdt::new(&t).err(), Some(Error::BadChecksum));
        let t = table(*b"XSDT", &[0; 8]);
        assert_eq!(Xsdt::new(&t[..40]).err(), Some(Error::Truncated));
        assert_eq!(table_size(&t[..20]), Err(Error::Truncated));
        assert_eq!(table_size(&t), Ok(44));
        assert_eq!(signature(&t), Some(*b"XSDT"));
    }

    #[test]
    fn madt_lists_both_kinds_of_cpu_and_skips_other_entries() {
        let io_apic = std::vec![1, 12, 0, 0, 0, 0, 0xc0, 0xfe, 0, 0, 0, 0];
        let t = madt(&[
            local_apic(0, 0, ENABLED),
            io_apic,
            local_apic(1, 2, ONLINE_CAPABLE),
            local_x2apic(0x100, ENABLED, 7),
            local_apic(2, 0xff, 0),
        ]);
        let cpus: Vec<_> = Madt::new(&t).unwrap().cpus().collect();
        let cpu = |apic_id, uid, state| Cpu {
            apic_id,
            uid,
            state,
        };
        assert_eq!(
            cpus,
            [
                cpu(0, 0, State::Enabled),
                cpu(2, 1, State::OnlineCapable),
                cpu(0x100, 7, State::Enabled),
                cpu(0xff, 2, State::Disabled),
            ]
        );
    }

    #[test]
    fn madt_rejects_entries_that_overrun_or_are_short() {
        let mut short = local_x2apic(1, ENABLED, 1);
        short[1] = 12;
        short.truncate(12);
        let t = madt(&[local_apic(0, 0, ENABLED), short]);
        assert_eq!(
            Madt::new(&t).err(),
            Some(Error::Malformed(MADT_ENTRIES + 8))
        );

        let mut long = local_apic(0, 0, ENABLED);
        long[1] = 9;
        let t = madt(&[long]);
        assert_eq!(Madt::new(&t).err(), Some(Error::Malformed(MADT_ENTRIES)));

        let t = madt(&[std::vec![LOCAL_APIC]]);
        assert_eq!(Madt::new(&t).err(), Some(Error::Malformed(MADT_ENTRIES)));

        let mut zero = local_apic(0, 0, ENABLED);
        zero[1] = 0;
        let t = madt(&[zero]);
        assert_eq!(Madt::new(&t).err(), Some(Error::Malformed(MADT_ENTRIES)));
    }

    #[test]
    fn madt_rejects_startable_duplicates_only() {
        let t = madt(&[
            local_apic(0, 3, ENABLED),
            local_x2apic(3, ONLINE_CAPABLE, 9),
        ]);
        assert_eq!(Madt::new(&t).err(), Some(Error::DuplicateId(3)));
        // Firmware pads the table with disabled entries that all say 0xff.
        let t = madt(&[local_apic(0, 0xff, 0), local_apic(1, 0xff, 0)]);
        assert!(Madt::new(&t).is_ok());
    }

    #[test]
    fn qemu_q35_lists_its_four_cpus() {
        let madt = Madt::new(include_bytes!("../data/qemu-q35-smp4.madt")).unwrap();
        let cpus: Vec<_> = madt.cpus().map(|c| (c.apic_id, c.uid, c.state)).collect();
        let enabled = |id| (id, id, State::Enabled);
        assert_eq!(cpus, [enabled(0), enabled(1), enabled(2), enabled(3)]);
    }

    #[test]
    fn the_83da_lists_18_cpus_and_30_placeholders() {
        let madt = Madt::new(include_bytes!("../data/83da.madt")).unwrap();
        let ids: Vec<_> = madt
            .cpus()
            .filter(|c| c.state == State::Enabled)
            .map(|c| c.apic_id)
            .collect();
        let expected = [
            16, 17, 24, 25, 32, 33, 40, 41, 0, 2, 4, 6, 8, 10, 12, 14, 64, 66,
        ];
        assert_eq!(ids, expected);
        let placeholders = madt
            .cpus()
            .filter(|c| c.state == State::Disabled && c.apic_id == u32::MAX)
            .count();
        assert_eq!(placeholders, 30);
    }
}
