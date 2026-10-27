use std::{fmt::Write as _, io::Cursor};

use arany::{ImageAttachment, MAX_ENCODED_IMAGE_BYTES, MAX_IMAGE_BYTES};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ratatui::layout::Rect;

use super::{PREVIEW_COLUMNS, PREVIEW_ROWS};

const MAX_CELL_PIXELS: u16 = 128;
pub(in crate::terminal) const PREVIEW_MAX_PIXELS: usize = PREVIEW_COLUMNS as usize
    * PREVIEW_ROWS as usize
    * MAX_CELL_PIXELS as usize
    * MAX_CELL_PIXELS as usize;
const PALETTE_RED: usize = 6;
const PALETTE_GREEN: usize = 7;
const PALETTE_BLUE: usize = 6;
const PALETTE_COLORS: usize = PALETTE_RED * PALETTE_GREEN * PALETTE_BLUE;
const SIXEL_ROWS: usize = 6;
const RGBA_CHANNELS: usize = 4;
const TRANSPARENT: u8 = u8::MAX;
const _: () = assert!(PALETTE_COLORS <= TRANSPARENT as usize);

pub(in crate::terminal) const LIMITS: &[arany::limits::Limit] = &[
    arany::limits::Limit {
        name: stringify!(MAX_CELL_PIXELS),
        value: MAX_CELL_PIXELS as u64,
        unit: "pixels",
        owner: "crates/arany-cli/src/terminal/graphics/sixel.rs",
    },
    arany::limits::Limit {
        name: stringify!(PREVIEW_MAX_PIXELS),
        value: PREVIEW_MAX_PIXELS as u64,
        unit: "pixels",
        owner: "crates/arany-cli/src/terminal/graphics/sixel.rs",
    },
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::terminal) struct CellPixels {
    width: u16,
    height: u16,
}

impl CellPixels {
    pub(in crate::terminal) fn new(width: u16, height: u16) -> Option<Self> {
        ((1..=MAX_CELL_PIXELS).contains(&width) && (1..=MAX_CELL_PIXELS).contains(&height))
            .then_some(Self { width, height })
    }

    #[cfg(unix)]
    pub(in crate::terminal) fn from_terminal() -> Option<Self> {
        let size = rustix::termios::tcgetwinsize(std::io::stderr()).ok()?;
        Self::new(
            size.ws_xpixel.checked_div(size.ws_col)?,
            size.ws_ypixel.checked_div(size.ws_row)?,
        )
    }
}

pub(in crate::terminal) fn image(
    image: &ImageAttachment,
    inner: Rect,
    cell: CellPixels,
) -> Option<Vec<u8>> {
    let bytes = STANDARD.decode(image.base64()).ok()?;
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(bytes),
        png::Limits {
            bytes: MAX_IMAGE_BYTES,
        },
    );
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let pixels = usize::try_from(reader.info().width)
        .ok()?
        .checked_mul(usize::try_from(reader.info().height).ok()?)?;
    if pixels > PREVIEW_MAX_PIXELS {
        return None;
    }
    let size = reader.output_buffer_size()?;
    if size > PREVIEW_MAX_PIXELS * RGBA_CHANNELS {
        return None;
    }
    let mut decoded = vec![0; size];
    let info = reader.next_frame(&mut decoded).ok()?;
    let width = info.width as usize;
    let height = info.height as usize;
    let max_width = usize::from(inner.width) * usize::from(cell.width);
    let max_height = usize::from(inner.height) * usize::from(cell.height);
    let scale = (max_width as f64 / width as f64).min(max_height as f64 / height as f64);
    let target_width = (width as f64 * scale).floor().max(1.0) as usize;
    let target_height = (height as f64 * scale).floor().max(1.0) as usize;
    if max_width == 0
        || max_height == 0
        || target_width.checked_mul(target_height)? > PREVIEW_MAX_PIXELS
    {
        return None;
    }
    let channels = info.color_type.samples();
    let mut indexed = Vec::with_capacity(target_width * target_height);
    for y in 0..target_height {
        for x in 0..target_width {
            let offset =
                ((y * height / target_height) * width + x * width / target_width) * channels;
            indexed.push(palette_index(
                &decoded[offset..offset + channels],
                info.color_type,
            )?);
        }
    }
    encode(&indexed, target_width, target_height, inner)
}

fn palette_index(pixel: &[u8], color: png::ColorType) -> Option<u8> {
    let (red, green, blue, alpha) = match color {
        png::ColorType::Grayscale => (pixel[0], pixel[0], pixel[0], 255),
        png::ColorType::GrayscaleAlpha => (pixel[0], pixel[0], pixel[0], pixel[1]),
        png::ColorType::Rgb => (pixel[0], pixel[1], pixel[2], 255),
        png::ColorType::Rgba => (pixel[0], pixel[1], pixel[2], pixel[3]),
        png::ColorType::Indexed => return None,
    };
    if alpha == 0 {
        return Some(TRANSPARENT);
    }
    let quantize = |value, levels| (usize::from(value) * (levels - 1) + 127) / 255;
    Some(
        ((quantize(red, PALETTE_RED) * PALETTE_GREEN + quantize(green, PALETTE_GREEN))
            * PALETTE_BLUE
            + quantize(blue, PALETTE_BLUE)) as u8,
    )
}

fn encode(pixels: &[u8], width: usize, height: usize, inner: Rect) -> Option<Vec<u8>> {
    let mut output = format!(
        "\x1b[s\x1b[{};{}H\x1bP0;1q\"1;1;{width};{height}",
        inner.y + 1,
        inner.x + 1
    );
    let mut used = [false; PALETTE_COLORS];
    for &pixel in pixels {
        if pixel != TRANSPARENT {
            used[usize::from(pixel)] = true;
        }
    }
    for (color, present) in used.into_iter().enumerate() {
        if present {
            let red = color / (PALETTE_GREEN * PALETTE_BLUE) * 100 / (PALETTE_RED - 1);
            let green = (color / PALETTE_BLUE) % PALETTE_GREEN * 100 / (PALETTE_GREEN - 1);
            let blue = color % PALETTE_BLUE * 100 / (PALETTE_BLUE - 1);
            write!(output, "#{color};2;{red};{green};{blue}").ok()?;
        }
    }
    for y in (0..height).step_by(SIXEL_ROWS) {
        let band = &pixels[y * width..((y + SIXEL_ROWS).min(height)) * width];
        let mut used = [false; PALETTE_COLORS];
        for &pixel in band {
            if pixel != TRANSPARENT {
                used[usize::from(pixel)] = true;
            }
        }
        for (color, present) in used.into_iter().enumerate() {
            if !present {
                continue;
            }
            write!(output, "#{color}").ok()?;
            for x in 0..width {
                let bits = column_mask(band, width, x, color as u8);
                output.push(char::from(b'?' + bits));
                if output.len() > MAX_ENCODED_IMAGE_BYTES {
                    return None;
                }
            }
            output.push('$');
        }
        if y + SIXEL_ROWS < height {
            output.push('-');
        }
    }
    output.push_str("\x1b\\\x1b[u");
    (output.len() <= MAX_ENCODED_IMAGE_BYTES).then(|| output.into_bytes())
}

fn column_mask(band: &[u8], width: usize, column: usize, color: u8) -> u8 {
    band.chunks_exact(width)
        .enumerate()
        .fold(0, |bits, (row, pixels)| {
            bits | (u8::from(pixels[column] == color) << row)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{GraphicsPlacement, GraphicsProtocol, graphics::preview_interior};
    use ratatui::{
        Terminal,
        backend::{Backend, TestBackend},
        buffer::Cell,
    };

    fn png_image(width: u32, height: u32, rgba: &[u8]) -> ImageAttachment {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            let mut writer = encoder.write_header().expect("PNG header");
            writer.write_image_data(rgba).expect("PNG pixels");
        }
        ImageAttachment::from_png(&bytes).expect("admitted PNG")
    }

    #[test]
    fn sixel_goldens_cover_red_pixel_palette_scaling_and_transparency() {
        let cell = CellPixels::new(1, 1).expect("cell pixels");
        let red = arany::test_support::test_image();
        assert_eq!(
            image(&red, Rect::new(4, 5, 1, 1), cell).expect("Sixel"),
            b"\x1b[s\x1b[6;5H\x1bP0;1q\"1;1;1;1#210;2;100;0;0#210@$\x1b\\\x1b[u"
        );
        let palette = png_image(
            2,
            2,
            &[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
        );
        assert_eq!(image(&palette, Rect::new(0, 0, 2, 2), cell).expect("Sixel"),
            b"\x1b[s\x1b[1;1H\x1bP0;1q\"1;1;2;2#5;2;0;0;100#36;2;0;100;0#210;2;100;0;0#251;2;100;100;100#5A?$#36?@$#210@?$#251?A$\x1b\\\x1b[u");
        assert_eq!(
            image(&palette, Rect::new(0, 0, 1, 1), cell),
            image(&red, Rect::new(0, 0, 1, 1), cell)
        );
        let transparent = png_image(1, 1, &[255, 0, 0, 0]);
        assert_eq!(
            image(&transparent, Rect::new(0, 0, 1, 1), cell).expect("Sixel"),
            b"\x1b[s\x1b[1;1H\x1bP0;1q\"1;1;1;1\x1b\\\x1b[u"
        );
    }

    fn draw_preview(
        terminal: &mut Terminal<TestBackend>,
        area: Rect,
        interior: crate::terminal::view::PreviewInterior,
    ) {
        terminal
            .draw(|frame| {
                crate::terminal::view::draw_image_preview(frame, area, "Image", interior);
            })
            .expect("preview frame");
    }

    fn contains_painted_cells(terminal: &Terminal<TestBackend>, area: Rect) -> bool {
        area.positions()
            .any(|position| terminal.backend().buffer()[position].symbol() == "█")
    }

    #[test]
    fn overlapping_opaque_to_transparent_switch_repaints_before_graphics_reservation() {
        let cell = CellPixels::new(1, 1).expect("cell size");
        let opaque = png_image(3, 2, &[255, 0, 0, 255].repeat(6));
        let transparent = png_image(3, 2, &[0, 0, 0, 0].repeat(6));
        let old_balloon = Rect::new(1, 1, 8, 6);
        let new_balloon = Rect::new(3, 1, 8, 6);
        let old = GraphicsPlacement {
            image_index: 0,
            digest: opaque.digest(),
            inner: Rect::new(2, 2, 6, 4),
        };
        let new = GraphicsPlacement {
            image_index: 1,
            digest: transparent.digest(),
            inner: Rect::new(4, 2, 6, 4),
        };
        let protocol = Some(GraphicsProtocol::Sixel(cell));
        let opaque_bytes = image(&opaque, old.inner, cell).expect("opaque raster");
        let transparent_bytes = image(&transparent, new.inner, cell).expect("transparent raster");
        assert!(opaque_bytes.contains(&b'#'));
        assert!(!transparent_bytes.contains(&b'#'));
        let mut terminal = Terminal::new(TestBackend::new(20, 10)).expect("terminal");
        let interior = preview_interior(protocol, None, Some(old), old);
        draw_preview(&mut terminal, old_balloon, interior);
        let painted = Cell::new("█");
        terminal
            .backend_mut()
            .draw(
                old.inner
                    .positions()
                    .map(|position| (position.x, position.y, &painted)),
            )
            .expect("opaque raster paints the old interior");
        assert!(contains_painted_cells(&terminal, old.inner));

        let interior = preview_interior(protocol, Some(old), None, new);
        draw_preview(&mut terminal, new_balloon, interior);
        assert!(!contains_painted_cells(&terminal, old.inner));
        let overlap = old.inner.intersection(new.inner);
        assert!(!overlap.is_empty());
        assert!(
            overlap
                .positions()
                .all(|position| terminal.backend().buffer()[position].symbol() == " ")
        );
        let interior = preview_interior(protocol, None, Some(new), new);
        draw_preview(&mut terminal, new_balloon, interior);
        assert!(!contains_painted_cells(&terminal, overlap));
        let interior = preview_interior(protocol, Some(new), None, new);
        draw_preview(&mut terminal, new_balloon, interior);
        terminal.draw(|_| {}).expect("repaint after leave");
        assert!(!contains_painted_cells(&terminal, new.inner));
    }

    #[test]
    fn malformed_raster_and_pixel_or_output_overflow_produce_no_graphics() {
        let mut bytes = STANDARD
            .decode(arany::test_support::test_image().base64())
            .expect("PNG");
        bytes[41] = 0;
        let crc = super::super::tests::png_crc32(&bytes[37..53]);
        bytes[53..57].copy_from_slice(&crc.to_be_bytes());
        let malformed =
            ImageAttachment::from_png(&bytes).expect("dimension admission does not inflate");
        let cell = CellPixels::new(1, 1).expect("cell");
        assert!(image(&malformed, Rect::new(0, 0, 1, 1), cell).is_none());
        let red = arany::test_support::test_image();
        assert!(image(&red, Rect::new(0, 0, u16::MAX, u16::MAX), cell).is_none());
        let width = 1024;
        let height = 12;
        let pixels: Vec<_> = (0..width * height)
            .map(|index| (index % PALETTE_COLORS) as u8)
            .collect();
        assert!(encode(&pixels, width, height, Rect::default()).is_none());
        for (width, height) in [
            (0, 16),
            (8, 0),
            (MAX_CELL_PIXELS + 1, 16),
            (8, MAX_CELL_PIXELS + 1),
        ] {
            assert!(CellPixels::new(width, height).is_none());
        }
    }
}
