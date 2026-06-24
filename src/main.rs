// Modules are wired into `main` incrementally across tasks; some public
// helpers are not yet referenced until later tasks connect them.
#[allow(dead_code)]
mod config;
#[allow(dead_code)]
mod feed;
#[allow(dead_code)]
mod notify;
#[allow(dead_code)]
mod state;

fn main() {
    // Stub entry point; wired up in later tasks.
    log::info!("status-notifications starting");
}
