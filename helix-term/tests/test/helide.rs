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

#[tokio::test(flavor = "multi_thread")]
async fn tabs_open_switch_and_close() -> anyhow::Result<()> {
    let file_a = tempfile::NamedTempFile::new()?;
    let file_b = tempfile::NamedTempFile::new()?;
    let a = path::normalize(file_a.path());
    let b = path::normalize(file_b.path());

    let mut app = helpers::AppBuilder::new().with_file(&a, None).build()?;

    test_key_sequences(
        &mut app,
        vec![
            (
                Some(&format!(":tabnew {}<ret>", b.display())),
                Some(&|app| {
                    assert_eq!(2, app.editor.tree.tab_count());
                    assert_eq!(1, app.editor.tree.active_tab());
                    assert_eq!(current_path(app), b);
                }),
            ),
            // Each tab keeps its own layout: splitting here leaves the first tab alone.
            (
                Some("<C-w>v<C-pageup>"),
                Some(&|app| {
                    assert_eq!(0, app.editor.tree.active_tab());
                    assert_eq!(current_path(app), a);
                    assert_eq!(1, app.editor.tree.views().count());
                    assert_eq!(2, app.editor.tree.tab_views(1).len());
                }),
            ),
            (
                Some(":tabn 2<ret>"),
                Some(&|app| assert_eq!(1, app.editor.tree.active_tab())),
            ),
            // Closing a buffer shown only in a background tab removes that tab.
            (
                Some(&format!(":tabn 1<ret>:buffer-close {}<ret>", b.display())),
                Some(&|app| {
                    assert_eq!(1, app.editor.tree.tab_count());
                    assert_eq!(current_path(app), a);
                }),
            ),
            // :q in a tab closes the tab instead of the editor.
            (
                Some(":tabnew<ret>:q<ret>"),
                Some(&|app| {
                    assert_eq!(1, app.editor.tree.tab_count());
                    assert!(!app.editor.should_close());
                }),
            ),
            (
                Some(":tabc<ret>"),
                Some(&|app| {
                    assert_eq!(1, app.editor.tree.tab_count());
                    assert!(!app.editor.should_close());
                }),
            ),
        ],
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn file_tree_opens_in_new_tab() -> anyhow::Result<()> {
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
            (
                Some("<C-5>jt"),
                Some(&|app| {
                    assert_eq!(2, app.editor.tree.tab_count());
                    assert_eq!(current_path(app), dir_path.join("b.rs"));
                }),
            ),
            // Quitting only closes the tab while others remain.
            (
                Some(":q<ret>"),
                Some(&|app| {
                    assert_eq!(1, app.editor.tree.tab_count());
                    assert_eq!(current_path(app), dir_path.join("a.rs"));
                }),
            ),
        ],
        false,
    )
    .await?;

    Ok(())
}
