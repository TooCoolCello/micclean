//! The app icon: a white mic on a disc, green while the gate is open and grey when closed.
//! Drawn in code at any size, for the window, the tray and the .ico embedded in the exe.

pub const SIZE: u32 = 32;
const SUPER: u32 = 4;

/// RGBA pixels, n x n. Drawn on a 32-unit grid and supersampled so every size has smooth edges.
pub fn rgba(open: bool, n: u32) -> Vec<u8> {
    let bg = if open { [70.0, 190.0, 110.0] } else { [120.0, 120.0, 130.0] };
    let mut px = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let (mut disc, mut white) = (0u32, 0u32);
            for sy in 0..SUPER {
                for sx in 0..SUPER {
                    let u = (x as f32 + (sx as f32 + 0.5) / SUPER as f32) * 32.0 / n as f32 - 16.0;
                    let v = (y as f32 + (sy as f32 + 0.5) / SUPER as f32) * 32.0 / n as f32;
                    let ring = u.hypot(v - 16.0);
                    if ring > 15.5 {
                        continue;
                    }
                    disc += 1;
                    let capsule = u.hypot(v - v.clamp(11.0, 16.0)) < 4.5;
                    let holder = v >= 16.0 && (6.2..=8.0).contains(&ring);
                    let stem = u.abs() < 1.2 && (22.0..24.5).contains(&v);
                    let base = u.abs() < 5.5 && (24.0..25.8).contains(&v);
                    white += (capsule || holder || stem || base) as u32;
                }
            }
            let t = if disc == 0 { 0.0 } else { white as f32 / disc as f32 };
            let c = |b: f32| (b + (255.0 - b) * t).round() as u8;
            px.extend_from_slice(&[c(bg[0]), c(bg[1]), c(bg[2]), (disc * 255 / (SUPER * SUPER)) as u8]);
        }
    }
    px
}

#[cfg(test)]
const ICO_SIZES: [u32; 6] = [16, 24, 32, 48, 64, 128];

/// A .ico with one uncompressed 32-bit image per size (no PNG encoder needed below 256 px).
#[cfg(test)]
fn ico(sizes: &[u32]) -> Vec<u8> {
    let mut head = vec![0, 0, 1, 0];
    head.extend((sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    let mut images = Vec::new();
    for &n in sizes {
        let mut img = Vec::new();
        img.extend(40u32.to_le_bytes()); // BITMAPINFOHEADER
        img.extend(n.to_le_bytes());
        img.extend((2 * n).to_le_bytes()); // height counts the AND mask too
        img.extend(1u16.to_le_bytes());
        img.extend(32u16.to_le_bytes());
        img.extend([0u8; 24]); // no compression, size, resolution or palette
        for row in rgba(true, n).chunks(n as usize * 4).rev() {
            for p in row.chunks(4) {
                img.extend([p[2], p[1], p[0], p[3]]); // BGRA, bottom row first
            }
        }
        img.resize(img.len() + (n.div_ceil(32) * 4 * n) as usize, 0); // AND mask: alpha does the work
        head.extend([n as u8, n as u8, 0, 0]);
        head.extend(1u16.to_le_bytes());
        head.extend(32u16.to_le_bytes());
        head.extend((img.len() as u32).to_le_bytes());
        head.extend(offset.to_le_bytes());
        offset += img.len() as u32;
        images.push(img);
    }
    head.extend(images.concat());
    head
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_is_white_on_a_coloured_disc() {
        let px = rgba(true, 32);
        let at = |x: usize, y: usize| &px[(y * 32 + x) * 4..][..4];
        assert_eq!(at(0, 0)[3], 0, "corner should be transparent");
        assert_eq!(at(4, 16), &[70, 190, 110, 255], "disc colour");
        assert_eq!(at(16, 12), &[255, 255, 255, 255], "mic capsule");
    }

    /// build.rs embeds assets/micclean.ico. WRITE_ICO=1 cargo test embedded_ico regenerates it.
    #[test]
    fn embedded_ico_is_current() {
        let want = ico(&ICO_SIZES);
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/micclean.ico");
        if std::env::var_os("WRITE_ICO").is_some() {
            std::fs::create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/assets")).unwrap();
            std::fs::write(path, &want).unwrap();
        }
        assert!(std::fs::read(path).is_ok_and(|have| have == want), "run with WRITE_ICO=1 to regenerate {path}");
    }
}
