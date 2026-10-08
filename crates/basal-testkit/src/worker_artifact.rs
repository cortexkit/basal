/// Use Cargo's executable, not an assumed target directory or profile layout.
pub fn reported_worker(output: &[u8]) -> Option<String> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|message| {
            (message["reason"] == "compiler-artifact"
                && message["target"]["name"] == "ck-basal-worker")
                .then(|| message["executable"].as_str().map(str::to_owned))
                .flatten()
        })
}
