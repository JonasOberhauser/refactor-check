#[derive(Debug, PartialEq, Eq)]
pub struct CodePiece {
    id: u64,
    label: String,
    before: String,
    after: String,
}

impl CodePiece {
    pub(crate) fn with_id(id: u64, label: &str, before: &str, after: &str) -> Self {
        Self {
            id,
            label: label.to_string(),
            before: before.to_string(),
            after: after.to_string(),
        }
    }

    pub fn id(&self) -> u64 { self.id }
    pub fn label(&self) -> &str { &self.label }
    pub fn before(&self) -> &str { &self.before }
    pub fn after(&self) -> &str { &self.after }
}