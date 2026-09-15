//! Shared cursor advancement for projection metadata reads and the test oracle.

const BATCH: usize = 512;

/// Drain a cursor-paged read family, retaining adaptive batch-size retry.
pub(crate) fn drain_after<K: Clone, R, E>(
    mut fetch: impl FnMut(Option<K>, usize) -> Result<Vec<R>, E>,
    mut key: impl FnMut(&R) -> K,
    mut emit: impl FnMut(R) -> Result<(), E>,
    mut retry_batch: impl FnMut(&E, usize) -> Option<usize>,
) -> Result<(), E> {
    let mut cursor = None;
    let mut batch = BATCH;
    loop {
        let rows = match fetch(cursor.clone(), batch) {
            Ok(rows) => rows,
            Err(error) => match retry_batch(&error, batch) {
                Some(next) if next > 0 && next < batch => {
                    batch = next;
                    continue;
                }
                _ => return Err(error),
            },
        };
        let len = rows.len();
        for row in rows {
            cursor = Some(key(&row));
            emit(row)?;
        }
        if len < batch {
            return Ok(());
        }
    }
}
