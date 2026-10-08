use std::path::Path;

use pss_codec::decode_battle_report;

#[test]
fn decodes_real_finalise_battle_capture() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/pss_31_req.bin");
    let raw = std::fs::read(&path).expect("Failed to read docs/pss_31_req.bin");

    let decoded = decode_battle_report(&raw).expect("decode should succeed on a real capture");
    let value: serde_json::Value =
        serde_json::from_str(&decoded).expect("decoded output must be valid JSON");

    assert!(value.get("battleReport").is_some());
}
