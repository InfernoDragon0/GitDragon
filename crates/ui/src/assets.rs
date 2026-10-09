//! App asset source: gpui-kit's default component icons plus the Lucide extras GitDragon uses.

use std::borrow::Cow;

use gpui_kit::assets::{Assets, icon_assets};
use gpui_kit::{AssetSource, Result, SharedString};

icon_assets!(
    pub ExtraIcons,
    [
        Archive,
        ArchiveRestore,
        CircleDot,
        Cloud,
        Columns2,
        Download,
        FileDiff,
        FolderGit2,
        FolderOpen,
        FolderPlus,
        GitBranch,
        GitGraph,
        GitMergeConflict,
        GitPullRequest,
        HardDriveDownload,
        List,
        ListTree,
        Lock,
        LockOpen,
        MessageSquare,
        Minus,
        Moon,
        Package,
        PanelLeft,
        PanelRight,
        Plus,
        RefreshCw,
        Rows2,
        Settings,
        SquareKanban,
        Sun,
        Tag,
        TriangleAlert,
        Undo2,
        Upload,
        X,
    ]
);

/// Register with `gpui_kit::application().with_assets(AppAssets)`.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
