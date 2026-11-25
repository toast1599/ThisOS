//! The loader's memory map, in the HAL's terms.

use core::slice;

use bootloader_api::info::{self, MemoryRegionKind};
use thisos_hal::{MemoryKind, MemoryRegion};

/// The regions of the loader's memory map. The loader keeps them in a
/// Loader region, so whatever reclaims Loader regions has to be done with
/// the map first.
/// Usable regions come split around the warm-reset experiment's blocks,
/// which it reports as Firmware regions.
#[derive(Clone, Debug)]
pub struct MemoryMap(
    slice::Iter<'static, info::MemoryRegion>,
    Option<MemoryRegion>,
);

impl MemoryMap {
    pub(crate) fn new(regions: &'static [info::MemoryRegion]) -> Self {
        Self(regions.iter(), None)
    }
}

impl Iterator for MemoryMap {
    type Item = MemoryRegion;

    fn next(&mut self) -> Option<MemoryRegion> {
        let region = match self.1.take() {
            Some(rest) => rest,
            None => self.loader_region()?,
        };
        if region.kind != MemoryKind::Usable {
            return Some(region);
        }
        let Some(hole) = crate::warm::first_hole(region.start, region.end) else {
            return Some(region);
        };
        if hole > region.start {
            self.1 = Some(MemoryRegion {
                start: hole,
                ..region
            });
            return Some(MemoryRegion {
                end: hole,
                ..region
            });
        }
        let end = (hole + crate::warm::BLOCK).min(region.end);
        if end < region.end {
            self.1 = Some(MemoryRegion {
                start: end,
                ..region
            });
        }
        Some(MemoryRegion {
            start: hole,
            end,
            kind: MemoryKind::Firmware(crate::warm::HOLE_KIND),
        })
    }
}

impl MemoryMap {
    fn loader_region(&mut self) -> Option<MemoryRegion> {
        let region = self.0.next()?;
        let kind = match region.kind {
            MemoryRegionKind::Usable => MemoryKind::Usable,
            MemoryRegionKind::Bootloader => MemoryKind::Loader,
            // The loader boots through UEFI only, so a BIOS type never
            // comes up; it is a firmware type all the same.
            MemoryRegionKind::UnknownUefi(ty) | MemoryRegionKind::UnknownBios(ty) => {
                MemoryKind::Firmware(ty)
            }
            // MemoryRegionKind is non_exhaustive, and a kind added later
            // isn't free RAM unless this match says so.
            _ => MemoryKind::Firmware(u32::MAX),
        };
        Some(MemoryRegion {
            start: region.start,
            end: region.end,
            kind,
        })
    }
}
