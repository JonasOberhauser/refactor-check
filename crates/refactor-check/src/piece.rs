use refactor_check_core::context_id::ContextId;
use std::sync::Mutex;

#[derive(Debug)]
pub struct CodePiece {
    context_id: Mutex<Option<Box<ContextId>>>,
    ctx_display: String,
    label: String,
    before: String,
    after: String,
}

impl CodePiece {
    pub(crate) fn new(ctx: ContextId, label: &str, before: &str, after: &str) -> Self {
        let ctx_display = ctx.to_string();
        Self {
            context_id: Mutex::new(Some(Box::new(ctx))),
            ctx_display,
            label: label.to_string(),
            before: before.to_string(),
            after: after.to_string(),
        }
    }

    pub fn with_ctx<R>(&self, f: impl FnOnce(&ContextId) -> R) -> R {
        let guard = self.context_id.lock().unwrap();
        let ctx = guard.as_ref().expect("context already taken");
        f(ctx)
    }

    pub fn take_context(&self) -> Box<ContextId> {
        self.context_id.lock().unwrap().take().unwrap_or_else(|| panic!("context already taken for piece {}", self.label))
    }

    pub fn restore_context(&self, ctx: Box<ContextId>) {
        *self.context_id.lock().unwrap() = Some(ctx);
    }

    pub fn ctx_display(&self) -> &str { &self.ctx_display }
    pub fn label(&self) -> &str { &self.label }
    pub fn before(&self) -> &str { &self.before }
    pub fn after(&self) -> &str { &self.after }
}
