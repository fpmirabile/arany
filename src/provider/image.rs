use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeStruct};
use sha2::{Digest, Sha256};
use std::{fmt, sync::Arc};

pub const MAX_IMAGE_BYTES: usize = 192 * 1024;
pub const MAX_MESSAGE_IMAGES: usize = 4;
pub(crate) const MAX_IMAGE_METADATA_BYTES: usize = 128;
const MAX_ENCODED_IMAGE_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;

#[derive(Clone, Eq, PartialEq)]
pub struct ImageAttachment {
    data: Arc<str>,
    bytes: usize,
    width: u32,
    height: u32,
    digest: [u8; 32],
}

impl ImageAttachment {
    pub fn from_png(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err("image is too large; choose a smaller PNG");
        }
        let (width, height) = png_dimensions(bytes).ok_or("invalid or unsupported PNG image")?;
        Ok(Self {
            data: STANDARD.encode(bytes).into(),
            bytes: bytes.len(),
            width,
            height,
            digest: Sha256::digest(bytes).into(),
        })
    }

    pub fn media_type(&self) -> &'static str {
        "image/png"
    }

    pub fn byte_len(&self) -> usize {
        self.bytes
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }

    pub fn base64(&self) -> &str {
        &self.data
    }

    pub(crate) fn context_bytes(&self) -> usize {
        self.data.len() + MAX_IMAGE_METADATA_BYTES
    }

    pub(crate) fn description(&self, index: usize) -> String {
        format!(
            "Image {}: PNG {}x{}, {} bytes",
            index + 1,
            self.width,
            self.height,
            self.bytes
        )
    }
}

pub(crate) fn valid_image_collection(images: &[ImageAttachment]) -> bool {
    images.len() <= MAX_MESSAGE_IMAGES
        && images.iter().map(ImageAttachment::byte_len).sum::<usize>() <= MAX_IMAGE_BYTES
}

#[cfg(test)]
pub(crate) fn test_image() -> ImageAttachment {
    let bytes = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC").expect("synthetic PNG bytes");
    ImageAttachment::from_png(&bytes).expect("synthetic PNG image")
}

pub(crate) fn deserialize_images<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ImageAttachment>, D::Error> {
    struct Images;
    impl<'de> serde::de::Visitor<'de> for Images {
        type Value = Vec<ImageAttachment>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("at most four PNG images totaling at most 192 KiB")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut images = Vec::new();
            for _ in 0..MAX_MESSAGE_IMAGES {
                let Some(image) = sequence.next_element::<ImageAttachment>()? else {
                    return Ok(images);
                };
                images.push(image);
                if !valid_image_collection(&images) {
                    return Err(serde::de::Error::custom("image byte limit exceeded"));
                }
            }
            if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom("image count limit exceeded"));
            }
            Ok(images)
        }
    }
    deserializer.deserialize_seq(Images)
}

impl fmt::Debug for ImageAttachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageAttachment")
            .field("media_type", &self.media_type())
            .field("bytes", &self.bytes)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("digest", &self.digest)
            .finish()
    }
}

impl Serialize for ImageAttachment {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut record = serializer.serialize_struct("ImageAttachment", 2)?;
        record.serialize_field("media_type", self.media_type())?;
        record.serialize_field("data", self.base64())?;
        record.end()
    }
}

impl<'de> Deserialize<'de> for ImageAttachment {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Payload {
            media_type: String,
            data: String,
        }
        let payload = Payload::deserialize(deserializer)?;
        if payload.media_type != "image/png" || payload.data.len() > MAX_ENCODED_IMAGE_BYTES {
            return Err(serde::de::Error::custom("invalid image attachment"));
        }
        let bytes = STANDARD
            .decode(&payload.data)
            .map_err(|_| serde::de::Error::custom("invalid image attachment"))?;
        let image = Self::from_png(&bytes).map_err(serde::de::Error::custom)?;
        if image.base64() != payload.data {
            return Err(serde::de::Error::custom("noncanonical image attachment"));
        }
        Ok(image)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageOrigin {
    Objective,
    History { turn_index: u8 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderImage {
    pub origin: ImageOrigin,
    pub image: ImageAttachment,
}

impl ProviderImage {
    pub(crate) fn label(&self, index: usize) -> String {
        let source = match self.origin {
            ImageOrigin::Objective => "objective".to_owned(),
            ImageOrigin::History { turn_index } => {
                format!("history turn {}", usize::from(turn_index) + 1)
            }
        };
        let (width, height) = self.image.dimensions();
        format!("Image {} ({source}): PNG {width}x{height}", index + 1)
    }
}

pub(super) fn validate_images(
    request: &super::ProviderRequest,
) -> Result<(), super::ProviderError> {
    if request.images.len() > MAX_MESSAGE_IMAGES
        || request
            .images
            .iter()
            .map(|value| value.image.byte_len())
            .sum::<usize>()
            > MAX_IMAGE_BYTES
        || request.images.iter().any(|value| match value.origin {
            ImageOrigin::Objective => false,
            ImageOrigin::History { turn_index } => {
                usize::from(turn_index) >= request.history.len()
                    || request.phase == super::AgentPhase::ChildWork
            }
        })
    {
        return Err(super::ProviderError::Rejected);
    }
    Ok(())
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let mut cursor = 8_usize;
    let mut dimensions = None;
    let mut palette = false;
    let mut color = 0;
    let mut depth = 0;
    let mut data_bytes = 0_usize;
    let mut data_started = false;
    let mut data_ended = false;
    while cursor < bytes.len() {
        let length = u32::from_be_bytes(bytes.get(cursor..cursor + 4)?.try_into().ok()?) as usize;
        let end = cursor.checked_add(12)?.checked_add(length)?;
        let chunk = bytes.get(cursor + 4..end)?;
        let kind = chunk.get(..4)?;
        let data = chunk.get(4..4 + length)?;
        if !kind.iter().all(u8::is_ascii_alphabetic) || !kind[2].is_ascii_uppercase() {
            return None;
        }
        let expected = u32::from_be_bytes(chunk.get(4 + length..)?.try_into().ok()?);
        let mut crc = !0_u32;
        for &byte in &chunk[..4 + length] {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
            }
        }
        if !crc != expected || dimensions.is_none() && kind != b"IHDR" {
            return None;
        }
        match kind {
            b"IHDR" if cursor == 8 && data.len() == 13 => {
                let width = u32::from_be_bytes(data[..4].try_into().ok()?);
                let height = u32::from_be_bytes(data[4..8].try_into().ok()?);
                if !(1..=4096).contains(&width)
                    || !(1..=4096).contains(&height)
                    || u64::from(width) * u64::from(height) > 4 * 1024 * 1024
                    || !matches!(
                        (data[9], data[8]),
                        (0, 1 | 2 | 4 | 8 | 16) | (2 | 4 | 6, 8 | 16) | (3, 1 | 2 | 4 | 8)
                    )
                    || data[10] != 0
                    || data[11] != 0
                    || data[12] > 1
                {
                    return None;
                }
                dimensions = Some((width, height));
                color = data[9];
                depth = data[8];
            }
            b"IHDR" => return None,
            b"PLTE" => {
                if palette
                    || data_started
                    || matches!(color, 0 | 4)
                    || data.is_empty()
                    || data.len() > 768
                    || !data.len().is_multiple_of(3)
                    || color == 3 && data.len() / 3 > 1 << depth
                {
                    return None;
                }
                palette = true;
            }
            b"IDAT" => {
                if data_ended || color == 3 && !palette {
                    return None;
                }
                data_started = true;
                data_bytes = data_bytes.checked_add(length)?;
            }
            b"IEND" => {
                return (data_bytes > 0 && data.is_empty() && end == bytes.len())
                    .then_some(dimensions?);
            }
            b"acTL" | b"fcTL" | b"fdAT" => return None,
            _ if kind[0].is_ascii_uppercase() => return None,
            _ => {
                data_ended |= data_started;
            }
        }
        cursor = end;
    }
    None
}
