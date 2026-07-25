use sha2::{Sha256, Digest};

/// A single chunk of a file
#[derive(Debug, Clone)]
pub struct Chunk {
    pub index: u32,
    pub data: Vec<u8>,
    pub checksum: String,
}

/// Split data into fixed-size chunks with checksums
pub fn split(data: &[u8], chunk_size: usize) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut offset = 0;
    let mut index = 0;

    while offset < data.len() {
        let end = std::cmp::min(offset + chunk_size, data.len());
        let chunk_data = data[offset..end].to_vec();
        let checksum = hex::encode(Sha256::digest(&chunk_data));
        
        chunks.push(Chunk {
            index,
            data: chunk_data,
            checksum,
        });

        offset = end;
        index += 1;
    }

    chunks
}

/// Assemble chunks back into original data (in order)
pub fn assemble(chunks: &[Chunk]) -> Vec<u8> {
    let mut sorted: Vec<&Chunk> = chunks.iter().collect();
    sorted.sort_by_key(|c| c.index);
    
    let mut result = Vec::new();
    for chunk in sorted {
        result.extend_from_slice(&chunk.data);
    }
    result
}

/// Verify a chunk's checksum
pub fn verify_chunk(chunk: &Chunk) -> bool {
    let expected = hex::encode(Sha256::digest(&chunk.data));
    expected == chunk.checksum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_small_file() {
        // Single chunk for file smaller than chunk_size
        let data = b"hello world";
        let chunks = split(data, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].index, 0);

        assert_eq!(chunks[0].data, data);
        assert!(verify_chunk(&chunks[0]));
    }

    #[test]
    fn test_split_exact_chunk_size() {
        // Exactly one chunk
        let data = vec![0xABu8; 32 * 1024 * 1024];
        let chunks = split(&data, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].data.len(), 32 * 1024 * 1024);
    }

    #[test]
    fn test_split_multiple_chunks() {
        // 100 MB -> should produce 4 chunks (32+32+32+4)
        let data = vec![0x42u8; 100 * 1024 * 1024];
        let chunks = split(&data, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].data.len(), 32 * 1024 * 1024);
        assert_eq!(chunks[1].data.len(), 32 * 1024 * 1024);
        assert_eq!(chunks[2].data.len(), 32 * 1024 * 1024);
        assert_eq!(chunks[3].data.len(), (100 - 96) * 1024 * 1024);
        
        // Verify all chunks have valid checksums
        for chunk in &chunks {
            assert!(verify_chunk(chunk), "Chunk {} failed checksum", chunk.index);
        }
    }

    #[test]
    fn test_assemble_roundtrip() {
        let original = b"the quick brown fox jumps over the lazy dog".to_vec();
        let chunks = split(&original, 10);
        let reassembled = assemble(&chunks);
        assert_eq!(original, reassembled, "Roundtrip failed");
    }

    #[test]
    fn test_assemble_out_of_order() {
        // Assemble should work even if chunks arrive out of order
        let original = b"1234567890abcdefghij".to_vec();
        let mut chunks = split(&original, 5);
        assert_eq!(chunks.len(), 4);
        
        // Reverse the order
        chunks.reverse();
        
        let reassembled = assemble(&chunks);
        assert_eq!(String::from_utf8_lossy(&reassembled), "1234567890abcdefghij");
    }

    #[test]
    fn test_assemble_large() {
        let original = vec![0x01u8; 65 * 1024 * 1024]; // 65 MB -> 3 chunks
        let chunks = split(&original, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 3);
        
        let reassembled = assemble(&chunks);
        assert_eq!(original.len(), reassembled.len());
        assert_eq!(original, reassembled);
    }

    #[test]
    fn test_empty_file() {
        let data: Vec<u8> = vec![];
        let chunks = split(&data, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 0, "Empty file should have 0 chunks");
        
        let reassembled = assemble(&chunks);
        assert!(reassembled.is_empty());
    }

    #[test]
    fn test_checksum_detects_corruption() {
        let data = b"hello world".to_vec();
        let mut chunks = split(&data, 32 * 1024 * 1024);
        assert!(verify_chunk(&chunks[0]));
        
        // Corrupt the data
        chunks[0].data[0] ^= 0xFF;
        assert!(!verify_chunk(&chunks[0]), "Checksum should detect corruption");
    }

    #[test]
    fn test_chunk_size_parameter() {
        let data = b"hello world".to_vec();
        
        // Test with larger chunk size (fits in 1 chunk)
        let chunks_large = split(&data, 1024);
        assert_eq!(chunks_large.len(), 1);
        
        // Test with tiny chunk size (forces many chunks)
        let chunks_tiny = split(&data, 2);
        assert_eq!(chunks_tiny.len(), 6); // "he" "ll" "o " "wo" "rl" "d"
        
        // Both should reassemble correctly
        assert_eq!(data, assemble(&chunks_large));
        assert_eq!(data, assemble(&chunks_tiny));
    }
}
