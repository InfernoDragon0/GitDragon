//! Hand a conflicted file to VS Code (3-way merge editor) or Zed (inline conflict UI).

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Editor {
    VsCode,
    Zed,
    /// The program picked under Settings > External editor; gets the file like Zed does.
    Custom,
}

impl Editor {
    pub fn label(self) -> &'static str {
        match self {
            Editor::VsCode => "Open in VS Code",
            Editor::Zed => "Open in Zed",
            Editor::Custom => "Open in custom editor",
        }
    }

    fn names(self) -> &'static [&'static str] {
        match (self, cfg!(windows)) {
            (Editor::VsCode, true) => &["code.cmd"],
            (Editor::VsCode, false) => &["code"],
            (Editor::Zed, true) => &["zed.exe"],
            (Editor::Zed, false) => &["zed", "zeditor"],
            (Editor::Custom, _) => &[],
        }
    }

    /// Install locations checked when the editor is not on `PATH`.
    fn known_locations(self) -> Vec<PathBuf> {
        let env = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let mut out = Vec::new();
        match self {
            Editor::VsCode => {
                if let Some(local) = env("LOCALAPPDATA") {
                    out.push(local.join("Programs/Microsoft VS Code/bin/code.cmd"));
                }
                if let Some(pf) = env("ProgramFiles") {
                    out.push(pf.join("Microsoft VS Code/bin/code.cmd"));
                }
                out.push(
                    "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code".into(),
                );
                out.push("/usr/bin/code".into());
                out.push("/snap/bin/code".into());
            }
            Editor::Custom => {}
            Editor::Zed => {
                if let Some(local) = env("LOCALAPPDATA") {
                    out.push(local.join("Programs/Zed/bin/zed.exe"));
                    out.push(local.join("Programs/Zed/zed.exe"));
                }
                out.push("/Applications/Zed.app/Contents/MacOS/cli".into());
                if let Some(home) = env("HOME") {
                    out.push(home.join(".local/bin/zed"));
                }
            }
        }
        out
    }

    /// The editor's absolute path, if installed.
    pub fn find(self) -> Option<PathBuf> {
        let dirs = git_core::detect::absolute_path_dirs(std::env::var_os("PATH"));
        dirs.iter()
            .flat_map(|dir| self.names().iter().map(move |n| dir.join(n)))
            .chain(self.known_locations())
            .find(|p| p.is_absolute() && p.is_file())
    }
}

/// Editors found on this machine (checked once per call; cheap file probes), the custom
/// program from Settings first when it is still an existing absolute path.
pub fn available(custom: Option<&Path>) -> Vec<(Editor, PathBuf)> {
    let custom = custom
        .filter(|p| p.is_absolute() && p.is_file())
        .map(|p| (Editor::Custom, p.to_path_buf()));
    custom
        .into_iter()
        .chain(
            [Editor::VsCode, Editor::Zed]
                .into_iter()
                .filter_map(|e| e.find().map(|p| (e, p))),
        )
        .collect()
}

/// Button text: the fixed name, or the custom program's file name.
pub fn label(editor: Editor, program: &Path) -> String {
    match (editor, program.file_stem()) {
        (Editor::Custom, Some(stem)) => format!("Open in {}", stem.to_string_lossy()),
        _ => editor.label().to_string(),
    }
}

/// The three stage files written for VS Code's merge editor.
pub struct StageFiles {
    pub ours: PathBuf,
    pub theirs: PathBuf,
    pub base: PathBuf,
}

/// Writes the stages (missing ones as empty files) into a new temp dir, named after `file`.
pub fn write_stages(
    file: &Path,
    stages: &git_core::conflicts::Stages,
) -> std::io::Result<StageFiles> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("gitdragon-merge-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let name = file
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let write = |side: &str, content: &Option<Vec<u8>>| -> std::io::Result<PathBuf> {
        let path = dir.join(format!("{side}.{name}"));
        std::fs::write(&path, content.as_deref().unwrap_or_default())?;
        Ok(path)
    };
    Ok(StageFiles {
        ours: write("ours", &stages.ours)?,
        theirs: write("theirs", &stages.theirs)?,
        base: write("base", &stages.base)?,
    })
}

/// Arguments for `editor`: VS Code's merge editor over the stage files writing into `result`,
/// or Zed on the worktree file (it resolves conflict markers inline).
pub fn args(editor: Editor, result: &Path, stages: Option<&StageFiles>) -> Option<Vec<PathBuf>> {
    let absolute = |p: &Path| p.is_absolute().then(|| p.to_path_buf());
    match (editor, stages) {
        (Editor::VsCode, Some(s)) => {
            let mut out = vec![PathBuf::from("--merge"), PathBuf::from("--")];
            for p in [&s.ours, &s.theirs, &s.base] {
                out.push(absolute(p)?);
            }
            out.push(absolute(result)?);
            Some(out)
        }
        (Editor::VsCode, None) => Some(vec![PathBuf::from("--"), absolute(result)?]),
        (Editor::Zed | Editor::Custom, _) => Some(vec![absolute(result)?]),
    }
}

/// Starts `program` (absolute) with `args`; does not wait.
pub fn launch(program: &Path, args: &[PathBuf]) -> std::io::Result<()> {
    if !program.is_absolute() {
        return Err(std::io::Error::other("editor path is not absolute"));
    }
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn().map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_absolute_and_after_double_dash() {
        let root = std::env::temp_dir();
        let stages = StageFiles {
            ours: root.join("ours.f"),
            theirs: root.join("theirs.f"),
            base: root.join("base.f"),
        };
        let result = root.join("f");
        let args = args(Editor::VsCode, &result, Some(&stages)).unwrap();
        assert_eq!(args[0], PathBuf::from("--merge"));
        assert_eq!(args[1], PathBuf::from("--"));
        assert_eq!(args.len(), 6);
        assert!(args[2..].iter().all(|p| p.is_absolute()));
        assert_eq!(args[5], result);
        // A relative path (could start with `-`) is refused.
        assert!(args_for_relative().is_none());
        assert_eq!(super::args(Editor::Zed, &result, None).unwrap(), [result]);
    }

    fn args_for_relative() -> Option<Vec<PathBuf>> {
        args(Editor::Zed, Path::new("-rf"), None)
    }

    #[test]
    fn custom_editor_comes_first_and_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("myedit.exe");
        std::fs::write(&program, "").unwrap();
        let found = available(Some(&program));
        assert_eq!(found[0], (Editor::Custom, program.clone()));
        assert_eq!(label(Editor::Custom, &program), "Open in myedit");
        assert_eq!(label(Editor::Zed, &program), "Open in Zed");
        // Missing, relative or a folder: ignored.
        for bad in [
            dir.path().join("gone"),
            PathBuf::from("rel/ed"),
            dir.path().to_path_buf(),
        ] {
            assert!(
                available(Some(&bad))
                    .iter()
                    .all(|(e, _)| *e != Editor::Custom)
            );
        }
        // The custom editor gets just the file, as an absolute path.
        let file = dir.path().join("f.txt");
        assert_eq!(args(Editor::Custom, &file, None).unwrap(), [file]);
        assert!(args(Editor::Custom, Path::new("-x"), None).is_none());
    }

    #[test]
    fn launch_needs_an_absolute_program() {
        assert!(launch(Path::new("code"), &[]).is_err());
    }
}
