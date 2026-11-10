use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};
use tokio::sync::{Notify, Semaphore};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    Locked,
    Charged,
    BeforeCancelCheck,
    PassedCancelCheck,
    Committed,
}

struct Hook {
    stage: Stage,
    reached: Notify,
    resume: Semaphore,
    lost_ack: bool,
}

fn registry() -> &'static Mutex<HashMap<String, Arc<Hook>>> {
    static HOOKS: OnceLock<Mutex<HashMap<String, Arc<Hook>>>> = OnceLock::new();
    HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) struct Guard {
    id: String,
    hook: Arc<Hook>,
}
impl Guard {
    pub(super) fn install(id: &str, stage: Stage, lost_ack: bool) -> Self {
        assert!(id.len() <= 256);
        let hook = Arc::new(Hook {
            stage,
            reached: Notify::new(),
            resume: Semaphore::new(0),
            lost_ack,
        });
        let mut entries = registry().lock().unwrap();
        assert!(entries.len() < 4 && !entries.contains_key(id));
        entries.insert(id.to_owned(), hook.clone());
        Self {
            id: id.to_owned(),
            hook,
        }
    }
    pub(super) async fn reached(&self) {
        self.hook.reached.notified().await;
    }
    pub(super) fn release(&self) {
        self.hook.resume.add_permits(1);
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        registry().lock().unwrap().remove(&self.id);
        self.release();
    }
}

pub(super) async fn visit(id: &str, stage: Stage) -> bool {
    let hook = registry().lock().unwrap().get(id).cloned();
    let Some(hook) = hook.filter(|hook| hook.stage == stage) else {
        return false;
    };
    hook.reached.notify_one();
    let _permit = hook.resume.acquire().await.expect("test gate remains open");
    hook.lost_ack
}
