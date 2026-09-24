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

    /// # Panics
    ///
    /// Panics if the piece's context was already taken (via
    /// [`Self::take_context`]) and not restored.
    pub fn with_ctx<R>(&self, f: impl FnOnce(&ContextId) -> R) -> R {
        // into_inner is sound here: poisoning can only originate in a
        // panic inside `f`, which only shared-reads the context; the
        // Option transitions under this lock are single atomic ops, so
        // the state is never left broken.
        let guard = self.context_id.lock().unwrap_or_else(|e| e.into_inner());
        let ctx = guard.as_ref().expect("context already taken");
        f(ctx)
    }

    /// A double-take (take without a paired [`Self::restore_context`]) is
    /// a caller logic bug: it is reported as an `Err` carrying the piece
    /// label instead of panicking.
    pub fn take_context(&self) -> Result<Box<ContextId>, String> {
        // See with_ctx: poisoning cannot leave the Option broken.
        self.context_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| format!("context already taken for piece {}", self.label))
    }

    pub fn restore_context(&self, ctx: Box<ContextId>) {
        // See with_ctx: poisoning cannot leave the Option broken.
        *self.context_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctx);
    }

    pub fn ctx_display(&self) -> &str { &self.ctx_display }
    pub fn label(&self) -> &str { &self.label }
    pub fn before(&self) -> &str { &self.before }
    pub fn after(&self) -> &str { &self.after }
}
