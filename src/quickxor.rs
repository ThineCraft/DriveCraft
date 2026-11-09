//! QuickXorHash, the content hash SharePoint and OneDrive for work report for files.
//!
//! The hash is a 160-bit circular bit vector. Each input byte is XORed in at a bit
//! offset that advances by 11 for every byte, wrapping at 160. The total length is
//! XORed into the last 8 bytes at the end.

use base64::Engine as _;
use std::io::Read;
use std::path::Path;

const WIDTH_BITS: usize = 160;
const SHIFT: usize = 11;

#[derive(Clone, Default)]
pub struct QuickXor {
    data: [u8; 20],
    len: u64,
    pos: usize,
}

impl QuickXor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        let mut pos = self.pos;
        for &b in bytes {
            let byte = pos / 8;
            let off = pos % 8;
            let v = (b as u16) << off;
            self.data[byte] ^= v as u8;
            if off > 0 {
                self.data[(byte + 1) % 20] ^= (v >> 8) as u8;
            }
            pos += SHIFT;
            if pos >= WIDTH_BITS {
                pos -= WIDTH_BITS;
            }
        }
        self.pos = pos;
        self.len += bytes.len() as u64;
    }

    pub fn finalize(&self) -> [u8; 20] {
        let mut out = self.data;
        for (i, b) in self.len.to_le_bytes().iter().enumerate() {
            out[12 + i] ^= b;
        }
        out
    }

    /// The hash in the base64 form Microsoft Graph reports.
    pub fn finalize_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.finalize())
    }
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut h = QuickXor::new();
    h.update(bytes);
    h.finalize_base64()
}

/// Hash a file on disk. Returns its size and base64 hash.
pub fn hash_file(path: &Path) -> std::io::Result<(u64, String)> {
    let mut f = std::fs::File::open(path)?;
    let mut h = QuickXor::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok((h.len, h.finalize_base64()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vectors from rclone's quickxorhash test suite (base64 input, base64 hash).
    const VECTORS: &[(&str, &str)] = &[
        ("", "AAAAAAAAAAAAAAAAAAAAAAAAAAA="),
        ("Sg==", "SgAAAAAAAAAAAAAAAQAAAAAAAAA="),
        ("tbQ=", "taAFAAAAAAAAAAAAAgAAAAAAAAA="),
        ("jRRDVA==", "jaDAEKgAAAAAAAAABAAAAAAAAAA="),
        ("J9iPGCbfZSTNyw==", "J8DGIzBggm+UgQTNUgYAAAAAAAA="),
        ("gQaybEqS/4UlDc8e4IJm", "gDCALNigBEn8oxAlZ8AzPAAOQZg="),
        ("2fuxhBJXtpWFe8dOfdGeHw==", "O9tHLAghgSvYohKFyMMxnNCHaHg="),
        ("T6LYJIfDh81JrAK309H2JMJTXis=", "zBTHrspn3mEcohlJdIUAbjGNaNg="),
        ("DWAAX5/CIfrmErgZa8ot6ZraeSbu", "LR2Z0PjuRYGKQB/mhQAuMrAGZbQ="),
        (
            "NbYXsp5/K6mR+NmHwExjvWeWDJFnXTKWVlzYHoesp2E=",
            "wjuAuWDiq04qDt1R8hHWDDcwVoQ=",
        ),
        (
            "0n7nl3YJtipy6yeUbVPWtc2h45WbF9u8hTz5tNwj3dZZwfXWkk+GN3g=",
            "YJLNK7JR64j9aODWfqDvEe/u6NU=",
        ),
    ];

    #[test]
    fn matches_reference_vectors() {
        for (input, want) in VECTORS {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(input)
                .unwrap();
            assert_eq!(&hash_bytes(&bytes), want, "input {input}");
        }
    }

    #[test]
    fn chunked_update_matches_single_update() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 31 % 251) as u8).collect();
        let mut h = QuickXor::new();
        for chunk in data.chunks(7) {
            h.update(chunk);
        }
        assert_eq!(h.finalize_base64(), hash_bytes(&data));
    }
}
