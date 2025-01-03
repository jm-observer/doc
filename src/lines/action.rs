use lsp_types::Position;

use crate::lines::fold::{FoldingDisplayItem, FoldingRange};

pub enum UpdateFolding {
    UpdateByItem(FoldingDisplayItem),
    UpdateByPhantom(Position),
    New(Vec<FoldingRange>)
}

impl From<FoldingDisplayItem> for UpdateFolding {
    fn from(value: FoldingDisplayItem) -> Self {
        Self::UpdateByItem(value)
    }
}

impl From<Position> for UpdateFolding {
    fn from(value: Position) -> Self {
        Self::UpdateByPhantom(value)
    }
}

impl From<Vec<FoldingRange>> for UpdateFolding {
    fn from(value: Vec<FoldingRange>) -> Self {
        Self::New(value)
    }
}
