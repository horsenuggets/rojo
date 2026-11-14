use hyper::{header::CONTENT_TYPE, Body, Response, StatusCode};
use serde::{Deserialize, Serialize};

/// Builds an HTTP response, falling back to an empty `500` response (rather than
/// panicking) if the response could not be constructed. With constant headers
/// and a valid status code this never actually fails, but routing every
/// response through here means a malformed response can never crash the server.
pub fn response(
    code: StatusCode,
    content_type: &'static str,
    body: impl Into<Body>,
) -> Response<Body> {
    Response::builder()
        .status(code)
        .header(CONTENT_TYPE, content_type)
        .body(body.into())
        .unwrap_or_else(|err| {
            log::error!("Failed to build HTTP response: {}", err);
            let mut fallback = Response::new(Body::empty());
            *fallback.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            fallback
        })
}

pub fn msgpack_ok<T: Serialize>(value: T) -> Response<Body> {
    msgpack(value, StatusCode::OK)
}

pub fn msgpack<T: Serialize>(value: T, code: StatusCode) -> Response<Body> {
    let mut serialized = Vec::new();
    let mut serializer = rmp_serde::Serializer::new(&mut serialized)
        .with_human_readable()
        .with_struct_map();

    if let Err(err) = value.serialize(&mut serializer) {
        return response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "text/plain",
            err.to_string(),
        );
    };

    response(code, "application/msgpack", serialized)
}

pub fn serialize_msgpack<T: Serialize>(value: T) -> anyhow::Result<Vec<u8>> {
    let mut serialized = Vec::new();
    let mut serializer = rmp_serde::Serializer::new(&mut serialized)
        .with_human_readable()
        .with_struct_map();

    value.serialize(&mut serializer)?;

    Ok(serialized)
}

pub fn deserialize_msgpack<'a, T: Deserialize<'a>>(
    input: &'a [u8],
) -> Result<T, rmp_serde::decode::Error> {
    let mut deserializer = rmp_serde::Deserializer::new(input).with_human_readable();

    T::deserialize(&mut deserializer)
}

pub fn json<T: Serialize>(value: T, code: StatusCode) -> Response<Body> {
    let serialized = match serde_json::to_string(&value) {
        Ok(v) => v,
        Err(err) => {
            return response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "text/plain",
                err.to_string(),
            );
        }
    };

    response(code, "application/json", serialized)
}

#[cfg(test)]
mod test {
    use super::*;
    use rbx_dom_weak::types::Variant;

    /// The plugin's msgpack encoder writes NaN and infinity as 32-bit floats,
    /// so a `Float64` attribute must still read them back from that form.
    #[test]
    fn non_finite_floats_from_the_plugin_decode_as_float64() {
        for (bytes, expected) in [
            ([0x7F, 0x80, 0x00, 0x00], f64::INFINITY),
            ([0xFF, 0x80, 0x00, 0x00], f64::NEG_INFINITY),
            ([0x7F, 0x80, 0x00, 0x01], f64::NAN),
        ] {
            // { "Float64": <float 32> }, as rbx_dom_lua encodes a number.
            let mut message = vec![0x81, 0xA7];
            message.extend_from_slice(b"Float64");
            message.push(0xCA);
            message.extend_from_slice(&bytes);
            let Variant::Float64(value) = deserialize_msgpack::<Variant>(&message).unwrap() else {
                panic!("expected a Float64");
            };
            assert!(
                value == expected || value.is_nan() && expected.is_nan(),
                "{value} should be {expected}"
            );
        }
    }

    #[test]
    fn non_finite_floats_round_trip_through_msgpack() {
        for value in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            let bytes = serialize_msgpack(Variant::Float64(value)).unwrap();
            let Variant::Float64(read) = deserialize_msgpack::<Variant>(&bytes).unwrap() else {
                panic!("expected a Float64");
            };
            assert_eq!(read.to_bits(), value.to_bits());
        }
    }
}
