use arany::ImageAttachment;
use ratatui::layout::Rect;

pub(super) const PREVIEW_COLUMNS: u16 = 32;
pub(super) const PREVIEW_ROWS: u16 = 10;
pub(super) const PREVIEW_MIN_ROWS: u16 = 4;
const CELL_HEIGHT_TO_WIDTH: f64 = 2.0;
const BORDER_CELLS: u16 = 2;
const IMAGE_ID: u16 = 31;
const CHUNK_CHARACTERS: usize = 4096;

pub(super) fn preview_geometry(
    area: Rect,
    chip: Rect,
    image: &ImageAttachment,
) -> Option<(Rect, Rect)> {
    let max_columns = PREVIEW_COLUMNS.min(area.width.saturating_sub(BORDER_CELLS));
    let max_rows = PREVIEW_ROWS.min(chip.y.saturating_sub(area.y).saturating_sub(BORDER_CELLS));
    if max_rows < PREVIEW_MIN_ROWS || max_columns == 0 {
        return None;
    }

    let (image_width, image_height) = image.dimensions();
    let columns_per_row = f64::from(image_width) / f64::from(image_height) * CELL_HEIGHT_TO_WIDTH;
    let available_aspect = f64::from(max_columns) / f64::from(max_rows);
    let (columns, rows) = if available_aspect > columns_per_row {
        (
            (f64::from(max_rows) * columns_per_row)
                .round()
                .clamp(1.0, f64::from(max_columns)) as u16,
            max_rows,
        )
    } else {
        (
            max_columns,
            (f64::from(max_columns) / columns_per_row)
                .round()
                .clamp(f64::from(PREVIEW_MIN_ROWS), f64::from(max_rows)) as u16,
        )
    };
    if rows < PREVIEW_MIN_ROWS {
        return None;
    }
    let width = columns + BORDER_CELLS;
    let height = rows + BORDER_CELLS;
    let x = chip.x.clamp(area.x, area.right().saturating_sub(width));
    let y = chip.y - height;
    let balloon = Rect::new(x, y, width, height);
    let inner = Rect::new(x + 1, y + 1, columns, rows);
    Some((balloon, inner))
}

pub(super) fn kitty_image(image: &ImageAttachment, inner: Rect) -> Vec<u8> {
    kitty_image_data(image.base64().as_bytes(), inner)
}

pub(super) fn inline_image(image: &ImageAttachment, inner: Rect) -> Vec<u8> {
    format!(
        "\x1b[s\x1b[{};{}H\x1b]1337;File=inline=1;width={};height={};preserveAspectRatio=1:{}\x1b\\\x1b[u",
        inner.y + 1,
        inner.x + 1,
        inner.width,
        inner.height,
        image.base64()
    )
    .into_bytes()
}

fn kitty_image_data(encoded: &[u8], inner: Rect) -> Vec<u8> {
    let mut bytes =
        Vec::with_capacity(encoded.len() + encoded.len().div_ceil(CHUNK_CHARACTERS) * 80);
    bytes.extend_from_slice(format!("\x1b[s\x1b[{};{}H", inner.y + 1, inner.x + 1).as_bytes());
    for (index, chunk) in encoded.chunks(CHUNK_CHARACTERS).enumerate() {
        let more = usize::from((index + 1) * CHUNK_CHARACTERS < encoded.len());
        let header = if index == 0 {
            format!(
                "\x1b_Ga=T,f=100,t=d,i={IMAGE_ID},c={},r={},q=2,C=1,m={more};",
                inner.width, inner.height
            )
        } else {
            format!("\x1b_Gm={more};")
        };
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(chunk);
        bytes.extend_from_slice(b"\x1b\\");
    }
    bytes.extend_from_slice(b"\x1b[u");
    bytes
}

pub(super) fn kitty_delete() -> &'static [u8] {
    b"\x1b_Ga=d,d=I,i=31,q=2\x1b\\"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_transfer_uses_first_chunk_metadata_and_continuation_headers() {
        let inner = Rect::new(4, 5, 12, 4);
        let image = large_test_image();
        let encoded = image.base64().as_bytes();
        assert!(encoded.len() > CHUNK_CHARACTERS);
        let mut expected = b"\x1b[s\x1b[6;5H".to_vec();
        for (index, chunk) in encoded.chunks(CHUNK_CHARACTERS).enumerate() {
            let more = usize::from((index + 1) * CHUNK_CHARACTERS < encoded.len());
            let header = if index == 0 {
                format!("\x1b_Ga=T,f=100,t=d,i=31,c=12,r=4,q=2,C=1,m={more};")
            } else {
                format!("\x1b_Gm={more};")
            };
            expected.extend_from_slice(header.as_bytes());
            expected.extend_from_slice(chunk);
            expected.extend_from_slice(b"\x1b\\");
        }
        expected.extend_from_slice(b"\x1b[u");
        assert_eq!(kitty_image(&image, inner), expected);
    }

    #[test]
    fn preview_geometry_respects_image_aspect_in_terminal_cells() {
        let area = Rect::new(0, 0, 80, 24);
        let chip = Rect::new(20, 22, 10, 1);
        for (dimensions, expected) in [
            ((100, 100), (20, 10)),
            ((50, 100), (10, 10)),
            ((200, 100), (32, 8)),
        ] {
            let image = image_with_dimensions(dimensions);
            let (_, inner) = preview_geometry(area, chip, &image).expect("preview fits");
            assert_eq!((inner.width, inner.height), expected);
        }
    }

    fn image_with_dimensions((width, height): (u32, u32)) -> ImageAttachment {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        let mut bytes = STANDARD
            .decode(arany::test_support::test_image().base64())
            .expect("test PNG base64");
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = png_crc32(&bytes[12..29]);
        bytes[29..33].copy_from_slice(&crc.to_be_bytes());
        ImageAttachment::from_png(&bytes).expect("dimensioned PNG")
    }

    fn large_test_image() -> ImageAttachment {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        let mut bytes = STANDARD
            .decode(arany::test_support::test_image().base64())
            .expect("test PNG base64");
        let mut chunk = Vec::with_capacity(4_100);
        chunk.extend_from_slice(b"tEXt");
        chunk.extend_from_slice(b"Key\0");
        chunk.extend(std::iter::repeat_n(b'a', 4_100));
        let mut text = Vec::new();
        text.extend_from_slice(
            &u32::try_from(chunk.len() - 4)
                .expect("PNG chunk length")
                .to_be_bytes(),
        );
        text.extend_from_slice(&chunk);
        text.extend_from_slice(&png_crc32(&chunk).to_be_bytes());
        bytes.splice(33..33, text);
        ImageAttachment::from_png(&bytes).expect("large PNG with ancillary text")
    }

    fn png_crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0_u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
            }
        }
        !crc
    }

    #[test]
    fn kitty_placement_deletion_is_byte_exact() {
        assert_eq!(kitty_delete(), b"\x1b_Ga=d,d=I,i=31,q=2\x1b\\");
    }

    #[test]
    fn inline_osc_image_is_byte_exact() {
        let image = arany::test_support::test_image();
        let inner = Rect::new(4, 5, 12, 4);
        let expected = format!(
            "\x1b[s\x1b[6;5H\x1b]1337;File=inline=1;width=12;height=4;preserveAspectRatio=1:{}\x1b\\\x1b[u",
            image.base64()
        );
        assert_eq!(inline_image(&image, inner), expected.as_bytes());
    }

    #[test]
    fn preview_geometry_clamps_and_rejects_too_little_height() {
        let image = arany::test_support::test_image();
        for width in [40, 80, 120] {
            let geometry = preview_geometry(
                Rect::new(0, 0, width, 12),
                Rect::new(width - 4, 10, 10, 1),
                &image,
            )
            .expect("preview fits");
            assert!(geometry.0.right() <= width);
            assert!(geometry.0.bottom() <= 10);
            assert_eq!(geometry.1.width, geometry.0.width - BORDER_CELLS);
            assert_eq!(geometry.1.height, geometry.0.height - BORDER_CELLS);
            assert_eq!(geometry.1.height, 8);
        }
        assert!(
            preview_geometry(Rect::new(0, 0, 40, 5), Rect::new(0, 4, 10, 1), &image,).is_none()
        );
    }
}
