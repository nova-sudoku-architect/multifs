#[cfg(test)]
mod tests {
    use crate::storage::metadata::MetadataDb;

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
