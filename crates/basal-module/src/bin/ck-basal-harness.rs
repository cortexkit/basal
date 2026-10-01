//! `ck-basal-harness`: the module against the mock hosts, over stdio. See
//! `basal_module::harness`.

fn main() -> std::process::ExitCode {
    basal_module::harness::main()
}
