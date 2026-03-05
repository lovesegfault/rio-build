//! rio-controller binary. F7 fills this in with the full
//! Controller::run loops + autoscaler. For F1, just a placeholder
//! so the crate compiles with a main.rs bin target.

fn main() {
    // F7 lands: figment config, kube::Client::try_default,
    // two Controller::new().owns().run() futures merged via
    // stream::select, Autoscaler::run spawned. For now: no-op.
    eprintln!("rio-controller: stub (F7 lands main loop)");
}
