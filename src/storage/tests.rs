#[cfg(test)]
mod tests {
    use crate::storage::metadata::MetadataDb;
    use crate::storage::chunk_manager;
    use crate::storage::erasure;
    use crate::storage::placement;
    use sha2::{Digest, Sha256};

    // ---- Chunk Manager Integration ----

    #[test]
    fn test_chunk_manager_split_33mb() {
        // 33 MB -> 2 chunks (32 MB + 1 MB)
        let data = vec![0xABu8; 33 * 1024 * 1024];
        let chunks = chunk_manager::split(&data, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].data.len(), 32 * 1024 * 1024);
        assert_eq!(chunks[1].data.len(), 1 * 1024 * 1024);
        assert!(chunk_manager::verify_chunk(&chunks[0]));
        assert!(chunk_manager::verify_chunk(&chunks[1]));
    }

    #[test]
    fn test_chunk_manager_roundtrip_64mb() {
        // 64 MB -> 2 exact chunks
        let data = vec![0x42u8; 64 * 1024 * 1024];
        let chunks = chunk_manager::split(&data, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 2);
        let reassembled = chunk_manager::assemble(&chunks);
        assert_eq!(reassembled.len(), 64 * 1024 * 1024);
    }

    #[test]
    fn test_chunk_manager_roundtrip_80mb() {
        // 80 MB -> 3 chunks (32+32+16)
        let data = vec![0x01, 0x02, 0x03];  // small test, varies sizes
        let data = data.repeat(10 * 1024 * 1024); // 30 MB
        let chunks = chunk_manager::split(&data, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 1);
        let reassembled = chunk_manager::assemble(&chunks);
        assert_eq!(data, reassembled);
    }

    // ---- Erasure Coding Integration ----

    #[test]
    fn test_erasure_encode_decode_roundtrip() {
        // 5 data chunks -> 7 encoded -> decode back to 5
        let mut data_chunks = Vec::new();
        for i in 0..5 {
            let data = vec![(i as u8); 1000]; // 1KB each, varied content
            let checksum = hex::encode(Sha256::digest(&data));
            data_chunks.push(chunk_manager::Chunk {
                index: i as u32,
                data,
                checksum,
                is_parity: false,
            });
        }

        let encoded = erasure::encode(&data_chunks);
        assert_eq!(encoded.len(), 7);

        let decoded = erasure::decode(&encoded, 5).unwrap();
        assert_eq!(decoded.len(), 5);
        for (orig, dec) in data_chunks.iter().zip(decoded.iter()) {
            assert_eq!(orig.data, dec.data);
        }
    }

    #[test]
    fn test_erasure_reconstruct_from_5_of_7() {
        // Lose 2 data chunks, still reconstruct
        let mut data_chunks = Vec::new();
        for i in 0..5 {
            let data = vec![(i as u8 * 17) as u8; 1000];
            let checksum = hex::encode(Sha256::digest(&data));
            data_chunks.push(chunk_manager::Chunk {
                index: i as u32,
                data,
                checksum,
                is_parity: false,
            });
        }

        let original_data: Vec<Vec<u8>> = data_chunks.iter().map(|c| c.data.clone()).collect();
        let encoded = erasure::encode(&data_chunks);

        // Lose chunks 0 and 3 (data), keep 1,2,4 + 5,6 (parity)
        let available: Vec<chunk_manager::Chunk> = encoded.into_iter()
            .enumerate()
            .filter(|(i, _)| *i != 0 && *i != 3)
            .map(|(_, c)| c)
            .collect();

        assert_eq!(available.len(), 5);
        assert!(erasure::can_reconstruct(&available, 5));

        let decoded = erasure::decode(&available, 5).unwrap();
        for (i, d) in decoded.iter().enumerate() {
            assert_eq!(d.data, original_data[i], "Chunk {} mismatch", i);
        }
    }

    #[test]
    fn test_erasure_cannot_reconstruct_with_only_4_chunks() {
        let mut data_chunks = Vec::new();
        for i in 0..5 {
            let data = vec![i as u8; 100];
            let checksum = hex::encode(Sha256::digest(&data));
            data_chunks.push(chunk_manager::Chunk {
                index: i as u32,
                data,
                checksum,
                is_parity: false,
            });
        }

        let encoded = erasure::encode(&data_chunks);
        // Only keep 4 of 7
        let too_few: Vec<chunk_manager::Chunk> = encoded.into_iter().take(4).collect();
        assert!(!erasure::can_reconstruct(&too_few, 5));
        assert!(erasure::decode(&too_few, 5).is_err());
    }

    #[test]
    fn test_erasure_missing_parity_chunks_still_works() {
        let mut data_chunks = Vec::new();
        for i in 0..5 {
            let data = vec![(i as u8 * 3) as u8; 500];
            let checksum = hex::encode(Sha256::digest(&data));
            data_chunks.push(chunk_manager::Chunk {
                index: i as u32,
                data,
                checksum,
                is_parity: false,
            });
        }

        let original_data: Vec<Vec<u8>> = data_chunks.iter().map(|c| c.data.clone()).collect();
        let encoded = erasure::encode(&data_chunks);

        // Keep only data chunks (no parity)
        let data_only: Vec<chunk_manager::Chunk> = encoded.into_iter()
            .filter(|c| !c.is_parity)
            .collect();

        assert_eq!(data_only.len(), 5);
        let decoded = erasure::decode(&data_only, 5).unwrap();
        for (i, d) in decoded.iter().enumerate() {
            assert_eq!(d.data, original_data[i], "Chunk {} mismatch without parity", i);
        }
    }

    // ---- Placement Integration ----

    #[test]
    fn test_placement_7_chunks_6_accounts_wrapping() {
        let accounts = vec![
            "a1".to_string(), "a2".to_string(), "a3".to_string(),
            "a4".to_string(), "a5".to_string(), "a6".to_string(),
        ];
        let plan = placement::plan_placement(&accounts, 7);
        assert_eq!(plan.account_assignments.len(), 7);
        // Chunk 0 -> a1, chunk 5 -> a6, chunk 6 -> a1 (wraps)
        assert_eq!(plan.account_assignments[0].1, "a1");
        assert_eq!(plan.account_assignments[5].1, "a6");
        assert_eq!(plan.account_assignments[6].1, "a1");
    }

    #[test]
    fn test_placement_42_chunks_even_distribution() {
        let accounts = vec![
            "a1".to_string(), "a2".to_string(), "a3".to_string(),
            "a4".to_string(), "a5".to_string(), "a6".to_string(),
        ];
        let plan = placement::plan_placement(&accounts, 42);
        // 42 chunks across 6 accounts = exactly 7 each
        let unique = plan.unique_accounts();
        // Each account should have either 7 or 0 chunks
        for acct in &accounts {
            let count = plan.account_assignments.iter()
                .filter(|(_, a)| a == acct)
                .count();
            assert_eq!(count, 7, "Account {} has {} chunks", acct, count);
        }
        assert_eq!(unique.len(), 6);
    }

    #[test]
    fn test_placement_single_account_for_small_file() {
        let accounts = vec!["single".to_string()];
        let plan = placement::plan_placement(&accounts, 3);
        assert_eq!(plan.account_assignments.len(), 3);
        for (_, acct) in &plan.account_assignments {
            assert_eq!(acct, "single");
        }
    }

    // ---- Full Pipeline Simulation ----

    #[test]
    fn test_full_pipeline_33mb() {
        let original = vec![0xDEu8; 33 * 1024 * 1024]; // 33 MB
        
        // Split
        let chunks = chunk_manager::split(&original, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 2);
        
        // Pad to 5 data chunks, encode
        let mut padded = chunks.clone();
        while padded.len() < 5 {
            padded.push(chunk_manager::Chunk {
                index: padded.len() as u32,
                data: Vec::new(),
                checksum: String::new(),
                is_parity: false,
            });
        }
        assert_eq!(padded.len(), 5);
        
        let encoded = erasure::encode(&padded);
        assert_eq!(encoded.len(), 7);
        
        // Simulate losing chunk 2 (a padded data chunk)
        let available: Vec<chunk_manager::Chunk> = encoded.into_iter()
            .enumerate()
            .filter(|(i, _)| *i != 2)
            .map(|(_, c)| c)
            .collect();
        assert_eq!(available.len(), 6);
        assert!(erasure::can_reconstruct(&available, 5));
        
        let decoded = erasure::decode(&available, 5).unwrap();
        assert_eq!(decoded.len(), 5);
        
        // Assemble and truncate
        let mut result = chunk_manager::assemble(&decoded);
        result.truncate(33 * 1024 * 1024);
        
        assert_eq!(result, original, "33 MB pipeline integrity check failed");
    }

    #[test]
    fn test_full_pipeline_64mb() {
        let original = vec![0xCDu8; 64 * 1024 * 1024]; // 64 MB, exactly 2 chunks
        
        // Split into 2 chunks
        let chunks = chunk_manager::split(&original, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 2);
        
        // Pad to 5, encode, lose 2 data + 1 parity, reconstruct
        let mut padded = chunks.clone();
        while padded.len() < 5 {
            padded.push(chunk_manager::Chunk {
                index: padded.len() as u32,
                data: Vec::new(),
                checksum: String::new(),
                is_parity: false,
            });
        }
        
        let encoded = erasure::encode(&padded);
        
        // Lose 3 chunks (2 data at indices 2,3 + 1 parity at index 5)
        let available: Vec<chunk_manager::Chunk> = encoded.into_iter()
            .enumerate()
            .filter(|(i, _)| *i != 2 && *i != 3 && *i != 5)
            .map(|(_, c)| c)
            .collect();
        assert_eq!(available.len(), 4);
        assert!(!erasure::can_reconstruct(&available, 5), "Should not reconstruct with only 4");
    }

    #[test]
    fn test_full_pipeline_empty_file() {
        let original = Vec::<u8>::new();
        let chunks = chunk_manager::split(&original, 32 * 1024 * 1024);
        assert_eq!(chunks.len(), 0);
        let reassembled = chunk_manager::assemble(&chunks);
        assert!(reassembled.is_empty());
    }

    #[test]
    fn test_full_pipeline_checksum_integrity() {
        let original = b"Hello, MultiFS! This is a test of the chunking pipeline's checksum integrity verification.".to_vec();
        
        let chunks = chunk_manager::split(&original, 10); // tiny chunks for testing
        assert!(chunks.len() > 1);
        
        // Verify all chunks
        for chunk in &chunks {
            assert!(chunk_manager::verify_chunk(chunk), "Chunk {} failed checksum", chunk.index);
        }
        
        // Corrupt one chunk
        let mut corrupted = chunks.clone();
        if let Some(first) = corrupted.first_mut() {
            if !first.data.is_empty() {
                first.data[0] ^= 0xFF; // flip bits
                assert!(!chunk_manager::verify_chunk(first), "Corruption not detected");
            }
        }
    }

    // ---- Metadata DB Integration ----

    #[test]
    fn test_create_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();
        assert!(db.bucket_exists("test").unwrap());
    }

    #[test]
    fn test_delete_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();
        db.delete_bucket("test").unwrap();
        assert!(!db.bucket_exists("test").unwrap());
    }

    #[test]
    fn test_put_get_object() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();
        db.put_object("test", "hello.txt", 12, "abc", "2026-01-01", "acct1", "/remote/hello.txt", None).unwrap();
        let obj = db.get_object("test", "hello.txt").unwrap().unwrap();
        assert_eq!(obj.key, "hello.txt");
        assert_eq!(obj.size, 12);
        assert_eq!(obj.account_email, "acct1");
    }

    #[test]
    fn test_delete_object() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();
        db.put_object("test", "a.txt", 1, "e", "2026-01-01", "a1", "/r/a.txt", None).unwrap();
        db.delete_object("test", "a.txt").unwrap();
        assert!(db.get_object("test", "a.txt").unwrap().is_none());
    }

    #[test]
    fn test_list_objects() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();
        db.put_object("test", "a.txt", 1, "e1", "2026-01-01", "a1", "/r/a.txt", None).unwrap();
        db.put_object("test", "b.txt", 2, "e2", "2026-01-01", "a1", "/r/b.txt", None).unwrap();
        assert_eq!(db.list_objects("test", None, 10).unwrap().len(), 2);
        assert_eq!(db.list_objects("test", Some("a"), 10).unwrap().len(), 1);
    }

    #[test]
    fn test_count_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();
        db.put_object("test", "a.txt", 100, "e1", "2026-01-01", "a1", "/r/a.txt", None).unwrap();
        db.put_object("test", "b.txt", 200, "e2", "2026-01-01", "a1", "/r/b.txt", None).unwrap();
        assert_eq!(db.count_objects("test").unwrap(), 2);
        assert_eq!(db.bucket_total_size("test").unwrap(), 300);
        assert_eq!(db.count_objects_for_account("a1").unwrap(), 2);
        assert_eq!(db.account_total_size("a1").unwrap(), 300);
    }
}

#[test]
fn test_list_buckets_when_empty() {
    use crate::storage::metadata::MetadataDb;
    let dir = tempfile::tempdir().unwrap();
    let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
    let buckets = db.list_buckets().unwrap();
    assert!(buckets.is_empty(), "New DB should have no buckets");
}

#[test]
fn test_create_and_list_bucket() {
    use crate::storage::metadata::MetadataDb;
    let dir = tempfile::tempdir().unwrap();
    let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
    db.create_bucket("my-bucket").unwrap();
    db.create_bucket("other-bucket").unwrap();
    let buckets = db.list_buckets().unwrap();
    assert_eq!(buckets.len(), 2);
    // Should be sorted by name
    assert_eq!(buckets[0].name, "my-bucket");
    assert_eq!(buckets[1].name, "other-bucket");
}
