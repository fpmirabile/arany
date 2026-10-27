use super::plan::{Plan, RefDigests};
use super::*;
use crate::tools::guard::transfer;
use crate::validate::{MAX_PATH_BYTES, valid_digest};
use std::io::{Read, Write};

const LENGTH_BYTES: usize = size_of::<u32>();
const HASH_BYTES: usize = size_of::<[u8; 32]>();
const OBJECT_ID_BYTES: usize = 40;
const OBJECT_NAME_BYTES: usize = OBJECT_ID_BYTES + 1;
const MAX_HOLDS: usize = 8;
// Counts, optional tags, identities, fixed digests, and the replacement HEAD.
const HEADER_BYTES: usize =
    2 * size_of::<u64>() + 16 * LENGTH_BYTES + 4 * HASH_BYTES + super::super::HEAD_BYTES;
const REF_UPDATE_BYTES: usize = MAX_PATH_BYTES + 3 * LENGTH_BYTES + 2 * OBJECT_ID_BYTES;
pub(super) const MAX_FRAME_BYTES: usize = 2 * MAX_SNAPSHOT_BYTES
    + MAX_REF_ENTRIES * (LENGTH_BYTES + MAX_PATH_BYTES + HASH_BYTES)
    + MAX_SNAPSHOT_FILES * (MAX_PATH_BYTES + HASH_BYTES + 6 * LENGTH_BYTES)
    + MAX_OBJECTS * (OBJECT_NAME_BYTES + 2 * LENGTH_BYTES)
    + MAX_HOLDS * (MAX_PATH_BYTES + LENGTH_BYTES)
    + REF_UPDATE_BYTES
    + HEADER_BYTES;
const ABSENT: u32 = u32::MAX;

pub(super) fn write(writer: &mut impl Write, plan: &Plan) -> Result<(), ToolError> {
    let bytes = encode(plan)?;
    transfer::write_frame(writer, &bytes, MAX_FRAME_BYTES)
}

pub(super) fn read(reader: &mut impl Read) -> Result<Plan, ToolError> {
    let bytes = transfer::read_frame(reader, MAX_FRAME_BYTES)?;
    decode(&bytes)
}

fn encode(plan: &Plan) -> Result<Vec<u8>, ToolError> {
    let publication = &plan.publication;
    let mut out = Encoder(Vec::new());
    out.0.extend_from_slice(&plan.identity.0.to_le_bytes());
    out.0.extend_from_slice(&plan.identity.1.to_le_bytes());
    out.0.extend_from_slice(&plan.refs.head);
    out.optional(plan.refs.packed.as_ref().map(|hash| hash.as_slice()))?;
    out.optional(plan.index_before.as_ref().map(|hash| hash.as_slice()))?;
    out.count(plan.refs.loose.len())?;
    for (name, hash) in &plan.refs.loose {
        out.field(name.as_bytes())?;
        out.0.extend_from_slice(hash);
    }
    out.optional(publication.index.as_deref())?;
    out.optional(publication.head.as_deref())?;
    out.count(usize::from(publication.reference.is_some()))?;
    if let Some(update) = &publication.reference {
        out.field(update.name.as_bytes())?;
        out.optional(update.before.as_deref().map(str::as_bytes))?;
        out.field(update.after.as_bytes())?;
    }
    out.count(publication.holds.len())?;
    for name in &publication.holds {
        out.field(name.as_bytes())?;
    }
    if publication.objects.len() != plan.objects.len() {
        return Err(ToolError::Configuration);
    }
    out.count(plan.objects.len())?;
    for (name, bytes) in publication.objects.iter().zip(&plan.objects) {
        out.field(name.as_bytes())?;
        out.field(bytes)?;
    }
    out.count(publication.files.len())?;
    for file in &publication.files {
        out.field(file.path.as_bytes())?;
        out.count(usize::from(file.before.is_some()))?;
        if let Some(before) = &file.before {
            out.0.extend_from_slice(&parse_hash(&before.sha256)?);
            out.count(usize::from(before.executable))?;
        }
        out.count(usize::from(file.after.is_some()))?;
        if let Some(after) = &file.after {
            out.count(usize::from(after.executable))?;
            out.field(
                &after
                    .bytes_with_limit(MAX_FILE_BYTES)
                    .ok_or(ToolError::Limit)?,
            )?;
        }
    }
    if out.0.len() > MAX_FRAME_BYTES {
        return Err(ToolError::Limit);
    }
    Ok(out.0)
}

struct Encoder(Vec<u8>);

impl Encoder {
    fn count(&mut self, count: usize) -> Result<(), ToolError> {
        self.0.extend_from_slice(
            &u32::try_from(count)
                .map_err(|_| ToolError::Limit)?
                .to_le_bytes(),
        );
        Ok(())
    }

    fn field(&mut self, bytes: &[u8]) -> Result<(), ToolError> {
        if self
            .0
            .len()
            .saturating_add(LENGTH_BYTES)
            .saturating_add(bytes.len())
            > MAX_FRAME_BYTES
        {
            return Err(ToolError::Limit);
        }
        self.count(bytes.len())?;
        self.0.extend_from_slice(bytes);
        Ok(())
    }

    fn optional(&mut self, bytes: Option<&[u8]>) -> Result<(), ToolError> {
        match bytes {
            Some(bytes) => self.field(bytes),
            None => self.count(ABSENT as usize),
        }
    }
}

fn decode(bytes: &[u8]) -> Result<Plan, ToolError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ToolError::Limit);
    }
    let mut input = Decoder(bytes);
    let identity = (
        u64::from_le_bytes(input.array()?),
        u64::from_le_bytes(input.array()?),
    );
    let head_digest = input.array()?;
    let packed = input.hash()?;
    let index_before = input.hash()?;
    let mut loose = Vec::new();
    for _ in 0..input.count(MAX_REF_ENTRIES)? {
        let name = input.text(MAX_PATH_BYTES)?;
        if loose.last().is_some_and(|(previous, _)| previous >= &name) {
            return Err(ToolError::Configuration);
        }
        loose.push((name, input.array()?));
    }
    let index = input.optional(MAX_SNAPSHOT_BYTES)?.map(<[u8]>::to_vec);
    let head = input
        .optional(super::super::HEAD_BYTES)?
        .map(<[u8]>::to_vec);
    if head
        .as_ref()
        .is_some_and(|bytes| branch_of(bytes).is_none())
    {
        return Err(ToolError::Configuration);
    }
    let reference = if input.count(1)? == 1 {
        Some(input.reference()?)
    } else {
        None
    };
    let mut holds = Vec::new();
    for _ in 0..input.count(MAX_HOLDS)? {
        let name = input.text(MAX_PATH_BYTES)?;
        if !lock_target(&name) || holds.contains(&name) {
            return Err(ToolError::Configuration);
        }
        holds.push(name);
    }
    let (names, objects) = input.objects(index.as_ref().map_or(0, Vec::len))?;
    let files = input.files()?;
    if !input.0.is_empty() {
        return Err(ToolError::Configuration);
    }
    Ok(Plan {
        identity,
        refs: RefDigests {
            head: head_digest,
            packed,
            loose,
        },
        index_before,
        publication: Publication {
            files,
            objects: names,
            reference,
            holds,
            index,
            head,
        },
        objects,
    })
}

struct Decoder<'a>(&'a [u8]);

impl<'a> Decoder<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], ToolError> {
        let (bytes, rest) = self
            .0
            .split_at_checked(count)
            .ok_or(ToolError::Configuration)?;
        self.0 = rest;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ToolError> {
        self.take(N)?
            .try_into()
            .map_err(|_| ToolError::Configuration)
    }

    fn count(&mut self, max: usize) -> Result<usize, ToolError> {
        let count = u32::from_le_bytes(self.array()?) as usize;
        if count > max {
            return Err(ToolError::Limit);
        }
        Ok(count)
    }

    fn field(&mut self, max: usize) -> Result<&'a [u8], ToolError> {
        let count = self.count(max)?;
        self.take(count)
    }

    fn optional(&mut self, max: usize) -> Result<Option<&'a [u8]>, ToolError> {
        let count = u32::from_le_bytes(self.array()?);
        if count == ABSENT {
            return Ok(None);
        }
        if count as usize > max {
            return Err(ToolError::Limit);
        }
        self.take(count as usize).map(Some)
    }

    fn text(&mut self, max: usize) -> Result<String, ToolError> {
        String::from_utf8(self.field(max)?.to_vec()).map_err(|_| ToolError::Configuration)
    }

    fn hash(&mut self) -> Result<Option<[u8; 32]>, ToolError> {
        self.optional(HASH_BYTES)?
            .map(|bytes| bytes.try_into().map_err(|_| ToolError::Configuration))
            .transpose()
    }

    fn reference(&mut self) -> Result<RefUpdate, ToolError> {
        let name = self.text(MAX_PATH_BYTES)?;
        if !name.starts_with("refs/heads/") || !lock_target(&name) {
            return Err(ToolError::Configuration);
        }
        let before = self
            .optional(OBJECT_ID_BYTES)?
            .map(|bytes| object_id(bytes).map(str::to_owned))
            .transpose()?;
        let after = object_id(self.field(OBJECT_ID_BYTES)?)?.to_owned();
        Ok(RefUpdate {
            name,
            before,
            after,
        })
    }

    fn objects(&mut self, mut total: usize) -> Result<(Vec<String>, Vec<Vec<u8>>), ToolError> {
        let mut names = Vec::new();
        let mut objects = Vec::new();
        for _ in 0..self.count(MAX_OBJECTS)? {
            let name = self.text(OBJECT_NAME_BYTES)?;
            if name.len() != OBJECT_NAME_BYTES || name.as_bytes()[2] != b'/' {
                return Err(ToolError::Configuration);
            }
            object_id(name.replace('/', "").as_bytes())?;
            if names.last().is_some_and(|previous| previous >= &name) {
                return Err(ToolError::Configuration);
            }
            let bytes = self.field(MAX_SNAPSHOT_BYTES.saturating_sub(total))?;
            total += bytes.len();
            names.push(name);
            objects.push(bytes.to_vec());
        }
        Ok((names, objects))
    }

    fn files(&mut self) -> Result<Vec<FileChange>, ToolError> {
        let mut files = Vec::new();
        let mut paths = BTreeSet::new();
        let mut total = 0;
        for _ in 0..self.count(MAX_SNAPSHOT_FILES)? {
            let path = self.text(MAX_PATH_BYTES)?;
            if !transfer_path(&path) || !paths.insert(path.clone()) {
                return Err(ToolError::Configuration);
            }
            let before = if self.count(1)? == 1 {
                Some(FileVersion {
                    sha256: plan::hex(self.array()?),
                    content: String::new(),
                    executable: self.count(1)? == 1,
                })
            } else {
                None
            };
            let after = if self.count(1)? == 1 {
                let executable = self.count(1)? == 1;
                let bytes = self.field(MAX_FILE_BYTES.min(MAX_SNAPSHOT_BYTES - total))?;
                total += bytes.len();
                Some(FileVersion::from_bytes(bytes, executable))
            } else {
                None
            };
            files.push(FileChange {
                path,
                before,
                after,
            });
        }
        Ok(files)
    }
}

fn transfer_path(path: &str) -> bool {
    !path.is_empty()
        && !path.chars().any(char::is_control)
        && !path.contains('\\')
        && path.split('/').all(|part| {
            !part.is_empty() && part != "." && part != ".." && !crate::validate::forbidden(part)
        })
}

fn lock_target(name: &str) -> bool {
    matches!(name, "HEAD" | "packed-refs")
        || name.strip_prefix("refs/").is_some_and(|rest| {
            rest.split('/').all(|part| {
                !part.is_empty()
                    && !part.starts_with('.')
                    && !part.ends_with('.')
                    && !part.ends_with(".lock")
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            })
        })
}

fn parse_hash(text: &str) -> Result<[u8; 32], ToolError> {
    if !valid_digest(text) {
        return Err(ToolError::Configuration);
    }
    let mut hash = [0; 32];
    for (byte, pair) in hash.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
        *byte = (pair[0] as char)
            .to_digit(16)
            .ok_or(ToolError::Configuration)? as u8
            * 16
            + (pair[1] as char)
                .to_digit(16)
                .ok_or(ToolError::Configuration)? as u8;
    }
    Ok(hash)
}

#[cfg(test)]
mod tests;
