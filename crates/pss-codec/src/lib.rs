use thiserror::Error;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("no base64 marker ('eyJ') found in the decompressed stream")]
    NoBase64Marker,
    #[error("base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Decompress an LZ4 "block format" stream (no size header), tolerant to truncation: stops cleanly
/// as soon as there isn't enough data left to finish the current sequence, instead of panicking.
///
/// Direct port of the algorithm validated against 14 control battles.
pub fn lz4_block_decompress(data: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0usize;

    while i < data.len() {
        let tok = data[i];
        i += 1;

        let mut lit = (tok >> 4) as usize;
        if lit == 15 {
            loop {
                if i >= data.len() {
                    return out;
                }
                let b = data[i];
                i += 1;
                lit += b as usize;
                if b != 255 {
                    break;
                }
            }
        }

        if i + lit > data.len() {
            out.extend_from_slice(&data[i..]);
            return out;
        }

        out.extend_from_slice(&data[i..i + lit]);
        i += lit;

        if i + 2 > data.len() {
            return out;
        }

        let off = u16::from_le_bytes([data[i], data[i + 1]]) as usize;
        i += 2;

        if off == 0 || off > out.len() {
            return out;
        }

        let mut ml = (tok & 0x0f) as usize;
        if ml == 15 {
            loop {
                if i >= data.len() {
                    return out;
                }

                let b = data[i];
                i += 1;
                ml += b as usize;

                if b != 255 {
                    break;
                }
            }
        }

        ml += 4;

        let start = out.len() - off;

        for k in 0..ml {
            let byte = out[start + k];
            out.push(byte);
        }
    }

    out
}

/// Decodes an intercepted `FinalBattle` request body: skips the 3-byte HTTP header prefix,
/// decompresses the LZ4 block, locates the base64 stream (marked by the `eyJ` pattern i.e. an
/// encoded `{"`), and decodes it into raw JSON.
pub fn decode_battle_report(raw: &[u8]) -> Result<String, CodecError> {
    let body = if raw.len() >= 3 { &raw[3..] } else { &[][..] };
    let decompressed = lz4_block_decompress(body);
    let text = String::from_utf8_lossy(&decompressed);

    let start = text.find("eyJ").ok_or(CodecError::NoBase64Marker)?;
    let mut b64 = text[start..].trim().to_string();

    let rem = b64.len() % 4;
    if rem != 0 {
        b64.push_str(&"=".repeat(4 - rem));
    }

    let json_bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &b64)?;
    let value: serde_json::Value = serde_json::from_slice(&json_bytes)?;

    Ok(serde_json::to_string(&value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes a minimal "literal-only" LZ4 block (no back-reference):
    /// enough to build test vectors, since our decoder tolerates the absence
    /// of a trailing offset/matchlen sequence.
    fn encode_literal_only_block(literal: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let len = literal.len();
        if len < 15 {
            out.push((len as u8) << 4);
        } else {
            out.push(0xf0);

            let mut rem = len - 15;
            while rem >= 255 {
                out.push(255);
                rem -= 255;
            }

            out.push(rem as u8);
        }

        out.extend_from_slice(literal);
        out
    }

    #[test]
    fn lz4_block_decompress_literal_only() {
        let literal = b"hello world, this is a literal-only lz4 block";
        let compressed = encode_literal_only_block(literal);
        let decompressed = lz4_block_decompress(&compressed);
        assert_eq!(decompressed, literal);
    }

    #[test]
    fn lz4_block_decompress_with_backreference() {
        // "ab" (literal) + back-reference offset=2, matchlen=2+4=6 -> repeats "ab" 3 times
        let mut compressed = vec![2u8 << 4]; // token: 2 literals, matchlen field = 0

        compressed.extend_from_slice(b"ab");
        compressed.extend_from_slice(&2u16.to_le_bytes()); // offset = 2
        compressed[0] |= 2; // matchlen field = 2 -> ml = 2 + 4 = 6

        let decompressed = lz4_block_decompress(&compressed);
        assert_eq!(decompressed, b"abababab".to_vec());
    }

    #[test]
    fn lz4_block_decompress_truncated_input_does_not_panic() {
        let truncated = vec![0x50, 0x01, 0x02]; // announces literal len=5, but data is missing
        let decompressed = lz4_block_decompress(&truncated);
        assert_eq!(decompressed, vec![0x01, 0x02]); // whatever remains is returned as-is
    }

    #[test]
    fn decode_battle_report_roundtrip() {
        use base64::Engine;

        let json = r#"{"battleReport":{"a":1,"b":"test"}}"#;
        let b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
        let compressed = encode_literal_only_block(b64.as_bytes());

        let mut raw = vec![0x00, 0x00, 0x00]; // simulated 3-byte HTTP header prefix
        raw.extend_from_slice(&compressed);

        let decoded = decode_battle_report(&raw).expect("decode should succeed");
        let value: serde_json::Value = serde_json::from_str(&decoded).unwrap();
        assert_eq!(value["battleReport"]["a"], 1);
        assert_eq!(value["battleReport"]["b"], "test");
    }
}
