//! Live probe; writes require explicit opt-in and a UUID-named test root.
use cloudreve_file_provider::{crfp_create, crfp_free, crfp_run, crfp_string_free};
use std::{
    ffi::{CStr, CString},
    io::Read,
};
fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    let request: serde_json::Value = serde_json::from_str(&input).unwrap();
    if !matches!(
        request["operation"].as_str(),
        Some("item" | "list" | "fetch")
    ) {
        assert!(std::env::args().any(|arg| arg == "--allow-test-writes"));
        let root =
            cloudreve_api::models::uri::CrUri::new(request["root"].as_str().unwrap()).unwrap();
        let components = root.elements();
        let suffix = components
            .last()
            .unwrap()
            .strip_prefix("macos-fp-smoke-")
            .unwrap();
        uuid::Uuid::parse_str(suffix)
            .expect("write probe requires an isolated UUID test directory");
    }
    unsafe {
        let input = CString::new(input).unwrap();
        let op = crfp_create(input.as_ptr());
        let output = crfp_run(op);
        let mut result: serde_json::Value =
            serde_json::from_slice(CStr::from_ptr(output).to_bytes()).unwrap();
        crfp_string_free(output);
        crfp_free(op);
        result.as_object_mut().unwrap().remove("tokens");
        println!("{}", result);
        if result.get("error").is_some() {
            std::process::exit(1);
        }
    }
}
