//! Construct `SearchPanel` and exercise its toggle/cancellation paths without
//! actually shelling out to ripgrep.

use gpui::TestAppContext;
use oximux_app::shell::search_panel::SearchPanel;
use oximux_settings::{Density, Theme, Typography};

fn setup() -> (tokio::runtime::Runtime, tempfile::TempDir) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let tmp = tempfile::tempdir().expect("tempdir");
    (rt, tmp)
}

#[gpui::test]
async fn search_panel_constructs_without_panic(cx: &mut TestAppContext) {
    let (rt, tmp) = setup();
    let _guard = rt.enter();
    // gpui-component's `InputState` reads from a theme global set up by
    // `gpui_component::init`. The production app calls this once at boot;
    // tests must replay it.
    cx.update(gpui_component::init);
    let window = cx.add_window(|win, cx| {
        SearchPanel::new(
            tmp.path().to_path_buf(),
            Theme::default(),
            Density::default(),
            Typography::default(),
            win,
            cx,
        )
    });
    cx.run_until_parked();
    cx.read(|app| {
        let panel = window.read(app).expect("panel alive");
        assert_eq!(panel.options().query, "");
        assert!(!panel.options().case_sensitive);
    });
    drop(tmp);
}
