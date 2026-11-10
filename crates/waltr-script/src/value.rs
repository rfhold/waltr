use crate::{Error, Limits};
use serde_json::Value;
use std::io::{self, Write};

/// Owned JSON-compatible data. Every engine boundary revalidates its own limits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructuredValue(Value);

impl StructuredValue {
    /// Validate bounded depth, nodes, strings, encoded bytes, and exact numbers.
    ///
    /// # Errors
    /// Returns `Invalid` for limits or `Value` for unsupported or oversized data.
    pub fn new(value: Value, limits: &Limits) -> Result<Self, Error> {
        limits.validate()?;
        bounded_encode(&value, limits)?;
        Ok(Self(value))
    }
    /// Borrow the validated data.
    #[must_use]
    pub const fn as_json(&self) -> &Value {
        &self.0
    }
    /// Recover owned data.
    #[must_use]
    pub fn into_json(self) -> Value {
        self.0
    }
    pub(crate) fn encode(&self, limits: &Limits) -> Result<Vec<u8>, Error> {
        limits.validate()?;
        bounded_encode(&self.0, limits)
    }
    pub(crate) fn decode(bytes: &[u8], limits: &Limits) -> Result<Self, Error> {
        if bytes.len() > limits.value_bytes {
            return Err(Error::Value);
        }
        Self::new(
            serde_json::from_slice(bytes).map_err(|_| Error::Value)?,
            limits,
        )
    }
}

fn bounded_encode(value: &Value, limits: &Limits) -> Result<Vec<u8>, Error> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "value limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut nodes = 0;
    check(value, limits, 0, &mut nodes)?;
    let mut output = Bounded {
        bytes: Vec::new(),
        limit: limits.value_bytes,
    };
    serde_json::to_writer(&mut output, value).map_err(|_| Error::Value)?;
    Ok(output.bytes)
}

fn check(value: &Value, limits: &Limits, depth: usize, nodes: &mut usize) -> Result<(), Error> {
    *nodes += 1;
    if depth > limits.value_depth || *nodes > limits.value_nodes {
        return Err(Error::Value);
    }
    match value {
        Value::String(s) if s.len() > limits.string_bytes => return Err(Error::Value),
        Value::Number(n) => {
            let number = n.as_f64().ok_or(Error::Value)?;
            if !number.is_finite()
                || (number.fract() == 0.0 && number.abs() > 9_007_199_254_740_991.0)
            {
                return Err(Error::Value);
            }
        }
        Value::Array(values) => {
            for value in values {
                check(value, limits, depth + 1, nodes)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                *nodes += 1;
                if key.len() > limits.string_bytes {
                    return Err(Error::Value);
                }
                check(value, limits, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}
