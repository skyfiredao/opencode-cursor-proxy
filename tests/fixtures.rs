use std::fs;
use std::path::Path;

#[test]
fn every_fixture_line_is_json() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut checked = 0;
    for entry in fs::read_dir(&dir).expect("fixtures dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("ndjson") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("read fixture");
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("{}:{}: {e}", path.display(), i + 1));
            checked += 1;
        }
    }
    assert!(checked > 0, "no fixture lines found");
}
