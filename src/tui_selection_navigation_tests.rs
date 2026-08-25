use crossterm::event::KeyCode;

use super::super::App;
use super::super::test_support::test_app;
use super::super::view::{Focus, IntranetDetailSection, LeftPaneSection};
use super::super::{DIRECT_CLASH_MODE, GLOBAL_CLASH_MODE, RULE_CLASH_MODE};
use super::next_clash_mode;
use crate::private_access::PrivateAccessRoute;
use crate::private_access_session::PrivateAccessProfileRuntime;

#[test]
fn next_clash_mode_cycles_controller_mode_list() {
    let modes = vec![
        GLOBAL_CLASH_MODE.to_string(),
        DIRECT_CLASH_MODE.to_string(),
        RULE_CLASH_MODE.to_string(),
    ];

    assert_eq!(
        next_clash_mode(Some(DIRECT_CLASH_MODE), &modes),
        RULE_CLASH_MODE
    );
    assert_eq!(
        next_clash_mode(Some(RULE_CLASH_MODE), &modes),
        GLOBAL_CLASH_MODE
    );
    assert_eq!(
        next_clash_mode(Some(GLOBAL_CLASH_MODE), &modes),
        DIRECT_CLASH_MODE
    );
}

#[test]
fn next_clash_mode_defaults_to_rule_after_direct() {
    assert_eq!(
        next_clash_mode(Some(DIRECT_CLASH_MODE), &[]),
        RULE_CLASH_MODE
    );
}

#[test]
fn intranet_detail_navigation_scrolls() {
    let mut app = test_app();
    app.private_access.focused_mut().routes = vec![PrivateAccessRoute {
        cidr: "10.20.0.0/16".to_string(),
    }];
    app.focus = Focus::Members;
    app.left_pane_section = LeftPaneSection::Intranet;

    app.move_next();
    assert_eq!(app.intranet_detail_scroll, 1);
    app.move_previous();
    assert_eq!(app.intranet_detail_scroll, 0);
}

#[test]
fn large_intranet_sections_fold_and_toggle_with_enter() {
    let mut app = test_app();
    app.private_access.focused_mut().routes = (0..103)
        .map(|index| PrivateAccessRoute {
            cidr: format!("10.20.{index}.0/24"),
        })
        .collect();
    app.focus = Focus::Members;
    app.left_pane_section = LeftPaneSection::Intranet;

    let route_range = app
        .intranet_detail_view(app.private_access.focused())
        .sections
        .iter()
        .find(|range| range.section == IntranetDetailSection::Routes)
        .copied()
        .expect("routes section");
    assert!(route_range.foldable);

    app.intranet_detail_scroll = route_range.start as u16;
    app.handle_key(KeyCode::Enter).expect("expand routes");
    let section_key = App::intranet_detail_section_key(
        &app.private_access.focused().id,
        IntranetDetailSection::Routes,
    );
    assert!(app.expanded_intranet_sections.contains(&section_key));

    app.handle_key(KeyCode::Enter).expect("fold routes");
    assert!(!app.expanded_intranet_sections.contains(&section_key));
}

#[test]
fn left_pane_navigation_crosses_between_internet_and_intranet_sections() {
    let mut app = test_app();
    app.private_access
        .profiles
        .push(PrivateAccessProfileRuntime::default_sonicwall().expect("SonicWall profile"));
    app.focus = Focus::Groups;
    app.left_pane_section = LeftPaneSection::Internet;

    app.move_next();
    assert_eq!(app.left_pane_section, LeftPaneSection::Intranet);
    assert_eq!(app.private_access.focused_index, 0);

    app.move_next();
    assert_eq!(app.private_access.focused_index, 1);

    app.move_previous();
    assert_eq!(app.private_access.focused_index, 0);
    app.move_previous();
    assert_eq!(app.left_pane_section, LeftPaneSection::Internet);
    assert_eq!(app.displayed_group_index(), 0);
}
