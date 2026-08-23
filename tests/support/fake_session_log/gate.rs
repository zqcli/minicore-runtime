use std::fmt;
use std::sync::Arc;

use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct ScriptGate {
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
}

impl ScriptGate {
    pub fn new() -> Self {
        Self {
            entered: Arc::new(Semaphore::new(0)),
            release: Arc::new(Semaphore::new(0)),
        }
    }

    pub async fn wait_entered(&self) {
        let permit = Arc::clone(&self.entered).acquire_owned().await.unwrap();
        permit.forget();
    }

    pub fn release(&self) {
        self.release.add_permits(1);
    }

    pub(super) async fn block(&self) {
        self.entered.add_permits(1);
        let permit = Arc::clone(&self.release).acquire_owned().await.unwrap();
        permit.forget();
    }
}

impl Default for ScriptGate {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ScriptGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ScriptGate")
    }
}
