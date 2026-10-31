use serde::Deserialize;

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
