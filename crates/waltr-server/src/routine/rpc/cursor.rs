//! Authenticated, durable observation positions, independent of execution authority.
use crate::routine::observation::ObservationScope;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest as _, Sha256};
use tonic::Status;

#[derive(Clone, Copy)]
pub(crate) struct Cursor {
    pub high: i64,
    pub position: i64,
    pub snapshot: bool,
}
pub(crate) struct Codec([u8; 32]);
impl Codec {
    pub fn new(key: [u8; 32]) -> Self {
        Self(key)
    }
    pub fn encode(&self, scope: &ObservationScope, cursor: Cursor) -> String {
        let mut message = vec![1, u8::from(cursor.snapshot)];
        message.extend_from_slice(&cursor.high.to_be_bytes());
        message.extend_from_slice(&cursor.position.to_be_bytes());
        message.extend_from_slice(&partition(scope));
        let tag = self.mac(&message);
        message.extend_from_slice(&tag);
        URL_SAFE_NO_PAD.encode(message)
    }
    pub fn decode(&self, scope: &ObservationScope, value: &str) -> Result<Cursor, Status> {
        let invalid =
            || Status::out_of_range("observation cursor unavailable; resnapshot required");
        if value.len() > 1024 {
            return Err(invalid());
        }
        let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?;
        if bytes.len() != 82 || bytes[0] != 1 || bytes[1] > 1 {
            return Err(invalid());
        }
        let tag = self.mac(&bytes[..50]);
        if !equal(&bytes[50..], &tag) || !equal(&bytes[18..50], &partition(scope)) {
            return Err(invalid());
        }
        let high = i64::from_be_bytes(bytes[2..10].try_into().map_err(|_| invalid())?);
        let position = i64::from_be_bytes(bytes[10..18].try_into().map_err(|_| invalid())?);
        if high < 0 || position < 0 || (bytes[1] == 1 && position > high) {
            return Err(invalid());
        }
        Ok(Cursor {
            high,
            position,
            snapshot: bytes[1] == 1,
        })
    }
    fn mac(&self, message: &[u8]) -> [u8; 32] {
        let mut inner = [0x36; 64];
        let mut outer = [0x5c; 64];
        for (i, byte) in self.0.iter().enumerate() {
            inner[i] ^= *byte;
            outer[i] ^= *byte;
        }
        let mut hash = Sha256::new();
        hash.update(inner);
        hash.update(message);
        let digest = hash.finalize();
        let mut hash = Sha256::new();
        hash.update(outer);
        hash.update(digest);
        hash.finalize().as_slice().try_into().expect("SHA256 size")
    }
}
fn partition(scope: &ObservationScope) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"waltr:routine-observation:v1\0");
    hash.update((scope.principal.len() as u64).to_be_bytes());
    hash.update(scope.principal.as_bytes());
    hash.update([u8::from(scope.source_session.is_some())]);
    if let Some(source) = &scope.source_session {
        hash.update((source.len() as u64).to_be_bytes());
        hash.update(source.as_bytes());
    }
    hash.finalize().as_slice().try_into().expect("SHA256 size")
}
pub(super) fn equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routine_cursor_rfc4231_and_partition_restart_binding() {
        let mut key = [0; 32];
        key[..20].fill(0x0b);
        assert_eq!(
            super::super::hex(&Codec::new(key).mac(b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        let scope = ObservationScope {
            principal: "owner".into(),
            source_session: Some("source".into()),
        };
        let token = Codec::new(key).encode(
            &scope,
            Cursor {
                high: 19,
                position: 7,
                snapshot: true,
            },
        );
        let decoded = Codec::new(key).decode(&scope, &token).unwrap();
        assert_eq!(
            (decoded.high, decoded.position, decoded.snapshot),
            (19, 7, true)
        );
        for other in [
            ObservationScope {
                principal: "foreign".into(),
                source_session: Some("source".into()),
            },
            ObservationScope {
                principal: "owner".into(),
                source_session: None,
            },
        ] {
            assert_eq!(
                Codec::new(key).decode(&other, &token).err().unwrap().code(),
                tonic::Code::OutOfRange
            );
        }
        assert!(Codec::new([1; 32]).decode(&scope, &token).is_err());
        assert!(Codec::new(key).decode(&scope, &(token + "x")).is_err());
    }
}
