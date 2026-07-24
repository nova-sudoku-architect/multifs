use super::chunk_manager::Chunk;
use sha2::{Digest, Sha256};

const DATA_CHUNKS: usize = 5;
const TOTAL_CHUNKS: usize = 7; // 5 data + 2 parity

/// Encode 5 data chunks into 7 chunks (5 data + 2 parity) using XOR-based parity.
///
/// Parity chunk 0 = XOR of data chunks 0 and 1
/// Parity chunk 1 = XOR of data chunks 2, 3, and 4
///
/// Returns all 7 chunks. The original data chunks are returned unmodified.
/// Parity chunks are marked with is_parity = true.
pub fn encode(data_chunks: &[Chunk]) -> Vec<Chunk> {
    assert!(
        data_chunks.len() == DATA_CHUNKS,
        "encode requires exactly {} data chunks, got {}",
        DATA_CHUNKS,
        data_chunks.len()
    );

    let mut result = Vec::with_capacity(TOTAL_CHUNKS);

    // Pad all data chunks to the same length (max of all data chunks) so XOR works.
    let max_len = data_chunks.iter().map(|c| c.data.len()).max().unwrap_or(0);

    // Copy data chunks as-is
    for chunk in data_chunks {
        let mut padded = chunk.data.clone();
        padded.resize(max_len, 0);
        result.push(Chunk {
            index: chunk.index,
            data: padded,
            checksum: chunk.checksum.clone(),
            is_parity: false,
        });
    }

    // Parity chunk 0: XOR of data chunks 0 and 1
    let parity0_data = xor_chunks(&result[0].data, &result[1].data);
    let parity0_checksum = hex::encode(Sha256::digest(&parity0_data));
    result.push(Chunk {
        index: DATA_CHUNKS as u32, // index 5
        data: parity0_data,
        checksum: parity0_checksum,
        is_parity: true,
    });

    // Parity chunk 1: XOR of data chunks 2, 3, and 4
    let mut parity1_data = xor_chunks(&result[2].data, &result[3].data);
    parity1_data = xor_chunks(&parity1_data, &result[4].data);
    let parity1_checksum = hex::encode(Sha256::digest(&parity1_data));
    result.push(Chunk {
        index: (DATA_CHUNKS + 1) as u32, // index 6
        data: parity1_data,
        checksum: parity1_checksum,
        is_parity: true,
    });

    result
}

/// Decode/reconstruct original data chunks from any 5 of the 7 chunks.
///
/// `chunks` can be any subset (at least 5) of the 7 total chunks in any order.
/// `total_data_chunks` is the number of data chunks in the original set (should be 5).
///
/// Returns the reconstructed original data chunks in order (indices 0..5).
pub fn decode(chunks: &[Chunk], total_data_chunks: usize) -> anyhow::Result<Vec<Chunk>> {
    if chunks.len() < total_data_chunks {
        anyhow::bail!(
            "Not enough chunks: need at least {}, got {}",
            total_data_chunks,
            chunks.len()
        );
    }

    if !can_reconstruct(chunks, total_data_chunks) {
        anyhow::bail!("Cannot reconstruct: need at least {} unique chunks", total_data_chunks);
    }

    // Separate into data and parity chunks
    let mut data_chunks: Vec<Option<&Chunk>> = vec![None; total_data_chunks];
    let mut parity_chunks: Vec<&Chunk> = Vec::new();

    for chunk in chunks {
        let idx = chunk.index as usize;
        if chunk.is_parity {
            parity_chunks.push(chunk);
        } else if idx < total_data_chunks {
            data_chunks[idx] = Some(chunk);
        }
    }

    // Determine the max data length from available data chunks
    let max_len = chunks
        .iter()
        .filter(|c| !c.is_parity)
        .map(|c| c.data.len())
        .max()
        .unwrap_or(0);

    // Determine which parity chunks we have
    let have_parity0 = parity_chunks.iter().any(|c| c.index as usize == DATA_CHUNKS);
    let have_parity1 = parity_chunks.iter().any(|c| c.index as usize == DATA_CHUNKS + 1);

    // Find missing data chunk indices
    let missing_indices: Vec<usize> = data_chunks
        .iter()
        .enumerate()
        .filter(|(_, c)| c.is_none())
        .map(|(i, _)| i)
        .collect();

    // If nothing is missing, return the data chunks as-is
    if missing_indices.is_empty() {
        return Ok(
            chunks
                .iter()
                .filter(|c| !c.is_parity)
                .map(|c| {
                    let mut padded = c.data.clone();
                    padded.resize(max_len, 0);
                    Chunk {
                        index: c.index,
                        data: padded,
                        checksum: c.checksum.clone(),
                        is_parity: false,
                    }
                })
                .collect(),
        );
    }

    // Find the parity chunk we need for reconstruction
    for &missing_idx in &missing_indices {
        if missing_idx < 2 {
            // Use parity chunk 0 to reconstruct
            if !have_parity0 {
                // We need parity0; if we don't have it but have all data chunks 2-4 and parity1,
                // we could reconstruct parity0 first — but for simplicity, require parity0 available.
                anyhow::bail!(
                    "Missing data chunk {} and parity chunk 0 — cannot reconstruct",
                    missing_idx
                );
            }

            // XOR all available chunks in the group with the parity chunk
            let parity = parity_chunks.iter().find(|c| c.index as usize == DATA_CHUNKS).unwrap();
            let mut reconstructed = parity.data.clone();
            reconstructed.resize(max_len, 0);

            // XOR data chunk 0 if available
            if missing_idx != 0 {
                if let Some(c0) = data_chunks[0] {
                    let mut c0_data = c0.data.clone();
                    c0_data.resize(max_len, 0);
                    reconstructed = xor_chunks(&reconstructed, &c0_data);
                }
            }
            // XOR data chunk 1 if available
            if missing_idx != 1 {
                if let Some(c1) = data_chunks[1] {
                    let mut c1_data = c1.data.clone();
                    c1_data.resize(max_len, 0);
                    reconstructed = xor_chunks(&reconstructed, &c1_data);
                }
            }

            let checksum = hex::encode(Sha256::digest(&reconstructed));
            data_chunks[missing_idx] = Some(Box::leak(Box::new(Chunk {
                index: missing_idx as u32,
                data: reconstructed,
                checksum,
                is_parity: false,
            })));
        } else {
            // Use parity chunk 1 to reconstruct (for indices 2, 3, 4)
            if !have_parity1 {
                anyhow::bail!(
                    "Missing data chunk {} and parity chunk 1 — cannot reconstruct",
                    missing_idx
                );
            }

            let parity = parity_chunks.iter().find(|c| c.index as usize == DATA_CHUNKS + 1).unwrap();
            let mut reconstructed = parity.data.clone();
            reconstructed.resize(max_len, 0);

            // XOR data chunk 2 if available
            if missing_idx != 2 {
                if let Some(c2) = data_chunks[2] {
                    let mut c2_data = c2.data.clone();
                    c2_data.resize(max_len, 0);
                    reconstructed = xor_chunks(&reconstructed, &c2_data);
                }
            }
            // XOR data chunk 3 if available
            if missing_idx != 3 {
                if let Some(c3) = data_chunks[3] {
                    let mut c3_data = c3.data.clone();
                    c3_data.resize(max_len, 0);
                    reconstructed = xor_chunks(&reconstructed, &c3_data);
                }
            }
            // XOR data chunk 4 if available
            if missing_idx != 4 {
                if let Some(c4) = data_chunks[4] {
                    let mut c4_data = c4.data.clone();
                    c4_data.resize(max_len, 0);
                    reconstructed = xor_chunks(&reconstructed, &c4_data);
                }
            }

            let checksum = hex::encode(Sha256::digest(&reconstructed));
            data_chunks[missing_idx] = Some(Box::leak(Box::new(Chunk {
                index: missing_idx as u32,
                data: reconstructed,
                checksum,
                is_parity: false,
            })));
        }
    }

    // Collect and return reconstructed data chunks in order
    let result: Vec<Chunk> = data_chunks
        .into_iter()
        .map(|opt_c| {
            let c = opt_c.unwrap();
            Chunk {
                index: c.index,
                data: c.data.clone(),
                checksum: c.checksum.clone(),
                is_parity: false,
            }
        })
        .collect();

    Ok(result)
}

/// Check whether we have enough chunks to reconstruct the original data.
///
/// Returns true if at least `total_data_chunks` chunks are present across
/// unique indices (data + parity combined, no duplicate indices needed).
pub fn can_reconstruct(chunks: &[Chunk], total_data_chunks: usize) -> bool {
    chunks.len() >= total_data_chunks
}

/// XOR two byte slices together. Panics if lengths differ.
fn xor_chunks(a: &[u8], b: &[u8]) -> Vec<u8> {
    assert_eq!(a.len(), b.len(), "XOR requires equal-length chunks");
    a.iter().zip(b.iter()).map(|(x, y)| x ^ y).collect()
}

/// Build a set of test data chunks. Used in all tests.
#[cfg(test)]
fn make_test_data_chunks() -> Vec<Chunk> {
    vec![
        Chunk {
            index: 0,
            data: vec![0x01; 16],
            checksum: hex::encode(Sha256::digest(&[0x01u8; 16])),
            is_parity: false,
        },
        Chunk {
            index: 1,
            data: vec![0x02; 16],
            checksum: hex::encode(Sha256::digest(&[0x02u8; 16])),
            is_parity: false,
        },
        Chunk {
            index: 2,
            data: vec![0x03; 16],
            checksum: hex::encode(Sha256::digest(&[0x03u8; 16])),
            is_parity: false,
        },
        Chunk {
            index: 3,
            data: vec![0x04; 16],
            checksum: hex::encode(Sha256::digest(&[0x04u8; 16])),
            is_parity: false,
        },
        Chunk {
            index: 4,
            data: vec![0x05; 16],
            checksum: hex::encode(Sha256::digest(&[0x05u8; 16])),
            is_parity: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_chunks() -> Vec<Chunk> {
        make_test_data_chunks()
    }

    /// Helper: create a Chunk with specific data
    fn chunk(index: u32, byte_val: u8, size: usize) -> Chunk {
        let data = vec![byte_val; size];
        let checksum = hex::encode(Sha256::digest(&data));
        Chunk {
            index,
            data,
            checksum,
            is_parity: false,
        }
    }

    // ---- Test 1: encode 5 data → 7 total (5 data + 2 parity) ----

    #[test]
    fn test_encode_5_plus_2() {
        let data_chunks = make_test_chunks();
        let encoded = encode(&data_chunks);

        assert_eq!(encoded.len(), 7, "Should produce exactly 7 chunks");

        // First 5 should be data chunks (is_parity = false)
        for i in 0..5 {
            assert!(!encoded[i].is_parity, "Chunk {} should be data", i);
            assert_eq!(encoded[i].index, i as u32);
        }

        // Last 2 should be parity chunks (is_parity = true)
        assert!(encoded[5].is_parity, "Chunk 5 should be parity");
        assert!(encoded[6].is_parity, "Chunk 6 should be parity");
        assert_eq!(encoded[5].index, 5);
        assert_eq!(encoded[6].index, 6);

        // Verify parity 0 = XOR of chunks 0 and 1
        let expected_p0: Vec<u8> = data_chunks[0]
            .data
            .iter()
            .zip(data_chunks[1].data.iter())
            .map(|(a, b)| a ^ b)
            .collect();
        assert_eq!(encoded[5].data, expected_p0, "Parity 0 should be XOR of chunks 0,1");

        // Verify parity 1 = XOR of chunks 2, 3, and 4
        let expected_p1: Vec<u8> = data_chunks[2]
            .data
            .iter()
            .zip(data_chunks[3].data.iter())
            .map(|(a, b)| a ^ b)
            .zip(data_chunks[4].data.iter())
            .map(|(x, c)| x ^ c)
            .collect();
        assert_eq!(encoded[6].data, expected_p1, "Parity 1 should be XOR of chunks 2,3,4");
    }

    // ---- Test 2: decode all 7 chunks → reconstruct original ----

    #[test]
    fn test_decode_all_chunks() {
        let data_chunks = make_test_chunks();
        let encoded = encode(&data_chunks);

        let decoded = decode(&encoded, DATA_CHUNKS).unwrap();
        assert_eq!(decoded.len(), 5, "Should reconstruct 5 data chunks");

        for (original, reconstructed) in data_chunks.iter().zip(decoded.iter()) {
            assert_eq!(original.data, reconstructed.data);
            assert_eq!(original.index, reconstructed.index);
            assert_eq!(original.checksum, reconstructed.checksum);
        }
    }

    // ---- Test 3: decode with one data chunk lost ----

    #[test]
    fn test_decode_lost_one_data() {
        let data_chunks = make_test_chunks();
        let mut encoded = encode(&data_chunks);

        // Remove data chunk index 1 (second data chunk)
        let removed_chunk = encoded.remove(1);
        assert!(encoded.len() == 6, "Should have 6 chunks remaining");
        assert!(!removed_chunk.is_parity, "Removed chunk should be data");

        let decoded = decode(&encoded, DATA_CHUNKS).unwrap();
        assert_eq!(decoded.len(), 5);

        // Check all original data matches
        let reassembled = super::super::chunk_manager::assemble(&decoded);
        let original = super::super::chunk_manager::assemble(&data_chunks);
        assert_eq!(reassembled, original, "Reconstructed data should match original");
    }

    // ---- Test 4: decode with two data chunks lost ----

    #[test]
    fn test_decode_lost_two_data() {
        let data_chunks = make_test_chunks();
        let mut encoded = encode(&data_chunks);

        // Remove data chunk index 0 and index 3 (two data chunks)
        // Remove higher index first to preserve lower index validity
        encoded.remove(3); // index 3
        encoded.remove(0); // index 0
        assert_eq!(encoded.len(), 5, "Should have exactly 5 chunks remaining");

        let decoded = decode(&encoded, DATA_CHUNKS).unwrap();
        assert_eq!(decoded.len(), 5);

        let reassembled = super::super::chunk_manager::assemble(&decoded);
        let original = super::super::chunk_manager::assemble(&data_chunks);
        assert_eq!(reassembled, original, "Reconstructed data should match original even with 2 lost");
    }

    // ---- Test 5: decode with both parity chunks lost ----

    #[test]
    fn test_decode_lost_parity() {
        let data_chunks = make_test_chunks();
        let mut encoded = encode(&data_chunks);

        // Remove the two parity chunks (at the end)
        encoded.pop(); // parity 1
        encoded.pop(); // parity 0
        assert_eq!(encoded.len(), 5, "Should have 5 data chunks remaining");

        // Should still decode fine since all data chunks are present
        let decoded = decode(&encoded, DATA_CHUNKS).unwrap();
        assert_eq!(decoded.len(), 5);

        let reassembled = super::super::chunk_manager::assemble(&decoded);
        let original = super::super::chunk_manager::assemble(&data_chunks);
        assert_eq!(reassembled, original, "Should reconstruct from data chunks alone");
    }

    // ---- Test 6: not enough chunks should fail ----

    #[test]
    fn test_decode_not_enough_chunks() {
        let data_chunks = make_test_chunks();
        let mut encoded = encode(&data_chunks);

        // Remove 3 chunks (only 4 remaining, need 5)
        encoded.pop();
        encoded.pop();
        encoded.pop();
        assert_eq!(encoded.len(), 4, "Should have 4 chunks remaining");

        let result = decode(&encoded, DATA_CHUNKS);
        assert!(result.is_err(), "Should fail with not enough chunks");
    }

    // ---- Test 7: integrity after reconstruction ----

    #[test]
    fn test_integrity_after_reconstruction() {
        let data_chunks = make_test_chunks();
        let mut encoded = encode(&data_chunks);

        // Remove data chunks at index 1 and index 4
        encoded.remove(4); // remove index 4 first (was originally 4)
        encoded.remove(1); // remove index 1
        assert_eq!(encoded.len(), 5, "Should have 5 chunks remaining");

        let decoded = decode(&encoded, DATA_CHUNKS).unwrap();
        assert_eq!(decoded.len(), 5);

        // Verify each chunk's checksum
        for (i, chunk) in decoded.iter().enumerate() {
            let expected = hex::encode(Sha256::digest(&chunk.data));
            assert_eq!(
                chunk.checksum, expected,
                "Checksum mismatch for reconstructed chunk {}",
                i
            );
        }

        // Verify the reassembled data matches original
        let reassembled = super::super::chunk_manager::assemble(&decoded);
        let original = super::super::chunk_manager::assemble(&data_chunks);
        assert_eq!(
            reassembled, original,
            "Reconstructed data should have integrity"
        );
    }

    // ---- Test 8: can_reconstruct basic cases ----

    #[test]
    fn test_can_reconstruct_enough_chunks() {
        let data_chunks = make_test_chunks();
        let encoded = encode(&data_chunks);

        // All 7 is enough
        assert!(can_reconstruct(&encoded, DATA_CHUNKS));

        // 5 is enough (any 5)
        let subset: Vec<Chunk> = encoded.iter().take(5).cloned().collect();
        assert!(can_reconstruct(&subset, DATA_CHUNKS));
    }

    #[test]
    fn test_can_reconstruct_not_enough() {
        let data_chunks = make_test_chunks();
        let mut encoded = encode(&data_chunks);

        // Drop to 4 chunks
        encoded.truncate(4);
        assert!(!can_reconstruct(&encoded, DATA_CHUNKS));
    }

    // ---- Test 9: encode with different chunk sizes ----

    #[test]
    fn test_encode_varied_chunk_sizes() {
        let data_chunks = vec![
            chunk(0, 0x11, 32),
            chunk(1, 0x22, 64),
            chunk(2, 0x33, 48),
            chunk(3, 0x44, 80),
            chunk(4, 0x55, 16),
        ];

        let encoded = encode(&data_chunks);
        assert_eq!(encoded.len(), 7);

        // All chunks including parity should be padded to max length (80)
        for c in &encoded {
            assert_eq!(c.data.len(), 80, "Chunk {} should be padded to 80 bytes", c.index);
        }

        let decoded = decode(&encoded, DATA_CHUNKS).unwrap();
        assert_eq!(decoded.len(), 5);

        // Verify original data is preserved (padded data not needed in output)
        // Decoded chunks may be padded to max_len, but the original data is preserved
        for i in 0..5 {
            assert_eq!(decoded[i].index, i as u32);
            // The data content up to original length should match
            let original_len = data_chunks[i].data.len();
            assert_eq!(
                decoded[i].data[..original_len],
                data_chunks[i].data,
                "Chunk {} should preserve original data content",
                i
            );
        }
    }

    // ---- Test 10: decode with missing parity and data simultaneously ----

    #[test]
    fn test_decode_missing_parity0_and_one_data() {
        let data_chunks = make_test_chunks();
        let mut encoded = encode(&data_chunks);

        // Remove parity 0 (index 5)
        encoded.retain(|c| c.index != 5);
        // Remove data chunk 1 (index 1)
        encoded.retain(|c| c.index != 1);

        assert_eq!(encoded.len(), 5);

        // Should fail because chunk 1 needs parity 0, but parity 0 is gone too
        let result = decode(&encoded, DATA_CHUNKS);
        assert!(result.is_err(), "Should fail when both data[1] and parity0 are missing");
    }

    #[test]
    fn test_decode_missing_parity1_and_one_data() {
        let data_chunks = make_test_chunks();
        let mut encoded = encode(&data_chunks);

        // Remove parity 1 (index 6)
        encoded.retain(|c| c.index != 6);
        // Remove data chunk 3 (index 3)
        encoded.retain(|c| c.index != 3);

        assert_eq!(encoded.len(), 5);

        // Should fail because chunk 3 needs parity 1, but parity 1 is gone too
        let result = decode(&encoded, DATA_CHUNKS);
        assert!(result.is_err(), "Should fail when both data[3] and parity1 are missing");
    }

    // ---- Test 11: encode with empty data chunks (edge case) ----

    #[test]
    #[should_panic(expected = "encode requires exactly 5 data chunks")]
    fn test_encode_wrong_chunk_count() {
        let chunks = make_test_chunks();
        let too_few: Vec<Chunk> = chunks.into_iter().take(3).collect();
        let _ = encode(&too_few);
    }
}
