use super::*;

use helix_stdx::path;
use helix_term::application::Application;
use helix_view::current_ref;

/// A scratch directory inside the working directory, so it shows up in the file tree. Its name
/// sorts it first among the tree's directories.
fn scratch_dir() -> anyhow::Result<tempfile::TempDir> {
    Ok(tempfile::Builder::new()
        .prefix("aaa-helide-tree-")
        .tempdir_in(".")?)
}

fn current_path(app: &Application) -> std::path::PathBuf {
    let (_, doc) = current_ref!(app.editor);
    doc.path().unwrap().to_path_buf()
}

#[tokio::test(flavor = "multi_thread")]
async fn file_tree_opens_creates_renames_and_deletes() -> anyhow::Result<()> {
    let dir = scratch_dir()?;
    let dir_path = path::normalize(dir.path().canonicalize()?);
    std::fs::write(dir_path.join("a.rs"), "")?;
    std::fs::write(dir_path.join("b.rs"), "")?;

    let mut app = helpers::AppBuilder::new()
        .with_file(dir_path.join("a.rs"), None)
        .build()?;

    let at = |name: &str| dir_path.join(name);
    test_key_sequences(
        &mut app,
        vec![
            // The tree opens on a.rs; `j` moves to b.rs, enter opens it and returns to the editor.
            (
                Some("<C-5>j<ret>ihi<esc>"),
                Some(&|app| {
                    assert_eq!(current_path(app), at("b.rs"));
                    let (_, doc) = current_ref!(app.editor);
                    assert_eq!(doc.text().to_string(), "hi");
                }),
            ),
            // `a` creates a file next to the selected one and opens it.
            (
                Some("<C-5>ac.rs<ret>"),
                Some(&|app| {
                    assert!(at("c.rs").is_file());
                    assert_eq!(current_path(app), at("c.rs"));
                }),
            ),
            // `r` renames the selected file, and the open buffer follows it.
            (
                Some("<C-5>r<C-u>d.rs<ret>"),
                Some(&|app| {
                    assert!(!at("c.rs").exists());
                    assert!(at("d.rs").is_file());
                    assert_eq!(current_path(app), at("d.rs"));
                }),
            ),
            // `d` deletes after confirmation; anything but `y` keeps the file.
            (
                Some("<esc><C-5>dn<ret>"),
                Some(&|_| assert!(at("d.rs").is_file())),
            ),
            (Some("dy<ret>"), Some(&|_| assert!(!at("d.rs").exists()))),
            // A trailing slash creates a directory.
            (Some("asub/<ret>"), Some(&|_| assert!(at("sub").is_dir()))),
        ],
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn file_tree_keys_toggle_focus() -> anyhow::Result<()> {
    let dir = scratch_dir()?;
    let dir_path = path::normalize(dir.path().canonicalize()?);
    std::fs::write(dir_path.join("a.rs"), "")?;
    std::fs::write(dir_path.join("b.rs"), "")?;

    let mut app = helpers::AppBuilder::new()
        .with_file(dir_path.join("a.rs"), None)
        .build()?;

    test_key_sequences(
        &mut app,
        vec![
            // C-2 hands focus back with the tree still open, so typing edits the buffer.
            (
                Some("<C-5>j<C-2>ix<esc>"),
                Some(&|app| {
                    assert_eq!(current_path(app), dir_path.join("a.rs"));
                    let (_, doc) = current_ref!(app.editor);
                    assert_eq!(doc.text().to_string(), "x");
                }),
            ),
            // C-5 twice shows then hides the tree; keys after it reach the editor.
            (
                Some("<C-5><C-5>iy<esc>"),
                Some(&|app| {
                    let (_, doc) = current_ref!(app.editor);
                    assert_eq!(doc.text().to_string(), "xy");
                }),
            ),
        ],
        false,
    )
    .await?;

    Ok(())
}
