//! Comparison sheets: crops, magnification, labelled panels side by side.
//!
//! Pure pixel pushing on [`Image`]s; writing files is left to the caller.

use crate::raster::Image;

/// Glyphs of a 5x7 bitmap font: one row per byte, bit 4 is the left column.
/// Lower-case letters are drawn as capitals; unknown characters as blanks.
fn glyph(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        ',' => [0x00, 0x00, 0x00, 0x00, 0x0C, 0x04, 0x08],
        ':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        '+' => [0x00, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x00],
        '=' => [0x00, 0x00, 0x1F, 0x00, 0x1F, 0x00, 0x00],
        '/' => [0x01, 0x02, 0x02, 0x04, 0x08, 0x08, 0x10],
        '|' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        '(' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        ')' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        '%' => [0x18, 0x19, 0x02, 0x04, 0x08, 0x13, 0x03],
        '>' => [0x08, 0x04, 0x02, 0x01, 0x02, 0x04, 0x08],
        '<' => [0x02, 0x04, 0x08, 0x10, 0x08, 0x04, 0x02],
        '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F],
        _ => [0; 7],
    }
}

/// Draws `text` with its top-left corner at `(x, y)`, every font pixel
/// `scale` image pixels wide. Text running out of the image is clipped.
pub fn draw_text(image: &mut Image, x: u32, y: u32, text: &str, scale: u32, color: [u8; 3]) {
    let scale = scale.max(1);
    for (k, c) in text.chars().enumerate() {
        let rows = glyph(c);
        let left = x + k as u32 * 6 * scale;
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..5u32 {
                if bits & (0x10 >> col) == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let (px, py) = (left + col * scale + dx, y + row as u32 * scale + dy);
                        if px < image.width && py < image.height {
                            let i = (py as usize * image.width as usize + px as usize) * 3;
                            image.rgb[i..i + 3].copy_from_slice(&color);
                        }
                    }
                }
            }
        }
    }
}

/// Width in pixels of `text` drawn by [`draw_text`] at `scale`.
pub fn text_width(text: &str, scale: u32) -> u32 {
    (text.chars().count() as u32 * 6).saturating_sub(1) * scale.max(1)
}

/// The rectangle `width x height` at `(x, y)`, clipped to the image.
pub fn crop(image: &Image, x: u32, y: u32, width: u32, height: u32) -> Image {
    let x = x.min(image.width);
    let y = y.min(image.height);
    let (w, h) = (width.min(image.width - x), height.min(image.height - y));
    let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
    for row in y..y + h {
        let start = (row as usize * image.width as usize + x as usize) * 3;
        rgb.extend_from_slice(&image.rgb[start..start + w as usize * 3]);
    }
    Image { width: w, height: h, rgb }
}

/// Magnifies by an integer factor, repeating pixels (no smoothing: what is
/// shown is what was rendered).
pub fn magnify(image: &Image, factor: u32) -> Image {
    let factor = factor.max(1);
    let (w, h) = (image.width * factor, image.height * factor);
    let mut out = Image::new(w, h, [0; 3]);
    for y in 0..h {
        for x in 0..w {
            let src = ((y / factor) as usize * image.width as usize + (x / factor) as usize) * 3;
            let dst = (y as usize * w as usize + x as usize) * 3;
            out.rgb[dst..dst + 3].copy_from_slice(&image.rgb[src..src + 3]);
        }
    }
    out
}

/// Copies `src` into `dst` with its top-left corner at `(x, y)`, clipped.
pub fn blit(dst: &mut Image, src: &Image, x: u32, y: u32) {
    for row in 0..src.height.min(dst.height.saturating_sub(y)) {
        let w = src.width.min(dst.width.saturating_sub(x)) as usize;
        let from = row as usize * src.width as usize * 3;
        let to = ((y + row) as usize * dst.width as usize + x as usize) * 3;
        dst.rgb[to..to + w * 3].copy_from_slice(&src.rgb[from..from + w * 3]);
    }
}

/// Puts a caption bar with `text` above the image.
pub fn labelled(image: &Image, text: &str, scale: u32) -> Image {
    let scale = scale.max(1);
    let bar = 11 * scale;
    let mut out = Image::new(image.width, image.height + bar, [24, 24, 24]);
    blit(&mut out, image, 0, bar);
    draw_text(&mut out, 2 * scale, 2 * scale, text, scale, [255, 255, 255]);
    out
}

/// Panels next to each other, separated by `gap` pixels of `fill`; shorter
/// panels are padded at the bottom.
pub fn beside(panels: &[Image], gap: u32, fill: [u8; 3]) -> Image {
    let height = panels.iter().map(|p| p.height).max().unwrap_or(0);
    let width = panels.iter().map(|p| p.width).sum::<u32>() + gap * panels.len().saturating_sub(1) as u32;
    let mut out = Image::new(width, height, fill);
    let mut x = 0;
    for panel in panels {
        blit(&mut out, panel, x, 0);
        x += panel.width + gap;
    }
    out
}

/// Panels on top of each other, separated by `gap` pixels of `fill`; narrower
/// panels are padded on the right.
pub fn stacked(panels: &[Image], gap: u32, fill: [u8; 3]) -> Image {
    let width = panels.iter().map(|p| p.width).max().unwrap_or(0);
    let height = panels.iter().map(|p| p.height).sum::<u32>() + gap * panels.len().saturating_sub(1) as u32;
    let mut out = Image::new(width, height, fill);
    let mut y = 0;
    for panel in panels {
        blit(&mut out, panel, 0, y);
        y += panel.height + gap;
    }
    out
}

/// Draws the outline of a rectangle (to mark where a crop comes from).
pub fn outline(image: &mut Image, x: u32, y: u32, width: u32, height: u32, thickness: u32, color: [u8; 3]) {
    let mut set = |px: u32, py: u32| {
        if px < image.width && py < image.height {
            let i = (py as usize * image.width as usize + px as usize) * 3;
            image.rgb[i..i + 3].copy_from_slice(&color);
        }
    };
    for t in 0..thickness {
        for px in x..x + width {
            set(px, y + t);
            set(px, (y + height).saturating_sub(1 + t));
        }
        for py in y..y + height {
            set(x + t, py);
            set((x + width).saturating_sub(1 + t), py);
        }
    }
}

/// Reduces the image by an integer factor, averaging blocks.
pub fn shrink(image: &Image, factor: u32) -> Image {
    let factor = factor.max(1);
    let (w, h) = (image.width / factor, image.height / factor);
    let mut out = Image::new(w, h, [0; 3]);
    let area = factor * factor;
    for y in 0..h {
        for x in 0..w {
            let mut sum = [0u32; 3];
            for dy in 0..factor {
                for dx in 0..factor {
                    let src = ((y * factor + dy) as usize * image.width as usize + (x * factor + dx) as usize) * 3;
                    for (c, s) in sum.iter_mut().enumerate() {
                        *s += image.rgb[src + c] as u32;
                    }
                }
            }
            let dst = (y as usize * w as usize + x as usize) * 3;
            for (c, s) in sum.iter().enumerate() {
                out.rgb[dst + c] = ((s + area / 2) / area) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: u32, height: u32) -> Image {
        let mut image = Image::new(width, height, [0; 3]);
        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) as usize * 3;
                image.rgb[i] = x as u8;
                image.rgb[i + 1] = y as u8;
            }
        }
        image
    }

    #[test]
    fn crop_magnify_and_shrink_move_the_right_pixels() {
        let image = gradient(40, 30);
        let part = crop(&image, 10, 5, 8, 6);
        assert_eq!((part.width, part.height), (8, 6));
        assert_eq!(part.pixel(0, 0), [10, 5, 0]);
        assert_eq!(part.pixel(7, 5), [17, 10, 0]);
        // Clipped at the border.
        assert_eq!((crop(&image, 36, 28, 10, 10).width, crop(&image, 36, 28, 10, 10).height), (4, 2));
        let big = magnify(&part, 3);
        assert_eq!((big.width, big.height), (24, 18));
        assert_eq!(big.pixel(5, 4), part.pixel(1, 1));
        assert_eq!(shrink(&big, 3), part);
    }

    #[test]
    fn panels_and_labels_line_up() {
        let a = Image::new(10, 8, [200, 0, 0]);
        let b = Image::new(6, 12, [0, 200, 0]);
        let row = beside(&[a.clone(), b.clone()], 2, [1, 2, 3]);
        assert_eq!((row.width, row.height), (18, 12));
        assert_eq!(row.pixel(9, 7), [200, 0, 0]);
        assert_eq!(row.pixel(10, 0), [1, 2, 3]);
        assert_eq!(row.pixel(12, 11), [0, 200, 0]);
        assert_eq!(row.pixel(0, 9), [1, 2, 3]);
        let column = stacked(&[a.clone(), b], 1, [9, 9, 9]);
        assert_eq!((column.width, column.height), (10, 21));
        assert_eq!(column.pixel(5, 9), [0, 200, 0]);
        assert_eq!(column.pixel(8, 9), [9, 9, 9]);

        let captioned = labelled(&a, "A1", 1);
        assert_eq!((captioned.width, captioned.height), (10, 19));
        assert_eq!(captioned.pixel(0, 11), [200, 0, 0]);
        // Some pixels of the caption are lit, none below the bar.
        let lit = (0..11).flat_map(|y| (0..10).map(move |x| (x, y))).filter(|&(x, y)| captioned.pixel(x, y) == [255; 3]).count();
        assert!(lit > 10, "{lit}");
        assert_eq!(text_width("AB", 2), 22);
    }

    #[test]
    fn text_and_outline_are_clipped() {
        let mut image = Image::new(8, 8, [0; 3]);
        draw_text(&mut image, 5, 5, "WWWW", 3, [255; 3]);
        outline(&mut image, 2, 2, 50, 50, 2, [9, 9, 9]);
        assert_eq!(image.pixel(2, 4), [9, 9, 9]);
        assert_eq!(image.pixel(3, 3), [9, 9, 9]);
    }
}
