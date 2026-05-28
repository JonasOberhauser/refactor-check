use refactor_check::piece_manager::DefaultPieceManager;

pub fn test_pm() -> DefaultPieceManager {
    DefaultPieceManager::new()
}

pub mod sequence;
pub mod replay;

