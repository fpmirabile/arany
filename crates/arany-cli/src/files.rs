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
    struct Visitor<T, const LIMIT: usize> {
        limit_error: &'static str,
        marker: std::marker::PhantomData<T>,
    }
    impl<'de, T: Deserialize<'de>, const LIMIT: usize> serde::de::Visitor<'de> for Visitor<T, LIMIT> {
        type Value = Vec<T>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "at most {LIMIT} entries")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Vec<T>, A::Error> {
            let mut values = Vec::new();
            while values.len() < LIMIT {
                let Some(value) = sequence.next_element()? else {
                    return Ok(values);
                };
                values.push(value);
            }
            if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom(self.limit_error));
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Visitor::<T, LIMIT> {
        limit_error,
        marker: std::marker::PhantomData,
    })
}
