use std::{
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write as _},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use waltr_agent::S3SessionWriteMode;

use super::ImageObjectError;

mod ledger;

pub(super) const SAVED_IMAGE_BYTES_PER_PRINCIPAL: u64 = 1024 * 1024 * 1024;
pub(super) const NAMESPACE: &str = "image-capacity-v1";
const COUNTER_BYTES: usize = size_of::<u64>();
const S3_ATTEMPTS: usize = 4;

pub(super) enum Capacity {
    Filesystem(PathBuf),
    S3 {
        client: aws_sdk_s3::Client,
        bucket: String,
        prefix: String,
        write_mode: S3SessionWriteMode,
    },
    #[cfg(test)]
    Unmetered,
}

impl Capacity {
    pub(super) async fn charge(
        &self,
        exact_identity: &str,
        amount: u64,
        limit: u64,
    ) -> Result<(), ImageObjectError> {
        next_total(0, amount, limit)?;
        let key = owner_key(exact_identity);
        match self {
            Self::Filesystem(root) => {
                let root = root.clone();
                tokio::task::spawn_blocking(move || filesystem_charge(&root, &key, amount, limit))
                    .await
                    .map_err(|_| ImageObjectError::Unavailable)?
            }
            Self::S3 {
                client,
                bucket,
                prefix,
                write_mode,
            } => {
                let counter_key = format!("{prefix}/{key}");
                match write_mode {
                    S3SessionWriteMode::ConditionalEtagCas => {
                        s3_charge(client, bucket, &counter_key, amount, limit).await
                    }
                    S3SessionWriteMode::SingleWriter => {
                        ledger::charge(client, bucket, &counter_key, &key, amount, limit).await
                    }
                }
            }
            #[cfg(test)]
            Self::Unmetered => Ok(()),
        }
    }
}

fn owner_key(exact_identity: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"waltr-saved-image-capacity-v1\0");
    hash.update(exact_identity.as_bytes());
    let mut key = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(&mut key, "{byte:02x}").expect("String writes cannot fail");
    }
    key
}

fn next_total(used: u64, amount: u64, limit: u64) -> Result<u64, ImageObjectError> {
    used.checked_add(amount)
        .filter(|next| *next <= limit)
        .ok_or(ImageObjectError::Capacity)
}

fn decode(counter: [u8; COUNTER_BYTES]) -> Result<u64, ImageObjectError> {
    let used = u64::from_be_bytes(counter);
    if used == 0 {
        return Err(ImageObjectError::Corrupt);
    }
    Ok(used)
}

fn filesystem_charge(
    root: &Path,
    key: &str,
    amount: u64,
    limit: u64,
) -> Result<(), ImageObjectError> {
    let mut directories = fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories.create(root).map_err(unavailable)?;

    let mut private_file = OpenOptions::new();
    private_file.create(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        private_file.mode(0o600);
    }

    // Never replace the lock inode. All independently opened stores lock it
    // while reading, replacing and syncing the counter's directory entry.
    let lock = private_file
        .open(root.join(format!("{key}.lock")))
        .map_err(unavailable)?;
    lock.lock().map_err(unavailable)?;
    let counter = root.join(key);
    let used = match File::open(&counter) {
        Ok(mut file) => {
            let mut bytes = [0; COUNTER_BYTES];
            file.read_exact(&mut bytes).map_err(counter_read_error)?;
            if file.read(&mut [0]).map_err(unavailable)? != 0 {
                return Err(ImageObjectError::Corrupt);
            }
            decode(bytes)?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(unavailable(error)),
    };
    let next = next_total(used, amount, limit)?;

    // At most one pending file per principal, including after a crash.
    let pending = root.join(format!("{key}.pending"));
    let mut file = private_file
        .truncate(true)
        .open(&pending)
        .map_err(unavailable)?;
    file.write_all(&next.to_be_bytes()).map_err(unavailable)?;
    file.sync_all().map_err(unavailable)?;
    drop(file);
    fs::rename(pending, counter).map_err(unavailable)?;

    // Include ancestors because this may be the first write in a new namespace.
    for directory in root.ancestors() {
        File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(unavailable)?;
    }
    Ok(())
}

fn unavailable(_: io::Error) -> ImageObjectError {
    ImageObjectError::Unavailable
}

fn counter_read_error(error: io::Error) -> ImageObjectError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        ImageObjectError::Corrupt
    } else {
        unavailable(error)
    }
}

async fn s3_charge(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    amount: u64,
    limit: u64,
) -> Result<(), ImageObjectError> {
    let ledger_total = ledger::total(client, bucket, key, limit).await?;
    for _ in 0..S3_ATTEMPTS {
        let (used, etag) = read_s3_counter(client, bucket, key).await?;
        let counter_limit = limit
            .checked_sub(ledger_total)
            .ok_or(ImageObjectError::Capacity)?;
        let next = next_total(used, amount, counter_limit)?;
        let put = client.put_object().bucket(bucket).key(key).body(
            aws_sdk_s3::primitives::ByteStream::from(next.to_be_bytes().to_vec()),
        );
        let put = match etag {
            Some(etag) => put.if_match(etag),
            None => put.if_none_match("*"),
        };
        match put.send().await {
            Ok(_) => return Ok(()),
            Err(error) if error.raw_response().map(|r| r.status().as_u16()) == Some(412) => {}
            // A timeout, service failure or ambiguous acknowledgement is not
            // proof the charge failed. Never retry or refund it.
            Err(_) => return Err(ImageObjectError::Unavailable),
        }
    }
    Err(ImageObjectError::Unavailable)
}

async fn read_s3_counter(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
) -> Result<(u64, Option<String>), ImageObjectError> {
    match client.get_object().bucket(bucket).key(key).send().await {
        Ok(object) => {
            if object.content_length() != Some(COUNTER_BYTES as i64) {
                return Err(ImageObjectError::Corrupt);
            }
            let etag = object.e_tag().filter(|etag| strong_etag(etag));
            let etag = etag.ok_or(ImageObjectError::Corrupt)?.to_owned();
            let mut body = object.body.into_async_read();
            let mut bytes = [0; COUNTER_BYTES];
            body.read_exact(&mut bytes)
                .await
                .map_err(counter_read_error)?;
            if body.read(&mut [0]).await.map_err(unavailable)? != 0 {
                return Err(ImageObjectError::Corrupt);
            }
            Ok((decode(bytes)?, Some(etag)))
        }
        Err(error)
            if error.raw_response().map(|r| r.status().as_u16()) == Some(404)
                && error.as_service_error().is_some_and(|e| e.is_no_such_key()) =>
        {
            Ok((0, None))
        }
        Err(_) => Err(ImageObjectError::Unavailable),
    }
}

fn strong_etag(etag: &str) -> bool {
    etag.strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .is_some_and(|value| {
            !value.is_empty() && value.bytes().all(|b| b.is_ascii_graphic() && b != b'"')
        })
}

#[cfg(test)]
mod tests;
