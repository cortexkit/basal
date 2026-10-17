use std::path::{Path, PathBuf};

/// Publishes the module and worker beside each other so the module can find its worker.
pub fn stage_module_worker(module: &Path, worker: &Path) -> (PathBuf, PathBuf) {
    let module_name = cortexkit_test_support::ckdev_file_name("ck-basal");
    let worker_name = cortexkit_test_support::ckdev_file_name("ck-basal-worker");
    let module_bytes = std::fs::read(module).expect("read module executable");
    let worker_bytes = std::fs::read(worker).expect("read worker executable");
    let directory = cortexkit_test_support::stable_executable_dir(&[
        (module_name.as_str(), module_bytes.as_slice()),
        (worker_name.as_str(), worker_bytes.as_slice()),
    ]);
    (directory.join(module_name), directory.join(worker_name))
}
