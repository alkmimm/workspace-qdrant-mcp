//! Test file and test directory detection utilities.

use std::path::Path;
use wqm_common::classification;

use super::classify::get_extension;

/// Check if a file is a test file based on naming conventions and path.
///
/// Test detection is independent of file_type — a test file is always also code.
/// Non-code files (e.g., `test_data.txt`) are NOT classified as test files.
///
/// Detects:
/// - Filename patterns: `test_*`, `*_test.*`, `*.test.*`, `*.spec.*`, `conftest.*`
/// - JVM class conventions: `FooTest.java`, `FooTests.kt`, `FooIT.java`, `FooSpec.groovy`
/// - Files under test directories: `tests/`, `test/`, `__tests__/`, `spec/`, `__spec__/`
///
/// Returns true only if the file has a code extension AND matches a test pattern.
pub fn is_test_file(file_path: &Path) -> bool {
    let extension = get_extension(file_path);

    // Must be a code file to be a test
    if !classification::is_file_type(&extension, "code") {
        return false;
    }

    let raw_filename = file_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");

    // The JVM check runs on the ORIGINAL filename — its safety comes from the
    // CamelCase boundary, which lowercasing destroys.
    if has_jvm_test_class_suffix(raw_filename, &extension) {
        return true;
    }

    let filename = raw_filename.to_lowercase();

    // Check filename patterns
    if has_test_filename_pattern(&filename) {
        return true;
    }

    // Check if under a test directory
    is_in_test_directory(file_path)
}

/// JVM ecosystem test-class conventions: `FooTest.java`, `FooTests.kt`,
/// `FooIT.java` / `FooITCase.java` (Failsafe integration tests), `FooSpec.groovy`
/// (Spock). None of them use a separator, so every pattern in
/// [`has_test_filename_pattern`] — all of which key on `_` or `.` — missed the
/// entire convention.
///
/// Measured consequence (DOC-V2, 2026-09-06): the Gradle `testFixtures` source
/// set holds `…/port/NotificationRepositoryContractTest.java` and friends. They
/// matched no filename pattern and no test directory, so they counted as
/// PRODUCTION — 7 of the top-25 "most critical untested symbols" were methods
/// of contract-test base classes. A standard Maven layout escapes this only
/// because `src/test/java/` supplies a test DIRECTORY.
///
/// Matched case-SENSITIVELY and only for JVM extensions, because the CamelCase
/// boundary is what makes it safe: `Latest.java` ends in a lowercase `test` and
/// must not match, while `ContractTest.java` ends in an uppercase-`T` `Test`
/// preceded by a lowercase letter. The preceding character must be lowercase or
/// a digit, so an acronym like `EDIT.java` or `UNIT.java` cannot match `IT`.
/// Only `.java`/`.kt`/`.kts` are listed: `is_test_file` returns early unless the
/// extension is in the `code` file-type set, and `.groovy`/`.scala` are not in it
/// today — listing them here would be dead code. Add them to both places
/// together if that set ever grows.
fn has_jvm_test_class_suffix(filename: &str, extension: &str) -> bool {
    const JVM_EXTENSIONS: &[&str] = &[".java", ".kt", ".kts"];
    if !JVM_EXTENSIONS.contains(&extension.to_lowercase().as_str()) {
        return false;
    }
    let stem = match filename.rfind('.') {
        Some(pos) => &filename[..pos],
        None => filename,
    };
    ["Test", "Tests", "IT", "ITCase", "Spec"]
        .iter()
        .filter_map(|suffix| stem.strip_suffix(suffix))
        .any(|prefix| {
            prefix
                .chars()
                .next_back()
                .is_some_and(|prev| prev.is_lowercase() || prev.is_ascii_digit())
        })
}

/// Check if a directory is a test directory.
///
/// Common test directory names:
/// - tests, test, __tests__
/// - spec, specs
/// - integration, e2e, unit
pub fn is_test_directory(directory_path: &Path) -> bool {
    let dir_name = directory_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_lowercase();

    classification::is_test_directory_name(&dir_name)
}

/// Check if a filename matches test file patterns.
fn has_test_filename_pattern(filename: &str) -> bool {
    // Common test file prefixes
    if filename.starts_with("test_") {
        return true;
    }

    // Get filename without extension
    let name_without_ext = if let Some(pos) = filename.rfind('.') {
        &filename[..pos]
    } else {
        filename
    };

    // Common test file suffixes
    if name_without_ext.ends_with("_test") {
        return true;
    }

    // .test. and .spec. patterns (JS/TS ecosystem)
    if filename.contains(".test.") || filename.contains(".spec.") {
        return true;
    }

    // Dot-separated suffixes
    if name_without_ext.ends_with(".test") || name_without_ext.ends_with(".spec") {
        return true;
    }

    // Special test file names (only with code extensions)
    if name_without_ext == "conftest" || name_without_ext == "test" || name_without_ext == "tests" {
        return true;
    }

    false
}

/// Check if a file is under a test directory.
fn is_in_test_directory(file_path: &Path) -> bool {
    for ancestor in file_path.ancestors() {
        let dir_name = ancestor
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_lowercase();

        if classification::is_test_directory_name(&dir_name) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The JVM convention uses no separator, so every pre-existing pattern
    /// (all of which key on an underscore or a dot) missed it. DOC-V2
    /// measured the cost:
    /// contract-test base classes counted as production and took 7 of the
    /// top-25 "most critical untested symbols".
    #[test]
    fn jvm_test_class_suffixes_are_detected() {
        for name in [
            "NotificationRepositoryContractTest.java",
            "UserServiceTests.kt",
            "PaymentFlowIT.java",
            "PaymentFlowITCase.java",
            "BuildLogicTest.kts",
            "Regression2Test.java",
        ] {
            assert!(
                is_test_file(Path::new(name)),
                "{name} follows a JVM test-class convention and must classify as a test",
            );
        }
    }

    /// The CamelCase boundary is the whole safety argument: a lowercase "test"
    /// tail or an acronym ending in "IT" must NOT be swept in.
    #[test]
    fn jvm_suffix_check_respects_the_camelcase_boundary() {
        for name in ["Latest.java", "EDIT.java", "UNIT.java", "Manifest.kt"] {
            assert!(
                !is_test_file(Path::new(name)),
                "{name} is production code — the JVM suffix check must not match it",
            );
        }
    }

    /// Scoped to JVM extensions on purpose: outside the JVM a bare "Test"
    /// suffix is not a convention, and matching it would reclassify
    /// production files.
    #[test]
    fn jvm_suffix_check_is_extension_scoped() {
        assert!(!is_test_file(Path::new("FooTest.py")));
        assert!(!is_test_file(Path::new("FooTest.rs")));
        // The separator conventions still apply to those languages.
        assert!(is_test_file(Path::new("foo_test.rs")));
        assert!(is_test_file(Path::new("test_foo.py")));
    }

    /// Gradle testFixtures: shared test scaffolding that ships in no production
    /// artifact. It matched neither a filename pattern nor a test directory.
    #[test]
    fn gradle_test_fixtures_source_set_is_a_test_directory() {
        assert!(is_test_file(Path::new(
            "doc-backend/domain/src/testFixtures/java/com/doc/domain/port/Helper.java"
        )));
        // Case-insensitive: the on-disk name is camelCase.
        assert!(is_test_file(Path::new("a/testfixtures/b/Helper.java")));
    }

    /// Guardrail against over-reach: the directory list must not swallow a
    /// production path that merely contains a similar word.
    #[test]
    fn similar_directory_names_stay_production() {
        assert!(!is_test_file(Path::new("src/fixtures/Loader.java")));
        assert!(!is_test_file(Path::new("src/latest/Loader.java")));
    }

    /// Pre-existing behavior must not regress.
    #[test]
    fn established_conventions_still_hold() {
        assert!(is_test_file(Path::new("foo.test.ts")));
        assert!(is_test_file(Path::new("foo.spec.ts")));
        assert!(is_test_file(Path::new("conftest.py")));
        assert!(is_test_file(Path::new("tests/helper.rs")));
        assert!(!is_test_file(Path::new("src/main.rs")));
        // Non-code files are never tests, whatever they are called.
        assert!(!is_test_file(Path::new("test_data.txt")));
    }
}
