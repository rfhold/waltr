use serde::{
    Deserialize, Deserializer,
    de::{self, SeqAccess, Visitor},
};
use std::{fmt, marker::PhantomData};

pub fn string<'de, D: Deserializer<'de>, const N: usize>(
    deserializer: D,
) -> Result<String, D::Error> {
    struct BoundedString<const N: usize>;
    impl<const N: usize> Visitor<'_> for BoundedString<N> {
        type Value = String;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a bounded string")
        }
        fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
            if value.len() > N {
                return Err(E::custom("string size limit exceeded"));
            }
            Ok(value.to_owned())
        }
        fn visit_string<E: de::Error>(self, value: String) -> Result<String, E> {
            if value.len() > N {
                return Err(E::custom("string size limit exceeded"));
            }
            Ok(value)
        }
    }
    deserializer.deserialize_string(BoundedString::<N>)
}

pub fn vec<'de, D: Deserializer<'de>, T: Deserialize<'de>, const N: usize>(
    deserializer: D,
) -> Result<Vec<T>, D::Error> {
    struct BoundedVec<T, const N: usize>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for BoundedVec<T, N> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a bounded sequence")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Vec<T>, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = sequence.next_element()? {
                if values.len() == N {
                    return Err(de::Error::custom("item count limit exceeded"));
                }
                values.push(value);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(BoundedVec::<T, N>(PhantomData))
}
