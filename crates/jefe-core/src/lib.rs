//! jefe-core: the sans-IO supervision loop behind jefe.
//!
//! jefe delegates a goal to coding agents through acc and supervises them. This
//! crate holds the job and worker state and the deterministic loop. It never
//! does I/O: the host feeds it [`Input`]s and runs the [`Effect`]s it returns.

/// Version of the JSON protocol jefe speaks when embedded. Consumers reject
/// majors they don't support.
pub const PROTOCOL_VERSION: u32 = 1;

/// Version of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What happened, for the loop to react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Tick,
}

/// What the host must do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {}

/// The supervision loop.
#[derive(Debug, Default)]
pub struct Jefe {}

impl Jefe {
    pub fn new() -> Self {
        Self {}
    }

    pub fn step(&mut self, _input: Input, _now_ms: u64) -> Vec<Effect> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_loop_does_nothing() {
        assert!(Jefe::new().step(Input::Tick, 0).is_empty());
    }
}
