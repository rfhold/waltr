use std::collections::HashSet;

use aws_sdk_s3::Client;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{ImageObjectError, next_total, read_s3_counter};

pub(super) const UNIT: u64 = 1024 * 1024;
const MAX_ENTRIES: usize = 1024;
const MAX_PAGE: usize = 1000;
const STRIPES: usize = 64;
static LOCKS: [Mutex<()>; STRIPES] = [const { Mutex::const_new(()) }; STRIPES];

pub(super) async fn charge(
    client: &Client,
    bucket: &str,
    counter_key: &str,
    owner: &str,
    amount: u64,
    limit: u64,
) -> Result<(), ImageObjectError> {
    let units = amount.div_ceil(UNIT);
    let rounded = units.checked_mul(UNIT).ok_or(ImageObjectError::Capacity)?;
    next_total(0, rounded, limit)?;
    let stripe =
        usize::from(u8::from_str_radix(&owner[..2], 16).map_err(|_| ImageObjectError::Invalid)?)
            % STRIPES;
    // Deployment permits only one process to write a single-writer namespace.
    let _guard = LOCKS[stripe].lock().await;
    let (counter_total, _) = read_s3_counter(client, bucket, counter_key).await?;
    let ledger_total = total(client, bucket, counter_key, limit).await?;
    let used = next_total(counter_total, ledger_total, limit)?;
    next_total(used, rounded, limit)?;

    // A late uncertain PUT may add charge but cannot replace a prior entry.
    let key = format!(
        "{counter_key}/ledger/{units:04}-{}",
        Uuid::new_v4().simple()
    );
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .if_none_match("*")
        .body(aws_sdk_s3::primitives::ByteStream::from_static(b""))
        .send()
        .await
        .map_err(|_| ImageObjectError::Unavailable)?;
    Ok(())
}

pub(super) async fn total(
    client: &Client,
    bucket: &str,
    counter_key: &str,
    limit: u64,
) -> Result<u64, ImageObjectError> {
    let prefix = format!("{counter_key}/ledger/");
    let mut tokens = HashSet::new();
    let mut keys = HashSet::new();
    let mut continuation = None;
    let mut total = 0;
    loop {
        let max_keys = (MAX_ENTRIES + 1 - keys.len()).min(MAX_PAGE);
        let page = client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(&prefix)
            .max_keys(i32::try_from(max_keys).map_err(|_| ImageObjectError::Unavailable)?)
            .set_continuation_token(continuation)
            .send()
            .await
            .map_err(|_| ImageObjectError::Unavailable)?;
        let entries = page.contents();
        if entries.len() > max_keys
            || !page.common_prefixes().is_empty()
            || page
                .key_count()
                .is_some_and(|count| usize::try_from(count).ok() != Some(entries.len()))
        {
            return Err(ImageObjectError::Corrupt);
        }
        for entry in entries {
            let suffix = entry
                .key()
                .and_then(|key| key.strip_prefix(&prefix))
                .ok_or(ImageObjectError::Corrupt)?;
            let amount = entry_amount(suffix)?;
            if entry.size() != Some(0) || !keys.insert(suffix.to_owned()) {
                return Err(ImageObjectError::Corrupt);
            }
            total = next_total(total, amount, limit)?;
        }
        if keys.len() > MAX_ENTRIES {
            return Err(ImageObjectError::Capacity);
        }
        match (page.is_truncated(), page.next_continuation_token()) {
            (Some(false), None) => return Ok(total),
            (Some(true), Some(token))
                if !entries.is_empty()
                    && !token.is_empty()
                    && token.len() <= 4096
                    && tokens.insert(token.to_owned()) =>
            {
                continuation = Some(token.to_owned());
            }
            _ => return Err(ImageObjectError::Corrupt),
        }
    }
}

fn entry_amount(entry: &str) -> Result<u64, ImageObjectError> {
    let (units, id) = entry.split_once('-').ok_or(ImageObjectError::Corrupt)?;
    if units.len() != 4
        || !units.bytes().all(|byte| byte.is_ascii_digit())
        || id.len() != 32
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || Uuid::parse_str(id).map_or(true, |id| id.get_version_num() != 4)
    {
        return Err(ImageObjectError::Corrupt);
    }
    let units = units
        .parse::<u64>()
        .map_err(|_| ImageObjectError::Corrupt)?;
    if !(1..=MAX_ENTRIES as u64).contains(&units) {
        return Err(ImageObjectError::Corrupt);
    }
    Ok(units * UNIT)
}
