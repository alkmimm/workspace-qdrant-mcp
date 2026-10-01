#[cfg(test)]
mod tests {
    use std::fs;
    use tempfile::TempDir;

    use super::super::scanner::grep_scan_files;
    use super::super::types::FileInfo;

    /// Create temp files with known content and return their paths.
    fn setup_test_files(dir: &TempDir) -> Vec<String> {
        let file1 = dir.path().join("test1.rs");
        fs::write(
            &file1,
            "fn main() {\n    println!(\"hello\");\n    let x = 42;\n}\n",
        )
        .unwrap();

        let file2 = dir.path().join("test2.rs");
        fs::write(
            &file2,
            "use std::io;\nfn read_input() {\n    io::stdin();\n}\n",
        )
        .unwrap();

        vec![
            file1.to_string_lossy().to_string(),
            file2.to_string_lossy().to_string(),
        ]
    }

    fn make_file_infos(paths: &[String]) -> Vec<FileInfo> {
        paths
            .iter()
            .map(|p| FileInfo {
                file_path: p.clone(),
                tenant_id: "test-tenant".to_string(),
                branch: Some("main".to_string()),
            })
            .collect()
    }

    #[test]
    fn test_grep_scan_simple_match() {
        let dir = TempDir::new().unwrap();
        let paths = setup_test_files(&dir);
        let files = make_file_infos(&paths);

        let (matches, truncated) = grep_scan_files("println", false, 0, 100, &files).unwrap();

        assert!(!truncated);
        assert_eq!(matches.len(), 1);
        assert!(matches[0].content.contains("println"));
        assert_eq!(matches[0].line_number, 2);
        assert_eq!(matches[0].tenant_id, "test-tenant");
    }

    #[test]
    fn test_grep_scan_regex_match() {
        let dir = TempDir::new().unwrap();
        let paths = setup_test_files(&dir);
        let files = make_file_infos(&paths);

        let (matches, _) = grep_scan_files(r"fn\s+\w+", false, 0, 100, &files).unwrap();

        assert_eq!(matches.len(), 2); // fn main, fn read_input
    }

    #[test]
    fn test_grep_scan_case_insensitive() {
        let dir = TempDir::new().unwrap();
        let paths = setup_test_files(&dir);
        let files = make_file_infos(&paths);

        let (matches, _) = grep_scan_files("PRINTLN", true, 0, 100, &files).unwrap();

        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_grep_scan_max_results_truncation() {
        let dir = TempDir::new().unwrap();
        let paths = setup_test_files(&dir);
        let files = make_file_infos(&paths);

        let (matches, truncated) = grep_scan_files(r"fn|let|use", false, 0, 2, &files).unwrap();

        assert_eq!(matches.len(), 2);
        assert!(truncated);
    }

    #[test]
    fn test_grep_scan_no_matches() {
        let dir = TempDir::new().unwrap();
        let paths = setup_test_files(&dir);
        let files = make_file_infos(&paths);

        let (matches, truncated) =
            grep_scan_files("NONEXISTENT_PATTERN_XYZ", false, 0, 100, &files).unwrap();

        assert!(matches.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn test_grep_scan_missing_file_skipped() {
        let files = vec![FileInfo {
            file_path: "/nonexistent/file.rs".to_string(),
            tenant_id: "test".to_string(),
            branch: None,
        }];

        let (matches, _) = grep_scan_files("pattern", false, 0, 100, &files).unwrap();

        assert!(matches.is_empty());
    }

    #[test]
    fn test_grep_scan_empty_file_list() {
        let (matches, truncated) = grep_scan_files("pattern", false, 0, 100, &[]).unwrap();

        assert!(matches.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn test_grep_scan_with_context_lines() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("ctx.rs");
        fs::write(&file, "line1\nline2\ntarget_line\nline4\nline5\n").unwrap();

        let files = vec![FileInfo {
            file_path: file.to_string_lossy().to_string(),
            tenant_id: "test".to_string(),
            branch: Some("main".to_string()),
        }];

        let (matches, _) = grep_scan_files("target_line", false, 2, 100, &files).unwrap();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 3);
        assert_eq!(matches[0].context_before, vec!["line1", "line2"]);
        assert_eq!(matches[0].context_after, vec!["line4", "line5"]);
    }

    /// The list this engine scans on disk: one entry per path, never an excluded
    /// one. Several content generations of a path are several `file_metadata`
    /// rows, and without the collapse the scanner opened the same file once per
    /// generation, reporting each of its matches that many times. An excluded
    /// path must not be scanned at all, or its matches spend the result budget.
    #[tokio::test]
    async fn path_exclude_and_generation_collapse_shape_the_scan_list() {
        use crate::code_lines_schema::UPSERT_FILE_METADATA_SQL;
        use crate::search_db::SearchDbManager;
        use crate::text_search::SearchOptions;

        let tmp = TempDir::new().unwrap();
        let db = SearchDbManager::new(&tmp.path().join("scan_list.db"))
            .await
            .unwrap();
        let rows: [(i64, &str); 4] = [
            (1, "/repo/src/a.rs"),
            (2, "/repo/src/a.rs"), // second generation of the same path
            (3, "/repo/old_project/b.rs"),
            (4, "/repo/src/c.rs"),
        ];
        for (file_id, path) in rows {
            sqlx::query(UPSERT_FILE_METADATA_SQL)
                .bind(file_id)
                .bind("proj1")
                .bind(Some("main"))
                .bind(path)
                .bind(None::<&str>)
                .bind(None::<&str>)
                .bind(None::<&str>)
                .bind(None::<i64>)
                .bind(0_i64)
                .execute(db.pool())
                .await
                .unwrap();
        }

        let options = SearchOptions {
            tenant_id: Some("proj1".to_string()),
            path_exclude: Some("old_project/**".to_string()),
            ..Default::default()
        };
        let files = super::super::query::query_file_paths(&db, &options, None)
            .await
            .unwrap();

        let paths: Vec<&str> = files.iter().map(|f| f.file_path.as_str()).collect();
        assert_eq!(paths, vec!["/repo/src/a.rs", "/repo/src/c.rs"]);
    }
}
