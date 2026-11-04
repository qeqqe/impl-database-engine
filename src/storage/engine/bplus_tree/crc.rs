const POLY: u32 = 0x82F6_3B78;

static TABLE: [[u32; 256]; 8] = build_tables();

const fn build_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        tables[0][i] = crc;
        i += 1;
    }
    let mut t = 1;
    while t < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = tables[t - 1][i];
            tables[t][i] = (prev >> 8) ^ tables[0][(prev & 0xFF) as usize];
            i += 1;
        }
        t += 1;
    }
    tables
}

#[derive(Clone, Copy)]
pub(crate) struct Crc32c(u32);

impl Crc32c {
    pub fn new() -> Self {
        Crc32c(!0)
    }

    pub fn update(self, bytes: &[u8]) -> Self {
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("sse4.2") {
            return unsafe { self.update_sse42(bytes) };
        }
        self.update_table(bytes)
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "sse4.2")]
    unsafe fn update_sse42(mut self, bytes: &[u8]) -> Self {
        use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
        let mut crc = self.0 as u64;
        let (chunks, remainder) = bytes.as_chunks::<8>();
        for chunk in chunks {
            crc = _mm_crc32_u64(crc, u64::from_le_bytes(*chunk));
        }
        let mut crc = crc as u32;
        for &byte in remainder {
            crc = _mm_crc32_u8(crc, byte);
        }
        self.0 = crc;
        self
    }

    fn update_table(mut self, bytes: &[u8]) -> Self {
        let mut crc = self.0;
        let (chunks, remainder) = bytes.as_chunks::<8>();
        for chunk in chunks {
            let lo = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) ^ crc;
            let hi = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            crc = TABLE[7][(lo & 0xFF) as usize]
                ^ TABLE[6][((lo >> 8) & 0xFF) as usize]
                ^ TABLE[5][((lo >> 16) & 0xFF) as usize]
                ^ TABLE[4][(lo >> 24) as usize]
                ^ TABLE[3][(hi & 0xFF) as usize]
                ^ TABLE[2][((hi >> 8) & 0xFF) as usize]
                ^ TABLE[1][((hi >> 16) & 0xFF) as usize]
                ^ TABLE[0][(hi >> 24) as usize];
        }
        for &byte in remainder {
            crc = (crc >> 8) ^ TABLE[0][((crc ^ byte as u32) & 0xFF) as usize];
        }
        self.0 = crc;
        self
    }

    pub fn finish(self) -> u32 {
        !self.0
    }
}

pub(crate) fn crc32c(bytes: &[u8]) -> u32 {
    Crc32c::new().update(bytes).finish()
}

pub(crate) fn page_checksum(page: &[u8], at: usize) -> u32 {
    Crc32c::new()
        .update(&page[..at])
        .update(&[0u8; 4])
        .update(&page[at + 4..])
        .finish()
}

pub(crate) fn seal_page(page: &mut [u8], at: usize) {
    let crc = page_checksum(page, at);
    page[at..at + 4].copy_from_slice(&crc.to_le_bytes());
}

pub(crate) fn page_is_intact(page: &[u8], at: usize) -> bool {
    let stored = u32::from_le_bytes([page[at], page[at + 1], page[at + 2], page[at + 3]]);
    stored == page_checksum(page, at) || page.iter().all(|&b| b == 0)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
        let ascending: Vec<u8> = (0..32).collect();
        assert_eq!(crc32c(&ascending), 0x46DD_794E);
    }

    #[test]
    fn sealed_page_detects_any_single_bit_flip() {
        let mut page: Vec<u8> = (0..4096u32).map(|i| (i * 7 % 253) as u8).collect();
        seal_page(&mut page, 22);
        assert!(page_is_intact(&page, 22));
        for at in (0..page.len()).step_by(97) {
            for bit in 0..8 {
                page[at] ^= 1 << bit;
                assert!(!page_is_intact(&page, 22), "flip at {at} bit {bit}");
                page[at] ^= 1 << bit;
            }
        }
        assert!(page_is_intact(&vec![0u8; 4096], 22));
    }

    #[test]
    fn hardware_and_table_agree() {
        let data: Vec<u8> = (0..9000u32).map(|i| (i * 131 % 257) as u8).collect();
        for len in [0, 1, 7, 8, 9, 63, 64, 65, 4096, 8192, 8999, 9000] {
            for start in [0, 1, 3, 8] {
                let part = &data[start.min(len)..len];
                assert_eq!(
                    Crc32c::new().update(part).finish(),
                    Crc32c::new().update_table(part).finish(),
                    "len {len} start {start}"
                );
            }
        }
    }

    #[test]
    fn incremental_matches_one_shot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 31 % 251) as u8).collect();
        for split in [0, 1, 7, 8, 9, 500, 999, 1000] {
            let (a, b) = data.split_at(split);
            assert_eq!(Crc32c::new().update(a).update(b).finish(), crc32c(&data));
        }
    }
}
