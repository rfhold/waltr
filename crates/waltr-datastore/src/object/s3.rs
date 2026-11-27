use std::time::Duration;

use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Builder, retry::RetryConfig};
use aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::EncodingType;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::timeout;
use uuid::Uuid;

use super::{
    Actor, Attribution, Error, Namespace, OPERATION_TIME, ObjectInfo, ObjectOperation,
    RECEIPT_BYTES, RECEIPT_VERSION, RECEIPTS, Reader, Receipt, S3ReplacePolicy, SCAN_LIMIT,
    object_info, object_key, receipt_name, spool,
};

const TOKEN_METADATA: &str = "waltr-datastore-token";
const REQUEST_TIME: Duration = Duration::from_secs(45);

#[derive(Clone)]
pub(super) struct S3 {
    client: Client,
    bucket: String,
    prefix: String,
    replace_policy: S3ReplacePolicy,
}

#[derive(Default)]
struct StoredReceipt {
    receipt: Option<Receipt>,
    etag: Option<String>,
}

struct Existing {
    etag: Option<String>,
    version: Option<String>,
    identity: Option<String>,
    size: u64,
}

impl S3 {
    pub(super) fn new(
        client: Client,
        bucket: String,
        prefix: String,
        replace_policy: S3ReplacePolicy,
    ) -> Result<Self, Error> {
        if bucket.is_empty()
            || (!prefix.is_empty() && object_key(&prefix).is_err())
            || prefix.len() > 44
        {
            return Err(Error::Invalid);
        }
        let prefix = if prefix.is_empty() {
            prefix
        } else {
            format!("{prefix}/")
        };
        Ok(Self {
            client,
            bucket,
            prefix,
            replace_policy,
        })
    }

    fn content_prefix(&self, ns: &Namespace) -> String {
        format!("{}{}/", self.prefix, ns.as_str())
    }

    fn content_key(&self, ns: &Namespace, key: &str) -> String {
        format!("{}{key}", self.content_prefix(ns))
    }

    fn receipt_key(&self, ns: &Namespace, key: &str) -> String {
        format!(
            "{}{RECEIPTS}/{RECEIPT_VERSION}/{}/{}",
            self.prefix,
            ns.as_str(),
            receipt_name(key),
        )
    }

    async fn head(&self, physical: &str) -> Result<Existing, Error> {
        let result = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(physical)
            .send()
            .await
            .map_err(|err| read_error(err.raw_response().map(|r| r.status().as_u16())))?;
        let version = version(result.version_id()).map(str::to_owned);
        let identity = identity(
            version.as_deref(),
            result.metadata().and_then(|m| m.get(TOKEN_METADATA)),
        );
        Ok(Existing {
            etag: result.e_tag().map(str::to_owned),
            version,
            identity,
            size: size(result.content_length())?,
        })
    }

    async fn no_ancestor(&self, ns: &Namespace, key: &str) -> Result<(), Error> {
        let prefix = self.content_prefix(ns);
        for (index, _) in key.match_indices('/') {
            match self.head(&format!("{prefix}{}", &key[..index])).await {
                Ok(_) => return Err(Error::Conflict),
                Err(Error::NotFound) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    async fn no_descendants(&self, physical: &str) -> Result<(), Error> {
        let result = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(format!("{physical}/"))
            .max_keys(1)
            .send()
            .await
            .map_err(|_| Error::Unavailable)?;
        if result.contents().is_empty() {
            Ok(())
        } else {
            Err(Error::Conflict)
        }
    }

    async fn stored_receipt(&self, physical: &str) -> Result<StoredReceipt, Error> {
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(physical)
            .send()
            .await;
        let result = match result {
            Ok(result) => result,
            Err(err) => match read_error(err.raw_response().map(|r| r.status().as_u16())) {
                Error::NotFound => return Ok(StoredReceipt::default()),
                err => return Err(err),
            },
        };
        let etag = result.e_tag().ok_or(Error::Unavailable)?.to_owned();
        let receipt = if size(result.content_length())? <= RECEIPT_BYTES as u64 {
            let mut bytes = Vec::new();
            result
                .body
                .into_async_read()
                .take(RECEIPT_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| Error::Unavailable)?;
            Receipt::parse(&bytes)
        } else {
            None
        };
        Ok(StoredReceipt {
            receipt,
            etag: Some(etag),
        })
    }

    async fn read_receipt(&self, ns: &Namespace, key: &str) -> Option<Receipt> {
        timeout(
            REQUEST_TIME,
            self.stored_receipt(&self.receipt_key(ns, key)),
        )
        .await
        .ok()?
        .ok()?
        .receipt
    }

    async fn write_receipt(
        &self,
        physical: &str,
        old: StoredReceipt,
        receipt: &Receipt,
        policy: S3ReplacePolicy,
    ) -> Result<(), Error> {
        let put = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(physical)
            .body(ByteStream::from(receipt.serialize()?))
            .content_type("application/json");
        let put = put_condition(put, old.etag.as_deref(), policy);
        put.customize()
            .config_override(no_retry())
            .send()
            .await
            .map_err(|_| Error::Uncertain)?;
        Ok(())
    }

    async fn previous_version(
        &self,
        physical: &str,
        new_version: Option<&str>,
        old_version: Option<&str>,
    ) -> bool {
        let (Some(new_version), Some(old_version)) = (new_version, old_version) else {
            return false;
        };
        let Ok(result) = self
            .client
            .list_object_versions()
            .bucket(&self.bucket)
            .prefix(physical)
            .key_marker(physical)
            .version_id_marker(new_version)
            .encoding_type(EncodingType::Url)
            .max_keys(1)
            .send()
            .await
        else {
            return false;
        };
        result.delete_markers().is_empty()
            && result.versions().first().is_some_and(|item| {
                item.key().and_then(decode_key).as_deref() == Some(physical)
                    && item.version_id() == Some(old_version)
            })
    }

    pub(super) async fn stat(&self, ns: &Namespace, key: &str) -> Result<ObjectInfo, Error> {
        self.no_ancestor(ns, key).await.map_err(hide_collision)?;
        let object = self.head(&self.content_key(ns, key)).await?;
        let receipt = self.read_receipt(ns, key).await;
        Ok(object_info(
            key,
            object.size,
            object.identity.as_deref(),
            receipt,
        ))
    }

    pub(super) async fn read(
        &self,
        ns: &Namespace,
        key: &str,
    ) -> Result<(ObjectInfo, Reader), Error> {
        self.no_ancestor(ns, key).await.map_err(hide_collision)?;
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.content_key(ns, key))
            .send()
            .await
            .map_err(|err| read_error(err.raw_response().map(|r| r.status().as_u16())))?;
        let size = size(result.content_length())?;
        let identity = identity(
            version(result.version_id()),
            result.metadata().and_then(|m| m.get(TOKEN_METADATA)),
        );
        let receipt = self.read_receipt(ns, key).await;
        let info = object_info(key, size, identity.as_deref(), receipt);
        Ok((info, Box::new(result.body.into_async_read())))
    }

    pub(super) async fn list(
        &self,
        ns: &Namespace,
        prefix: Option<&str>,
        after: Option<&str>,
        count: usize,
    ) -> Result<Vec<ObjectInfo>, Error> {
        let mut items = Vec::new();
        if let Some(prefix) = prefix {
            if after.is_none() {
                match self.stat(ns, prefix).await {
                    Ok(item) => return Ok(vec![item]),
                    Err(Error::NotFound) => {}
                    Err(err) => return Err(err),
                }
            }
            match self.no_ancestor(ns, &format!("{prefix}/")).await {
                Ok(()) => {}
                Err(Error::Conflict) => return Ok(items),
                Err(err) => return Err(err),
            }
        }
        let content = self.content_prefix(ns);
        let scan_prefix =
            prefix.map_or_else(|| content.clone(), |prefix| format!("{content}{prefix}/"));
        let mut token = None;
        let mut scanned = 0;
        loop {
            let request = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&scan_prefix)
                .encoding_type(EncodingType::Url)
                .max_keys(100)
                .set_continuation_token(token.clone());
            let request = if token.is_none() {
                request.set_start_after(after.map(|key| format!("{content}{key}")))
            } else {
                request
            };
            let page = request.send().await.map_err(|_| Error::Unavailable)?;
            for object in page.contents() {
                scanned += 1;
                if scanned > SCAN_LIMIT {
                    return Err(Error::Limit);
                }
                let Some(physical) = object.key().and_then(decode_key) else {
                    continue;
                };
                let Some(key) = physical.strip_prefix(&content) else {
                    continue;
                };
                if object_key(key).is_err() {
                    continue;
                }
                match self.stat(ns, key).await {
                    Ok(item) => items.push(item),
                    Err(Error::NotFound) => {}
                    Err(err) => return Err(err),
                }
                if items.len() == count {
                    return Ok(items);
                }
            }
            if !page.is_truncated().unwrap_or(false) {
                return Ok(items);
            }
            let next = page.next_continuation_token().ok_or(Error::Unavailable)?;
            if token.as_deref() == Some(next) {
                return Err(Error::Unavailable);
            }
            token = Some(next.to_owned());
        }
    }

    pub(super) async fn put(
        &self,
        ns: &Namespace,
        key: &str,
        actor: &Actor,
        body: impl AsyncRead + Unpin,
        operation: ObjectOperation,
        permit: OwnedSemaphorePermit,
    ) -> Result<ObjectInfo, Error> {
        let tempfile = tempfile::NamedTempFile::new().map_err(|_| Error::Unavailable)?;
        let mut file =
            tokio::fs::File::from_std(tempfile.reopen().map_err(|_| Error::Unavailable)?);
        let size = spool(body, &mut file).await?;
        file.flush().await.map_err(|_| Error::Unavailable)?;
        drop(file);
        let stream = ByteStream::from_path(tempfile.path())
            .await
            .map_err(|_| Error::Unavailable)?;
        let prepared = timeout(OPERATION_TIME, self.prepare(ns, key, operation))
            .await
            .map_err(|_| Error::Unavailable)??;
        let backend = self.clone();
        let key = key.to_owned();
        let actor = actor.clone();
        tokio::spawn(async move {
            let (_permit, _tempfile) = (permit, tempfile);
            timeout(
                OPERATION_TIME,
                backend.publish(key, actor, operation, size, stream, prepared),
            )
            .await
            .map_err(|_| Error::Uncertain)?
        })
        .await
        .map_err(|_| Error::Uncertain)?
    }

    async fn prepare(
        &self,
        ns: &Namespace,
        key: &str,
        operation: ObjectOperation,
    ) -> Result<Prepared, Error> {
        self.no_ancestor(ns, key).await.map_err(|err| {
            if operation == ObjectOperation::Create {
                err
            } else {
                hide_collision(err)
            }
        })?;
        let physical = self.content_key(ns, key);
        let old_object = if operation == ObjectOperation::Create {
            self.no_descendants(&physical).await?;
            None
        } else {
            let old = self.head(&physical).await?;
            if old.etag.is_none() {
                return Err(Error::Unavailable);
            }
            Some(old)
        };
        let receipt_key = self.receipt_key(ns, key);
        let stored = timeout(REQUEST_TIME, self.stored_receipt(&receipt_key))
            .await
            .map_err(|_| Error::Unavailable)??;
        Ok(Prepared {
            physical,
            old_object,
            receipt_key,
            stored,
        })
    }

    async fn publish(
        &self,
        key: String,
        actor: Actor,
        operation: ObjectOperation,
        size: u64,
        stream: ByteStream,
        prepared: Prepared,
    ) -> Result<ObjectInfo, Error> {
        let token = Uuid::new_v4().to_string();
        let put = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(&prepared.physical)
            .body(stream)
            .content_length(i64::try_from(size).map_err(|_| Error::Limit)?)
            .content_type("application/octet-stream")
            .metadata(TOKEN_METADATA, &token);
        let policy = if prepared.old_object.is_some() {
            self.replace_policy
        } else {
            S3ReplacePolicy::Conditional
        };
        let put = put_condition(
            put,
            prepared.old_object.as_ref().and_then(|old| old.etag.as_deref()),
            policy,
        );
        let result = put
            .customize()
            .config_override(no_retry())
            .send()
            .await
            .map_err(|err| write_error(err.raw_response().map(|r| r.status().as_u16())))?;
        let version = version(result.version_id());
        let identity = identity(version, Some(&token));
        let previous = self.verified_previous(&prepared, version).await;
        let receipt = Receipt::new(operation, &actor, previous, identity.clone());
        self.write_receipt(&prepared.receipt_key, prepared.stored, &receipt, policy)
            .await?;
        Ok(object_info(&key, size, identity.as_deref(), Some(receipt)))
    }

    pub(super) async fn delete(
        &self,
        ns: &Namespace,
        key: &str,
        actor: &Actor,
        permit: OwnedSemaphorePermit,
    ) -> Result<(), Error> {
        let prepared = timeout(
            OPERATION_TIME,
            self.prepare(ns, key, ObjectOperation::Delete),
        )
        .await
        .map_err(|_| Error::Unavailable)??;
        let backend = self.clone();
        let actor = actor.clone();
        tokio::spawn(async move {
            let _permit = permit;
            timeout(OPERATION_TIME, backend.remove(prepared, actor))
                .await
                .map_err(|_| Error::Uncertain)?
        })
        .await
        .map_err(|_| Error::Uncertain)?
    }

    async fn remove(&self, prepared: Prepared, actor: Actor) -> Result<(), Error> {
        let etag = prepared
            .old_object
            .as_ref()
            .and_then(|old| old.etag.as_deref())
            .ok_or(Error::Unavailable)?;
        let result = self
            .client
            .delete_object()
            .bucket(&self.bucket)
            .key(&prepared.physical)
            .if_match(etag)
            .customize()
            .config_override(no_retry())
            .send()
            .await
            .map_err(|err| write_error(err.raw_response().map(|r| r.status().as_u16())))?;
        let previous = self
            .verified_previous(&prepared, version(result.version_id()))
            .await;
        let receipt = Receipt::new(ObjectOperation::Delete, &actor, previous, None);
        self.write_receipt(
            &prepared.receipt_key,
            prepared.stored,
            &receipt,
            S3ReplacePolicy::Conditional,
        )
        .await
    }

    async fn verified_previous(
        &self,
        prepared: &Prepared,
        new_version: Option<&str>,
    ) -> Attribution {
        if let Some((old, receipt)) = prepared
            .old_object
            .as_ref()
            .zip(prepared.stored.receipt.as_ref())
            && old.identity.is_some()
            && old.identity == receipt.identity
            && timeout(
                REQUEST_TIME,
                self.previous_version(&prepared.physical, new_version, old.version.as_deref()),
            )
            .await
            .unwrap_or(false)
        {
            return receipt.write.attribution.clone();
        }
        Attribution::default()
    }
}

struct Prepared {
    physical: String,
    old_object: Option<Existing>,
    receipt_key: String,
    stored: StoredReceipt,
}

fn put_condition(
    put: PutObjectFluentBuilder,
    etag: Option<&str>,
    policy: S3ReplacePolicy,
) -> PutObjectFluentBuilder {
    match (etag, policy) {
        (None, _) => put.if_none_match("*"),
        (Some(etag), S3ReplacePolicy::Conditional) => put.if_match(etag),
        (Some(_), S3ReplacePolicy::AllowStaleOverwrite) => put,
    }
}

fn no_retry() -> Builder {
    Builder::new().retry_config(RetryConfig::disabled())
}

fn version(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty() && *value != "null" && value.len() <= 1024)
}

fn decode_key(value: &str) -> Option<String> {
    fn unhex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }

    let mut decoded = Vec::with_capacity(value.len());
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = unhex(bytes.next()?)?;
            let low = unhex(bytes.next()?)?;
            decoded.push(high * 16 + low);
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).ok()
}

fn identity(version: Option<&str>, token: Option<&String>) -> Option<String> {
    let (version, token) = (version?, token?);
    Uuid::parse_str(token).ok()?;
    Some(format!("{token}:{version}"))
}

fn size(value: Option<i64>) -> Result<u64, Error> {
    u64::try_from(value.ok_or(Error::Unavailable)?).map_err(|_| Error::Unavailable)
}

fn hide_collision(error: Error) -> Error {
    if error == Error::Conflict {
        Error::NotFound
    } else {
        error
    }
}

fn read_error(code: Option<u16>) -> Error {
    match code {
        Some(404) => Error::NotFound,
        _ => Error::Unavailable,
    }
}

fn write_error(code: Option<u16>) -> Error {
    match code {
        Some(404) => Error::NotFound,
        Some(409 | 412) => Error::Conflict,
        Some(400 | 401 | 403 | 405 | 501) => Error::Unavailable,
        _ => Error::Uncertain,
    }
}
