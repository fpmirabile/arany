use arany::StateRoot;
use serde::Deserialize;

pub(crate) fn open_optional_state(
    path: &std::path::Path,
) -> Result<Option<StateRoot>, arany::StoreError> {
    match StateRoot::open_existing(path) {
        Ok(root) => Ok(Some(root)),
        Err(arany::StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(path) {
                Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => Ok(None),
                _ => Err(arany::StoreError::Io(error)),
            }
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn bounded_list<'de, D, T, const LIMIT: usize>(
    deserializer: D,
    limit_error: &'static str,
) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    arany::bounded_vec::<D, T, LIMIT>(deserializer, limit_error)
}
